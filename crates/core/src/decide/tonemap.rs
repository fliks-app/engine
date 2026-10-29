//! Tone-map path resolution and the effective encoder backend of a transcode.
use super::codec::{Encoder, resolve_descriptor};
use crate::api::{CodecVariant, HostCaps, HwAccel, Platform, TonemapAlgo};
use crate::hw::GpuDeint;
use crate::hw::decoders::{DecodeCodec, Surface, source_decoder};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TonemapPath {
    Vaapi,
    Opencl,
    Qsv,
    Vulkan,
}

/// Windows QSV OpenCL is the D3D11 bridge; elsewhere it is the VAAPI-derived one.
fn opencl_bridge_ok(caps: &HostCaps, has_crop: bool) -> bool {
    match (caps.platform, has_crop) {
        (Platform::Win32, _) => caps.probe("qsvOpencl"),
        (_, true) => caps.probe("tonemapOpenclCrop"),
        (_, false) => caps.probe("tonemapOpencl"),
    }
}

pub struct TonemapNeeds {
    pub has_crop: bool,
    pub dv_no_base: bool,
    pub dv_apply_rpu: bool,
}

pub fn resolve_tonemap_path(caps: &HostCaps, algo: TonemapAlgo, n: &TonemapNeeds) -> TonemapPath {
    // A no-base DV source overrides the admin pick: OpenCL, then Vulkan.
    if n.dv_no_base {
        if opencl_bridge_ok(caps, n.has_crop) {
            return TonemapPath::Opencl;
        }
        if caps.has_vaapi() && caps.probe("vulkan") {
            return TonemapPath::Vulkan;
        }
    }
    if (algo == TonemapAlgo::Auto || n.dv_apply_rpu) && opencl_bridge_ok(caps, n.has_crop) {
        return TonemapPath::Opencl;
    }
    if algo == TonemapAlgo::Auto && !caps.has_vaapi() && caps.probe("vppQsv") {
        return TonemapPath::Qsv;
    }
    match algo {
        TonemapAlgo::Opencl => TonemapPath::Opencl,
        TonemapAlgo::Qsv => TonemapPath::Qsv,
        TonemapAlgo::Auto | TonemapAlgo::Vaapi => TonemapPath::Vaapi,
    }
}

/// A no-base DV source reshaped to HDR10 needs its own probed `apply_dovi=1` recipe per GPU path.
pub fn dv_no_base_hdr10_path_supported(
    caps: &HostCaps,
    algo: TonemapAlgo,
    has_crop: bool,
    has_burn_in: bool,
) -> bool {
    match caps.hw_accel {
        HwAccel::Videotoolbox => false,
        HwAccel::Nvenc if caps.probe("cuda") => caps.probe("cudaHdr10"),
        HwAccel::Nvenc | HwAccel::Amf if caps.probe("opencl") => caps.probe("openclHdr10"),
        HwAccel::Nvenc | HwAccel::Amf => true,
        hw => {
            let needs = TonemapNeeds {
                has_crop,
                dv_no_base: true,
                dv_apply_rpu: false,
            };
            match resolve_tonemap_path(caps, algo, &needs) {
                TonemapPath::Opencl if caps.platform == Platform::Win32 => {
                    caps.probe("qsvOpenclHdr10")
                }
                TonemapPath::Opencl => caps.probe("tonemapOpenclHdr10"),
                path => !(path == TonemapPath::Vulkan && hw == HwAccel::Vaapi && !has_burn_in),
            }
        }
    }
}

pub struct EncodeContext<'a> {
    pub crop: bool,
    /// Text burn-in only: an image burn-in composites without leaving the GPU.
    pub burn_in: bool,
    pub tonemap: bool,
    pub algo: TonemapAlgo,
    pub source_video_codec: &'a str,
    pub dv_no_base: bool,
    pub dv_apply_rpu: bool,
    /// Gates the AMF zero-copy decoder (H.264 Hi10P exceeds its 8-bit max).
    pub source_bit_depth: u32,
    /// An interlaced source needs the host's hardware deinterlacer on the QSV-native chain.
    pub interlaced: bool,
}

/// The encode pipeline a frozen variant runs on this host, shared by the decision and the argv.
/// [ts:backend/src/modules/streaming/transcoding/encode-pipeline.ts:147-259]
pub struct Pipeline {
    pub requested: HwAccel,
    pub encoder: Option<&'static Encoder>,
    /// The backend the encoder really runs on, after the registry's CPU fallback.
    pub effective: HwAccel,
    pub path: TonemapPath,
    pub qsv_native: bool,
    pub use_vaapi_tonemap: bool,
    pub amf_opencl_available: bool,
}

/// QSV crop splice to VAAPI, CPU for a no-base DV tone-map without a bridge, registry CPU fallback.
pub fn resolve_pipeline(variant: &CodecVariant, c: &EncodeContext, caps: &HostCaps) -> Pipeline {
    let hw = caps.hw_accel;
    let codec = DecodeCodec::from_source(c.source_video_codec);
    let path = resolve_tonemap_path(
        caps,
        c.algo,
        &TonemapNeeds {
            has_crop: c.crop,
            dv_no_base: c.dv_no_base,
            dv_apply_rpu: c.tonemap && c.dv_apply_rpu,
        },
    );
    let decoder_on = |suffix: &str| {
        codec.is_some_and(|k| {
            !caps
                .disabled_decoders
                .contains(&format!("{}{suffix}", k.name()))
        })
    };
    let qsv_decoder_usable = hw == HwAccel::Qsv
        && !c.burn_in
        && decoder_on(if caps.platform == Platform::Win32 {
            "_qsv_d3d11_decode"
        } else {
            "_qsv_decode"
        });
    let amf_max_depth = codec.map_or(10, DecodeCodec::max_bit_depth);
    let amf_opencl_available = hw == HwAccel::Amf
        && caps.probe("amfOpencl")
        && c.source_bit_depth <= amf_max_depth
        && decoder_on("_d3d11va_native_decode");
    // The Vulkan chain maps a VAAPI surface: a source its decoder cannot take would reach it as CPU frames.
    let vulkan_usable = path == TonemapPath::Vulkan
        && c.tonemap
        && !c.burn_in
        && hw == HwAccel::Vaapi
        && source_decoder(Some(c.source_video_codec), c.source_bit_depth, hw, caps).surface()
            == Surface::Vaapi;
    let dv_no_base_needs_cpu =
        c.dv_no_base && c.tonemap && path != TonemapPath::Opencl && !vulkan_usable;
    let qsv_native_available = !dv_no_base_needs_cpu
        && qsv_decoder_usable
        && (!c.interlaced || caps.probe(GpuDeint::Qsv.probe_name()))
        && (!caps.has_vaapi() || c.crop || matches!(path, TonemapPath::Qsv | TonemapPath::Opencl))
        && (!c.tonemap
            || (path == TonemapPath::Qsv && caps.probe("vppQsv"))
            || (path == TonemapPath::Opencl && opencl_bridge_ok(caps, c.crop)));
    let qsv_can_crop =
        qsv_native_available || (hw == HwAccel::Qsv && c.crop && c.tonemap && !c.burn_in);

    // QSV cannot crop on the VAAPI-decode chain without a native path: splice to VAAPI.
    let mut requested = if hw == HwAccel::Qsv && c.crop && !qsv_can_crop && caps.has_vaapi() {
        HwAccel::Vaapi
    } else {
        hw
    };
    if (!caps.has_vaapi() && hw == HwAccel::Qsv && !qsv_native_available)
        || (dv_no_base_needs_cpu && matches!(hw, HwAccel::Qsv | HwAccel::Vaapi))
        || (c.tonemap && path == TonemapPath::Vulkan && c.burn_in)
    {
        requested = HwAccel::None;
    }
    let encoder = resolve_descriptor(variant, requested, caps);
    let effective = encoder.map_or(HwAccel::None, |e| e.hw);
    let falls_back_to_vaapi =
        c.tonemap && path == TonemapPath::Qsv && !qsv_native_available && caps.has_vaapi();
    Pipeline {
        requested,
        encoder,
        effective,
        path,
        qsv_native: qsv_native_available && effective == HwAccel::Qsv,
        use_vaapi_tonemap: !dv_no_base_needs_cpu
            && c.tonemap
            && effective != HwAccel::Amf
            && (path == TonemapPath::Vaapi || falls_back_to_vaapi),
        amf_opencl_available,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::VideoCodec;

    fn pipeline(disabled: &[&str]) -> Pipeline {
        let caps = HostCaps {
            hw_accel: HwAccel::Vaapi,
            probes: vec!["vulkan".into()],
            disabled_decoders: disabled.iter().map(|d| d.to_string()).collect(),
            ..HostCaps::default()
        };
        let variant = CodecVariant {
            codec: VideoCodec::H264,
            bit_depth: 8,
            hdr: None,
        };
        let c = EncodeContext {
            crop: false,
            burn_in: false,
            tonemap: true,
            algo: TonemapAlgo::Auto,
            source_video_codec: "hevc",
            dv_no_base: true,
            dv_apply_rpu: true,
            source_bit_depth: 10,
            interlaced: false,
        };
        resolve_pipeline(&variant, &c, &caps)
    }

    #[test]
    fn vulkan_needs_a_vaapi_surface_decoder() {
        assert_eq!(pipeline(&[]).requested, HwAccel::Vaapi);
        let cpu = pipeline(&["hevc_vaapi_decode", "hevc_qsv_decode"]);
        assert_eq!(cpu.requested, HwAccel::None);
    }
}

//! Decoder choice, tonemap bounces and the per-stage report.

use super::super::VodArgs;
use crate::api::{CodecVariant, HdrFormat, HostCaps, HwAccel, Platform};
use crate::decide::codec::codec_name;
use crate::decide::tonemap::{Pipeline, TonemapPath};
use crate::hw::Device;
use crate::hw::GpuDeint;
use crate::hw::decoders::{DECODERS, DecodeCodec, Decoder, Surface, find};
use crate::strs;

pub(super) struct Sdr {
    pub(super) space: String,
    pub(super) primaries: String,
    pub(super) transfer: String,
    pub(super) range: &'static str,
}

/// Source tags for SDR->SDR, BT.709 for a tone-map; an untagged axis falls back to BT.709 limited.
/// [ts:backend/src/modules/streaming/transcoding/ffmpeg-args.ts:268-290]
pub(super) fn sdr_tags(o: &VodArgs) -> Sdr {
    let tag = |v: &Option<String>| {
        v.as_deref()
            .filter(|v| !v.is_empty() && *v != "unknown" && *v != "reserved")
            .unwrap_or("bt709")
            .to_string()
    };
    let full = matches!(
        o.source_color_range
            .as_deref()
            .map(str::to_lowercase)
            .as_deref(),
        Some("pc" | "full" | "jpeg")
    );
    if o.tonemap {
        return Sdr {
            space: "bt709".into(),
            primaries: "bt709".into(),
            transfer: "bt709".into(),
            range: "tv",
        };
    }
    Sdr {
        space: tag(&o.source_color_space),
        primaries: tag(&o.source_color_primaries),
        transfer: tag(&o.source_color_transfer),
        range: if full { "pc" } else { "tv" },
    }
}

pub(super) fn color_args(c: &Sdr) -> Vec<String> {
    strs(&[
        "-color_primaries",
        &c.primaries,
        "-color_trc",
        &c.transfer,
        "-colorspace",
        &c.space,
        "-color_range",
        c.range,
    ])
}

/// `{"bitDepth":..,"codec":..,"hdr":..}`: the host's `JSON.stringify` of the stored variant.
pub(super) fn variant_json(v: &CodecVariant) -> String {
    let hdr = match v.hdr {
        None => "null",
        Some(HdrFormat::Hdr10) => "\"HDR10\"",
        Some(HdrFormat::Hlg) => "\"HLG\"",
    };
    format!(
        "{{\"bitDepth\":{},\"codec\":\"{}\",\"hdr\":{hdr}}}",
        v.bit_depth,
        codec_name(v.codec)
    )
}

pub(super) fn hw_name(h: HwAccel) -> &'static str {
    match h {
        HwAccel::Vaapi => "vaapi",
        HwAccel::Nvenc => "nvenc",
        HwAccel::Qsv => "qsv",
        HwAccel::Amf => "amf",
        HwAccel::Videotoolbox => "videotoolbox",
        HwAccel::None => "none",
    }
}

/// Decoder registry: same backend, then any other HW one, then CPU.
/// [ts:backend/src/modules/streaming/transcoding/codec/decoders/index.ts:66-103]
fn resolve_decoder(
    codec: DecodeCodec,
    depth: u32,
    preferred: HwAccel,
    caps: &HostCaps,
) -> &'static Decoder {
    let registry = DECODERS.iter().take_while(|d| d.id != "cpu");
    let usable = |d: &&Decoder| {
        d.supported(caps.platform)
            && d.codec.is_none_or(|c| c == codec)
            && depth <= d.max_bit_depth()
            && !caps.disabled_decoders.iter().any(|x| x == d.id)
    };
    registry
        .clone()
        .filter(|d| d.hw() == preferred)
        .find(usable)
        .or_else(|| {
            (preferred != HwAccel::None)
                .then(|| {
                    registry
                        .filter(|d| d.hw() != preferred && d.hw() != HwAccel::None)
                        .find(usable)
                })
                .flatten()
        })
        .unwrap_or_else(|| find("cpu"))
}

/// VideoToolbox frames stay on the GPU only if its decoder is there and, for an interlaced source,
/// so is its deinterlacer.
fn vt_surface_decodable(codec: Option<DecodeCodec>, interlaced: bool, caps: &HostCaps) -> bool {
    (!interlaced || caps.probe(GpuDeint::Videotoolbox.probe_name()))
        && codec.is_some_and(|c| {
            resolve_decoder(c, 10, HwAccel::Videotoolbox, caps).hw() == HwAccel::Videotoolbox
        })
}

/// The GPU deinterlacer of the surface the chain runs on, if the host probed it.
pub(super) fn gpu_deint(
    p: &Pipeline,
    surface: Surface,
    interlaced: bool,
    caps: &HostCaps,
) -> Option<GpuDeint> {
    let g = match (p.effective, surface) {
        (HwAccel::Qsv, _) if p.qsv_native => GpuDeint::Qsv,
        (HwAccel::Vaapi | HwAccel::Qsv, Surface::Vaapi) => GpuDeint::Vaapi,
        (HwAccel::Nvenc, Surface::Cuda) => GpuDeint::Cuda,
        (HwAccel::Videotoolbox, Surface::Videotoolbox) => GpuDeint::Videotoolbox,
        _ => return None,
    };
    (interlaced && caps.probe(g.probe_name())).then_some(g)
}

pub(super) struct DecodeStage {
    pub(super) args: Vec<String>,
    pub(super) decoder: &'static Decoder,
    pub(super) vt_metal: bool,
    pub(super) vt_surface: bool,
    pub(super) qsv_opencl_tonemap: bool,
}

pub(super) struct DecodeNeeds<'a> {
    pub(super) source_video_codec: Option<&'a str>,
    pub(super) source_bit_depth: u32,
    pub(super) tonemap: bool,
    pub(super) hdr_output: bool,
    pub(super) burn_in: bool,
    pub(super) crop: bool,
    pub(super) opencl_tonemap: bool,
    pub(super) interlaced: bool,
}

/// The decoder a resolved pipeline reads the source with. [ts:backend/src/modules/streaming/transcoding/ffmpeg-args.ts:660-705]
fn pick_decoder(
    p: &Pipeline,
    source_video_codec: Option<&str>,
    depth: u32,
    caps: &HostCaps,
) -> &'static Decoder {
    let hw = p.effective;
    let codec = source_video_codec.and_then(DecodeCodec::from_source);
    let reported = source_video_codec.is_some_and(|c| !c.is_empty());
    match codec {
        Some(c) if p.qsv_native => find(&format!(
            "{}{}",
            c.name(),
            if caps.platform == Platform::Win32 {
                "_qsv_d3d11_decode"
            } else {
                "_qsv_decode"
            }
        )),
        Some(c) if p.amf_opencl_available && hw == HwAccel::Amf => {
            find(&format!("{}_d3d11va_native_decode", c.name()))
        }
        // A reported codec without a HW decoder here decodes on the CPU.
        _ => resolve_decoder(
            codec.unwrap_or(DecodeCodec::H264),
            depth,
            if reported && codec.is_none() {
                HwAccel::None
            } else {
                hw
            },
            caps,
        ),
    }
}

/// `tonemap_cuda` on NVENC, else the OpenCL bounce of NVENC/AMF and of no-base or RPU sources.
/// [ts:backend/src/modules/streaming/transcoding/encode-pipeline.ts:31-53]
pub(super) fn tonemap_bounces(
    tonemap: bool,
    hw: HwAccel,
    dv_no_base: bool,
    dv_apply_rpu: bool,
    caps: &HostCaps,
) -> (bool, bool) {
    let cuda = tonemap && hw == HwAccel::Nvenc && caps.probe("cuda");
    let opencl = tonemap
        && !cuda
        && caps.probe("opencl")
        && (matches!(hw, HwAccel::Nvenc | HwAccel::Amf)
            || dv_no_base
            || (dv_apply_rpu && hw == HwAccel::None));
    (cuda, opencl)
}

/// What a session's pipeline reports: its tone-map step, whether that step takes a curve, and
/// its decoder. [ts:backend/src/modules/streaming/transcoding/encode-pipeline.ts:306-358]
pub(crate) struct StageReport {
    pub tonemap: Option<&'static str>,
    pub curve_applies: bool,
    pub decoder_hw: HwAccel,
    pub decoder_id: &'static str,
}

pub(crate) struct ReportNeeds<'a> {
    pub tonemap: bool,
    pub dv_no_base: bool,
    pub dv_apply_rpu: bool,
    /// Any burn-in.
    pub burn_in: bool,
    pub source_video_codec: Option<&'a str>,
    pub source_bit_depth: u32,
    /// No-base DV reshaped to HDR10: `apply_dovi` runs no curve.
    pub hdr10_target: bool,
    pub interlaced: bool,
}

pub(crate) fn stage_report(p: &Pipeline, n: &ReportNeeds, caps: &HostCaps) -> StageReport {
    let hw = p.effective;
    let (cuda, opencl) = tonemap_bounces(n.tonemap, hw, n.dv_no_base, n.dv_apply_rpu, caps);
    let codec = n.source_video_codec.and_then(DecodeCodec::from_source);
    let tonemap = n.tonemap.then(|| match hw {
        HwAccel::Qsv | HwAccel::Vaapi if p.use_vaapi_tonemap => "vaapi",
        HwAccel::Qsv | HwAccel::Vaapi => match p.path {
            TonemapPath::Vaapi => "vaapi",
            TonemapPath::Opencl => "opencl",
            TonemapPath::Qsv => "qsv",
            TonemapPath::Vulkan => "vulkan",
        },
        _ if cuda => "cuda",
        _ if opencl || (hw == HwAccel::Amf && p.amf_opencl_available) => "opencl",
        HwAccel::Videotoolbox if !n.burn_in && vt_surface_decodable(codec, n.interlaced, caps) => {
            "videotoolbox"
        }
        _ => "cpu",
    });
    let decoder = pick_decoder(p, n.source_video_codec, n.source_bit_depth, caps);
    StageReport {
        tonemap,
        curve_applies: !n.hdr10_target
            && matches!(tonemap, Some("opencl" | "vulkan" | "cpu" | "cuda")),
        decoder_hw: decoder.hw(),
        decoder_id: decoder.id,
    }
}

/// How the source reaches memory, plus the tone-map device bridges.
/// [ts:backend/src/modules/streaming/transcoding/ffmpeg-args.ts:660-882]
pub(super) fn decode_stage(
    p: &Pipeline,
    n: &DecodeNeeds,
    caps: &HostCaps,
    dev: &Device,
) -> DecodeStage {
    let hw = p.effective;
    let codec = n.source_video_codec.and_then(DecodeCodec::from_source);
    let amf_native = p.amf_opencl_available && hw == HwAccel::Amf;
    let decoder = pick_decoder(p, n.source_video_codec, n.source_bit_depth, caps);
    let surface = decoder.surface();
    let mut args = decoder.input_args(dev);
    // A CPU decoder inits no device, yet the GPU encoder's filters need one.
    if surface == Surface::Cpu && hw == HwAccel::Qsv {
        args.extend(dev.qsv_init());
        args.extend(strs(&["-filter_hw_device", "qs"]));
    }
    if surface == Surface::Cpu && hw == HwAccel::Vaapi {
        args.extend(dev.vaapi_init());
        args.extend(strs(&["-filter_hw_device", "va"]));
    }
    let vulkan = p.path == TonemapPath::Vulkan && surface == Surface::Vaapi && hw == HwAccel::Vaapi;
    if let Some(k) = args.iter().position(|a| a == "-filter_hw_device")
        && vulkan
    {
        let tail = args.split_off(k + 2);
        args = dev.vulkan_init();
        args.extend(tail);
    }
    let vt_eligible = n.tonemap
        && hw == HwAccel::Videotoolbox
        && !n.burn_in
        && vt_surface_decodable(codec, n.interlaced, caps);
    let vt_passthrough = n.hdr_output
        && hw == HwAccel::Videotoolbox
        && !n.burn_in
        && !n.crop
        && vt_surface_decodable(codec, n.interlaced, caps);
    let vt_surface = vt_eligible || vt_passthrough;
    if vt_surface {
        args.extend(strs(&["-hwaccel_output_format", "videotoolbox_vld"]));
    }
    if n.tonemap
        && !p.use_vaapi_tonemap
        && surface == Surface::Vaapi
        && !p.qsv_native
        && !n.opencl_tonemap
        && p.path != TonemapPath::Vulkan
        && hw != HwAccel::Nvenc
    {
        args.extend(strs(&["-init_hw_device", "opencl=ocl:0.0"]));
    }
    if n.opencl_tonemap && !amf_native {
        args.extend(dev.opencl_tonemap_init());
    }
    let qsv_opencl_tonemap = n.tonemap
        && p.path == TonemapPath::Opencl
        && hw == HwAccel::Qsv
        && surface == Surface::D3d11;
    if let Some(k) = args.iter().rposition(|a| a == "-filter_hw_device")
        && qsv_opencl_tonemap
    {
        args[k + 1] = "ocl".into();
        args.splice(k..k, strs(&["-init_hw_device", "opencl=ocl@dx"]));
    }
    DecodeStage {
        args,
        decoder,
        vt_metal: vt_eligible && !n.crop,
        vt_surface,
        qsv_opencl_tonemap,
    }
}

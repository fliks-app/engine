//! Output codec selection: which variant the encoder registry can produce for this client.
use crate::api::{CodecVariant, DeviceProfile, HdrFormat, HostCaps, HwAccel, Platform, VideoCodec};
use crate::ladder::resolution_fits_cap;
use std::collections::HashSet;

/// ffprobe codec name to `VideoCodec`, aliases collapsed.
pub fn normalise_source_codec(name: &str) -> Option<VideoCodec> {
    match name.to_lowercase().as_str() {
        "h264" | "avc" | "avc1" => Some(VideoCodec::H264),
        "hevc" | "h265" | "hvc1" | "hev1" => Some(VideoCodec::Hevc),
        "av1" | "av01" => Some(VideoCodec::Av1),
        _ => None,
    }
}

/// One compiled-in encoder. The bit depth follows `hdr` (10 with HDR, else 8).
struct Encoder {
    hw: HwAccel,
    codec: VideoCodec,
    hdr: Option<HdrFormat>,
}

const fn enc(hw: HwAccel, codec: VideoCodec, hdr: Option<HdrFormat>) -> Encoder {
    Encoder { hw, codec, hdr }
}

use HwAccel::{Amf, Nvenc, Qsv, Vaapi, Videotoolbox};
use VideoCodec::{Av1, H264, Hevc};
const SDR: Option<HdrFormat> = None;
const HDR10: Option<HdrFormat> = Some(HdrFormat::Hdr10);
const HLG: Option<HdrFormat> = Some(HdrFormat::Hlg);

/// In registry order: HW entries first, CPU fallbacks last.
const ENCODERS: &[Encoder] = &[
    enc(Qsv, Hevc, SDR),
    enc(Qsv, Hevc, HDR10),
    enc(Qsv, Hevc, HLG),
    enc(Vaapi, Hevc, SDR),
    enc(Vaapi, Hevc, HDR10),
    enc(Vaapi, Hevc, HLG),
    enc(Nvenc, Hevc, SDR),
    enc(Nvenc, Hevc, HDR10),
    enc(Nvenc, Hevc, HLG),
    enc(Amf, Hevc, SDR),
    enc(Amf, Hevc, HDR10),
    enc(Amf, Hevc, HLG),
    enc(Videotoolbox, Hevc, SDR),
    enc(Videotoolbox, Hevc, HDR10),
    enc(Videotoolbox, Hevc, HLG),
    enc(Qsv, Av1, SDR),
    enc(Qsv, Av1, HDR10),
    enc(Vaapi, Av1, SDR),
    enc(Vaapi, Av1, HDR10),
    enc(Nvenc, Av1, SDR),
    enc(Nvenc, Av1, HDR10),
    enc(Nvenc, Av1, HLG),
    enc(Amf, Av1, SDR),
    enc(Amf, Av1, HDR10),
    enc(Amf, Av1, HLG),
    enc(Qsv, H264, SDR),
    enc(Vaapi, H264, SDR),
    enc(Nvenc, H264, SDR),
    enc(Amf, H264, SDR),
    enc(Videotoolbox, H264, SDR),
    enc(HwAccel::None, Hevc, SDR),
    enc(HwAccel::None, Hevc, HDR10),
    enc(HwAccel::None, Hevc, HLG),
    enc(HwAccel::None, Av1, SDR),
    enc(HwAccel::None, Av1, HDR10),
    enc(HwAccel::None, Av1, HLG),
    enc(HwAccel::None, H264, SDR),
];

impl Encoder {
    /// Descriptor id as the boot probe names it: `hevc_qsv_main10`, `av1_nvenc_hlg`, `libx265`.
    fn id(&self) -> String {
        let hdr = match (self.codec, self.hdr) {
            (_, None) => "",
            (Hevc, Some(HdrFormat::Hdr10)) => "_main10",
            (Hevc, Some(HdrFormat::Hlg)) => "_hlg",
            (_, Some(HdrFormat::Hdr10)) => "_hdr10",
            (_, Some(HdrFormat::Hlg)) => "_hlg",
        };
        if self.hw == HwAccel::None {
            let base = match self.codec {
                H264 => "libx264",
                Hevc => "libx265",
                Av1 => "libsvtav1",
            };
            return format!("{base}{hdr}");
        }
        let (codec, hw) = (codec_name(self.codec), hw_name(self.hw));
        format!("{codec}_{hw}{hdr}")
    }

    fn usable(&self, caps: &HostCaps) -> bool {
        if self.hw == Amf && caps.platform != Platform::Win32 {
            return false;
        }
        // VAAPI writes HDR metadata only on some drivers; every other HDR encoder does.
        if self.hdr.is_some() && self.hw == Vaapi && !caps.vaapi_writes_hdr_metadata {
            return false;
        }
        !caps.disabled_encoders.contains(&self.id())
    }
}

pub fn codec_name(c: VideoCodec) -> &'static str {
    match c {
        H264 => "h264",
        Hevc => "hevc",
        Av1 => "av1",
    }
}

fn hw_name(h: HwAccel) -> &'static str {
    match h {
        Qsv => "qsv",
        Vaapi => "vaapi",
        Nvenc => "nvenc",
        Amf => "amf",
        Videotoolbox => "videotoolbox",
        HwAccel::None => "none",
    }
}

/// First usable encoder for `variant` on `hw`, else the CPU one of the same variant.
pub fn resolve_encoder(variant: &CodecVariant, hw: HwAccel, caps: &HostCaps) -> Option<HwAccel> {
    let find = |on: HwAccel| {
        ENCODERS.iter().find(|e| {
            e.hw == on && e.codec == variant.codec && e.hdr == variant.hdr && e.usable(caps)
        })
    };
    find(hw)
        .or_else(|| (hw != HwAccel::None).then(|| find(HwAccel::None)).flatten())
        .map(|e| e.hw)
}

/// Like `resolve_encoder` but a CPU fallback does not count on a HW host.
fn resolve_hw_encoder(variant: &CodecVariant, hw: HwAccel, caps: &HostCaps) -> bool {
    resolve_encoder(variant, hw, caps).is_some_and(|got| got == hw)
}

pub struct SourceForSelector {
    pub width: u32,
    pub height: u32,
    pub hdr: Option<HdrFormat>,
    pub codec: Option<VideoCodec>,
}

/// Clients send aliases (`hvc1`, `avc1`, `av01`), so both sides are normalised.
fn client_supports(set: &HashSet<String>, codec: VideoCodec) -> bool {
    let any = |names: &[&str]| names.iter().any(|n| set.contains(*n));
    match codec {
        H264 => any(&["h264", "avc1"]),
        Hevc => any(&["hevc", "h265", "hvc1", "hev1"]),
        Av1 => any(&["av1", "av01"]),
    }
}

/// A codec whose declared decode ceiling cannot fit the source is unusable for the master.
fn client_decodes_resolution(d: &DeviceProfile, codec: VideoCodec, w: u32, h: u32) -> bool {
    let name = codec_name(codec);
    match d
        .codec_conditions
        .iter()
        .flatten()
        .find(|c| c.codec == name)
    {
        None => true,
        Some(c) => resolution_fits_cap(Some(w), Some(h), c.max_width, c.max_height),
    }
}

/// The top-ranked variant: HDR first when the source is HDR and the client shows or tone-maps it,
/// then SDR, each HW-first with a CPU pass when no HW encoder exists. `h264` SDR if nothing fits.
pub fn pick_primary_variant(
    source: &SourceForSelector,
    d: &DeviceProfile,
    caps: &HostCaps,
) -> CodecVariant {
    let hw = caps.hw_accel;
    let client_codecs: HashSet<String> = d
        .direct_play_profiles
        .iter()
        .flat_map(|p| p.video_codecs.iter().map(|c| c.to_lowercase()))
        .collect();
    let efficiency = [Av1, Hevc, H264];
    let order: Vec<VideoCodec> = if d.reject_copy == Some(true) {
        efficiency
            .into_iter()
            .filter(|c| Some(*c) != source.codec)
            .chain(source.codec)
            .collect()
    } else if source
        .codec
        .is_some_and(|c| client_supports(&client_codecs, c))
    {
        source
            .codec
            .into_iter()
            .chain(efficiency.into_iter().filter(|c| Some(*c) != source.codec))
            .collect()
    } else {
        efficiency.to_vec()
    };
    let eligible = |codec: VideoCodec| {
        client_supports(&client_codecs, codec)
            && client_decodes_resolution(d, codec, source.width, source.height)
    };

    if let Some(hdr) = source
        .hdr
        .filter(|_| d.supports_hdr == Some(true) || d.tonemaps_hdr_locally == Some(true))
    {
        let variants = order
            .iter()
            .filter(|c| **c != H264 && eligible(**c))
            .map(|c| CodecVariant {
                codec: *c,
                bit_depth: 10,
                hdr: Some(hdr),
            });
        let first_hw = variants.clone().find(|v| resolve_hw_encoder(v, hw, caps));
        let first_any = || {
            variants
                .clone()
                .find(|v| resolve_encoder(v, hw, caps).is_some())
        };
        if let Some(v) = first_hw.or_else(first_any) {
            return v;
        }
    }

    let sdr = || {
        order
            .iter()
            .filter(|c| eligible(**c))
            .map(|c| CodecVariant {
                codec: *c,
                bit_depth: 8,
                hdr: None,
            })
    };
    sdr()
        .find(|v| resolve_hw_encoder(v, hw, caps))
        .or_else(|| sdr().find(|v| resolve_encoder(v, hw, caps).is_some()))
        .unwrap_or(CodecVariant {
            codec: H264,
            bit_depth: 8,
            hdr: None,
        })
}

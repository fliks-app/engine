//! Dolby Vision classification and the checks that gate a copy.
use crate::api::{DeviceProfile, HdrFormat, VideoStreamInfo};

/// Single-layer profiles (5, 8, 10) carry DV in the base NALs, so a raw copy keeps it; dual-layer
/// P7's enhancement layer cannot ride HLS.
pub struct DvInfo {
    pub profile: Option<u32>,
    pub compat_id: Option<u32>,
    pub single_layer: bool,
}

pub fn derive(v: Option<&VideoStreamInfo>) -> DvInfo {
    let profile = v.and_then(|v| v.dv_profile);
    DvInfo {
        profile,
        compat_id: v.and_then(|v| v.dv_bl_signal_compat_id),
        single_layer: matches!(profile, Some(5 | 8 | 10))
            && v.and_then(|v| v.dv_el_present) != Some(true),
    }
}

/// Profiles the client presents; a bare `supportsDolbyVision` means 5 and 8, never 10.
pub fn client_profiles(d: &DeviceProfile) -> Vec<i32> {
    match (&d.dolby_vision_profiles, d.supports_dolby_vision) {
        (Some(p), _) => p.clone(),
        (None, Some(true)) => vec![5, 8],
        _ => vec![],
    }
}

/// P5, or P10 with compat 0/unknown: no HDR10/HLG base to fall back to.
pub fn has_no_base(profile: Option<u32>, compat: Option<u32>) -> bool {
    profile == Some(5) || (profile == Some(10) && matches!(compat, Some(0) | None))
}

/// No base, or an HLG base (P8.4/P10.4): the RPU must be applied when tone-mapping on GPU.
pub fn applies_rpu(profile: Option<u32>, compat: Option<u32>) -> bool {
    has_no_base(profile, compat) || (matches!(profile, Some(8 | 10)) && compat == Some(4))
}

/// A DV base layer that has a SUPPLEMENTAL-CODECS string (P8.1/8.4/10.1/10.4 passing the muxer gate).
pub fn has_supplemental_codecs(v: Option<&VideoStreamInfo>) -> bool {
    let Some(v) = v else { return false };
    if !matches!(v.dv_profile, Some(8 | 10)) || v.dv_level.unwrap_or(0) == 0 {
        return false;
    }
    if v.color_range.as_deref() != Some("tv") || v.color_space.as_deref() != Some("bt2020nc") {
        return false;
    }
    if !matches!(v.pixel_format.as_deref(), Some("yuv420p10" | "yuv420p10le")) {
        return false;
    }
    matches!(
        (v.dv_bl_signal_compat_id, v.hdr_format),
        (Some(1), Some(HdrFormat::Hdr10)) | (Some(4), Some(HdrFormat::Hlg))
    )
}

/// A no-base DV stream (P5, P10.0) that has a standalone CODECS string, i.e. a probed level.
pub fn has_standalone_codecs(v: Option<&VideoStreamInfo>) -> bool {
    let Some(v) = v else { return false };
    v.dv_level.unwrap_or(0) != 0
        && (v.dv_profile == Some(5)
            || (v.dv_profile == Some(10) && matches!(v.dv_bl_signal_compat_id, Some(0) | None)))
}

/// Whether a CODECS string can be built from the probed profile and level of a copied stream.
pub fn has_copy_codec_string(v: Option<&VideoStreamInfo>) -> bool {
    let Some(v) = v else { return false };
    let profile = v
        .profile
        .as_deref()
        .filter(|p| !p.is_empty())
        .map(str::to_lowercase);
    let (Some(profile), Some(level)) = (profile, v.level.filter(|l| *l > 0)) else {
        return false;
    };
    if v.codec.is_empty() {
        return false;
    }
    match v.codec.to_lowercase().as_str() {
        "h264" | "avc1" => {
            level <= 255
                && matches!(
                    profile.as_str(),
                    "baseline"
                        | "constrained baseline"
                        | "main"
                        | "extended"
                        | "high"
                        | "high 10"
                        | "high 4:2:2"
                        | "high 4:4:4 predictive"
                )
        }
        "hevc" | "h265" => level >= 30 && matches!(profile.as_str(), "main" | "main 10"),
        "av1" => {
            level <= 31
                && matches!(profile.as_str(), "main" | "high" | "professional")
                && (matches!(v.bit_depth, Some(8 | 10 | 12))
                    || v.pixel_format.as_deref().is_some_and(|p| !p.is_empty()))
        }
        _ => false,
    }
}

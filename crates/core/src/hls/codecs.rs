//! RFC 6381 CODECS strings: per rung (what the encoder emits) and per copy (what was probed).
use crate::api::{AudioPlan, CodecVariant, VideoCodec, VideoStreamInfo};

/// Luma samples per second of a rung's output.
fn luma_rate(w: f64, h: f64, fps: f64) -> f64 {
    w * h * fps
}

/// `avc1.6400LL`: the lowest level whose MaxFS and MaxMBPS cover the rung.
pub fn h264(w: f64, h: f64, fps: f64) -> String {
    const LEVELS: [(&str, f64, f64); 14] = [
        ("0a", 99.0, 1485.0),
        ("0c", 396.0, 3000.0),
        ("0d", 396.0, 11880.0),
        ("15", 792.0, 19800.0),
        ("16", 1620.0, 20250.0),
        ("1e", 1620.0, 40500.0),
        ("1f", 3600.0, 108000.0),
        ("20", 5120.0, 216000.0),
        ("28", 8192.0, 245760.0),
        ("29", 8192.0, 245760.0),
        ("2a", 8704.0, 522240.0),
        ("32", 22080.0, 589824.0),
        ("33", 36864.0, 983040.0),
        ("34", 36864.0, 2073600.0),
    ];
    let mb_per_frame = (w / 16.0).ceil() * (h / 16.0).ceil();
    let mb_per_sec = mb_per_frame * fps;
    let lvl = LEVELS
        .iter()
        .find(|(_, fs, mbps)| mb_per_frame <= *fs && mb_per_sec <= *mbps)
        .map_or("34", |l| l.0);
    format!("avc1.6400{lvl}")
}

/// HEVC `level_idc` (level * 30) from the luma rate; NaN falls to the top level.
fn hevc_level(w: f64, h: f64, fps: f64) -> u32 {
    match luma_rate(w, h, fps) {
        x if x <= 33_177_600.0 => 93,
        x if x <= 66_846_720.0 => 120,
        x if x <= 133_693_440.0 => 123,
        x if x <= 267_386_880.0 => 150,
        x if x <= 534_773_760.0 => 153,
        _ => 156,
    }
}

fn av1_level(w: f64, h: f64, fps: f64) -> &'static str {
    match luma_rate(w, h, fps) {
        x if x > 1_069_547_520.0 => "13",
        x if x > 530_841_600.0 => "12",
        x if x > 311_951_360.0 => "09",
        x if x > 155_975_680.0 => "08",
        x if x > 83_558_400.0 => "06",
        _ => "04",
    }
}

/// The CODECS video entry of a rung of `variant` at `w x h`.
pub fn video(variant: &CodecVariant, w: f64, h: f64, fps: f64) -> String {
    match variant.codec {
        VideoCodec::Av1 => format!("av01.0.{}M.{:02}", av1_level(w, h, fps), variant.bit_depth),
        VideoCodec::Hevc if variant.hdr.is_some() => {
            format!("hvc1.2.4.L{}.B0", hevc_level(w, h, fps))
        }
        VideoCodec::Hevc => format!("hvc1.1.6.L{}.B0", hevc_level(w, h, fps)),
        VideoCodec::H264 => h264(w, h, fps),
    }
}

pub(super) fn plan_codec(p: &AudioPlan) -> &str {
    match p {
        AudioPlan::Copy { codec, .. } => codec,
        AudioPlan::Transcode { codec, .. } => codec.name(),
    }
}

fn plan_profile(p: &AudioPlan) -> Option<&str> {
    match p {
        AudioPlan::Copy { profile, .. } => profile.as_deref(),
        AudioPlan::Transcode { .. } => None,
    }
}

fn audio_codec_string(codec: &str) -> Option<&'static str> {
    match codec.to_lowercase().as_str() {
        "aac" => Some("mp4a.40.2"),
        "ac3" => Some("ac-3"),
        "eac3" => Some("ec-3"),
        "opus" => Some("Opus"),
        "flac" => Some("fLaC"),
        _ => None,
    }
}

/// The CODECS audio entry of a group: HE-AAC plays on an AAC-LC decoder, so the least type wins.
pub fn audio_group(plans: &[AudioPlan]) -> Option<String> {
    let codec = plans
        .first()
        .map_or("aac".into(), |p| plan_codec(p).to_lowercase());
    if codec != "aac" {
        return audio_codec_string(&codec).map(String::from);
    }
    let types: Vec<u32> = plans
        .iter()
        .map(
            |p| match plan_profile(p).map(str::to_lowercase).as_deref() {
                Some("he-aac") => 5,
                Some("he-aacv2") => 29,
                _ => 2,
            },
        )
        .collect();
    let least = [2, 5].into_iter().find(|t| types.contains(t)).unwrap_or(29);
    Some(format!("mp4a.40.{least}"))
}

/// EXT-X-MEDIA CHANNELS of a planned rendition: `16/JOC` for a copied Atmos E-AC-3.
pub fn channels_attr(p: &AudioPlan) -> Option<String> {
    let atmos = plan_codec(p).to_lowercase() == "eac3"
        && plan_profile(p).is_some_and(|s| s.to_lowercase().contains("atmos"));
    if atmos {
        return Some("16/JOC".into());
    }
    match p {
        AudioPlan::Copy { channels, .. } => channels.map(|c| c.to_string()),
        AudioPlan::Transcode { channels, .. } => Some(channels.to_string()),
    }
}

/// CHANNELS without a plan: AAC as stereo, else the source count (2 when unknown).
pub fn rendition_channels(output_codec: &str, source: Option<u32>) -> u32 {
    if output_codec.to_lowercase() == "aac" {
        return 2;
    }
    source.filter(|c| *c > 0).unwrap_or(2)
}

fn probed_bit_depth(v: &VideoStreamInfo) -> Option<u32> {
    if let Some(d @ (8 | 10 | 12)) = v.bit_depth {
        return Some(d);
    }
    let pf = v.pixel_format.as_deref();
    match pf {
        Some(p) if p.contains("p12") => Some(12),
        Some(p) if p.contains("p10") => Some(10),
        Some("") | None => None,
        Some(_) => Some(8),
    }
}

/// CODECS of a copied stream from its probed profile and level; `None` when not certain.
pub fn copy_source(v: &VideoStreamInfo) -> Option<String> {
    let codec = v.codec.to_lowercase();
    let profile = v.profile.as_deref()?.to_lowercase();
    let level = v.level.filter(|l| *l > 0)?;
    if codec.is_empty() || profile.is_empty() {
        return None;
    }
    match codec.as_str() {
        "h264" | "avc1" => {
            let idc = match profile.as_str() {
                "baseline" | "constrained baseline" => "42",
                "main" => "4d",
                "extended" => "58",
                "high" => "64",
                "high 10" => "6e",
                "high 4:2:2" => "7a",
                "high 4:4:4 predictive" => "f4",
                _ => return None,
            };
            (level <= 255).then(|| format!("avc1.{idc}00{level:02x}"))
        }
        "hevc" | "h265" if level >= 30 => match profile.as_str() {
            "main" => Some(format!("hvc1.1.6.L{level}.B0")),
            "main 10" => Some(format!("hvc1.2.4.L{level}.B0")),
            _ => None,
        },
        "av1" if level <= 31 => {
            let p = match profile.as_str() {
                "main" => 0,
                "high" => 1,
                "professional" => 2,
                _ => return None,
            };
            let depth = probed_bit_depth(v)?;
            Some(format!("av01.{p}.{level:02}M.{depth:02}"))
        }
        _ => None,
    }
}

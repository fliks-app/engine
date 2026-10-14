use super::{Rung, resolution::profile_resolution};

/// Relative bits to reach a given quality, H.264 = 1.
fn codec_factor(codec: &str) -> f64 {
    match codec.to_lowercase().as_str() {
        "hevc" | "h265" | "hvc1" | "hev1" => 0.6,
        "vp9" => 0.7,
        "av1" | "av01" => 0.5,
        _ => 1.0,
    }
}

/// JS `Math.round`: halves go up.
pub fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

/// The probed per-stream bitrate, else the container bitrate minus the audio sum when that leaves
/// more than 10 kbps.
pub fn resolve_source_video_bitrate_bps(
    video: Option<f64>,
    format: Option<f64>,
    audio_sum: f64,
) -> Option<f64> {
    if let Some(v) = video.filter(|v| *v > 0.0) {
        return Some(v);
    }
    let est = format.filter(|f| *f > 0.0)? - audio_sum.max(0.0);
    (est > 10_000.0).then_some(est)
}

pub struct RungBitrateContext<'a> {
    pub output_codec: &'a str,
    pub source_width: u32,
    pub source_height: u32,
    pub source_frame_rate: f64,
    pub source_video_bitrate_bps: Option<f64>,
    pub source_video_codec: &'a str,
}

/// HEVC level table from the luma rate: Main-tier `MaxBR`. NaN falls to the top level.
fn hevc_main_tier_cap_bps(luma_per_sec: f64) -> f64 {
    match luma_per_sec {
        x if x <= 33_177_600.0 => 10e6,
        x if x <= 66_846_720.0 => 12e6,
        x if x <= 133_693_440.0 => 20e6,
        x if x <= 267_386_880.0 => 25e6,
        x if x <= 534_773_760.0 => 40e6,
        _ => 60e6,
    }
}

/// The video bitrate a rung really encodes at: its nominal, capped to the codec-scaled source
/// bitrate, then for HEVC clamped to its level's Main-tier ceiling.
pub fn capped_rung_video_bitrate_bps(rung: &Rung, ctx: &RungBitrateContext) -> f64 {
    let mut bps = match ctx.source_video_bitrate_bps.filter(|b| *b > 0.0) {
        None => rung.video_bps,
        Some(src_bps) => {
            let headroom =
                (codec_factor(ctx.output_codec) / codec_factor(ctx.source_video_codec)).max(1.0);
            rung.video_bps.min(js_round(src_bps * headroom))
        }
    };
    if ctx.output_codec == "hevc" {
        let (w, h) = profile_resolution(rung, ctx.source_width, ctx.source_height);
        bps = bps.min(hevc_main_tier_cap_bps(w * h * ctx.source_frame_rate));
    }
    bps
}

//! Source timeline, frame-rate parsing and the segment grids audio padding is judged against.
use crate::api::{MediaInfo, SourceScan, VideoStreamInfo};
use crate::ladder::js_round;

pub const DEFAULT_FPS: f64 = 24.0;

/// `"24000/1001"` or a decimal; `None` when unknown or zero.
pub fn parse_source_fps(frame_rate: Option<&str>) -> Option<f64> {
    let s = frame_rate.filter(|s| !s.is_empty())?;
    if let Some((num, den)) = s.split_once('/') {
        let (n, d) = (js_number(num), js_number(den));
        return (n > 0.0 && d > 0.0).then(|| n / d);
    }
    Some(parse_float(s)).filter(|f| *f != 0.0 && !f.is_nan())
}

/// JS `Number(s)`: blank is 0, junk is NaN.
fn js_number(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        0.0
    } else {
        t.parse().unwrap_or(f64::NAN)
    }
}

/// JS `parseFloat`: the longest numeric prefix, NaN when there is none.
fn parse_float(s: &str) -> f64 {
    let t = s.trim_start();
    (1..=t.len())
        .rev()
        .filter(|i| t.is_char_boundary(*i))
        .find_map(|i| t[..i].parse::<f64>().ok())
        .unwrap_or(f64::NAN)
}

pub fn frame_seconds_of(fps: Option<f64>) -> f64 {
    1.0 / fps.filter(|f| *f > 0.0).unwrap_or(DEFAULT_FPS)
}

/// Segment length at the real frame rate, snapped to the millisecond the true GOP lands on.
pub fn real_segment_seconds(segment_duration: f64, fps: Option<f64>) -> f64 {
    let Some(fps) = fps.filter(|f| *f > 0.0) else {
        return segment_duration;
    };
    let gop = (js_round(segment_duration * fps)).max(1.0) / fps;
    js_round(gop * 1000.0) / 1000.0
}

/// Segments of a uniform grid over `duration`, one per segment a frame starts in.
pub fn uniform_segment_count(duration: f64, segment: f64, frame: f64) -> f64 {
    (((duration - frame) / segment + 1e-9).floor() + 1.0).max(1.0)
}

/// Where a video's picture starts, falling back to its first packet.
pub fn video_presentation_start(v: Option<&VideoStreamInfo>) -> Option<f64> {
    v.and_then(|v| v.first_frame_seconds.or(v.start_time_seconds))
}

/// `(origin, end)` of a probed file; `end` is where the video ends or its clock breaks.
pub fn source_timeline(info: &MediaInfo) -> (f64, Option<f64>) {
    let t = crate::mp4::source_timeline(Some(info));
    (t.origin, t.end)
}

/// Segment boundaries of a copy cut on its own keyframes. `None` without a usable keyframe.
pub fn remux_grid_boundaries(
    scan: &SourceScan,
    origin: f64,
    segment_duration: f64,
    frame_seconds: f64,
) -> Option<Vec<f64>> {
    crate::mp4::compute_segment_grid(
        &scan.keyframes,
        origin,
        scan.end,
        segment_duration,
        frame_seconds,
    )
    .map(|g| g.boundaries)
}

/// How the video of a multi-audio layout is cut, which decides where its last segment starts.
pub enum VideoCut<'a> {
    /// Copied on a scanned keyframe grid.
    Grid(&'a [f64]),
    /// Copied but not scanned: uniform grid at the configured duration.
    CopyUnscanned,
    /// Re-encoded: uniform grid at the real-fps duration.
    Reencode,
}

/// Source time the last video segment starts at; `None` when the end is unknown.
pub fn last_video_segment_start(
    info: &MediaInfo,
    segment_duration: f64,
    cut: &VideoCut,
) -> Option<f64> {
    let (origin, end) = source_timeline(info);
    let end = end?;
    let fps = parse_source_fps(info.video.first().and_then(|v| v.frame_rate.as_deref()));
    let seg = match cut {
        VideoCut::Grid(b) => return b.len().checked_sub(2).map(|i| b[i]),
        VideoCut::CopyUnscanned => segment_duration,
        VideoCut::Reencode => real_segment_seconds(segment_duration, fps),
    };
    Some(origin + (uniform_segment_count(end - origin, seg, frame_seconds_of(fps)) - 1.0) * seg)
}

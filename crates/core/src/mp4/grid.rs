//! Keyframe grid of the copy path and the source timeline (`segment-boundaries.ts`, ...).
use crate::api::{Keyframe, MediaInfo, SourceScan};
use crate::decide::timeline::{frame_seconds_of, parse_source_fps, video_presentation_start};
use crate::jsnum::max;
use serde::{Deserialize, Serialize};

/// How far under its first keyframe's decode time the run from the start seeks.
pub const DECODE_TIME_TOLERANCE_SECONDS: f64 = 0.01;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyframeGrid {
    /// Presented length of each segment (the playlist EXTINF).
    pub durations: Vec<f64>,
    /// Source time each segment starts at, then the end.
    pub boundaries: Vec<f64>,
    /// Kept keyframes, decode order.
    pub keyframes: Vec<Keyframe>,
    /// Index in `keyframes` of each segment's first keyframe, then their count.
    pub first_keyframe: Vec<usize>,
}

/// Cut at the first keyframe past a target advancing `seg` per cut, from `origin` to `end`.
pub fn compute_segment_grid(
    all: &[Keyframe],
    origin: f64,
    end: f64,
    seg: f64,
    frame_seconds: f64,
) -> Option<KeyframeGrid> {
    let same_frame = frame_seconds / 2.0;
    let keyframes: Vec<Keyframe> = all
        .iter()
        .filter(|k| k.pts >= origin - same_frame)
        .cloned()
        .collect();
    if keyframes.is_empty() || seg <= 0.0 {
        return None;
    }
    let total = max(end, keyframes[keyframes.len() - 1].pts);
    let start = max(origin, keyframes[0].pts);
    let (mut boundaries, mut first_keyframe) = (vec![start], vec![0]);
    let mut target = start + seg;
    for (i, kf) in keyframes.iter().enumerate().skip(1) {
        if kf.pts < target || total - kf.pts < same_frame {
            continue;
        }
        boundaries.push(kf.pts);
        first_keyframe.push(i);
        target += seg;
    }
    boundaries.push(total);
    first_keyframe.push(keyframes.len());
    let durations = boundaries.windows(2).map(|w| w[1] - w[0]).collect();
    Some(KeyframeGrid {
        durations,
        boundaries,
        keyframes,
        first_keyframe,
    })
}

/// Segment whose window holds content position `seconds` (counted from `origin`).
pub fn grid_segment_index(boundaries: &[f64], seconds: f64, origin: f64) -> usize {
    if seconds <= 0.0 || boundaries.len() < 2 {
        return 0;
    }
    let at = seconds + origin;
    (0..boundaries.len() - 1)
        .find(|i| at < boundaries[i + 1])
        .unwrap_or(boundaries.len() - 2)
}

/// The grid a remux keeps for `scan` on the timeline starting at `origin`.
pub fn remux_segment_grid(
    scan: Option<&SourceScan>,
    origin: f64,
    seg: f64,
    frame_rate: Option<&str>,
) -> Option<KeyframeGrid> {
    let scan = scan?;
    let frame = frame_seconds_of(parse_source_fps(frame_rate));
    compute_segment_grid(&scan.keyframes, origin, scan.end, seg, frame)
}

/// The source times every served timeline and seek derives from.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceTimeline {
    /// First presented video frame: where every run's output and the grid start.
    pub origin: f64,
    /// Container start: what an input `-ss` and a container-timed sidecar count from.
    pub format_start: f64,
    /// Where the video ends or its clock breaks; unknown when unprobed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<f64>,
    /// Source time an MPEG-TS clock breaks at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_break: Option<f64>,
}

pub fn source_timeline(info: Option<&MediaInfo>) -> SourceTimeline {
    let v = info.and_then(|i| i.video.first());
    let format_start = info
        .and_then(|i| i.format_start_seconds)
        .unwrap_or_else(|| v.and_then(|v| v.start_time_seconds).unwrap_or(0.0));
    let clock_break = info.and_then(|i| i.timestamp_break_seconds.flatten());
    let end = clock_break
        .or_else(|| v.and_then(|v| v.end_seconds))
        .or_else(|| {
            info.and_then(|i| i.duration_seconds)
                .map(|d| format_start + d)
        });
    SourceTimeline {
        origin: video_presentation_start(v).unwrap_or(0.0),
        format_start,
        end,
        clock_break,
    }
}

/// Input `-ss` landing on content position `content` (ffmpeg adds the container start).
pub fn input_seek_seconds(content: f64, t: &SourceTimeline) -> f64 {
    content + (t.origin - t.format_start)
}

/// What the served timeline adds to source time: lifts a video starting before 0 onto 0.
pub fn served_shift(origin: f64) -> f64 {
    max(0.0, -origin)
}

pub fn served_origin(origin: f64) -> f64 {
    origin + served_shift(origin)
}

/// Where a cue at 0 of a container-timed subtitle sits on the served timeline.
pub fn cue_offset_seconds(t: &SourceTimeline) -> f64 {
    t.format_start + served_shift(t.origin)
}

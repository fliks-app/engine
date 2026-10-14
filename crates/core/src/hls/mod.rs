//! HLS playlist text (`docs/spec/hls.md`): every URI hangs off a caller-given prefix.
mod codecs;
mod master;

pub use codecs::copy_source as copy_source_codec_string;
pub use master::{
    AudioStreamMeta, HdrPassThrough, MasterOptions, SubtitleKind, SubtitleRendition,
    iframe_resolution, master_playlist, pinned_rung_name, remux_bandwidth_bps,
};

pub use crate::decide::timeline::parse_source_fps;
use crate::decide::timeline::{frame_seconds_of, real_segment_seconds, uniform_segment_count};
use crate::jsnum::{js_string, max, min, to_fixed3};

/// Segment `N` is at `{prefix}{N:04}{suffix}`.
#[derive(Debug, Clone, Copy)]
pub struct SegUrl<'a> {
    pub prefix: &'a str,
    pub suffix: &'a str,
}

impl SegUrl<'_> {
    fn at(&self, i: usize) -> String {
        format!("{}{i:04}{}", self.prefix, self.suffix)
    }
}

fn head(target: f64, init: Option<&str>) -> Vec<String> {
    let mut lines = vec![
        "#EXTM3U".to_string(),
        "#EXT-X-VERSION:7".into(),
        format!("#EXT-X-TARGETDURATION:{}", js_string(target)),
        "#EXT-X-MEDIA-SEQUENCE:0".into(),
        "#EXT-X-PLAYLIST-TYPE:VOD".into(),
        "#EXT-X-INDEPENDENT-SEGMENTS".into(),
    ];
    if let Some(init) = init {
        lines.push(format!("#EXT-X-MAP:URI=\"{init}\""));
    }
    lines
}

/// Uniform grid: one entry per segment a frame starts in, the last clamped to the end.
pub fn vod_playlist(
    duration: f64,
    seg: SegUrl,
    init: Option<&str>,
    seg_duration: f64,
    frame_seconds: f64,
) -> String {
    let count = uniform_segment_count(duration, seg_duration, frame_seconds);
    let mut lines = head(seg_duration.ceil(), init);
    let mut i = 0;
    while (i as f64) < count {
        let len = min(seg_duration, duration - i as f64 * seg_duration);
        if len <= 0.0 {
            break;
        }
        lines.push(format!("#EXTINF:{},", to_fixed3(len)));
        lines.push(seg.at(i));
        i += 1;
    }
    lines.push("#EXT-X-ENDLIST".into());
    lines.join("\n")
}

/// I-frames-only variant of the uniform grid, without init.
pub fn iframe_playlist(
    duration: f64,
    seg: SegUrl,
    seg_duration: f64,
    frame_seconds: f64,
) -> String {
    vod_playlist(duration, seg, None, seg_duration, frame_seconds).replacen(
        "#EXT-X-INDEPENDENT-SEGMENTS",
        "#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-I-FRAMES-ONLY",
        1,
    )
}

/// One entry per real (keyframe-cut) duration.
pub fn variable_vod_playlist(durations: &[f64], seg: SegUrl, init: Option<&str>) -> String {
    let target = durations.iter().copied().fold(0.0, max).ceil();
    let mut lines = head(target, init);
    for (i, d) in durations.iter().enumerate() {
        lines.push(format!("#EXTINF:{},", to_fixed3(*d)));
        lines.push(seg.at(i));
    }
    lines.push("#EXT-X-ENDLIST".into());
    lines.join("\n")
}

/// A subtitle rendition: one WebVTT segment spanning `duration` (0 when unknown).
pub fn subtitle_playlist(duration: f64, url: &str) -> String {
    let or = |x: f64, y: f64| if x != 0.0 && !x.is_nan() { x } else { y };
    let target = max(1.0, or(duration, 1.0).ceil());
    [
        "#EXTM3U".to_string(),
        "#EXT-X-VERSION:7".into(),
        "#EXT-X-PLAYLIST-TYPE:VOD".into(),
        format!("#EXT-X-TARGETDURATION:{}", js_string(target)),
        "#EXT-X-MEDIA-SEQUENCE:0".into(),
        format!("#EXTINF:{},", to_fixed3(or(duration, target))),
        url.into(),
        "#EXT-X-ENDLIST".into(),
    ]
    .join("\n")
}

/// What a media playlist route knows about its session.
#[derive(Debug, Clone, Copy)]
pub struct Rendition<'a> {
    /// Query suffix of every URI.
    pub token: &'a str,
    /// MPEG-TS segments, no init.
    pub use_ts: bool,
    /// Frozen keyframe-grid durations of a remux session.
    pub remux_durations: Option<&'a [f64]>,
    pub duration: f64,
    /// The session's frozen frame rate.
    pub source_fps: Option<f64>,
    pub segment_duration: f64,
}

fn media_playlist(base: &str, init: &str, remux: bool, r: &Rendition) -> String {
    let ext = if r.use_ts { "ts" } else { "m4s" };
    let prefix = format!("{base}/seg-");
    let suffix = format!(".{ext}{}", r.token);
    let seg = SegUrl {
        prefix: &prefix,
        suffix: &suffix,
    };
    let init = (!r.use_ts).then(|| format!("{base}/{init}{}", r.token));
    match r.remux_durations.filter(|_| remux) {
        Some(d) => variable_vod_playlist(d, seg, init.as_deref()),
        None => {
            let seg_duration = if remux {
                r.segment_duration
            } else {
                real_segment_seconds(r.segment_duration, r.source_fps)
            };
            vod_playlist(
                r.duration,
                seg,
                init.as_deref(),
                seg_duration,
                frame_seconds_of(r.source_fps),
            )
        }
    }
}

/// `{prefix}/{quality}/index.m3u8`; `multi_audio` is the var_stream_map layout (`init_0.mp4`).
pub fn variant_playlist(prefix: &str, quality: &str, multi_audio: bool, r: &Rendition) -> String {
    let remux = quality == "remux";
    let init = if multi_audio && !remux {
        "init_0.mp4"
    } else {
        "init.mp4"
    };
    media_playlist(&format!("{prefix}/{quality}"), init, remux, r)
}

/// `{prefix}/audio/{index}/index.m3u8`; `remux` when the session copies the video.
pub fn audio_playlist(prefix: &str, index: u32, remux: bool, r: &Rendition) -> String {
    let init = format!("init_{}.mp4", u64::from(index) + 1);
    media_playlist(&format!("{prefix}/audio/{index}"), &init, remux, r)
}

/// `{prefix}/iframe/index.m3u8`: the trick-play grid of the variant's real segment length.
pub fn iframe_index(
    prefix: &str,
    token: &str,
    duration: f64,
    source_fps: Option<f64>,
    segment_duration: f64,
) -> String {
    let p = format!("{prefix}/iframe/seg-");
    let s = format!(".ts{token}");
    iframe_playlist(
        duration,
        SegUrl {
            prefix: &p,
            suffix: &s,
        },
        real_segment_seconds(segment_duration, source_fps),
        frame_seconds_of(source_fps),
    )
}

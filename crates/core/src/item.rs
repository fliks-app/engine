//! Items: ordered parts played as one timeline (an audiobook's files). Pure: the timeline and its
//! seek map, the session plan, and the ffmpeg argv of one part's run. Spec: `docs/spec/item.md`.
use crate::api::{
    AudioPlan, DecideRequest, DecideSelection, Decision, HostCaps, ItemPart, MuxFlavour,
    OpenSessionRequest, PlayMethod, QoePipeline, QoeSource,
};
use crate::args::{AudioEnc, audio_stream_args};
use crate::decide::audio::REMUX_STEREO_BPS;
use crate::jsnum::seconds;
use crate::mp4::SourceTimeline;
use crate::session::{
    CACHE_NAMESPACE, OpenReport, SEGMENT_TIMELINE_VERSION, SessionPlan, audio_encoder_id, fnv_hex,
    session_kind,
};
use serde::{Deserialize, Serialize};

/// Nominal segment length of every part; a part's last segment is what remains.
pub const SEGMENT_SECONDS: f64 = 6.0;
/// A remainder under this is folded into the part's last segment: the muxer writes no segment
/// for a few frames.
const MIN_TAIL_SECONDS: f64 = 0.5;
/// Under `-hls_time` so the muxer cuts on the segment grid, not one frame after it.
const HLS_TIME_MARGIN: f64 = 0.0001;

/// The item's parts: `media.parts`.
pub fn parts_of(req: &DecideRequest) -> &[ItemPart] {
    &req.media.parts
}

/// Largest key change of an item, in semitones.
pub const MAX_PITCH_SEMITONES: f64 = 12.0;

/// The key change a selection asks for, clamped to +-12 semitones; 0 for none or a non-number.
pub fn pitch_shift(sel: &DecideSelection) -> f64 {
    sel.pitch_semitones
        .filter(|p| p.is_finite())
        .map_or(0.0, |p| p.clamp(-MAX_PITCH_SEMITONES, MAX_PITCH_SEMITONES))
}

/// A request for an item: it names its parts. A plain audio file keeps the file path.
pub fn is_item(req: &DecideRequest) -> bool {
    !req.media.parts.is_empty()
}

/// MIME type of an audio file by extension (no dot, any case). `mp4` is not here: it is a video
/// container first.
pub fn audio_mime(ext: &str) -> Option<&'static str> {
    Some(match ext.to_lowercase().as_str() {
        "mp3" => "audio/mpeg",
        "m4a" | "m4b" => "audio/mp4",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        _ => return None,
    })
}

/// Where one part sits on the item's timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PartSpan {
    pub offset_s: f64,
    pub duration_s: f64,
    /// Global index of the part's first segment.
    pub first_segment: u32,
    /// 0 for a part with no duration.
    pub segments: u32,
}

/// A position mapped onto a part.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Located {
    pub part: usize,
    /// Seconds from the part's start.
    pub offset_s: f64,
    pub segment: u32,
    /// The segment's index inside its part.
    pub local: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemTimeline {
    pub parts: Vec<PartSpan>,
}

impl ItemTimeline {
    pub fn new(durations: &[f64]) -> Self {
        let (mut offset, mut first) = (0.0, 0);
        let parts = durations
            .iter()
            .map(|d| {
                let d = if d.is_finite() { d.max(0.0) } else { 0.0 };
                let whole = (d / SEGMENT_SECONDS + 1e-9).floor();
                let tail = d - whole * SEGMENT_SECONDS;
                let segments = if d <= 0.0 {
                    0
                } else {
                    (whole as u32 + u32::from(tail >= MIN_TAIL_SECONDS)).max(1)
                };
                let span = PartSpan {
                    offset_s: offset,
                    duration_s: d,
                    first_segment: first,
                    segments,
                };
                offset += d;
                first += segments;
                span
            })
            .collect();
        Self { parts }
    }

    pub fn duration(&self) -> f64 {
        self.parts.last().map_or(0.0, |p| p.offset_s + p.duration_s)
    }

    pub fn segment_count(&self) -> u32 {
        self.parts
            .last()
            .map_or(0, |p| p.first_segment + p.segments)
    }

    /// The part and part-local index of a global segment; `None` past the end.
    pub fn segment(&self, global: u32) -> Option<(usize, u32)> {
        let i = self
            .parts
            .iter()
            .position(|p| global >= p.first_segment && global - p.first_segment < p.segments)?;
        Some((i, global - self.parts[i].first_segment))
    }

    /// Seconds segment `local` of `part` lasts: the nominal one, or for the last what the part
    /// has left (up to a folded tail longer).
    pub fn segment_seconds(&self, part: usize, local: u32) -> f64 {
        let p = &self.parts[part];
        if local + 1 >= p.segments {
            return (p.duration_s - f64::from(local) * SEGMENT_SECONDS).max(0.0);
        }
        SEGMENT_SECONDS
    }

    /// Duration of every segment, in playlist order.
    pub fn durations(&self) -> Vec<f64> {
        (0..self.parts.len())
            .flat_map(|i| (0..self.parts[i].segments).map(move |l| (i, l)))
            .map(|(i, l)| self.segment_seconds(i, l))
            .collect()
    }

    /// Maps a position on the item to its part, offset and segment; a position past the end is
    /// the end of the last part. `None` for an item with no duration.
    pub fn locate(&self, position: f64) -> Option<Located> {
        let at = if position.is_finite() {
            position.max(0.0)
        } else {
            0.0
        };
        let part = self
            .parts
            .iter()
            .rposition(|p| p.segments > 0 && p.offset_s <= at)?;
        let p = &self.parts[part];
        let offset_s = (at - p.offset_s).min(p.duration_s);
        let local = ((offset_s / SEGMENT_SECONDS) as u32).min(p.segments - 1);
        Some(Located {
            part,
            offset_s,
            segment: p.first_segment + local,
            local,
        })
    }
}

/// What a session of an item freezes: its timeline and where each part's audio is.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemPlan {
    pub timeline: ItemTimeline,
    pub parts: Vec<PartRun>,
    /// Key change of every run, in semitones.
    pub pitch_semitones: f64,
}

/// One part as a run reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct PartRun {
    pub path: String,
    /// File seconds the part starts at.
    pub start_s: f64,
    /// Stream index of the picked audio track, `None` for the first one.
    pub stream_index: Option<u32>,
}

/// The session plan of an item: one run directory per part, no scan, no early companion.
pub fn plan(req: &OpenSessionRequest, d: &Decision, caps: &HostCaps) -> SessionPlan {
    let parts = parts_of(&req.decide);
    let timeline = ItemTimeline::new(&parts.iter().map(ItemPart::duration_s).collect::<Vec<_>>());
    let use_ts = d.mux_flavour == MuxFlavour::Ts;
    let picked = req.decide.selection.audio_stream_index;
    let runs: Vec<PartRun> = parts
        .iter()
        .map(|p| PartRun {
            path: p.path.clone(),
            start_s: p.start_s.unwrap_or(0.0).max(0.0),
            stream_index: picked
                .filter(|i| *i >= 0)
                .and_then(|i| p.info.audio.get(i as usize))
                .map(|a| a.stream_index),
        })
        .collect();
    let pitch_semitones = pitch_shift(&req.decide.selection);
    let canonical = canonical(
        parts,
        &runs,
        &d.response.audio_plan,
        use_ts,
        caps,
        req,
        pitch_semitones,
    );
    SessionPlan {
        kind: session_kind(d.response.play_method),
        profile_hash: fnv_hex(&canonical),
        use_ts,
        multi_audio: false,
        source_fps: None,
        segment_duration: SEGMENT_SECONDS,
        timeline: SourceTimeline {
            origin: 0.0,
            format_start: 0.0,
            end: None,
            clock_break: None,
        },
        duration: timeline.duration(),
        remux_grid: None,
        probes_seg_zero: false,
        item: (d.response.play_method != PlayMethod::DirectPlay).then_some(ItemPlan {
            timeline,
            parts: runs,
            pitch_semitones,
        }),
    }
}

/// Everything that decides the bytes of the segments: the output format and each part's slice.
fn canonical(
    parts: &[ItemPart],
    runs: &[PartRun],
    plan: &AudioPlan,
    use_ts: bool,
    caps: &HostCaps,
    req: &OpenSessionRequest,
    pitch_semitones: f64,
) -> String {
    let (codec, channels, encoder) = match plan {
        AudioPlan::Copy {
            codec, channels, ..
        } => (codec.clone(), channels.unwrap_or(2), "copy".to_string()),
        AudioPlan::Transcode { codec, channels } => (
            codec.name().to_string(),
            *channels,
            audio_encoder_id(*codec, *channels, caps),
        ),
    };
    let slices: Vec<String> = parts
        .iter()
        .zip(runs)
        .map(|(p, r)| {
            format!(
                "{}@{}-{}#{}",
                p.path,
                p.start_s.unwrap_or(0.0),
                p.end_s.map_or(String::new(), |e| e.to_string()),
                r.stream_index.map_or(String::new(), |i| i.to_string())
            )
        })
        .collect();
    let pitch = if pitch_semitones == 0.0 {
        String::new()
    } else {
        format!("|pitch={pitch_semitones}")
    };
    format!(
        "{}|item=1|tl={SEGMENT_TIMELINE_VERSION}|mux={}|a={codec}|ac={channels}|ae={encoder}|sd={}|sv={}|p={}{pitch}",
        CACHE_NAMESPACE,
        if use_ts { "ts" } else { "fmp4" },
        SEGMENT_SECONDS,
        req.source_version.clone().unwrap_or_default(),
        slices.join(";")
    )
}

/// What an item's open reports: no clock offset, no video stage, only the audio's fate.
pub fn open_report(d: &Decision) -> OpenReport {
    let r = &d.response;
    OpenReport {
        playback_clock_offset_s: 0.0,
        tonemap_algo: None,
        tonemap_curve: None,
        qoe_pipeline: QoePipeline {
            source: Some(QoeSource {
                codec: r.source.audio_codec.clone().into(),
                width: None,
                height: None,
                hdr: None,
            })
            .filter(|s| s.codec.as_deref() != Some("")),
            output: None,
            decode: None,
            encode: None,
            tonemap: None,
            burn_in: None,
            crop: None,
            audio_transcode: (r.play_method != PlayMethod::DirectPlay)
                .then_some(!r.audio_copy_stream),
        },
    }
}

/// One part's run: segments from `start_segment` to the end of the part.
pub struct RunArgs<'a> {
    pub input_path: &'a str,
    pub output_dir: &'a str,
    /// File seconds the part starts at.
    pub part_start_s: f64,
    /// Seconds the part plays.
    pub part_duration_s: f64,
    /// Where the part starts on the item's timeline (MPEG-TS stamps it).
    pub part_offset_s: f64,
    pub start_segment: u32,
    pub stream_index: Option<u32>,
    pub plan: &'a AudioPlan,
    pub use_ts: bool,
    pub libfdk_aac: bool,
    /// Key change in semitones; 0 leaves the pitch.
    pub pitch_semitones: f64,
}

/// ffmpeg argv of one part's run into HLS segments, `seg-%04d` numbered from `start_segment`.
pub fn run_args(a: &RunArgs) -> Vec<String> {
    let into = f64::from(a.start_segment) * SEGMENT_SECONDS;
    let run_s = a.part_duration_s - into;
    let mut v = crate::strs(&["-hide_banner", "-loglevel", "warning"]);
    if a.part_start_s + into > 0.0 {
        v.extend(["-ss".into(), seconds(a.part_start_s + into)]);
    }
    v.extend(["-i".into(), a.input_path.into()]);
    v.extend(["-t".into(), seconds(run_s)]);
    let map = a
        .stream_index
        .map_or("0:a:0".to_string(), |i| format!("0:{i}"));
    v.extend(["-map".into(), map]);
    v.extend(crate::strs(&["-vn", "-sn", "-dn"]));
    v.extend(audio_stream_args(
        "",
        a.plan,
        &AudioEnc {
            stereo_bps: REMUX_STEREO_BPS,
            align_start: 0.0,
            end: None,
            use_ts: a.use_ts,
            // The pitch-shifting ffmpeg is not the one that carries libfdk.
            libfdk_aac: a.libfdk_aac && a.pitch_semitones == 0.0,
            pitch_semitones: a.pitch_semitones,
        },
    ));
    let (ext, dir) = (if a.use_ts { "ts" } else { "m4s" }, a.output_dir);
    if a.use_ts {
        // A headroom second keeps every timestamp above 0.
        let ts = 1.0 + a.part_offset_s + into;
        v.extend(["-output_ts_offset".into(), seconds(ts)]);
    } else {
        v.extend(crate::strs(&[
            "-movflags",
            "+cmaf",
            "-avoid_negative_ts",
            "disabled",
        ]));
    }
    v.extend(crate::strs(&["-f", "hls", "-hls_time"]));
    v.extend([seconds(SEGMENT_SECONDS - HLS_TIME_MARGIN)]);
    v.extend(crate::strs(&["-hls_list_size", "0", "-start_number"]));
    v.push(a.start_segment.to_string());
    v.extend(crate::strs(&[
        "-hls_segment_type",
        if a.use_ts { "mpegts" } else { "fmp4" },
    ]));
    if !a.use_ts {
        v.extend(crate::strs(&["-hls_fmp4_init_filename", "init.mp4"]));
    }
    v.extend(crate::strs(&[
        "-hls_flags",
        "independent_segments+temp_file",
    ]));
    v.extend([
        "-hls_segment_filename".into(),
        format!("{dir}/seg-%04d.{ext}"),
        format!("{dir}/index.m3u8"),
    ]);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timeline() -> ItemTimeline {
        ItemTimeline::new(&[13.0, 0.0, 6.0, 7.5])
    }

    #[test]
    fn timeline_counts_segments_per_part() {
        let t = timeline();
        let spans: Vec<(f64, u32, u32)> = t
            .parts
            .iter()
            .map(|p| (p.offset_s, p.first_segment, p.segments))
            .collect();
        assert_eq!(
            spans,
            [(0.0, 0, 3), (13.0, 3, 0), (13.0, 3, 1), (19.0, 4, 2)]
        );
        assert_eq!((t.duration(), t.segment_count()), (26.5, 6));
        assert_eq!(t.durations(), [6.0, 6.0, 1.0, 6.0, 6.0, 1.5]);
        assert_eq!(t.segment(3), Some((2, 0)));
        assert_eq!(t.segment(5), Some((3, 1)));
        assert_eq!(t.segment(6), None);
    }

    #[test]
    fn a_tail_under_half_a_second_joins_the_last_segment() {
        let t = ItemTimeline::new(&[12.005, 12.5, 0.4]);
        let counts: Vec<u32> = t.parts.iter().map(|p| p.segments).collect();
        assert_eq!(counts, [2, 3, 1]);
        let ms: Vec<f64> = t.durations().iter().map(|d| (d * 1e3).round()).collect();
        assert_eq!(ms, [6000.0, 6005.0, 6000.0, 6000.0, 500.0, 400.0]);
        assert_eq!(t.segment(2), Some((1, 0)));
        // The folded tail is the last segment's own seconds.
        let l = t.locate(12.0).unwrap();
        assert_eq!((l.part, l.local), (0, 1));
    }

    #[test]
    fn locate_maps_a_position_onto_a_part() {
        let t = timeline();
        let at = |p| {
            t.locate(p)
                .map(|l| (l.part, l.offset_s, l.segment, l.local))
        };
        assert_eq!(at(0.0), Some((0, 0.0, 0, 0)));
        assert_eq!(at(12.9), Some((0, 12.9, 2, 2)));
        // A boundary belongs to the part that starts there; empty parts are skipped.
        assert_eq!(at(13.0), Some((2, 0.0, 3, 0)));
        assert_eq!(at(20.0), Some((3, 1.0, 4, 0)));
        assert_eq!(at(25.0), Some((3, 6.0, 5, 1)));
        assert_eq!(at(1e9), Some((3, 7.5, 5, 1)));
        assert_eq!(at(-3.0), at(0.0));
        assert_eq!(at(f64::NAN), at(0.0));
        assert_eq!(ItemTimeline::new(&[0.0]).locate(1.0), None);
    }

    #[test]
    fn a_run_starts_where_the_segment_is_and_runs_to_the_end_of_the_part() {
        let plan = AudioPlan::Transcode {
            codec: crate::api::AudioCodec::Aac,
            channels: 2,
        };
        let run = |start, dur| {
            run_args(&RunArgs {
                input_path: "/a.mp3",
                output_dir: "/c/p0",
                part_start_s: 0.0,
                part_duration_s: dur,
                part_offset_s: 0.0,
                start_segment: start,
                stream_index: None,
                plan: &plan,
                use_ts: false,
                libfdk_aac: false,
                pitch_semitones: 0.0,
            })
        };
        let at = |v: &[String], k: &str| v[v.iter().position(|a| a == k).unwrap() + 1].clone();
        let v = run(0, 100.0);
        assert!(!v.iter().any(|a| a.contains("rubberband")));
        assert!(!v.contains(&"-ss".to_string()));
        assert_eq!(at(&v, "-t"), "100");
        let v = run(10, 1000.0);
        assert_eq!(
            (at(&v, "-ss").as_str(), at(&v, "-t").as_str()),
            ("60", "940")
        );
        assert_eq!(at(&v, "-start_number"), "10");
        assert_eq!(at(&v, "-hls_time"), "5.9999");
    }
}

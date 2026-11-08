//! The checks an output must pass against its source: probe facts, packet counts and SSIM.
use super::{audio_out, container_of, norm, plan_of, source_container, subtitles_out};
use crate::api::{MediaInfo, OptimizeContainer, OptimizePlan, VideoStreamInfo};
use crate::decide::timeline::parse_source_fps;

/// Longest gap between two keyframes of an output the engine's HLS remux can still cut.
const MAX_KEYFRAME_GAP_SECONDS: f64 = 6.0;
const DURATION_TOLERANCE_SECONDS: f64 = 1.0;
const FALLBACK_FRAME_SECONDS: f64 = 0.05;

/// Video packets of a stream: count and payload bytes, from `ffprobe -show_entries packet=size
/// -of csv=p=0` lines.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PacketStats {
    pub count: u64,
    pub bytes: f64,
}

impl PacketStats {
    pub fn add_line(&mut self, line: &str) {
        if let Ok(n) = line.trim().trim_end_matches(',').parse::<f64>() {
            self.count += 1;
            self.bytes += n;
        }
    }
}

pub fn packet_args(file: &str) -> Vec<String> {
    crate::strs(&[
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "packet=size",
        "-of",
        "csv=p=0",
        file,
    ])
}

/// A full software decode that stops at the first error: only for an output.
pub fn decode_args(file: &str, xerror: bool) -> Vec<String> {
    let mut a = crate::strs(&[
        "-hide_banner",
        "-v",
        "error",
        "-nostats",
        "-progress",
        "pipe:1",
    ]);
    if xerror {
        a.push("-xerror".into());
    }
    a.extend(crate::strs(&[
        "-i", file, "-map", "0:v:0", "-map", "0:a?", "-f", "null", "-",
    ]));
    a
}

/// Mean SSIM of one window of the source against the output, the source scaled to the output.
pub fn ssim_args(src: &str, out: &str, start: f64, len: f64, w: u32, h: u32) -> Vec<String> {
    let (s, l) = (format!("{start:.3}"), format!("{len:.3}"));
    let g = format!(
        "[0:v:0]scale={w}:{h}:flags=bicubic,format=yuv420p[a];[1:v:0]format=yuv420p[b];[a][b]ssim"
    );
    crate::strs(&[
        "-hide_banner",
        "-nostats",
        "-ss",
        &s,
        "-t",
        &l,
        "-i",
        src,
        "-ss",
        &s,
        "-t",
        &l,
        "-i",
        out,
        "-an",
        "-sn",
        "-filter_complex",
        &g,
        "-f",
        "null",
        "-",
    ])
}

/// `All:0.97` of ffmpeg's final `SSIM Y:... All:...` line.
pub fn parse_ssim(lines: &[String]) -> Option<f64> {
    let l = lines
        .iter()
        .rev()
        .find(|l| l.contains("SSIM") && l.contains("All:"))?;
    let at = l.find("All:")? + 4;
    l[at..].split_whitespace().next()?.parse().ok()
}

/// What was measured on a file besides its probe.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Facts {
    pub packets: Option<PacketStats>,
    pub max_keyframe_gap: Option<f64>,
}

pub struct OutputCheck<'a> {
    pub src: &'a MediaInfo,
    pub out: &'a MediaInfo,
    pub plan: &'a OptimizePlan,
    pub src_facts: Facts,
    pub out_facts: Facts,
    /// The video was encoded (else copied).
    pub reencoded: bool,
}

fn ends(bad: &mut Vec<String>, kind: &str, index: u32, src: Option<f64>, out: Option<f64>) {
    if let (Some(a), Some(b)) = (src, out)
        && (a - b).abs() > DURATION_TOLERANCE_SECONDS
    {
        bad.push(format!("{kind} {index} ends at {b:.1}s, source at {a:.1}s"));
    }
}

/// Everything the output gets wrong against the source and the plan; empty when it conforms.
pub fn verify_output(c: &OutputCheck) -> Vec<String> {
    let (src, out, plan) = (c.src, c.out, c.plan);
    let mut bad = vec![];
    let (Some(sv), Some(ov)) = (src.video.first(), out.video.first()) else {
        return vec!["no video stream in the output".into()];
    };
    let frame = parse_source_fps(sv.avg_frame_rate.as_deref().or(sv.frame_rate.as_deref()))
        .map_or(FALLBACK_FRAME_SECONDS, |f| 1.0 / f);
    check_video(&mut bad, c, sv, ov, frame);
    check_audio(&mut bad, src, out, plan, frame);
    check_subtitles(&mut bad, src, out, plan);
    check_container(&mut bad, src, out, plan);
    check_encoding(&mut bad, c, sv, ov);
    bad
}

fn check_video(
    bad: &mut Vec<String>,
    c: &OutputCheck,
    sv: &VideoStreamInfo,
    ov: &VideoStreamInfo,
    frame: f64,
) {
    match (c.src_facts.packets, c.out_facts.packets) {
        (Some(a), Some(b)) if a.count != b.count => bad.push(format!(
            "video packets: {} in the output, {} in the source",
            b.count, a.count
        )),
        (Some(_), None) => bad.push("video packets of the output not counted".into()),
        _ => {}
    }
    // Stream ends decide; the container duration (wrong in some files) only when the video has none.
    match (sv.end_seconds, ov.end_seconds) {
        (Some(a), Some(b)) => {
            if (a - b).abs() > DURATION_TOLERANCE_SECONDS {
                bad.push(format!("video ends at {b:.1}s, source at {a:.1}s"));
            }
        }
        _ => {
            if let (Some(a), Some(b)) = (c.src.duration_seconds, c.out.duration_seconds)
                && (a - b).abs() > DURATION_TOLERANCE_SECONDS
            {
                bad.push(format!("duration {b:.1}s, source {a:.1}s"));
            }
        }
    }
    if let (Some(a), Some(b)) = (sv.start_time_seconds, ov.start_time_seconds)
        && (a - b).abs() > frame
    {
        bad.push(format!("video starts at {b:.3}s, source at {a:.3}s"));
    }
}

fn check_audio(
    bad: &mut Vec<String>,
    src: &MediaInfo,
    out: &MediaInfo,
    plan: &OptimizePlan,
    frame: f64,
) {
    let want = audio_out(src, plan);
    if out.audio.len() != want.len() {
        bad.push(format!(
            "{} audio tracks, expected {}",
            out.audio.len(),
            want.len()
        ));
    }
    for (w, o) in want.iter().zip(&out.audio) {
        let planned = plan_of(plan, w.src.stream_index);
        let lang = planned
            .and_then(|p| p.language.as_deref())
            .unwrap_or(&w.src.language);
        if o.language != lang {
            bad.push(format!("audio language {}, expected {lang}", o.language));
        }
        let src_default = w.src.is_default;
        let keeps_original = planned.is_some_and(|p| p.keep_original);
        let want_default = match (w.encoded, keeps_original) {
            (true, true) => planned.and_then(|p| p.default).or(Some(false)),
            (false, true) => src_default,
            _ => planned.and_then(|p| p.default).or(src_default),
        };
        if let Some(d) = want_default
            && o.is_default.unwrap_or(false) != d
        {
            bad.push(format!("audio default {}, expected {d}", !d));
        }
        if !w.encoded {
            let title = match planned.and_then(|p| p.title.as_ref()) {
                Some(t) if !planned.is_some_and(|p| p.keep_original) => t.as_str(),
                _ => norm(&w.src.title),
            };
            if norm(&o.title) != title {
                bad.push(format!(
                    "audio title {:?}, expected {title:?}",
                    norm(&o.title)
                ));
            }
            if let (Some(a), Some(b)) = (w.src.start_time_seconds, o.start_time_seconds)
                && (a - b).abs() > frame
            {
                bad.push(format!("audio starts at {b:.3}s, source at {a:.3}s"));
            }
        }
        // A source whose statistics tags are stale has an untrustworthy `DURATION`.
        if w.src.has_statistics_tags != Some(false) {
            ends(
                bad,
                "audio",
                w.src.stream_index,
                w.src.end_seconds,
                o.end_seconds,
            );
        }
        if o.channel_layout
            .as_deref()
            .is_none_or(|l| l.is_empty() || l == "unknown")
        {
            bad.push("audio channel layout unknown".into());
        }
    }
}

fn check_subtitles(bad: &mut Vec<String>, src: &MediaInfo, out: &MediaInfo, plan: &OptimizePlan) {
    let subs = subtitles_out(src, plan);
    let extra = plan.extra_subtitles.len();
    if out.subtitles.len() != subs.len() + extra {
        bad.push(format!(
            "{} subtitle tracks, expected {}",
            out.subtitles.len(),
            subs.len() + extra
        ));
    }
    for (w, o) in subs.iter().zip(&out.subtitles) {
        let planned = plan_of(plan, w.stream_index);
        let lang = planned
            .and_then(|p| p.language.as_deref())
            .unwrap_or(&w.language);
        let title = planned
            .and_then(|p| p.title.as_deref())
            .unwrap_or(norm(&w.title));
        if w.has_statistics_tags != Some(false) {
            ends(
                bad,
                "subtitle",
                w.stream_index,
                w.end_seconds,
                o.end_seconds,
            );
        }
        let default = planned.and_then(|p| p.default).or(w.is_default);
        if o.language != lang
            || norm(&o.title) != title
            || o.forced != w.forced
            || o.hearing_impaired != w.hearing_impaired
            || default.is_some_and(|d| o.is_default.unwrap_or(false) != d)
        {
            bad.push(format!("subtitle {} differs from the plan", w.stream_index));
        }
    }
    for (x, o) in plan
        .extra_subtitles
        .iter()
        .zip(out.subtitles.iter().skip(subs.len()))
    {
        let lang = x.language.as_deref().unwrap_or(&o.language);
        if o.language != lang
            || norm(&o.title) != x.title.as_deref().unwrap_or("")
            || o.forced != x.forced.unwrap_or(false)
            || o.hearing_impaired != x.hearing_impaired.unwrap_or(false)
            || x.default
                .is_some_and(|d| o.is_default.unwrap_or(false) != d)
        {
            bad.push(format!("extra subtitle {} differs from the plan", x.path));
        }
    }
}

fn check_container(bad: &mut Vec<String>, src: &MediaInfo, out: &MediaInfo, plan: &OptimizePlan) {
    if src.cover.is_some() && out.cover.is_none() {
        bad.push("cover missing".into());
    }
    if out.video.len() != 1 {
        bad.push(format!("{} video streams, expected 1", out.video.len()));
    }
    if container_of(src, plan) == OptimizeContainer::Mkv
        && source_container(src) == OptimizeContainer::Mkv
        && let Some(a) = src.attachments
        && a != out.attachments.unwrap_or(0)
    {
        bad.push(format!(
            "{} attachments, source {a}",
            out.attachments.unwrap_or(0)
        ));
    }
    if src.chapters.as_ref().map_or(0, Vec::len) != out.chapters.as_ref().map_or(0, Vec::len) {
        bad.push("chapters differ from the source".into());
    }
}

fn check_encoding(
    bad: &mut Vec<String>,
    c: &OutputCheck,
    sv: &VideoStreamInfo,
    ov: &VideoStreamInfo,
) {
    if c.reencoded {
        if let (Some(a), Some(b)) = (sv.bit_depth, ov.bit_depth)
            && b < a
        {
            bad.push(format!("video depth {b} bits, source {a} bits"));
        }
        if sv.hdr_format.is_some()
            && (ov.hdr_format != sv.hdr_format
                || ov.color_space != sv.color_space
                || ov.color_transfer != sv.color_transfer
                || ov.color_primaries != sv.color_primaries
                || ov.hdr_metadata != sv.hdr_metadata)
        {
            bad.push("HDR colour tags or mastering metadata differ from the source".into());
        }
        if let Some(g) = c.out_facts.max_keyframe_gap
            && g > MAX_KEYFRAME_GAP_SECONDS
        {
            bad.push(format!(
                "keyframes {g:.1}s apart, at most {MAX_KEYFRAME_GAP_SECONDS}s"
            ));
        }
    } else if (ov.dv_profile, ov.dv_bl_signal_compat_id, ov.dv_rpu_present)
        != (sv.dv_profile, sv.dv_bl_signal_compat_id, sv.dv_rpu_present)
    {
        bad.push("Dolby Vision configuration differs from the source".into());
    }
    if container_of(c.src, c.plan) == OptimizeContainer::Mp4
        && ov.codec == "hevc"
        && c.plan.hvc1_tag
        && ov.codec_tag.as_deref() != Some("hvc1")
    {
        bad.push(format!(
            "MP4 sample entry {:?}, expected hvc1",
            ov.codec_tag
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::optimize::tests::{info, plan};

    #[test]
    fn stream_ends_decide_not_the_container_duration() {
        let mut src = info();
        src.duration_seconds = Some(100.0);
        src.video[0].end_seconds = Some(90.0);
        src.audio[0].end_seconds = Some(90.0);
        src.audio[1].end_seconds = Some(90.0);
        src.subtitles[0].end_seconds = Some(85.0);
        let facts = Facts::default();
        let run = |src: &MediaInfo, out: &MediaInfo| {
            verify_output(&OutputCheck {
                src,
                out,
                plan: &plan(),
                src_facts: facts,
                out_facts: facts,
                reencoded: false,
            })
        };
        for a in &mut src.audio {
            a.channel_layout = Some("stereo".into());
        }
        let mut out = src.clone();
        // The remux fixed the container duration: no failure.
        out.duration_seconds = Some(90.0);
        assert_eq!(run(&src, &out), Vec::<String>::new());
        out.audio[1].end_seconds = Some(60.0);
        assert!(run(&src, &out).iter().any(|b| b.contains("audio 2 ends")));
        out.audio[1].end_seconds = Some(90.0);
        out.subtitles[0].end_seconds = Some(10.0);
        assert!(
            run(&src, &out)
                .iter()
                .any(|b| b.contains("subtitle 3 ends"))
        );
        // Stale statistics tags of the source: its ends are not trusted.
        src.subtitles[0].has_statistics_tags = Some(false);
        assert!(
            !run(&src, &out)
                .iter()
                .any(|b| b.contains("subtitle 3 ends"))
        );
    }

    #[test]
    fn a_conforming_output_passes_and_a_short_one_fails() {
        let src = info();
        let p = plan();
        let facts = Facts {
            packets: Some(PacketStats {
                count: 10,
                bytes: 100.0,
            }),
            max_keyframe_gap: Some(5.0),
        };
        let check = |out: &MediaInfo, f: Facts| {
            verify_output(&OutputCheck {
                src: &src,
                out,
                plan: &p,
                src_facts: facts,
                out_facts: f,
                reencoded: true,
            })
        };
        assert_eq!(check(&src, facts), Vec::<String>::new());
        let mut short = src.clone();
        short.duration_seconds = Some(60.0);
        assert!(check(&short, facts)[0].contains("duration"));
        let fewer = Facts {
            packets: Some(PacketStats {
                count: 9,
                bytes: 1.0,
            }),
            ..facts
        };
        assert!(check(&src, fewer)[0].contains("packets"));
        let sparse = Facts {
            max_keyframe_gap: Some(9.0),
            ..facts
        };
        assert!(check(&src, sparse)[0].contains("keyframes"));
    }

    #[test]
    fn a_lost_cover_or_a_second_video_track_fails() {
        let mut src = info();
        src.cover = Some(crate::api::CoverArt {
            stream_index: 3,
            codec: "mjpeg".into(),
            width: None,
            height: None,
        });
        let facts = Facts::default();
        let run = |src: &MediaInfo, out: &MediaInfo| {
            verify_output(&OutputCheck {
                src,
                out,
                plan: &plan(),
                src_facts: facts,
                out_facts: facts,
                reencoded: false,
            })
        };
        let mut out = src.clone();
        assert_eq!(run(&src, &out), Vec::<String>::new());
        out.cover = None;
        assert_eq!(run(&src, &out), ["cover missing"]);
        out.cover = src.cover.clone();
        out.video.push(out.video[0].clone());
        assert_eq!(run(&src, &out), ["2 video streams, expected 1"]);
    }

    #[test]
    fn a_depth_cut_in_the_output_fails() {
        let mut src = info();
        src.video[0].bit_depth = Some(10);
        let out = info();
        let facts = Facts {
            packets: None,
            max_keyframe_gap: None,
        };
        let bad = verify_output(&OutputCheck {
            src: &src,
            out: &out,
            plan: &plan(),
            src_facts: facts,
            out_facts: facts,
            reencoded: true,
        });
        assert!(bad.iter().any(|b| b.contains("depth")), "{bad:?}");
    }

    #[test]
    fn the_final_ssim_line_gives_the_mean() {
        let l = |s: &str| vec![s.to_string()];
        assert_eq!(
            parse_ssim(&l(
                "[Parsed_ssim_0 @ 0x1] SSIM Y:0.9 U:0.9 V:0.9 All:0.912345 (10.5)"
            )),
            Some(0.912345)
        );
    }
}

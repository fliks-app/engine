//! Pure half of the `Optimize` and `Verify` jobs: the plan turned into argv inputs, the checks an
//! output must pass, and how ffmpeg's stderr is read. Spec: `docs/spec/jobs.md` section 11.
use crate::api::{
    AudioStreamInfo, JobErrorClass, MediaInfo, OptimizeContainer, OptimizePlan, OptimizeQuality,
    StreamPlan, SubtitleStreamInfo, VideoStreamInfo,
};
use crate::args::{
    ArchiveAudio, ArchiveAudioEnc, ArchiveExtraSub, ArchiveFile, ArchiveTrack, FileArgs,
};
use serde_json::json;

/// Samples encoded to estimate the savings and the speed.
pub const SAMPLE_COUNT: usize = 3;
pub const SAMPLE_SECONDS: f64 = 20.0;
pub const DEFAULT_MAX_SOFTWARE_HOURS: f64 = 12.0;
/// Longest gap between two keyframes of an output the engine's HLS remux can still cut.
pub const MAX_KEYFRAME_GAP_SECONDS: f64 = 6.0;
pub const DURATION_TOLERANCE_SECONDS: f64 = 1.0;
/// Decoding errors per hour of film below which a readable source is `damaged`, not `corrupt`.
pub const DAMAGED_ERRORS_PER_HOUR: f64 = 10.0;
/// Mean SSIM under which a hardware output is taken for green or black frames.
pub const MIN_SSIM: f64 = 0.9;
const FALLBACK_FRAME_SECONDS: f64 = 0.05;

/// The container the output is written in.
pub fn container_of(info: &MediaInfo, plan: &OptimizePlan) -> OptimizeContainer {
    plan.container.unwrap_or_else(|| {
        let f = info.format_name.as_deref().unwrap_or("");
        if f.contains("mp4") || f.contains("mov") {
            OptimizeContainer::Mp4
        } else {
            OptimizeContainer::Mkv
        }
    })
}

pub fn is_matroska(info: &MediaInfo) -> bool {
    info.format_name
        .as_deref()
        .is_some_and(|f| f.contains("matroska"))
}

/// The main video: the first one (the probe leaves attached pictures out).
pub fn main_video(info: &MediaInfo) -> Result<&VideoStreamInfo, String> {
    info.video.first().ok_or_else(|| "no video stream".into())
}

/// Why the video of a planned re-encode is copied instead; `None` when it can be encoded.
pub fn copy_video_reason(
    v: &VideoStreamInfo,
    plan: &OptimizePlan,
    hardware: bool,
) -> Option<&'static str> {
    let px = v.pixel_format.as_deref().unwrap_or("");
    let interlaced = v.field_order.as_deref().is_some_and(|f| f != "progressive");
    let chroma = hardware && (px.contains("422") || px.contains("444"));
    let dv = match v.dv_profile {
        Some(5) => true,
        Some(_) => plan.skip_hdr10plus_dv.unwrap_or(true),
        None => false,
    };
    let plus = v.hdr10_plus == Some(true) && plan.skip_hdr10plus_dv.unwrap_or(true);
    // The encoder variants carry 10 bits only with HDR: SDR 10-bit has none.
    let sdr10 = v.bit_depth.is_some_and(|d| d > 8) && v.hdr_format.is_none();
    (interlaced || chroma || dv || plus || sdr10).then_some("unsupported_source")
}

fn plan_of(plan: &OptimizePlan, index: u32) -> Option<&StreamPlan> {
    plan.streams.iter().find(|s| s.stream_index == index)
}

fn track(index: u32, sp: Option<&StreamPlan>) -> ArchiveTrack {
    ArchiveTrack {
        stream_index: index,
        language: sp.and_then(|s| s.language.clone()),
        title: sp.and_then(|s| s.title.clone()),
        default: sp.and_then(|s| s.default),
    }
}

/// One output audio track: the source stream it comes from and whether it is encoded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioOut<'a> {
    pub src: &'a AudioStreamInfo,
    pub encoded: bool,
}

/// The audio tracks of the output, in order.
pub fn audio_out<'a>(info: &'a MediaInfo, plan: &OptimizePlan) -> Vec<AudioOut<'a>> {
    let mut out = vec![];
    for a in &info.audio {
        let sp = plan_of(plan, a.stream_index);
        if sp.is_some_and(|s| !s.keep) {
            continue;
        }
        match sp.and_then(|s| s.encode) {
            None => out.push(AudioOut {
                src: a,
                encoded: false,
            }),
            Some(_) => {
                if sp.is_some_and(|s| s.keep_original) {
                    out.push(AudioOut {
                        src: a,
                        encoded: false,
                    });
                }
                out.push(AudioOut {
                    src: a,
                    encoded: true,
                });
            }
        }
    }
    out
}

/// The subtitle tracks of the output (source streams kept), in order.
pub fn subtitles_out<'a>(info: &'a MediaInfo, plan: &OptimizePlan) -> Vec<&'a SubtitleStreamInfo> {
    info.subtitles
        .iter()
        .filter(|s| plan_of(plan, s.stream_index).is_none_or(|p| p.keep))
        .collect()
}

/// The explicit mapping of the output for `args::file`.
pub fn archive_file(
    info: &MediaInfo,
    plan: &OptimizePlan,
    copy_video: bool,
) -> Result<ArchiveFile, String> {
    let container = container_of(info, plan);
    let mut audio = vec![];
    for o in audio_out(info, plan) {
        let sp = plan_of(plan, o.src.stream_index);
        let channels = o.src.channels.unwrap_or(2);
        let (t, action) = match (o.encoded, sp.and_then(|s| s.encode)) {
            (true, Some(crate::api::StreamEncode::Eac3)) => (
                track(o.src.stream_index, sp),
                ArchiveAudioEnc::Eac3 { channels },
            ),
            (true, _) => (
                track(o.src.stream_index, sp),
                ArchiveAudioEnc::Aac { channels },
            ),
            // The original beside an encode keeps its own tags and flags.
            (false, Some(_)) => (track(o.src.stream_index, None), ArchiveAudioEnc::Copy),
            (false, None) => (track(o.src.stream_index, sp), ArchiveAudioEnc::Copy),
        };
        audio.push(ArchiveAudio { track: t, action });
    }
    let subs = subtitles_out(info, plan);
    if container == OptimizeContainer::Mp4 && subs.iter().any(|s| s.is_image_based) {
        return Err("image subtitles cannot be written to MP4".into());
    }
    Ok(ArchiveFile {
        copy_video,
        hvc1_tag: plan.hvc1_tag && container == OptimizeContainer::Mp4,
        video_index: Some(main_video(info)?.stream_index),
        cover_art: info.cover.iter().map(|c| c.stream_index).collect(),
        audio,
        subtitles: subs
            .iter()
            .map(|s| track(s.stream_index, plan_of(plan, s.stream_index)))
            .collect(),
        extra_subtitles: plan
            .extra_subtitles
            .iter()
            .map(|x| ArchiveExtraSub {
                path: x.path.clone(),
                language: x.language.clone(),
                title: x.title.clone(),
                default: x.default,
            })
            .collect(),
    })
}

fn fps(v: &VideoStreamInfo) -> Option<f64> {
    let s = v.avg_frame_rate.as_deref().or(v.frame_rate.as_deref())?;
    let (n, d) = s.split_once('/').unwrap_or((s, "1"));
    let (n, d) = (n.parse::<f64>().ok()?, d.parse::<f64>().ok()?);
    (n > 0.0 && d > 0.0).then_some(n / d)
}

/// What `build_file_args` writes and how.
pub struct Output<'a> {
    pub input_path: &'a str,
    pub path: &'a str,
    /// `hevc` or `av1`, the plan's target codec.
    pub codec: &'a str,
    /// `none` for the CPU, else the detected accelerator (`vaapi`, `nvenc`...).
    pub hw: &'a str,
    pub threads: u32,
    pub copy_video: bool,
}

/// The `FileArgs` of an optimisation output (audio and subtitles mapped by the plan).
pub fn build_file_args(
    info: &MediaInfo,
    plan: &OptimizePlan,
    o: &Output,
) -> Result<FileArgs, String> {
    let v = main_video(info)?;
    let quality = match plan.quality.unwrap_or(OptimizeQuality::Balanced) {
        OptimizeQuality::Small => "small",
        OptimizeQuality::Balanced => "balanced",
        OptimizeQuality::Faithful => "faithful",
    };
    let ten = v.hdr_format.is_some() && v.bit_depth.is_some_and(|d| d > 8);
    let mut encode = json!({
        "inputPath": o.input_path,
        "outputDir": "/unused",
        "hwAccel": o.hw,
        "profile": { "name": "archive", "maxWidth": 16384, "maxHeight": 16384,
            "videoBitrate": "8M", "audioBitrate": "192k" },
        "archive": { "quality": quality },
        "encodeThreads": o.threads,
        "sourceVideoCodec": v.codec,
        "videoVariant": { "codec": o.codec, "bitDepth": if ten { 10 } else { 8 }, "hdr": v.hdr_format },
        "videoStreamIndex": v.stream_index,
    });
    let m = encode.as_object_mut().expect("object");
    let mut set = |k: &str, val: serde_json::Value| {
        if !val.is_null() {
            m.insert(k.into(), val);
        }
    };
    set("sourceWidth", json!(v.width));
    set("sourceHeight", json!(v.height));
    set("sourceFps", json!(fps(v)));
    set("sourceBitDepth", json!(v.bit_depth));
    set("sourceVideoBitrateBps", json!(v.bit_rate));
    set("sourceHdrMetadata", json!(v.hdr_metadata));
    set("sourceDvProfile", json!(v.dv_profile));
    set("sourceDvBlSignalCompatId", json!(v.dv_bl_signal_compat_id));
    set("sourceColorSpace", json!(v.color_space));
    set("sourceColorPrimaries", json!(v.color_primaries));
    set("sourceColorTransfer", json!(v.color_transfer));
    set("sourceColorRange", json!(v.color_range));
    set("crop", json!(plan.crop));
    let mp4 = container_of(info, plan) == OptimizeContainer::Mp4;
    let mut file = serde_json::from_value::<FileArgs>(json!({
        "encode": encode,
        "path": o.path,
        "format": if mp4 { "mp4" } else { "mkv" },
    }))
    .map_err(|e| e.to_string())?;
    file.archive = Some(archive_file(info, plan, o.copy_video)?);
    Ok(file)
}

/// `-ss`/`-t` on the video of an argv built for one input and one output: a sample, input-seeked.
pub fn windowed(mut argv: Vec<String>, input: &str, start: f64, len: f64) -> Vec<String> {
    if let Some(at) = argv.windows(2).position(|w| w[0] == "-i" && w[1] == input) {
        argv.splice(at..at, ["-ss".into(), format!("{start:.3}")]);
    }
    let out = argv.len().saturating_sub(1);
    argv.splice(out..out, ["-t".into(), format!("{len:.3}")]);
    argv
}

/// `-y` (the staged file is ours and exists) and a `-progress` stream on stdout.
pub fn runnable(mut argv: Vec<String>) -> Vec<String> {
    argv.insert(0, "-y".into());
    let out = argv.len().saturating_sub(1);
    argv.splice(
        out..out,
        ["-nostats".into(), "-progress".into(), "pipe:1".into()],
    );
    argv
}

/// Start of each sample: evenly spread, the first after the opening credits' first minute when the
/// film is long enough. Fewer (or none) for a short file.
pub fn sample_starts(duration: f64) -> Vec<f64> {
    if !(duration.is_finite() && duration > SAMPLE_SECONDS) {
        return vec![];
    }
    let n = if duration >= 3.0 * SAMPLE_SECONDS * 2.0 {
        SAMPLE_COUNT
    } else {
        1
    };
    (0..n)
        .map(|k| {
            let span = duration - SAMPLE_SECONDS;
            (span * (k as f64 + 1.0) / (n as f64 + 1.0)).max(0.0)
        })
        .collect()
}

/// Share of the video saved, in percent (negative when the output is bigger).
pub fn savings_pct(src_bytes: f64, out_bytes: f64) -> f64 {
    if src_bytes <= 0.0 {
        return 0.0;
    }
    (1.0 - out_bytes / src_bytes) * 100.0
}

/// Hours a software encode of the whole file would take, from the samples' wall time.
pub fn estimated_hours(sample_wall_seconds: f64, sampled_seconds: f64, duration: f64) -> f64 {
    if sampled_seconds <= 0.0 {
        return 0.0;
    }
    sample_wall_seconds / sampled_seconds * duration / 3600.0
}

/// Decoding errors tolerated before `damaged` becomes `corrupt`.
pub fn damaged_limit(duration_seconds: f64) -> u32 {
    (DAMAGED_ERRORS_PER_HOUR * duration_seconds.max(0.0) / 3600.0).ceil() as u32
}

/// An ffmpeg stderr line that reports a damaged stream; timestamp complaints are benign.
pub fn is_decode_error(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    if l.contains("non monotonically increasing dts")
        || l.contains("non-monotonic dts")
        || l.contains("timestamps are unset")
        || l.contains("past duration too large")
    {
        return false;
    }
    [
        "error while decoding",
        "invalid data found",
        "corrupt",
        "error submitting packet",
        "invalid nal",
        "missing reference picture",
        "concealing",
        "truncated",
        "decode_slice_header",
        "cabac",
        "invalid frame size",
        "unable to decode",
    ]
    .iter()
    .any(|p| l.contains(p))
}

/// The class of a failed ffmpeg run from the end of its stderr.
pub fn classify_stderr(lines: &[String]) -> JobErrorClass {
    let t = lines.join("\n").to_ascii_lowercase();
    let has = |ps: &[&str]| ps.iter().any(|p| t.contains(p));
    if has(&["no space left on device"]) {
        JobErrorClass::Nospace
    } else if has(&["input/output error"]) {
        JobErrorClass::Io
    } else if has(&[
        "permission denied",
        "read-only file system",
        "no such file or directory",
    ]) {
        JobErrorClass::Access
    } else if has(&[
        "moov atom not found",
        "ebml header parsing failed",
        "invalid data found when processing input",
    ]) {
        JobErrorClass::Corrupt
    } else {
        JobErrorClass::Engine
    }
}

/// The `damaged` or `corrupt` failure of a source that showed `errors` decoding errors.
pub fn decode_failure(errors: u32, duration_seconds: f64, excerpt: &str) -> String {
    let class = if errors > damaged_limit(duration_seconds) {
        JobErrorClass::Corrupt
    } else {
        JobErrorClass::Damaged
    };
    class.message(format!("{errors} decoding errors: {excerpt}"))
}

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
    [
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "packet=size",
        "-of",
        "csv=p=0",
        file,
    ]
    .map(String::from)
    .to_vec()
}

/// A full software decode that stops at the first error: only for an output.
pub fn decode_args(file: &str, xerror: bool) -> Vec<String> {
    let mut a: Vec<String> = [
        "-hide_banner",
        "-v",
        "error",
        "-nostats",
        "-progress",
        "pipe:1",
    ]
    .map(String::from)
    .to_vec();
    if xerror {
        a.push("-xerror".into());
    }
    a.extend(
        [
            "-i", file, "-map", "0:v:0", "-map", "0:a?", "-f", "null", "-",
        ]
        .map(String::from),
    );
    a
}

/// Mean SSIM of one window of the source against the output, the source scaled to the output.
pub fn ssim_args(src: &str, out: &str, start: f64, len: f64, w: u32, h: u32) -> Vec<String> {
    let (s, l) = (format!("{start:.3}"), format!("{len:.3}"));
    let g = format!(
        "[0:v:0]scale={w}:{h}:flags=bicubic,format=yuv420p[a];[1:v:0]format=yuv420p[b];[a][b]ssim"
    );
    [
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
    ]
    .map(String::from)
    .to_vec()
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

fn norm(s: &Option<String>) -> &str {
    s.as_deref().unwrap_or("")
}

/// Everything the output gets wrong against the source and the plan; empty when it conforms.
pub fn verify_output(c: &OutputCheck) -> Vec<String> {
    let (src, out, plan) = (c.src, c.out, c.plan);
    let mut bad = vec![];
    let (Some(sv), Some(ov)) = (src.video.first(), out.video.first()) else {
        return vec!["no video stream in the output".into()];
    };
    match (c.src_facts.packets, c.out_facts.packets) {
        (Some(a), Some(b)) if a.count != b.count => bad.push(format!(
            "video packets: {} in the output, {} in the source",
            b.count, a.count
        )),
        (Some(_), None) => bad.push("video packets of the output not counted".into()),
        _ => {}
    }
    if let (Some(a), Some(b)) = (src.duration_seconds, out.duration_seconds)
        && (a - b).abs() > DURATION_TOLERANCE_SECONDS
    {
        bad.push(format!("duration {b:.1}s, source {a:.1}s"));
    }
    if let (Some(a), Some(b)) = (sv.end_seconds, ov.end_seconds)
        && (a - b).abs() > DURATION_TOLERANCE_SECONDS
    {
        bad.push(format!("video ends at {b:.1}s, source at {a:.1}s"));
    }
    let frame = fps(sv).map_or(FALLBACK_FRAME_SECONDS, |f| 1.0 / f);
    if let (Some(a), Some(b)) = (sv.start_time_seconds, ov.start_time_seconds)
        && (a - b).abs() > frame
    {
        bad.push(format!("video starts at {b:.3}s, source at {a:.3}s"));
    }
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
        if !w.encoded {
            let title = match planned.and_then(|p| p.title.as_ref()) {
                Some(t) if !w.encoded && !planned.is_some_and(|p| p.keep_original) => t.as_str(),
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
        if o.channel_layout
            .as_deref()
            .is_none_or(|l| l.is_empty() || l == "unknown")
        {
            bad.push("audio channel layout unknown".into());
        }
    }
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
        if o.language != lang
            || norm(&o.title) != title
            || o.forced != w.forced
            || o.hearing_impaired != w.hearing_impaired
        {
            bad.push(format!("subtitle {} differs from the plan", w.stream_index));
        }
    }
    if src.chapters.as_ref().map_or(0, Vec::len) != out.chapters.as_ref().map_or(0, Vec::len) {
        bad.push("chapters differ from the source".into());
    }
    if c.reencoded {
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
    if container_of(src, plan) == OptimizeContainer::Mp4
        && ov.codec == "hevc"
        && plan.hvc1_tag
        && ov.codec_tag.as_deref() != Some("hvc1")
    {
        bad.push(format!(
            "MP4 sample entry {:?}, expected hvc1",
            ov.codec_tag
        ));
    }
    bad
}

/// `mkvpropedit` arguments for a metadata-level plan, on `file`.
pub fn mkvpropedit_args(info: &MediaInfo, plan: &OptimizePlan, file: &str) -> Vec<String> {
    let mut a = vec![file.to_string()];
    if plan.clear_title {
        a.extend(["--edit", "info", "--delete", "title"].map(String::from));
    }
    let rank = |kind: &str, index: u32| -> Option<usize> {
        let mut all: Vec<u32> = match kind {
            "v" => info.video.iter().map(|s| s.stream_index).collect(),
            "a" => info.audio.iter().map(|s| s.stream_index).collect(),
            _ => info.subtitles.iter().map(|s| s.stream_index).collect(),
        };
        all.sort_unstable();
        all.iter().position(|i| *i == index).map(|p| p + 1)
    };
    for sp in &plan.streams {
        let kind = if info.video.iter().any(|s| s.stream_index == sp.stream_index) {
            "v"
        } else if info.audio.iter().any(|s| s.stream_index == sp.stream_index) {
            "a"
        } else {
            "s"
        };
        let Some(n) = rank(kind, sp.stream_index) else {
            continue;
        };
        let mut set = vec![];
        if let Some(l) = &sp.language {
            set.extend(["--set".into(), format!("language={l}")]);
        }
        match sp.title.as_deref() {
            Some("") => set.extend(["--delete".into(), "name".into()]),
            Some(t) => set.extend(["--set".into(), format!("name={t}")]),
            None => {}
        }
        if let Some(d) = sp.default {
            set.extend(["--set".into(), format!("flag-default={}", u8::from(d))]);
        }
        if !set.is_empty() {
            a.extend(["--edit".into(), format!("track:{kind}{n}")]);
            a.extend(set);
        }
    }
    if plan.strip_statistics_tags {
        a.push("--add-track-statistics-tags".into());
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::StreamEncode;

    fn plan() -> OptimizePlan {
        serde_json::from_value(json!({ "level": "remux" })).unwrap()
    }

    fn info() -> MediaInfo {
        serde_json::from_value(json!({
            "formatName": "matroska,webm", "durationSeconds": 100.0,
            "video": [{ "streamIndex": 0, "codec": "h264", "width": 1920, "height": 1080,
                "avgFrameRate": "24000/1001", "pixelFormat": "yuv420p", "bitDepth": 8 }],
            "audio": [
                { "streamIndex": 1, "codec": "dts", "language": "fre", "channels": 6, "channelLayout": "5.1" },
                { "streamIndex": 2, "codec": "aac", "language": "eng", "channels": 2, "channelLayout": "stereo" }],
            "subtitles": [
                { "streamIndex": 3, "codec": "subrip", "language": "fre", "forced": false,
                  "hearingImpaired": false, "isImageBased": false }]
        }))
        .unwrap()
    }

    #[test]
    fn a_video_files_cover_is_mapped_and_too_slow_defaults_to_hardware() {
        let mut i = info();
        i.cover = serde_json::from_value(json!({ "streamIndex": 4, "codec": "mjpeg" })).unwrap();
        assert_eq!(archive_file(&i, &plan(), true).unwrap().cover_art, [4]);
        assert_eq!(plan().too_slow, crate::api::OptimizeTooSlow::Hardware);
        let p: OptimizePlan =
            serde_json::from_value(json!({ "level": "reencode", "tooSlow": "skip" })).unwrap();
        assert_eq!(p.too_slow, crate::api::OptimizeTooSlow::Skip);
    }

    #[test]
    fn audio_replace_add_and_drop_follow_the_plan() {
        let mut p = plan();
        p.streams = vec![
            StreamPlan {
                stream_index: 1,
                keep: true,
                language: None,
                title: Some("Français 5.1 E-AC3".into()),
                default: Some(false),
                encode: Some(StreamEncode::Eac3),
                keep_original: true,
            },
            StreamPlan {
                stream_index: 2,
                keep: false,
                language: None,
                title: None,
                default: None,
                encode: None,
                keep_original: false,
            },
        ];
        let src = info();
        let out = audio_out(&src, &p);
        assert_eq!(
            out.iter().map(|o| o.encoded).collect::<Vec<_>>(),
            [false, true]
        );
        let f = archive_file(&info(), &p, true).unwrap();
        assert_eq!(f.audio.len(), 2);
        assert_eq!(f.audio[0].action, ArchiveAudioEnc::Copy);
        assert_eq!(f.audio[0].track.title, None, "the original keeps its tags");
        assert_eq!(f.audio[1].action, ArchiveAudioEnc::Eac3 { channels: 6 });
        assert_eq!(f.audio[1].track.default, Some(false));
        assert_eq!(f.subtitles.len(), 1);
    }

    #[test]
    fn unsupported_sources_keep_their_video() {
        let p = plan();
        let mut v = info().video[0].clone();
        assert_eq!(copy_video_reason(&v, &p, true), None);
        v.field_order = Some("tt".into());
        assert_eq!(copy_video_reason(&v, &p, false), Some("unsupported_source"));
        v.field_order = None;
        v.pixel_format = Some("yuv422p10le".into());
        assert_eq!(copy_video_reason(&v, &p, true), Some("unsupported_source"));
        v.pixel_format = Some("yuv420p".into());
        v.dv_profile = Some(8);
        assert!(copy_video_reason(&v, &p, false).is_some());
        let mut keep = p.clone();
        keep.skip_hdr10plus_dv = Some(false);
        assert_eq!(copy_video_reason(&v, &keep, false), None);
        v.dv_profile = Some(5);
        assert!(
            copy_video_reason(&v, &keep, false).is_some(),
            "profile 5 never"
        );
        v.dv_profile = None;
        v.bit_depth = Some(10);
        assert!(
            copy_video_reason(&v, &p, false).is_some(),
            "SDR 10-bit has no variant"
        );
    }

    #[test]
    fn samples_spread_and_short_files_get_fewer() {
        assert!(sample_starts(10.0).is_empty());
        assert_eq!(sample_starts(60.0).len(), 1);
        let s = sample_starts(7200.0);
        assert_eq!(s.len(), SAMPLE_COUNT);
        assert!(s.windows(2).all(|w| w[0] < w[1]) && s[2] + SAMPLE_SECONDS < 7200.0);
        assert!((savings_pct(100.0, 40.0) - 60.0).abs() < 1e-9);
        assert_eq!(damaged_limit(7200.0), 20);
    }

    #[test]
    fn file_args_cover_a_copy_and_an_extra_subtitle() {
        let mut p = plan();
        p.extra_subtitles = vec![crate::api::ExtraSubtitle {
            path: "/tmp/ocr.srt".into(),
            language: Some("fre".into()),
            title: None,
            default: None,
        }];
        let f = build_file_args(
            &info(),
            &p,
            &Output {
                input_path: "/m/in.mkv",
                path: "/m/.stage.tmp",
                codec: "hevc",
                hw: "none",
                threads: 4,
                copy_video: true,
            },
        )
        .unwrap();
        let argv = crate::args::file(
            &f,
            &Default::default(),
            &crate::hw::Device {
                platform: crate::api::Platform::Linux,
                render_node: None,
                opencl_device: None,
            },
        )
        .unwrap();
        let i = argv.iter().position(|a| a == "/m/in.mkv").unwrap();
        assert_eq!(&argv[i + 1..i + 3], ["-i", "/tmp/ocr.srt"]);
        assert!(argv.windows(2).any(|w| w == ["-map", "1:0"]));
        let w = windowed(argv, "/m/in.mkv", 30.0, 20.0);
        assert!(w.windows(2).any(|x| x == ["-ss", "30.000"]));
        let r = runnable(w);
        assert_eq!(r[0], "-y");
        assert_eq!(r.last().unwrap(), "/m/.stage.tmp");
    }

    #[test]
    fn stderr_is_read_for_errors_and_classes() {
        assert!(is_decode_error(
            "[hevc @ 0x1] Error while decoding stream #0:0: Invalid data found"
        ));
        assert!(!is_decode_error(
            "Application provided invalid, non monotonically increasing dts to muxer"
        ));
        let l = |s: &str| vec![s.to_string()];
        assert_eq!(
            classify_stderr(&l("moov atom not found")),
            JobErrorClass::Corrupt
        );
        assert_eq!(
            classify_stderr(&l("No space left on device")),
            JobErrorClass::Nospace
        );
        assert_eq!(
            classify_stderr(&l("encoder crashed")),
            JobErrorClass::Engine
        );
        let m = decode_failure(3, 3600.0, "x");
        assert!(m.starts_with("damaged: "), "{m}");
        assert!(decode_failure(11, 3600.0, "x").starts_with("corrupt: "));
        assert_eq!(
            parse_ssim(&l(
                "[Parsed_ssim_0 @ 0x1] SSIM Y:0.9 U:0.9 V:0.9 All:0.912345 (10.5)"
            )),
            Some(0.912345)
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
    fn mkvpropedit_edits_tracks_by_type_rank() {
        let mut p = plan();
        p.clear_title = true;
        p.strip_statistics_tags = true;
        p.streams = vec![StreamPlan {
            stream_index: 2,
            keep: true,
            language: Some("eng".into()),
            title: Some(String::new()),
            default: Some(true),
            encode: None,
            keep_original: false,
        }];
        let a = mkvpropedit_args(&info(), &p, "/m/a.mkv");
        assert_eq!(a[0], "/m/a.mkv");
        assert!(a.windows(2).any(|w| w == ["--edit", "track:a2"]));
        assert!(a.windows(2).any(|w| w == ["--set", "flag-default=1"]));
        assert!(a.windows(2).any(|w| w == ["--delete", "name"]));
        assert_eq!(a.last().unwrap(), "--add-track-statistics-tags");
    }
}

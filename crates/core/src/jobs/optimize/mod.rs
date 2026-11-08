//! Pure half of the `Optimize` and `Verify` jobs: the plan turned into argv inputs, the checks an
//! output must pass, and how ffmpeg's stderr is read. Spec: `docs/spec/jobs.md` section 11.
use crate::api::{
    AudioStreamInfo, HwAccel, MediaInfo, OptimizeContainer, OptimizePlan, OptimizeQuality,
    StreamPlan, SubtitleStreamInfo, VideoStreamInfo,
};
use crate::args::{ArchiveAudio, ArchiveAudioEnc, ArchiveFile, ArchiveTrack, FileArgs};
use crate::decide::timeline::parse_source_fps;
use serde_json::json;

mod check;
mod stderr;

pub use check::*;
pub use stderr::*;

/// Samples encoded to estimate the savings and the speed.
pub const SAMPLE_COUNT: usize = 3;
pub const SAMPLE_SECONDS: f64 = 20.0;
pub const DEFAULT_MAX_SOFTWARE_HOURS: f64 = 12.0;
/// Mean SSIM under which a hardware output is taken for green or black frames.
pub const MIN_SSIM: f64 = 0.9;

fn norm(s: &Option<String>) -> &str {
    s.as_deref().unwrap_or("")
}

/// The container the output is written in.
pub fn container_of(info: &MediaInfo, plan: &OptimizePlan) -> OptimizeContainer {
    plan.container.unwrap_or_else(|| source_container(info))
}

fn source_container(info: &MediaInfo) -> OptimizeContainer {
    let f = info.format_name.as_deref().unwrap_or("");
    if f.contains("mp4") || f.contains("mov") {
        OptimizeContainer::Mp4
    } else {
        OptimizeContainer::Mkv
    }
}

/// Whether the plan, its video copied, changes nothing: the output would be the source rewritten as is.
pub fn is_noop_copy(info: &MediaInfo, plan: &OptimizePlan) -> bool {
    let Some(v) = info.video.first() else {
        return false;
    };
    let mp4 = container_of(info, plan) == OptimizeContainer::Mp4;
    let hvc1 = plan.hvc1_tag && mp4 && v.codec == "hevc" && v.codec_tag.as_deref() != Some("hvc1");
    let title = !plan.clear_title || info.title.is_none();
    if container_of(info, plan) != source_container(info)
        || !title
        || hvc1
        || !plan.extra_subtitles.is_empty()
    {
        return false;
    }
    let same = |sp: Option<&StreamPlan>,
                lang: &str,
                from_title: Option<bool>,
                t: &Option<String>,
                d: Option<bool>| {
        sp.is_none_or(|p| {
            p.keep
                && p.encode.is_none()
                && p.language
                    .as_deref()
                    .is_none_or(|l| l == lang && from_title != Some(true))
                && p.title.as_deref().is_none_or(|x| x == norm(t))
                && p.default.is_none_or(|x| x == d.unwrap_or(false))
        })
    };
    let audio = audio_out(info, plan);
    let subs = subtitles_out(info, plan);
    audio.len() == info.audio.len()
        && subs.len() == info.subtitles.len()
        && audio.iter().zip(&info.audio).all(|(o, a)| {
            !o.encoded
                && o.src.stream_index == a.stream_index
                && same(
                    plan_of(plan, a.stream_index),
                    &a.language,
                    a.language_from_title,
                    &a.title,
                    a.is_default,
                )
        })
        && subs.iter().zip(&info.subtitles).all(|(o, s)| {
            o.stream_index == s.stream_index
                && same(
                    plan_of(plan, s.stream_index),
                    &s.language,
                    s.language_from_title,
                    &s.title,
                    s.is_default,
                )
        })
}

pub fn is_matroska(info: &MediaInfo) -> bool {
    crate::probe::is_matroska(info.format_name.as_deref())
}

/// The main video: the first one (the probe leaves attached pictures out).
pub fn main_video(info: &MediaInfo) -> Result<&VideoStreamInfo, String> {
    info.video.first().ok_or_else(|| "no video stream".into())
}

/// Why the video of a re-encode is copied; `OptimizeMeta.copy_reason` carries `as_str`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyReason {
    UnsupportedSource,
    TooSlow,
    LowSavings,
}

impl CopyReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedSource => "unsupported_source",
            Self::TooSlow => "too_slow",
            Self::LowSavings => "low_savings",
        }
    }
}

/// Why the video of a planned re-encode is copied instead; `None` when it can be encoded.
pub fn copy_video_reason(
    v: &VideoStreamInfo,
    plan: &OptimizePlan,
    hardware: bool,
) -> Option<CopyReason> {
    let px = v.pixel_format.as_deref().unwrap_or("");
    let interlaced = v.field_order.as_deref().is_some_and(|f| f != "progressive");
    let chroma = hardware && (px.contains("422") || px.contains("444"));
    let dv = match v.dv_profile {
        Some(5) => true,
        Some(_) => plan.skip_hdr10plus_dv.unwrap_or(true),
        None => false,
    };
    let plus = v.hdr10_plus == Some(true) && plan.skip_hdr10plus_dv.unwrap_or(true);
    (interlaced || chroma || dv || plus).then_some(CopyReason::UnsupportedSource)
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
        codec: None,
    }
}

/// `items` in the order of `plan.streams`; a stream the plan does not list keeps its place after the listed ones.
fn in_plan_order<'a, T>(
    items: &'a [T],
    plan: &OptimizePlan,
    index: impl Fn(&T) -> u32,
) -> Vec<&'a T> {
    let mut v: Vec<&T> = items.iter().collect();
    v.sort_by_key(|t| {
        let i = index(t);
        plan.streams
            .iter()
            .position(|s| s.stream_index == i)
            .unwrap_or(usize::MAX)
    });
    v
}

/// One output audio track: the source stream it comes from and whether it is encoded.
#[derive(Debug, Clone, Copy, PartialEq)]
struct AudioOut<'a> {
    pub src: &'a AudioStreamInfo,
    pub encoded: bool,
}

/// The audio tracks of the output, in order.
fn audio_out<'a>(info: &'a MediaInfo, plan: &OptimizePlan) -> Vec<AudioOut<'a>> {
    let mut out = vec![];
    for a in in_plan_order(&info.audio, plan, |a| a.stream_index) {
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
fn subtitles_out<'a>(info: &'a MediaInfo, plan: &OptimizePlan) -> Vec<&'a SubtitleStreamInfo> {
    in_plan_order(&info.subtitles, plan, |s| s.stream_index)
        .into_iter()
        .filter(|s| plan_of(plan, s.stream_index).is_none_or(|p| p.keep))
        .collect()
}

/// The explicit mapping of the output for `args::file`.
fn archive_file(
    info: &MediaInfo,
    plan: &OptimizePlan,
    copy_video: bool,
) -> Result<ArchiveFile, String> {
    let container = container_of(info, plan);
    let mut audio = vec![];
    for o in audio_out(info, plan) {
        let sp = plan_of(plan, o.src.stream_index);
        let channels = o.src.channels.unwrap_or(2);
        // Beside its original, the encode is never default unless the plan says so.
        let encoded = || {
            let mut t = track(o.src.stream_index, sp);
            if sp.is_some_and(|s| s.keep_original) {
                t.default = t.default.or(Some(false));
            }
            t
        };
        let (t, action) = match (o.encoded, sp.and_then(|s| s.encode)) {
            (true, Some(crate::api::StreamEncode::Eac3)) => {
                (encoded(), ArchiveAudioEnc::Eac3 { channels })
            }
            (true, _) => (encoded(), ArchiveAudioEnc::Aac { channels }),
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
        clear_title: plan.clear_title,
        audio,
        subtitles: subs
            .iter()
            .map(|s| ArchiveTrack {
                codec: Some(s.codec.clone()),
                ..track(s.stream_index, plan_of(plan, s.stream_index))
            })
            .collect(),
        extra_subtitles: plan.extra_subtitles.clone(),
    })
}

/// What `build_file_args` writes and how.
pub struct Output<'a> {
    pub input_path: &'a str,
    pub path: &'a str,
    /// `hevc` or `av1`, the plan's target codec.
    pub codec: &'a str,
    /// `HwAccel::None` for the CPU, else the detected accelerator.
    pub hw: HwAccel,
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
    let quality = plan.quality.unwrap_or(OptimizeQuality::Balanced);
    // HEVC keeps the source's depth, AV1 is always 10-bit.
    let ten = v.hdr_format.is_some() || v.bit_depth.is_some_and(|d| d > 8) || o.codec == "av1";
    let mut encode = json!({
        "inputPath": o.input_path,
        "outputDir": "/unused",
        "hwAccel": o.hw,
        "profile": { "name": "archive", "maxWidth": 16384, "maxHeight": 16384,
            "videoBitrate": "8M", "audioBitrate": "192k" },
        "archive": { "quality": quality, "capPools": true },
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
    set(
        "sourceFps",
        json!(parse_source_fps(
            v.avg_frame_rate.as_deref().or(v.frame_rate.as_deref())
        )),
    );
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

/// The input options of a source read over HTTP, put before every `-i <url>` (or before the
/// positional `url` of an ffprobe argv): the bearer token and reconnects on a dropped connection.
pub fn http_input(mut argv: Vec<String>, url: &str, token: Option<&str>) -> Vec<String> {
    let mut opts: Vec<String> = ["-reconnect", "1", "-reconnect_on_network_error", "1"]
        .map(String::from)
        .to_vec();
    if let Some(t) = token {
        opts.extend(["-headers".into(), format!("Authorization: Bearer {t}\r\n")]);
    }
    let inputs: Vec<usize> = argv
        .windows(2)
        .enumerate()
        .filter(|(_, w)| w[0] == "-i" && w[1] == url)
        .map(|(i, _)| i)
        .collect();
    let inputs = if inputs.is_empty() {
        argv.iter().rposition(|a| a == url).into_iter().collect()
    } else {
        inputs
    };
    for at in inputs.into_iter().rev() {
        argv.splice(at..at, opts.iter().cloned());
    }
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

/// `mkvpropedit` arguments for a metadata-level plan, on `file`; the statistics tags are always rewritten.
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
    a.push("--add-track-statistics-tags".into());
    a
}

/// `mkvpropedit` arguments that regenerate the statistics tags of a Matroska output.
pub fn statistics_tags_args(file: &str) -> Vec<String> {
    vec![file.into(), "--add-track-statistics-tags".into()]
}

/// `mkvpropedit` arguments that attach the extracted cover `picture` (`.jpg` or `.png`).
pub fn cover_attachment_args(picture: &str) -> Vec<String> {
    let (name, mime) = if picture.ends_with(".png") {
        ("cover.png", "image/png")
    } else {
        ("cover.jpg", "image/jpeg")
    };
    crate::strs(&[
        "--attachment-name",
        name,
        "--attachment-mime-type",
        mime,
        "--add-attachment",
        picture,
    ])
}

#[cfg(test)]
mod tests {
    #[test]
    fn http_input_options_precede_each_url_input() {
        let url = "http://h/f.mkv";
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let got = super::http_input(a(&["-i", url, "-i", url, "o.mkv"]), url, Some("tk"));
        let at = got.iter().position(|x| x == "-headers").unwrap();
        assert_eq!(got[at + 1], "Authorization: Bearer tk\r\n");
        assert_eq!(got.iter().filter(|x| *x == "-reconnect").count(), 2);
        assert_eq!(got.last().unwrap(), "o.mkv");
        let probe = super::http_input(a(&["-v", "error", url]), url, None);
        assert_eq!(
            probe[2..6],
            ["-reconnect", "1", "-reconnect_on_network_error", "1"]
        );
        assert_eq!(probe.last().unwrap(), url);
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
        assert_eq!(
            copy_video_reason(&v, &p, false),
            Some(CopyReason::UnsupportedSource)
        );
        v.field_order = None;
        v.pixel_format = Some("yuv422p10le".into());
        assert_eq!(
            copy_video_reason(&v, &p, true),
            Some(CopyReason::UnsupportedSource)
        );
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
        assert_eq!(
            copy_video_reason(&v, &p, false),
            None,
            "SDR 10-bit is encodable"
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
    }

    #[test]
    fn file_args_cover_a_copy_and_an_extra_subtitle() {
        let mut p = plan();
        p.extra_subtitles = vec![crate::api::ExtraSubtitle {
            path: "/tmp/ocr.srt".into(),
            language: Some("fre".into()),
            title: None,
            default: None,
            forced: None,
            hearing_impaired: None,
        }];
        let f = build_file_args(
            &info(),
            &p,
            &Output {
                input_path: "/m/in.mkv",
                path: "/m/.stage.tmp",
                codec: "hevc",
                hw: HwAccel::None,
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
                cuda_index: None,
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

    fn argv_of(i: &MediaInfo, p: &OptimizePlan, codec: &str, copy: bool) -> Vec<String> {
        let f = build_file_args(
            i,
            p,
            &Output {
                input_path: "/m/in.mkv",
                path: "/m/.stage.tmp",
                codec,
                hw: HwAccel::None,
                threads: 4,
                copy_video: copy,
            },
        )
        .unwrap();
        let dev = crate::hw::Device {
            platform: crate::api::Platform::Linux,
            render_node: None,
            opencl_device: None,
            cuda_index: None,
        };
        crate::args::file(&f, &Default::default(), &dev).unwrap()
    }

    #[test]
    fn a_demoted_default_keeps_forced_and_the_title_can_be_cleared() {
        let mut p = plan();
        p.clear_title = true;
        p.streams = vec![StreamPlan {
            stream_index: 3,
            keep: true,
            language: None,
            title: None,
            default: Some(false),
            encode: None,
            keep_original: false,
        }];
        let a = argv_of(&info(), &p, "hevc", true);
        assert!(a.windows(2).any(|w| w == ["-disposition:s:0", "-default"]));
        assert!(a.windows(2).any(|w| w == ["-metadata", "title="]));
    }

    #[test]
    fn sdr_above_eight_bits_and_av1_encode_in_ten() {
        let mut i = info();
        i.video[0].bit_depth = Some(10);
        i.video[0].pixel_format = Some("yuv420p10le".into());
        let p = plan();
        assert_eq!(copy_video_reason(&i.video[0], &p, false), None);
        let a = argv_of(&i, &p, "hevc", false);
        assert!(
            a.windows(2).any(|w| w == ["-pix_fmt:v:0", "yuv420p10le"]),
            "{a:?}"
        );
        assert!(
            !a.iter().any(|x| x.contains("smpte2084")),
            "no HDR tags on SDR"
        );
        let a = argv_of(&info(), &p, "av1", false);
        assert!(
            a.windows(2).any(|w| w == ["-pix_fmt:v:0", "yuv420p10le"]),
            "{a:?}"
        );
        let a = argv_of(&info(), &p, "hevc", false);
        assert!(!a.iter().any(|x| x == "yuv420p10le"), "8-bit stays 8-bit");
    }

    #[test]
    fn an_mp4_reencode_maps_the_cover_and_indexes_the_video_options() {
        let mut i = info();
        i.cover = serde_json::from_value(json!({ "streamIndex": 4, "codec": "mjpeg" })).unwrap();
        let mut p = plan();
        p.container = Some(OptimizeContainer::Mp4);
        p.streams = vec![];
        let a = argv_of(&i, &p, "hevc", false);
        assert!(a.iter().any(|x| x == "-filter:v:0"), "{a:?}");
        for bare in ["-vf", "-pix_fmt", "-profile:v", "-c:v"] {
            assert!(!a.iter().any(|x| x == bare), "{bare} in {a:?}");
        }
        assert!(a.windows(2).any(|w| w == ["-map", "0:4"]), "{a:?}");
        assert!(a.windows(2).any(|w| w == ["-c:v:1", "copy"]));
    }

    #[test]
    fn a_matroska_output_leaves_the_cover_to_the_attachment() {
        let mut i = info();
        i.cover = serde_json::from_value(json!({ "streamIndex": 4, "codec": "mjpeg" })).unwrap();
        let a = argv_of(&i, &plan(), "hevc", false);
        assert!(!a.windows(2).any(|w| w == ["-map", "0:4"]), "{a:?}");
        assert!(!a.iter().any(|x| x == "-c:v:1"), "{a:?}");
        assert_eq!(
            cover_attachment_args("/m/.stage.mkv.cover.png").join(" "),
            "--attachment-name cover.png --attachment-mime-type image/png \
             --add-attachment /m/.stage.mkv.cover.png"
        );
    }

    fn sp(i: u32) -> StreamPlan {
        StreamPlan {
            stream_index: i,
            keep: true,
            language: None,
            title: None,
            default: None,
            encode: None,
            keep_original: false,
        }
    }

    #[test]
    fn mov_text_into_matroska_is_written_as_srt() {
        let mut i = info();
        i.subtitles[0].codec = "mov_text".into();
        let a = argv_of(&i, &plan(), "hevc", true);
        assert!(a.windows(2).any(|w| w == ["-c:s:0", "srt"]), "{a:?}");
        let a = argv_of(&info(), &plan(), "hevc", true);
        assert!(!a.iter().any(|x| x == "-c:s:0"), "{a:?}");
    }

    #[test]
    fn extra_subtitle_dispositions_are_one_flag_set() {
        let mut p = plan();
        p.extra_subtitles = vec![crate::api::ExtraSubtitle {
            path: "/tmp/x.srt".into(),
            language: None,
            title: None,
            default: Some(false),
            forced: Some(true),
            hearing_impaired: Some(true),
        }];
        let a = argv_of(&info(), &p, "hevc", true);
        let want = ["-disposition:s:1", "-default+forced+hearing_impaired"];
        assert!(a.windows(2).any(|w| w == want), "{a:?}");
        assert_eq!(a.iter().filter(|x| *x == "-disposition:s:1").count(), 1);
        let mut out = info();
        let mut extra = out.subtitles[0].clone();
        extra.is_default = Some(false);
        extra.forced = true;
        extra.hearing_impaired = true;
        out.subtitles.push(extra.clone());
        out.subtitles[1].language = "fre".into();
        let check = |out: &MediaInfo| {
            verify_output(&OutputCheck {
                src: &info(),
                out,
                plan: &p,
                src_facts: Facts::default(),
                out_facts: Facts::default(),
                reencoded: false,
            })
        };
        assert_eq!(check(&out), Vec::<String>::new());
        out.subtitles[1].forced = false;
        assert!(check(&out).iter().any(|b| b.contains("extra subtitle")));
    }

    #[test]
    fn the_output_follows_the_order_of_the_plan() {
        let mut p = plan();
        p.streams = vec![sp(2), sp(1), sp(3)];
        let order: Vec<u32> = audio_out(&info(), &p)
            .iter()
            .map(|o| o.src.stream_index)
            .collect();
        assert_eq!(order, [2, 1]);
        let f = archive_file(&info(), &p, true).unwrap();
        assert_eq!(f.audio[0].track.stream_index, 2);
        let mut out = info();
        out.audio.swap(0, 1);
        let check = |out: &MediaInfo, p: &OptimizePlan| {
            verify_output(&OutputCheck {
                src: &info(),
                out,
                plan: p,
                src_facts: Facts::default(),
                out_facts: Facts::default(),
                reencoded: false,
            })
        };
        assert!(check(&out, &p).is_empty());
        assert!(
            !check(&out, &plan()).is_empty(),
            "source order is a difference"
        );
    }

    #[test]
    fn a_false_default_clears_the_flag_and_the_added_track_takes_the_plan_title() {
        let mut p = plan();
        let mut added = sp(1);
        (added.encode, added.keep_original) = (Some(StreamEncode::Aac), true);
        added.title = Some("Stereo".into());
        p.streams = vec![added];
        let mut i = info();
        i.audio[0].is_default = Some(true);
        let a = argv_of(&i, &p, "hevc", true);
        assert!(
            a.windows(2)
                .any(|w| w == ["-metadata:s:a:1", "title=Stereo"]),
            "{a:?}"
        );
        assert!(
            a.windows(2).any(|w| w == ["-disposition:a:1", "-default"]),
            "{a:?}"
        );
        assert!(
            !a.iter().any(|x| x == "-disposition:a:0"),
            "the original keeps its flag"
        );
        let mut s = plan();
        let mut x = sp(3);
        x.default = Some(false);
        s.streams = vec![x];
        let a = argv_of(&i, &s, "hevc", true);
        assert!(a.windows(2).any(|w| w == ["-disposition:s:0", "-default"]));
    }

    #[test]
    fn the_output_must_keep_attachments_and_defaults() {
        let mut src = info();
        src.attachments = Some(2);
        src.audio[0].is_default = Some(true);
        let check = |out: &MediaInfo| {
            verify_output(&OutputCheck {
                src: &src,
                out,
                plan: &plan(),
                src_facts: Facts::default(),
                out_facts: Facts::default(),
                reencoded: false,
            })
        };
        assert_eq!(check(&src), Vec::<String>::new());
        let mut out = src.clone();
        out.attachments = Some(0);
        assert!(check(&out).iter().any(|b| b.contains("attachments")));
        let mut out = src.clone();
        out.audio[0].is_default = Some(false);
        assert!(check(&out).iter().any(|b| b.contains("audio default")));
    }

    #[test]
    fn a_copy_that_changes_nothing_is_a_noop() {
        let p = plan();
        assert!(is_noop_copy(&info(), &p));
        let mut keep = p.clone();
        keep.streams = vec![sp(1), sp(2), sp(3)];
        assert!(is_noop_copy(&info(), &keep));
        let mut t = p.clone();
        let mut x = sp(2);
        x.title = Some("Commentary".into());
        t.streams = vec![x];
        assert!(!is_noop_copy(&info(), &t), "a retitle is a change");
        let mut r = p.clone();
        r.streams = vec![sp(2), sp(1)];
        assert!(!is_noop_copy(&info(), &r), "a reorder is a change");
        let mut d = p.clone();
        let mut x = sp(3);
        x.keep = false;
        d.streams = vec![x];
        assert!(!is_noop_copy(&info(), &d));
        let mut c = p.clone();
        c.container = Some(OptimizeContainer::Mp4);
        assert!(!is_noop_copy(&info(), &c));
        let mut e = p.clone();
        e.extra_subtitles = vec![crate::api::ExtraSubtitle {
            path: "/a.srt".into(),
            language: None,
            title: None,
            default: None,
            forced: None,
            hearing_impaired: None,
        }];
        assert!(!is_noop_copy(&info(), &e));
    }

    #[test]
    fn mkvpropedit_edits_tracks_by_type_rank() {
        let mut p = plan();
        p.clear_title = true;
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
    use super::*;
    use crate::api::StreamEncode;

    pub(crate) fn plan() -> OptimizePlan {
        serde_json::from_value(json!({ "level": "remux" })).unwrap()
    }

    pub(crate) fn info() -> MediaInfo {
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
}

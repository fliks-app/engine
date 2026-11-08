//! Archival encodes: rate control per encoder, and the file's explicit stream mapping.
//! Spec: `docs/spec/args.md` §Archive. [new]
use super::encode::{encode, video_map};
use super::{ArchiveQuality, ArchiveRate, FileArgs, FileFormat};
use crate::api::{HostCaps, VideoCodec};
use crate::decide::codec::Encoder;
use crate::hw::Device;
use crate::strs;
use serde::Deserialize;

/// The value of `v` (small, balanced, faithful) for `q`.
fn by<T: ToString>(q: ArchiveQuality, v: [T; 3]) -> String {
    v[q as usize].to_string()
}

/// Rate control, tuning and IDR flags of a hardware encoder: everything but `-vf` and `-g`.
pub(super) fn hw_rate(e: &Encoder, a: &ArchiveRate) -> Result<Vec<String>, String> {
    use crate::api::HwAccel::*;
    let (q, hevc) = (a.quality, e.codec == VideoCodec::Hevc);
    Ok(match e.hw {
        Qsv => strs(&[
            "-preset",
            "veryslow",
            "-look_ahead_depth",
            "40",
            "-extbrc",
            "1",
            "-global_quality",
            &by(q, [26, 23, 20]),
            "-forced_idr",
            "1",
        ]),
        Vaapi => strs(&["-rc_mode", "ICQ", "-global_quality", &by(q, [26, 23, 20])]),
        Nvenc => strs(&[
            "-preset",
            "p7",
            "-tune",
            "hq",
            "-rc",
            "vbr",
            "-cq",
            &by(q, if hevc { [30, 26, 22] } else { [34, 30, 26] }),
            "-b:v",
            "0",
            "-rc-lookahead",
            "32",
            "-spatial-aq",
            "1",
            "-temporal-aq",
            "1",
            "-bf",
            "4",
            "-multipass",
            "fullres",
            "-forced-idr",
            "1",
        ]),
        Amf => strs(&[
            "-quality",
            "quality",
            "-rc",
            "qvbr",
            "-qvbr_quality_level",
            &by(q, [30, 26, 22]),
            "-preanalysis",
            "1",
            "-forced_idr",
            "1",
        ]),
        Videotoolbox => match a.bitrate_bps {
            Some(b) => strs(&["-b:v", &crate::jsnum::js_string(b)]),
            _ => strs(&["-q:v", &by(q, [55, 65, 75])]),
        },
        None => return Err("archive: not a hardware encoder".into()),
    })
}

/// `-crf` and `-preset` of libx265 / libsvtav1.
pub(super) fn cpu_rate(codec: VideoCodec, q: ArchiveQuality) -> Vec<String> {
    let faithful = q == ArchiveQuality::Faithful;
    match codec {
        VideoCodec::Av1 => strs(&[
            "-crf",
            &by(q, [34, 28, 23]),
            "-preset",
            if faithful { "4" } else { "6" },
        ]),
        _ => strs(&[
            "-crf",
            &by(q, [26, 22, 19]),
            "-preset",
            if faithful { "slow" } else { "medium" },
        ]),
    }
}

/// One source stream kept in the output, with the tags to write on it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveTrack {
    /// Absolute index in the source.
    pub stream_index: u32,
    /// ISO 639-2 tag to write.
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    /// `Some` sets or clears the default flag; `None` keeps the source's.
    #[serde(default)]
    pub default: Option<bool>,
    /// Source codec of a subtitle track: text that Matroska cannot hold as is is written as `srt`.
    #[serde(default)]
    pub codec: Option<String>,
}

/// What happens to the audio of a track. A source stream listed twice is copied and encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum ArchiveAudioEnc {
    Copy,
    /// 640 kb/s up to 5.1 (the encoder has no 7.1: it folds to 5.1).
    Eac3 {
        channels: u32,
    },
    /// Stereo, 256 kb/s; multichannel sources go through a `pan` downmix.
    Aac {
        channels: u32,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveAudio {
    #[serde(flatten)]
    pub track: ArchiveTrack,
    pub action: ArchiveAudioEnc,
}

/// A text subtitle file added as an extra input (the OCR'd SRT of an image track).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveExtraSub {
    pub path: String,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub default: Option<bool>,
    #[serde(default)]
    pub forced: Option<bool>,
    #[serde(default)]
    pub hearing_impaired: Option<bool>,
}

/// Explicit mapping of an archival file: only what is listed is written.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveFile {
    /// Video copied instead of encoded (`encode` then only gives the input).
    #[serde(default)]
    pub copy_video: bool,
    /// `hvc1` instead of `hev1` on HEVC, MP4 only.
    #[serde(default)]
    pub hvc1_tag: bool,
    /// Absolute index of the main video, else the first video stream.
    #[serde(default)]
    pub video_index: Option<u32>,
    /// Cover art streams, copied.
    #[serde(default)]
    pub cover_art: Vec<u32>,
    /// Empties the global `title` (`-map_metadata 0` would carry it).
    #[serde(default)]
    pub clear_title: bool,
    pub audio: Vec<ArchiveAudio>,
    #[serde(default)]
    pub subtitles: Vec<ArchiveTrack>,
    /// Text files written as subtitle tracks after `subtitles`, one extra input each.
    #[serde(default)]
    pub extra_subtitles: Vec<ArchiveExtraSub>,
}

/// `pan` that folds up to 5.1 into stereo with a boosted centre, no clipping (coefficients sum to 1).
const STEREO_PAN: &str = "aformat=channel_layouts=5.1,pan=stereo|FL=0.32*FL+0.45*FC+0.23*BL|FR=0.32*FR+0.45*FC+0.23*BR,aformat=channel_layouts=stereo";

fn audio_args(k: usize, act: ArchiveAudioEnc) -> Vec<String> {
    let c = |o: &str| format!("{o}:a:{k}");
    match act {
        ArchiveAudioEnc::Copy => strs(&[&c("-c"), "copy"]),
        ArchiveAudioEnc::Eac3 { channels } => {
            let kbps = if channels <= 2 { "256k" } else { "640k" };
            strs(&[
                &c("-c"),
                "eac3",
                &c("-b"),
                kbps,
                &c("-filter"),
                "aformat=channel_layouts=5.1|stereo",
            ])
        }
        ArchiveAudioEnc::Aac { channels } => {
            let f = if channels <= 2 {
                "aformat=channel_layouts=stereo"
            } else {
                STEREO_PAN
            };
            strs(&[&c("-c"), "aac", &c("-b"), "256k", &c("-filter"), f])
        }
    }
}

fn tags(kind: char, k: usize, t: &ArchiveTrack) -> Vec<String> {
    let mut a = vec![];
    if let Some(l) = &t.language {
        a.extend([format!("-metadata:s:{kind}:{k}"), format!("language={l}")]);
    }
    if let Some(title) = &t.title {
        a.extend([format!("-metadata:s:{kind}:{k}"), format!("title={title}")]);
    }
    a.extend(disposition(kind, k, &[("default", t.default)]));
    a
}

/// One `-disposition` of the stream (a second one would replace the first); each flag is prefixed,
/// a bare value would replace forced, comment, hearing_impaired...
fn disposition(kind: char, k: usize, flags: &[(&str, Option<bool>)]) -> Vec<String> {
    let v: String = flags
        .iter()
        .filter_map(|(n, f)| f.map(|f| format!("{}{n}", if f { '+' } else { '-' })))
        .collect();
    if v.is_empty() {
        return vec![];
    }
    vec![format!("-disposition:{kind}:{k}"), v]
}

/// An archival file: the encode (or a video copy) of `f.encode`, explicitly mapped.
pub(super) fn file(
    f: &FileArgs,
    ar: &ArchiveFile,
    caps: &HostCaps,
    dev: &Device,
) -> Result<Vec<String>, String> {
    let o = &f.encode;
    let (mut a, vmap) = if ar.copy_video {
        let mut a = strs(&["-hide_banner", "-loglevel", "warning"]);
        a.extend(strs(&["-analyzeduration", "200M", "-probesize", "200M"]));
        a.extend(["-i".into(), o.input_path.clone()]);
        (a, video_map(ar.video_index))
    } else {
        let mut e = o.clone();
        e.video_stream_index = ar.video_index;
        let e = encode(&e, caps, dev)?;
        // Cover art is copied: the video options must target only the first output video stream.
        let a = e
            .args
            .into_iter()
            .map(|x| match x.as_str() {
                "-vf" => "-filter:v:0".into(),
                "-pix_fmt" => "-pix_fmt:v:0".into(),
                "-profile:v" => "-profile:v:0".into(),
                "-c:v" => "-c:v:0".into(),
                _ => x,
            })
            .collect();
        (a, e.video_map)
    };
    let mkv = f.format == FileFormat::Mkv;
    if let Some(at) = a
        .windows(2)
        .position(|w| w[0] == "-i" && w[1] == o.input_path)
    {
        let extra = ar
            .extra_subtitles
            .iter()
            .flat_map(|x| ["-i".into(), x.path.clone()]);
        a.splice(at + 2..at + 2, extra.collect::<Vec<String>>());
    }
    a.extend(["-map".into(), vmap]);
    // Matroska has no cover stream: the picture is attached after the mux (`cover_attachment_args`).
    for c in ar.cover_art.iter().filter(|_| !mkv) {
        a.extend(["-map".into(), format!("0:{c}")]);
    }
    for t in ar.audio.iter().map(|x| &x.track).chain(&ar.subtitles) {
        a.extend(["-map".into(), format!("0:{}", t.stream_index)]);
    }
    for k in 1..=ar.extra_subtitles.len() {
        a.extend(["-map".into(), format!("{k}:0")]);
    }
    if mkv {
        a.extend(strs(&["-map", "0:t?"]));
    }
    a.extend(strs(&[
        "-dn",
        "-map_chapters",
        "0",
        "-map_metadata",
        "0",
        "-fps_mode",
        "passthrough",
        "-max_muxing_queue_size",
        "4096",
    ]));
    if ar.clear_title {
        a.extend(strs(&["-metadata", "title="]));
    }
    if ar.copy_video {
        a.extend(strs(&["-c:v", "copy"]));
    }
    for k in (1..=ar.cover_art.len()).filter(|_| !mkv) {
        a.extend([format!("-c:v:{k}"), "copy".into()]);
    }
    for (k, x) in ar.audio.iter().enumerate() {
        a.extend(audio_args(k, x.action));
        a.extend(tags('a', k, &x.track));
    }
    if !ar.subtitles.is_empty() || !ar.extra_subtitles.is_empty() {
        // Text from a file is `subrip`: copied into MKV, `mov_text` in MP4.
        a.extend(strs(&["-c:s", if mkv { "copy" } else { "mov_text" }]));
    }
    for (k, t) in ar.subtitles.iter().enumerate() {
        if mkv && matches!(t.codec.as_deref(), Some("mov_text" | "webvtt")) {
            a.extend([format!("-c:s:{k}"), "srt".into()]);
        }
        a.extend(tags('s', k, t));
    }
    for (k, x) in ar.extra_subtitles.iter().enumerate() {
        let t = ArchiveTrack {
            stream_index: 0,
            language: x.language.clone(),
            title: x.title.clone(),
            default: None,
            codec: None,
        };
        let k = ar.subtitles.len() + k;
        a.extend(tags('s', k, &t));
        a.extend(disposition(
            's',
            k,
            &[
                ("default", x.default),
                ("forced", x.forced),
                ("hearing_impaired", x.hearing_impaired),
            ],
        ));
    }
    if mkv {
        a.extend(strs(&["-c:t", "copy"]));
        a.extend(strs(&["-f", "matroska", "-default_mode", "passthrough"]));
        a.extend(strs(&["-cues_to_front", "1"]));
    } else {
        if ar.hvc1_tag {
            a.extend(strs(&["-tag:v:0", "hvc1"]));
        }
        a.extend(strs(&["-f", "mp4", "-movflags", "+faststart"]));
    }
    a.push(f.path.clone());
    Ok(a)
}

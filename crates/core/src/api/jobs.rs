//! Background jobs (M5): `POST /v1/jobs` and the `BgJob*` events. Spec: `docs/spec/jobs.md`.
use super::{
    AlignLine, AlignMeta, Chapter, Fingerprint, MediaFamily, MediaInfo, MediaRef, MediaTags,
    MelodyMeta, MelodySpan, OptimizeLevel, OptimizePayload, SeparateMeta, SourceScan,
    VerifyPayload,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// What a job does; new kinds are additive variants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type")]
pub enum JobKind {
    /// Seek-bar thumbnails: `sprite.jpg` + `sprite.json`, served by the data plane.
    #[schema(title = "SpriteJob")]
    Sprite { info: MediaInfo },
    /// Keyframe scan of the whole file; needs a video stream in `info`.
    #[schema(title = "ScanJob")]
    Scan { info: MediaInfo },
    /// Chromaprint of the head of the file (an intro) or of its tail (an outro).
    #[schema(title = "FingerprintJob")]
    Fingerprint {
        window: FingerprintWindow,
        /// `introMaxLookupMinutes`: minutes of audio read; absent or 0: 10.
        #[serde(
            default,
            rename = "maxLookupMinutes",
            skip_serializing_if = "Option::is_none"
        )]
        max_lookup_minutes: Option<f64>,
    },
    /// Every embedded text track of the file as WebVTT, stored in the engine cache and served by
    /// the subtitle routes (`info.subtitles` names the tracks; image-based ones are skipped).
    #[schema(title = "SubtitleExtractJob")]
    SubtitleExtract { info: MediaInfo },
    /// Image-based embedded track (PGS or VobSub) OCR'd to SRT text.
    #[schema(title = "SubtitleOcrJob")]
    SubtitleOcr {
        info: MediaInfo,
        #[serde(rename = "streamIndex")]
        stream_index: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
    },
    /// Sidecar subtitle re-timed against audio or a reference track.
    #[schema(title = "SubtitleSyncJob")]
    SubtitleSync {
        info: MediaInfo,
        subtitle: SubtitleText,
        reference: SyncReference,
        #[serde(
            default,
            rename = "maxOffsetSeconds",
            skip_serializing_if = "Option::is_none"
        )]
        max_offset_seconds: Option<f64>,
        #[serde(
            default,
            rename = "noFixFramerate",
            skip_serializing_if = "Option::is_none"
        )]
        no_fix_framerate: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gss: Option<bool>,
    },
    /// A non-SRT sidecar or an embedded text track, converted to SRT text.
    #[schema(title = "SubtitleToSrtJob")]
    SubtitleToSrt { info: MediaInfo, source: SrtSource },
    /// The embedded picture of an audio file (`info.cover`), kept in the engine cache and read
    /// with `GET /v1/covers/{mediaFileId}`.
    #[schema(title = "CoverJob")]
    Cover { info: MediaInfo },
    /// The ordered audio files of one audiobook merged into one `.m4b` written at `output`, with a
    /// chapter per part (or the part's own chapters), the first cover and the parts' tags.
    /// `media` names the first part: it identifies the job (cancel, invalidate).
    #[schema(title = "MergeJob")]
    Merge {
        parts: Vec<MergePart>,
        /// Absolute `.m4b` path; the job fails when it exists. Written whole or not at all.
        output: String,
        /// Fields that win over what the parts carry (the book's own title, author...).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tags: Option<MediaTags>,
    },
    /// The vocals and the accompaniment of the audio file `media` names, as Opus files written at
    /// the absolute paths given (at least one). `guide` is the accompaniment with the vocals
    /// kept low, to sing along. Needs a separation model (`GET /v1/models`) and an ONNX runtime.
    #[schema(title = "SeparateJob")]
    Separate {
        info: MediaInfo,
        /// A `separation` model id; absent: the default one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        vocals: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instrumental: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        guide: Option<String>,
        /// Share of the vocals mixed into `guide`, 0 to 1; absent: 0.25.
        #[serde(
            default,
            rename = "guideVocalsGain",
            skip_serializing_if = "Option::is_none"
        )]
        guide_vocals_gain: Option<f64>,
        /// Opus bitrate of every output; absent: 96.
        #[serde(
            default,
            rename = "bitrateKbps",
            skip_serializing_if = "Option::is_none"
        )]
        bitrate_kbps: Option<u32>,
        /// Hardware providers the networks may run on (`cpu` always may); absent: every one the
        /// runtime lists but `openvino`; empty: the CPU only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        providers: Option<Vec<String>>,
    },
    /// Times the words of `lines` on the vocal stem `media` names: a CTC network forces the known
    /// words onto the audio, else speech recognition gives word times they are matched onto.
    /// Lines with times keep within them. Needs an `alignment` model.
    #[schema(title = "AlignJob")]
    Align {
        lines: Vec<AlignLine>,
        /// ISO 639 code of the singing; absent: detected from the lyrics.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
        /// An `alignment` model id; absent: the installed CTC model of the language, else the
        /// speech recogniser's.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// Hardware providers a CTC network may run on (`cpu` always may); absent or empty: the
        /// CPU only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        providers: Option<Vec<String>>,
    },
    /// The notes sung in each of `spans` of the vocal stem `media` names. Needs a `melody` model
    /// and an ONNX runtime.
    #[schema(title = "MelodyJob")]
    Melody {
        spans: Vec<MelodySpan>,
        /// A `melody` model id; absent: the default one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// Optimisation of one file by the plan (`docs/spec/jobs.md` section 11); the worker loop runs it
    /// from a lease (`PROTOCOL.md`), `/v1/jobs` accepts it too.
    #[schema(title = "OptimizeJob")]
    Optimize { payload: OptimizePayload },
    /// Integrity check of one file: full read, then a counted software decode.
    #[schema(title = "VerifyJob")]
    Verify { payload: VerifyPayload },
}

/// One audio file of a `Merge`, in listening order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MergePart {
    /// Absolute path, already authorised by the host.
    pub path: String,
    pub info: MediaInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SubtitleFormat {
    Srt,
    Sub,
    Ass,
    Ssa,
    Vtt,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleText {
    pub format: SubtitleFormat,
    pub text: String,
}

/// Where the SRT conversion reads. `file` is an absolute sidecar; `stream` is a text track of the job's media.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SrtSource {
    File { path: String },
    Stream { index: u32 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SyncReference {
    Auto,
    AudioStream { index: u32 },
    SubtitleStream { index: u32 },
    File { path: String },
}

/// Which audio the fingerprint reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum FingerprintWindow {
    /// The first lookup seconds, read by fpcalc itself (`baseOffsetSec` 0).
    Head,
    /// The last lookup seconds of a file of known duration.
    Outro {
        #[serde(rename = "durationSeconds")]
        duration_seconds: f64,
    },
}

impl JobKind {
    pub fn name(&self) -> &'static str {
        match self {
            JobKind::Sprite { .. } => "Sprite",
            JobKind::Scan { .. } => "Scan",
            JobKind::Fingerprint { .. } => "Fingerprint",
            JobKind::SubtitleExtract { .. } => "SubtitleExtract",
            JobKind::SubtitleOcr { .. } => "SubtitleOcr",
            JobKind::SubtitleSync { .. } => "SubtitleSync",
            JobKind::SubtitleToSrt { .. } => "SubtitleToSrt",
            JobKind::Cover { .. } => "Cover",
            JobKind::Merge { .. } => "Merge",
            JobKind::Separate { .. } => "Separate",
            JobKind::Align { .. } => "Align",
            JobKind::Melody { .. } => "Melody",
            JobKind::Optimize { .. } => "Optimize",
            JobKind::Verify { .. } => "Verify",
        }
    }

    /// The probe result the job is built from; `Fingerprint` reads the file alone.
    pub fn info(&self) -> Option<&MediaInfo> {
        match self {
            JobKind::Sprite { info }
            | JobKind::Scan { info }
            | JobKind::SubtitleExtract { info }
            | JobKind::SubtitleOcr { info, .. }
            | JobKind::SubtitleSync { info, .. }
            | JobKind::SubtitleToSrt { info, .. }
            | JobKind::Separate { info, .. }
            | JobKind::Cover { info } => Some(info),
            JobKind::Fingerprint { .. }
            | JobKind::Merge { .. }
            | JobKind::Align { .. }
            | JobKind::Melody { .. }
            | JobKind::Optimize { .. }
            | JobKind::Verify { .. } => None,
        }
    }

    /// The families of file the kind applies to.
    pub fn families(&self) -> &'static [MediaFamily] {
        match self {
            JobKind::Sprite { .. }
            | JobKind::Scan { .. }
            | JobKind::Fingerprint { .. }
            | JobKind::SubtitleExtract { .. }
            | JobKind::SubtitleOcr { .. }
            | JobKind::SubtitleToSrt { .. }
            | JobKind::Optimize { .. }
            | JobKind::Verify { .. } => VIDEO_ONLY,
            JobKind::SubtitleSync { .. } => ANY,
            JobKind::Cover { .. }
            | JobKind::Merge { .. }
            | JobKind::Separate { .. }
            | JobKind::Align { .. }
            | JobKind::Melody { .. } => AUDIO_ONLY,
        }
    }
}

const VIDEO_ONLY: &[MediaFamily] = &[MediaFamily::Video];
const AUDIO_ONLY: &[MediaFamily] = &[MediaFamily::Audio];
// Sync aligns cues on the audio track, so a probe without video is enough.
const ANY: &[MediaFamily] = &[MediaFamily::Video, MediaFamily::Audio];

/// Which families of file each job kind applies to (`hello.jobFamilies`); `families` is the
/// per-request form, a test keeps the two equal. `Optimize` and `Verify` are leased by workers
/// (accepted on the cpu lane through `/v1/jobs`, tests only), so they are not listed.
pub const JOB_FAMILIES: &[(&str, &[MediaFamily])] = &[
    ("Sprite", VIDEO_ONLY),
    ("Scan", VIDEO_ONLY),
    ("Fingerprint", VIDEO_ONLY),
    ("SubtitleExtract", VIDEO_ONLY),
    ("SubtitleOcr", VIDEO_ONLY),
    ("SubtitleSync", ANY),
    ("SubtitleToSrt", VIDEO_ONLY),
    ("Cover", AUDIO_ONLY),
    ("Merge", AUDIO_ONLY),
    ("Separate", AUDIO_ONLY),
    ("Align", AUDIO_ONLY),
    ("Melody", AUDIO_ONLY),
];

/// The staged file of an `Optimize`, verified and hashed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OptimizeMeta {
    /// Where the output is.
    pub staged_path: String,
    /// f64: no 64-bit integers on the wire.
    pub out_size: f64,
    pub out_sha256: String,
    /// Probe of the output.
    pub out_info: MediaInfo,
    /// What was really done: a `reencode` whose video was copied reports `remux`.
    pub level: OptimizeLevel,
    /// Why a planned re-encode copied the video (`unsupported_source`, `low_savings`, `too_slow`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_reason: Option<String>,
    /// The output was produced on the CPU after the hardware encoder failed its run or its check.
    pub cpu_fallback: bool,
    /// A copied video left nothing to change: no output was staged (`stagedPath` is not a file).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub nothing_to_do: bool,
}

/// A source that read and decoded cleanly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct VerifyMeta {
    pub bytes: f64,
    pub duration_seconds: f64,
}

/// A job kind and the families of file it applies to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobFamilies {
    /// `JobKind` `type`.
    pub kind: String,
    pub families: Vec<MediaFamily>,
}

/// The `.m4b` a `Merge` wrote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MergeMeta {
    /// The `output` of the request.
    pub path: String,
    /// f64: no 64-bit integers on the wire.
    pub bytes: f64,
    pub duration_seconds: f64,
    /// On the merged timeline, in listening order.
    pub chapters: Vec<Chapter>,
    /// The audio was encoded again to AAC; false when the parts' AAC was copied.
    pub reencoded: bool,
}

/// The stored cover of `GET /v1/covers/{mediaFileId}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CoverMeta {
    /// `image/jpeg` or `image/png`.
    pub mime: String,
    pub bytes: u32,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum JobPriority {
    High,
    #[default]
    Normal,
    Low,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobRequest {
    pub kind: JobKind,
    pub media: MediaRef,
    /// Order among queued jobs; playback always comes first. Absent: `normal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<JobPriority>,
    /// Redo the work even when its output exists.
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobAccepted {
    /// The in-flight job of an identical request, else a new one.
    pub job_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum BgJobState {
    Queued,
    Running,
    /// Running, every ffmpeg run it holds preempted by playback.
    Paused,
    Done,
    Failed,
    Cancelled,
}

impl BgJobState {
    pub fn ended(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

/// `sprite.json`: tile `i` covers `[i * interval, (i + 1) * interval)` seconds of content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SpriteMeta {
    pub interval: u32,
    pub columns: u32,
    pub thumb_width: u32,
    pub thumb_height: u32,
    pub count: u32,
}

/// What a finished job hands back, as data (files stay in the engine and are served by it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type")]
pub enum JobResult {
    /// Served at `/api/stream/{mediaFileId}/thumbnails/sprite.{jpg,json}`.
    Sprite {
        sprite: SpriteMeta,
    },
    /// `schemaVersion` stamped; the decide input's `scan`.
    Scan {
        scan: SourceScan,
    },
    Fingerprint {
        fingerprint: Fingerprint,
    },
    /// Stream indices whose VTT is now cached, served at `/api/stream/{id}/subtitles/embedded/{idx}`.
    SubtitleExtract {
        streams: Vec<u32>,
    },
    SubtitleOcr {
        srt: String,
    },
    SubtitleSync {
        subtitle: SubtitleText,
    },
    SubtitleToSrt {
        srt: String,
    },
    /// The file is read with `GET /v1/covers/{mediaFileId}`.
    Cover {
        cover: CoverMeta,
    },
    /// The file is at the request's `output`.
    Merge {
        merge: MergeMeta,
    },
    /// The stems are at the request's paths.
    Separate {
        separate: SeparateMeta,
    },
    Align {
        align: AlignMeta,
    },
    Melody {
        melody: MelodyMeta,
    },
    Optimize {
        optimize: Box<OptimizeMeta>,
    },
    /// A faulty file fails the job with class `damaged` or `corrupt`.
    Verify {
        verify: VerifyMeta,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobSnapshot {
    pub job_id: String,
    /// The request's `kind.type`.
    pub kind: String,
    pub media_file_id: u32,
    pub priority: JobPriority,
    pub state: BgJobState,
    /// 0 to 100.
    pub pct: f64,
    pub created_ms: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<JobResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_reads_with_defaults_and_unknown_fields() {
        let r: JobRequest = serde_json::from_value(json!({
            "kind": { "type": "Sprite", "info": { "video": [], "audio": [], "subtitles": [] } },
            "media": { "mediaFileId": 7, "path": "/m.mkv" }, "future": 1
        }))
        .unwrap();
        assert_eq!(
            (r.priority, r.force, r.kind.name()),
            (None, false, "Sprite")
        );
        assert!(JobPriority::High < JobPriority::Normal && JobPriority::Normal < JobPriority::Low);
    }

    #[test]
    fn the_families_table_matches_every_kind() {
        let info = || json!({ "video": [], "audio": [], "subtitles": [] });
        let kinds = [
            json!({ "type": "Sprite", "info": info() }),
            json!({ "type": "Scan", "info": info() }),
            json!({ "type": "Fingerprint", "window": { "type": "head" } }),
            json!({ "type": "SubtitleExtract", "info": info() }),
            json!({ "type": "SubtitleOcr", "info": info(), "streamIndex": 0 }),
            json!({ "type": "SubtitleSync", "info": info(), "subtitle": { "format": "srt",
                "text": "" }, "reference": { "type": "auto" } }),
            json!({ "type": "SubtitleToSrt", "info": info(), "source": { "type": "stream",
                "index": 0 } }),
            json!({ "type": "Cover", "info": info() }),
            json!({ "type": "Merge", "parts": [], "output": "/a.m4b" }),
            json!({ "type": "Separate", "info": info(), "vocals": "/v.opus" }),
            json!({ "type": "Align", "lines": [] }),
            json!({ "type": "Melody", "spans": [] }),
        ];
        assert_eq!(kinds.len(), JOB_FAMILIES.len());
        for k in kinds {
            let kind: JobKind = serde_json::from_value(k).unwrap();
            let row = JOB_FAMILIES.iter().find(|(n, _)| *n == kind.name());
            assert_eq!(row.map(|r| r.1), Some(kind.families()), "{}", kind.name());
        }
    }

    #[test]
    fn a_future_family_reads_as_unknown() {
        let f: JobFamilies =
            serde_json::from_value(json!({ "kind": "Lyrics", "families": ["text", "audio"] }))
                .unwrap();
        assert_eq!(f.families, [MediaFamily::Unknown, MediaFamily::Audio]);
    }
}

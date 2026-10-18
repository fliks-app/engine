//! Background jobs (M5): `POST /v1/jobs` and the `BgJob*` events. Spec: `docs/spec/jobs.md`.
use super::{Fingerprint, MediaInfo, MediaRef, SourceScan};
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
        }
    }
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
}

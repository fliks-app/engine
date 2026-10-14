//! Background jobs (M5): `POST /v1/jobs` and the `BgJob*` events. Spec: `docs/spec/jobs.md`.
use super::{MediaInfo, MediaRef};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// What a job does; new kinds are additive variants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type")]
pub enum JobKind {
    /// Seek-bar thumbnails: `sprite.jpg` + `sprite.json`, served by the data plane.
    Sprite { info: MediaInfo },
}

impl JobKind {
    pub fn name(&self) -> &'static str {
        match self {
            JobKind::Sprite { .. } => "Sprite",
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
    Sprite { sprite: SpriteMeta },
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

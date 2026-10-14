//! Wire types of the session runtime (M3): sessions, jobs, cache, revocation, events and the
//! host resolver contract. Times are f64 epoch milliseconds (no 64-bit integers on the wire).
use super::{DecideRequest, Decision, HwAccel};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;

/// Display facts of the client a session plays on; opaque to the engine.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct ClientInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum BurnInKind {
    Text,
    Image,
}

/// A subtitle burnt into the video, already resolved by the host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BurnInSpec {
    #[serde(rename = "type")]
    pub kind: BurnInKind,
    /// The libass `-vf` step (`subtitles='...'`) of a text track.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// Stream index of an image track, overlaid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_index: Option<u32>,
}

/// `POST /v1/sessions`: decide, open the session and prewarm its job, atomically.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OpenSessionRequest {
    pub decide: DecideRequest,
    /// Opaque owner id (`[A-Za-z0-9_.-]`, 1 to 64 chars); absent = anonymous.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,
    #[serde(default)]
    pub client: ClientInfo,
    /// Opaque to the engine, echoed in snapshots (e.g. the host's connection ids).
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
    /// Resume position, seconds on the content timeline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_at_s: Option<f64>,
    /// Offline download: the session lives on the pinned TTL and is never throttled.
    #[serde(default)]
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burn_in: Option<BurnInSpec>,
    /// Version of the source file (rescan counter, mtime): part of the cache key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_version: Option<String>,
    /// Master `SUBTITLES` group for clients with `supportsHlsSubtitles`; absent: the embedded
    /// text tracks of `info`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle_renditions: Option<Vec<SubtitleTrack>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SubtitleTrackKind {
    Embedded,
    External,
}

/// One text subtitle rendition of the master.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleTrack {
    pub kind: SubtitleTrackKind,
    /// Stream index (embedded) or subtitle id (external).
    pub key: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language_tag: Option<String>,
    pub name: String,
    #[serde(default)]
    pub forced: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Hints {
    /// The probe blob predates the engine's schema version.
    pub probe_stale: bool,
    /// No source scan: the remux keyframe grid and audio checks were skipped.
    pub scan_missing: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OpenSessionResponse {
    pub session_id: String,
    /// Cache profile hash; absent on Direct Play.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_hash: Option<String>,
    /// `decision.response.playUrl` with `sid=` appended.
    pub play_path: String,
    pub decision: Decision,
    pub hints: Hints,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SessionKind {
    Directplay,
    Remux,
    Transcode,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct HeartbeatRequest {
    /// When given, must be the session's owner (403 otherwise).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_s: Option<f64>,
    /// Player state, opaque (`playing`, `paused`, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatResponse {
    /// The sid is unknown: the client re-opens a session.
    pub session_lost: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum SessionAction {
    Stop,
    PauseJob,
    ResumeJob,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SessionCommand {
    pub action: SessionAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Running,
    Paused,
    /// Exited 0.
    Done,
    /// Exited non-zero or by a signal it was not sent.
    Failed,
    /// Stopped by the engine (seek, rung switch, idle, revoke).
    Killed,
}

/// The ffmpeg run behind a session ("stats for nerds").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobStats {
    pub state: JobState,
    pub quality: String,
    pub requested_hw: HwAccel,
    /// `none` after a hardware failure fell back to the CPU.
    pub actual_hw: HwAccel,
    pub start_segment: u32,
    /// Latest segment written, absent before the first one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontier_segment: Option<u32>,
    pub transcode_percent: f64,
    pub throttled: bool,
    /// Spawns of this job key after the first (seeks, rung switches, fallbacks).
    pub restarts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Last stderr lines, bounded.
    pub stderr_tail: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionSnapshot {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,
    pub media_file_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_hash: Option<String>,
    pub kind: SessionKind,
    /// Last rung served or reported by a heartbeat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
    pub client: ClientInfo,
    pub tags: BTreeMap<String, String>,
    pub opened_at_ms: f64,
    pub last_beat_ms: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_segment_at_ms: Option<f64>,
    pub position_s: f64,
    pub state: String,
    pub pinned: bool,
    /// `EngineConfig.revision` the session froze.
    pub config_revision: u32,
    pub decision: Decision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<JobStats>,
}

/// `POST /v1/revoke`: at least one field.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct RevokeRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_file_ids: Option<Vec<u32>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RevokeResult {
    pub sessions: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct InvalidateRequest {
    pub media_file_id: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct InvalidateResult {
    pub jobs: u32,
    pub cache_entries: u32,
}

/// One `<owner>/<file>/<profileHash>` directory of the cache.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CacheEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,
    pub media_file_id: u32,
    pub profile_hash: String,
    pub bytes: f64,
    pub last_access_ms: f64,
    /// A session or job uses it: never evicted.
    pub live: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CacheSummary {
    pub used_bytes: f64,
    pub entries: Vec<CacheEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CacheDeleteResult {
    pub removed: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Track {
    Video,
    Audio,
}

/// `GET /v1/events` (SSE): the SSE event name is `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum EngineEvent {
    SessionOpened {
        session: Box<SessionSnapshot>,
    },
    /// `stopped`, `expired`, `evicted`, `revoked`.
    SessionEnded {
        session_id: String,
        reason: String,
    },
    /// The rung or hardware a session is served from changed.
    PipelineChanged {
        session_id: String,
        quality: String,
        hw_accel: HwAccel,
    },
    SegmentServed {
        session_id: String,
        track: Track,
        rung: String,
        index: u32,
        /// How long the request waited for the segment to be produced.
        waited_ms: f64,
        /// Already on disk when requested.
        cache_hit: bool,
        /// The request's CMCD data, raw.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cmcd_raw: Option<String>,
    },
    JobStarted {
        job_key: String,
        media_file_id: u32,
        quality: String,
        start_segment: u32,
        hw_accel: HwAccel,
    },
    JobEnded {
        job_key: String,
        state: JobState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
    },
    HwFallback {
        job_key: String,
        from: HwAccel,
        to: HwAccel,
        stderr_tail: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ResolvePurpose {
    Stream,
    Download,
    Sprite,
    Subtitle,
    Live,
}

/// `mediaId` of a resolve: a media file id, or a live session id (`purpose: live`, M6).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum ResolveMediaId {
    File(u32),
    Live(String),
}

/// `POST {hostUrl}/engine/resolve`, implemented by the host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResolveRequest {
    pub media_id: ResolveMediaId,
    /// The client credential exactly as received, one of: the `Cookie` header value, the
    /// `Authorization` header value (`Bearer <jwt>`), the bare `?token=` value.
    pub credential: String,
    pub purpose: ResolvePurpose,
    /// External subtitle (`purpose: subtitle`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle_id: Option<u32>,
    /// Embedded subtitle stream (`purpose: subtitle`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_index: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResolveResponse {
    pub allow: bool,
    /// Media file; `sprite.jpg` (`sprite`, may not exist); the subtitle file (external
    /// subtitle) or the media file (embedded subtitle).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,
    /// MIME type of a media file; a subtitle's extension (`srt`, `ass`, `vtt`) or `embedded`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Cache lifetime of this answer; 60 when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_s: Option<f64>,
}

/// 410 body of a data-plane request whose `sid` the engine no longer knows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SessionExpired {
    /// Always `session_expired`.
    pub code: String,
    pub sid: Option<String>,
    pub message: String,
}

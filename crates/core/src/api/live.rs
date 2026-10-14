//! Wire types of live TV (M6): open, viewer snapshots, settings. Spec: `docs/spec/live.md`.
use super::QoePipeline;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum LiveMode {
    /// The upstream bytes teed to every viewer (`direct.ts`), no ffmpeg.
    Direct,
    /// Video copied into ffmpeg's sliding-window HLS.
    Remux,
    /// H.264 encoded into ffmpeg's sliding-window HLS.
    Transcode,
}

/// `EngineConfig.livetv`: the host's `livetv_*` settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct LiveTvConfig {
    /// HLS segment length; floored at 1.
    pub segment_seconds: i32,
    /// DVR window; floored at 1.
    pub timeshift_minutes: i32,
    /// ffmpeg's input probe window; never below 3.
    pub probe_seconds: i32,
    /// `true`: transcode, `false`: remux; absent: transcode on a hardware host only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fast_zap: Option<bool>,
    /// A session without viewers is torn down after this long.
    pub channel_idle_seconds: i32,
}

impl Default for LiveTvConfig {
    fn default() -> Self {
        Self {
            segment_seconds: 2,
            timeshift_minutes: 15,
            probe_seconds: 3,
            fast_zap: None,
            channel_idle_seconds: 30,
        }
    }
}

/// One way to reach the channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LiveUpstream {
    pub url: String,
    /// Lower first; ties keep the request's order.
    #[serde(default)]
    pub priority: i32,
    /// Sent with every request to the upstream; `User-Agent` defaults to VLC's.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// From an earlier probe (`LiveAttempt.probe`): the engine skips its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probed_container: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probed_video_codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probed_audio_codec: Option<String>,
}

/// What the viewer's player takes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct LiveCaps {
    pub direct_play: bool,
    /// Forces a transcode; snapped to a rung of 1.5, 3, 6, 10 or 20 Mb/s.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bitrate_bps: Option<f64>,
    /// MPEG-TS segments instead of fMP4.
    pub use_ts: bool,
}

/// `POST /v1/live`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OpenLiveRequest {
    /// Opaque; viewers of one channel, mode, container and bitrate share one upstream.
    pub channel_id: String,
    pub upstreams: Vec<LiveUpstream>,
    #[serde(default)]
    pub caps: LiveCaps,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,
    /// The host's capacity hold; given back by `LiveReleased` when the session ends.
    pub capacity_token: String,
    /// The host's capacity policy refuses a new upstream: answered 409 unless a session is joined.
    #[serde(default)]
    pub at_capacity: bool,
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
}

/// ffprobe's view of an upstream.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct LiveProbe {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_codec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_codec: Option<String>,
    /// ffprobe's `format_name` (comma list).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OpenLiveResponse {
    /// This viewer's id: the data-plane sid, `DELETE /v1/sessions/{sid}`.
    pub session_id: String,
    /// `/api/livetv/sessions/<sid>/index.m3u8`, or `direct.ts` for `direct`.
    pub play_path: String,
    /// The mode served (a remux of a non-H.264 upstream is a transcode).
    pub mode: LiveMode,
    /// The feeding upstream, a position in the upstreams of the open that started the session.
    pub upstream: u32,
    /// The session's hold: the request's when this open started it, else the joined session's.
    pub capacity_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<LiveProbe>,
    /// 0 for `direct`.
    pub dvr_window_seconds: f64,
    pub segment_seconds: f64,
    pub qoe_pipeline: QoePipeline,
}

/// One viewer of a live session (`GET /v1/live`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LiveSnapshot {
    pub session_id: String,
    pub channel_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,
    pub tags: BTreeMap<String, String>,
    pub mode: LiveMode,
    pub capacity_token: String,
    /// The feeding upstream, a position in the upstreams of the open that started the session.
    pub upstream: u32,
    /// URL of the feeding upstream, so a host can name its source after a restart.
    pub upstream_url: String,
    /// `priority` of the feeding upstream.
    pub upstream_priority: i32,
    pub opened_at_ms: f64,
    pub last_seen_ms: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_codec: Option<String>,
    /// `fmp4` or `mpegts`.
    pub container: String,
    /// Viewers sharing the upstream, this one included.
    pub viewers_on_upstream: u32,
    /// Failed attempts since the session started.
    pub restarts: u32,
}

/// Body of a refused `POST /v1/live`: `live_at_capacity` (409) or `live_unavailable` (503).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct LiveError {
    pub code: String,
}

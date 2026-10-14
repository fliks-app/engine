//! Durations of the session runtime, sent in `EngineConfig.lifetime`. Defaults are the host's.
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub const DEFAULT_LIVE_SESSION_TTL_MS: u32 = 30_000;
pub const DEFAULT_LIVE_SESSION_GC_INTERVAL_MS: u32 = 5_000;
pub const DEFAULT_SESSION_REVIVE_TTL_MS: u32 = 15 * 60 * 1000;
pub const DEFAULT_MAX_SESSIONS_PER_OWNER: u32 = 10;
pub const DEFAULT_JOB_GRACE_MS: u32 = 60_000;
pub const DEFAULT_JOB_FALLBACK_TIMEOUT_MS: u32 = 30 * 60 * 1000;
pub const DEFAULT_PINNED_SESSION_TTL_MS: u32 = 60 * 60 * 1000;
pub const DEFAULT_CACHE_GC_INTERVAL_MS: u32 = 5 * 60 * 1000;

/// How long sessions, ffmpeg jobs and cache sweeps live; the cache TTL and budget are in
/// `StreamingConfig`. Zero is not a duration: `session::LifetimeConfig` keeps the default then.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct LifetimeConfig {
    /// A session without a heartbeat for this long is dropped (kept revivable).
    pub live_session_ttl_ms: u32,
    pub live_session_gc_interval_ms: u32,
    /// How long a dropped session's id can still revive.
    pub session_revive_ttl_ms: u32,
    /// Beyond it the owner's oldest session is evicted, never the new one.
    pub max_sessions_per_owner: u32,
    /// Delay between the last viewer leaving and the ffmpeg job being killed.
    pub job_grace_ms: u32,
    /// Idle window of a job that never had a viewer.
    pub job_fallback_timeout_ms: u32,
    /// Heartbeat TTL of pinned (download, offline) sessions.
    pub pinned_session_ttl_ms: u32,
    pub cache_gc_interval_ms: u32,
}

impl Default for LifetimeConfig {
    fn default() -> Self {
        Self {
            live_session_ttl_ms: DEFAULT_LIVE_SESSION_TTL_MS,
            live_session_gc_interval_ms: DEFAULT_LIVE_SESSION_GC_INTERVAL_MS,
            session_revive_ttl_ms: DEFAULT_SESSION_REVIVE_TTL_MS,
            max_sessions_per_owner: DEFAULT_MAX_SESSIONS_PER_OWNER,
            job_grace_ms: DEFAULT_JOB_GRACE_MS,
            job_fallback_timeout_ms: DEFAULT_JOB_FALLBACK_TIMEOUT_MS,
            pinned_session_ttl_ms: DEFAULT_PINNED_SESSION_TTL_MS,
            cache_gc_interval_ms: DEFAULT_CACHE_GC_INTERVAL_MS,
        }
    }
}

impl LifetimeConfig {
    /// The host reads each env var as a positive integer, else the default; zero means unset.
    pub fn resolved(&self) -> Self {
        let d = Self::default();
        let pick = |v: u32, dv: u32| if v > 0 { v } else { dv };
        Self {
            live_session_ttl_ms: pick(self.live_session_ttl_ms, d.live_session_ttl_ms),
            live_session_gc_interval_ms: pick(
                self.live_session_gc_interval_ms,
                d.live_session_gc_interval_ms,
            ),
            session_revive_ttl_ms: pick(self.session_revive_ttl_ms, d.session_revive_ttl_ms),
            max_sessions_per_owner: pick(self.max_sessions_per_owner, d.max_sessions_per_owner),
            job_grace_ms: pick(self.job_grace_ms, d.job_grace_ms),
            job_fallback_timeout_ms: pick(self.job_fallback_timeout_ms, d.job_fallback_timeout_ms),
            pinned_session_ttl_ms: pick(self.pinned_session_ttl_ms, d.pinned_session_ttl_ms),
            cache_gc_interval_ms: pick(self.cache_gc_interval_ms, d.cache_gc_interval_ms),
        }
    }
}

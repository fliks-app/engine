//! Session and job lifetimes. [ts:live-session.service.ts] [ts:transcoding/transcoding.service.ts:1553-1620]
use crate::api::LifetimeConfig;
use serde::{Deserialize, Serialize};

/// A session without a beat for the TTL (the pinned one for download sessions) is dropped.
pub fn live_session_expired(
    last_beat_ms: i64,
    pinned: bool,
    now_ms: i64,
    cfg: &LifetimeConfig,
) -> bool {
    let c = cfg.resolved();
    let ttl = if pinned {
        c.pinned_session_ttl_ms
    } else {
        c.live_session_ttl_ms
    };
    last_beat_ms < now_ms - i64::from(ttl)
}

/// A dropped session's id revives while inside the revive window (a pinned one's counts from the
/// moment it was dropped).
pub fn tombstone_revivable(
    last_beat_ms: i64,
    now_ms: i64,
    pinned: bool,
    cfg: &LifetimeConfig,
) -> bool {
    let c = cfg.resolved();
    let window = if pinned {
        i64::from(c.pinned_session_ttl_ms) + i64::from(c.pinned_revive_ttl_ms)
    } else {
        i64::from(c.session_revive_ttl_ms)
    };
    last_beat_ms >= now_ms - window
}

/// At the cap the owner's oldest beat goes (the first one on ties), never the new session.
pub fn session_to_evict(owner_last_beats_ms: &[i64], cfg: &LifetimeConfig) -> Option<usize> {
    if (owner_last_beats_ms.len() as u64) < u64::from(cfg.resolved().max_sessions_per_owner) {
        return None;
    }
    owner_last_beats_ms
        .iter()
        .enumerate()
        .reduce(|a, b| if a.1 <= b.1 { a } else { b })
        .map(|(i, _)| i)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobWatchInput {
    pub now_ms: i64,
    pub started_ms: i64,
    pub last_progress_ms: i64,
    pub paused: bool,
    /// CPU time the process burnt since the previous tick (0 where it cannot be read).
    pub cpu_delta_ms: u64,
    /// Media time of a run with a natural end (the early companion's `-to`), in ms.
    pub natural_end_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JobWatch {
    Ok,
    Stalled,
    Overran,
}

/// One watchdog tick of a running job; a paused one is exempt. [new]
pub fn job_watch(i: &JobWatchInput, cfg: &LifetimeConfig) -> JobWatch {
    let stall = i64::from(cfg.resolved().job_stall_timeout_ms);
    if stall == 0 || i.paused {
        return JobWatch::Ok;
    }
    if i.natural_end_ms
        .is_some_and(|e| i.now_ms - i.started_ms > e + stall)
    {
        return JobWatch::Overran;
    }
    if i.cpu_delta_ms == 0 && i.now_ms - i.last_progress_ms > stall {
        JobWatch::Stalled
    } else {
        JobWatch::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> LifetimeConfig {
        LifetimeConfig::default()
    }

    #[test]
    fn expiry_is_strict_and_pinned_lives_longer() {
        assert!(!live_session_expired(70_000, false, 100_000, &cfg()));
        assert!(live_session_expired(69_999, false, 100_000, &cfg()));
        assert!(!live_session_expired(0, true, 3_600_000, &cfg()));
        assert!(live_session_expired(0, true, 3_600_001, &cfg()));
        assert!(tombstone_revivable(0, 900_000, false, &cfg()));
        assert!(!tombstone_revivable(0, 900_001, false, &cfg()));
        // A pinned one revives for a day after its hour.
        assert!(tombstone_revivable(0, 3_600_000 + 86_400_000, true, &cfg()));
        assert!(!tombstone_revivable(
            0,
            3_600_001 + 86_400_000,
            true,
            &cfg()
        ));
    }

    #[test]
    fn zero_durations_keep_the_defaults() {
        let zero = LifetimeConfig {
            live_session_ttl_ms: 0,
            ..cfg()
        };
        assert!(!live_session_expired(70_000, false, 100_000, &zero));
    }

    #[test]
    fn cap_evicts_the_oldest_first_on_ties() {
        let c = LifetimeConfig {
            max_sessions_per_owner: 3,
            ..cfg()
        };
        assert_eq!(session_to_evict(&[5, 9], &c), None);
        assert_eq!(session_to_evict(&[5, 3, 3], &c), Some(1));
    }

    fn watch_input(now: i64, progress: i64) -> JobWatchInput {
        JobWatchInput {
            now_ms: now,
            started_ms: 0,
            last_progress_ms: progress,
            paused: false,
            cpu_delta_ms: 0,
            natural_end_ms: None,
        }
    }

    #[test]
    fn a_job_burning_cpu_is_not_stalled_but_a_flat_one_is() {
        let busy = JobWatchInput {
            cpu_delta_ms: 10,
            ..watch_input(999_999, 0)
        };
        assert_eq!(job_watch(&busy, &cfg()), JobWatch::Ok);
        assert_eq!(
            job_watch(&watch_input(999_999, 0), &cfg()),
            JobWatch::Stalled
        );
    }

    #[test]
    fn a_job_without_progress_is_stalled_past_the_timeout() {
        assert_eq!(job_watch(&watch_input(30_000, 0), &cfg()), JobWatch::Ok);
        assert_eq!(
            job_watch(&watch_input(30_001, 0), &cfg()),
            JobWatch::Stalled
        );
        assert_eq!(
            job_watch(&watch_input(40_000, 15_000), &cfg()),
            JobWatch::Ok
        );
    }

    #[test]
    fn paused_and_disabled_jobs_are_exempt() {
        let paused = JobWatchInput {
            paused: true,
            ..watch_input(999_999, 0)
        };
        assert_eq!(job_watch(&paused, &cfg()), JobWatch::Ok);
        let off = LifetimeConfig {
            job_stall_timeout_ms: 0,
            ..cfg()
        };
        assert_eq!(job_watch(&watch_input(999_999, 0), &off), JobWatch::Ok);
        let custom = LifetimeConfig {
            job_stall_timeout_ms: 1_000,
            ..cfg()
        };
        assert_eq!(
            job_watch(&watch_input(1_001, 0), &custom),
            JobWatch::Stalled
        );
    }

    #[test]
    fn a_run_with_a_natural_end_is_killed_past_it_even_with_progress() {
        let early = |now| JobWatchInput {
            natural_end_ms: Some(7_000),
            cpu_delta_ms: 50,
            ..watch_input(now, now)
        };
        assert_eq!(job_watch(&early(37_000), &cfg()), JobWatch::Ok);
        assert_eq!(job_watch(&early(37_001), &cfg()), JobWatch::Overran);
        let off = LifetimeConfig {
            job_stall_timeout_ms: 0,
            ..cfg()
        };
        assert_eq!(job_watch(&early(1_000_000), &off), JobWatch::Ok);
    }

    #[test]
    fn the_stall_field_defaults_to_30_s_when_absent() {
        let c: LifetimeConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(c.job_stall_timeout_ms, 30_000);
        let c: LifetimeConfig = serde_json::from_str(r#"{"jobStallTimeoutMs":0}"#).unwrap();
        assert_eq!(c.resolved().job_stall_timeout_ms, 0);
    }
}

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

/// A dropped session's id revives while inside the revive window.
pub fn tombstone_revivable(last_beat_ms: i64, now_ms: i64, cfg: &LifetimeConfig) -> bool {
    last_beat_ms >= now_ms - i64::from(cfg.resolved().session_revive_ttl_ms)
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

/// What the cleanup tick remembers of one job.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobIdleState {
    pub seen_any_viewer: bool,
    pub zero_viewers_since_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobIdleInput {
    pub now_ms: i64,
    pub last_access_ms: i64,
    /// The job carries a base profile hash (viewers can be matched to it).
    pub has_base_hash: bool,
    pub viewers: u32,
    pub process_running: bool,
    pub state: JobIdleState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum JobIdleAction {
    Keep,
    /// Drop the entry; kill ffmpeg when `kill` (the cache stays).
    Reap {
        kill: bool,
    },
}

/// One cleanup tick of one job: the new state and what to do.
pub fn job_idle(i: &JobIdleInput, cfg: &LifetimeConfig) -> (JobIdleState, JobIdleAction) {
    let c = cfg.resolved();
    let (grace, fallback) = (
        i64::from(c.job_grace_ms),
        i64::from(c.job_fallback_timeout_ms),
    );
    let mut s = i.state;
    // No viewer was ever paired: a plain idle timeout.
    let idle_fallback = |s| {
        if i.now_ms - i.last_access_ms <= fallback {
            (s, JobIdleAction::Keep)
        } else {
            (
                s,
                JobIdleAction::Reap {
                    kill: i.process_running,
                },
            )
        }
    };
    if !i.has_base_hash {
        return idle_fallback(s);
    }
    if i.viewers > 0 {
        s.seen_any_viewer = true;
        s.zero_viewers_since_ms = None;
        return (s, JobIdleAction::Keep);
    }
    // A recent segment fetch keeps the job alive through a missed heartbeat.
    if i.now_ms - i.last_access_ms < grace {
        s.zero_viewers_since_ms = None;
        return (s, JobIdleAction::Keep);
    }
    if !s.seen_any_viewer {
        return idle_fallback(s);
    }
    match s.zero_viewers_since_ms {
        None => {
            s.zero_viewers_since_ms = Some(i.now_ms);
            (s, JobIdleAction::Keep)
        }
        Some(t) if i.now_ms - t < grace => (s, JobIdleAction::Keep),
        Some(_) => (s, JobIdleAction::Reap { kill: true }),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobWatchInput {
    pub now_ms: i64,
    pub started_ms: i64,
    pub last_progress_ms: i64,
    pub paused: bool,
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
    if i.now_ms - i.last_progress_ms > stall {
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
        assert!(tombstone_revivable(0, 900_000, &cfg()));
        assert!(!tombstone_revivable(0, 900_001, &cfg()));
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

    fn input(now: i64, last: i64, viewers: u32, state: JobIdleState) -> JobIdleInput {
        JobIdleInput {
            now_ms: now,
            last_access_ms: last,
            has_base_hash: true,
            viewers,
            process_running: true,
            state,
        }
    }

    #[test]
    fn a_job_survives_the_grace_after_its_last_viewer() {
        let seen = JobIdleState {
            seen_any_viewer: true,
            zero_viewers_since_ms: None,
        };
        let (s, a) = job_idle(&input(100_000, 0, 0, seen), &cfg());
        assert_eq!(
            (a, s.zero_viewers_since_ms),
            (JobIdleAction::Keep, Some(100_000))
        );
        let (_, a) = job_idle(&input(159_999, 0, 0, s), &cfg());
        assert_eq!(a, JobIdleAction::Keep);
        let (_, a) = job_idle(&input(160_000, 0, 0, s), &cfg());
        assert_eq!(a, JobIdleAction::Reap { kill: true });
        let (s2, _) = job_idle(&input(160_000, 0, 1, s), &cfg());
        assert_eq!(s2.zero_viewers_since_ms, None);
    }

    #[test]
    fn an_unpaired_job_times_out_on_the_fallback() {
        let fresh = JobIdleState::default();
        let (_, a) = job_idle(&input(1_800_000, 0, 0, fresh), &cfg());
        assert_eq!(a, JobIdleAction::Keep);
        let (_, a) = job_idle(&input(1_800_001, 0, 0, fresh), &cfg());
        assert_eq!(a, JobIdleAction::Reap { kill: true });
        let dead = JobIdleInput {
            process_running: false,
            has_base_hash: false,
            ..input(1_800_001, 0, 0, fresh)
        };
        assert_eq!(
            job_idle(&dead, &cfg()).1,
            JobIdleAction::Reap { kill: false }
        );
    }

    fn watch_input(now: i64, progress: i64) -> JobWatchInput {
        JobWatchInput {
            now_ms: now,
            started_ms: 0,
            last_progress_ms: progress,
            paused: false,
            natural_end_ms: None,
        }
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

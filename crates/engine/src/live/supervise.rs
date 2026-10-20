//! Watching a started upstream, failing it over and tearing it down.

use super::{
    Pipe, STALL_CHECK_EVERY, STALL_TIMEOUT, TICK, attempt, highest_segment, recover, stop_transport,
};
use crate::cache::remove_dir;
use crate::lock::Locked;
use crate::runtime::{Runtime, now_ms};
use fliks_engine_core::api::EngineEvent;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::time::Instant;

/// The steady-state judge: new segments, exits, stalls, viewer expiry and idle teardown.
/// [ts:svc:281-302,1049-1076,1140-1145]
pub(super) async fn supervise(rt: Arc<Runtime>, pipe: Arc<Pipe>) {
    let mut stall_check = Instant::now();
    loop {
        tokio::time::sleep(TICK).await;
        if pipe.m.locked().ended {
            return;
        }
        let top = match &pipe.dir {
            Some(d) => highest_segment(d).await,
            None => None,
        };
        let config = rt.current_config();
        let ttl = config.lifetime.resolved().live_session_ttl_ms as i64;
        let idle = Duration::from_secs(config.livetv.channel_idle_seconds.max(0) as u64);
        let now = Instant::now();
        let (expired, idle_over, failure) = {
            let mut m = pipe.m.locked();
            if top.is_some() && top > m.last_seg {
                m.last_seg = top;
                m.last_segment_at = now;
            }
            let cutoff = now_ms() - ttl;
            let flowing: Vec<String> = m
                .tees
                .iter()
                .filter(|(_, tx)| !tx.is_closed())
                .map(|(s, _)| s.clone())
                .collect();
            let expired: Vec<String> = m
                .viewers
                .iter()
                .filter(|(sid, v)| v.last_seen_ms <= cutoff && !flowing.contains(sid))
                .map(|(sid, _)| sid.clone())
                .collect();
            for sid in &expired {
                m.viewers.remove(sid);
            }
            let idle_over = if m.viewers.is_empty() {
                now.duration_since(*m.idle_since.get_or_insert(now)) >= idle
            } else {
                m.idle_since = None;
                false
            };
            let failure = if let Some(e) = m.proc.as_ref().and_then(|p| p.exited()) {
                let tail = m.proc.as_ref().unwrap().stderr_tail().join("\n");
                let start = tail.char_indices().rev().nth(299).map_or(0, |(i, _)| i);
                let code = e.code.map_or("null".into(), |c| c.to_string());
                Some(format!("exited (code={code}): {}", &tail[start..]))
            } else if m
                .direct
                .as_ref()
                .is_some_and(|d| d.down.load(Ordering::SeqCst))
            {
                Some("upstream closed".to_string())
            } else if now.duration_since(stall_check) >= STALL_CHECK_EVERY {
                stall_check = now;
                (now.duration_since(m.last_segment_at) > STALL_TIMEOUT)
                    .then(|| format!("no new segment for {} s", STALL_TIMEOUT.as_secs()))
            } else {
                None
            };
            (expired, idle_over, failure)
        };
        if !expired.is_empty() {
            let mut reg = rt.live.reg.locked();
            for sid in &expired {
                reg.viewers.remove(sid);
            }
        }
        for sid in expired {
            rt.events.emit(EngineEvent::SessionEnded {
                session_id: sid,
                reason: "expired".into(),
            });
        }
        if idle_over {
            teardown(&rt, &pipe, "idle", true).await;
            return;
        }
        if let Some(error) = failure
            && !handle_failure(&rt, &pipe, error).await
        {
            return;
        }
    }
}

/// The running upstream failed: count it and recover in place. [ts:svc:1087-1102]
async fn handle_failure(rt: &Arc<Runtime>, pipe: &Arc<Pipe>, error: String) -> bool {
    tracing::warn!(key = pipe.key, "live upstream failed: {error}");
    stop_transport(pipe).await;
    let pos = pipe.m.locked().current;
    attempt(rt, pipe, pipe.order[pos], Some(error));
    {
        let mut m = pipe.m.locked();
        m.failover.failed(pos);
        m.last_seg = None;
    }
    if recover(rt, pipe, true).await {
        return true;
    }
    teardown(rt, pipe, "failed", true).await;
    false
}

pub(super) fn unregister(rt: &Runtime, pipe: &Pipe) {
    let mut reg = rt.live.reg.locked();
    if reg
        .pipes
        .get(&pipe.key)
        .is_some_and(|p| std::ptr::eq(&**p, pipe))
    {
        reg.pipes.remove(&pipe.key);
    }
}

/// Ends the pipe: transport, viewers, directory, then `LiveReleased` when `release`.
/// [ts:svc:1147-1164]
pub(super) async fn teardown(rt: &Runtime, pipe: &Pipe, reason: &str, release: bool) {
    let viewers: Vec<String> = {
        let mut m = pipe.m.locked();
        if m.ended {
            return;
        }
        m.ended = true;
        m.tees.clear();
        m.viewers.drain().map(|(sid, _)| sid).collect()
    };
    unregister(rt, pipe);
    {
        let mut reg = rt.live.reg.locked();
        for sid in &viewers {
            reg.viewers.remove(sid);
        }
    }
    stop_transport(pipe).await;
    if let Some(d) = pipe.dir.clone() {
        let _ = tokio::task::spawn_blocking(move || remove_dir(&d)).await;
    }
    for sid in viewers {
        rt.events.emit(EngineEvent::SessionEnded {
            session_id: sid,
            reason: reason.into(),
        });
    }
    tracing::info!(key = pipe.key, reason, "live session ended");
    if release {
        rt.events.emit(EngineEvent::LiveReleased {
            capacity_token: pipe.capacity_token.clone(),
            reason: reason.into(),
        });
    }
}

/// Every pipe, at engine shutdown.
pub async fn shutdown(rt: &Arc<Runtime>) {
    let pipes: Vec<Arc<Pipe>> = rt.live.reg.locked().pipes.values().cloned().collect();
    for p in pipes {
        teardown(rt, &p, "shutdown", true).await;
    }
}

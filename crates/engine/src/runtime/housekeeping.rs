//! Session expiry, throttling and cache GC.

use super::{Job, JobKind, Runtime, kill_all, now_ms, take_orphans};
use crate::cache::{Cache, remove_dir};
use crate::lock::Locked;
use fliks_engine_core::api::EngineEvent;
use fliks_engine_core::session::{
    self as core_session, CacheEntryInfo, CachePolicy, RunProgress, SessionVariant,
    ThrottleDecision, ThrottleInput, TranscodeRun, Viewer, live_session_expired, plan_gc,
    tombstone_revivable,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

impl Runtime {
    /// Session expiry, then the jobs no session owns are stopped, every `liveSessionGcIntervalMs`.
    pub async fn gc_tick(&self) {
        let now = now_ms();
        let lifetime = &self.current_config().lifetime;
        let (expired, orphans) = {
            let mut reg = self.reg.locked();
            let expired: Vec<String> = reg
                .sessions
                .values()
                .filter(|s| {
                    live_session_expired(s.m.locked().last_beat_ms, s.pinned(), now, lifetime)
                })
                .map(|s| s.sid.clone())
                .collect();
            for sid in &expired {
                if let Some(s) = reg.sessions.remove(sid) {
                    reg.tombstones.insert(sid.clone(), s);
                }
            }
            reg.tombstones
                .retain(|_, s| tombstone_revivable(s.m.locked().last_beat_ms, now, lifetime));
            (expired, take_orphans(&mut reg))
        };
        for sid in expired {
            self.events.emit(EngineEvent::SessionEnded {
                session_id: sid,
                reason: "expired".into(),
            });
        }
        kill_all(orphans).await;
    }

    /// One throttle pass over the running jobs. [ts:modules/streaming/transcoding/ffmpeg-throttle.service.ts:94-150]
    pub async fn throttle_tick(&self) {
        let cap = self.pause_capability();
        let streaming = &self.current_config().streaming;
        let work: Vec<(Arc<Job>, Vec<Viewer>)> = {
            let reg = self.reg.locked();
            reg.jobs
                .values()
                .filter(|j| j.proc.exited().is_none())
                .map(|j| {
                    let viewers = reg
                        .sessions
                        .values()
                        .filter(|s| j.serves(s))
                        .map(|s| s.viewer())
                        .collect();
                    (j.clone(), viewers)
                })
                .collect()
        };
        for (job, viewers) in work {
            let variant = match job.kind {
                JobKind::Main => SessionVariant::Main,
                JobKind::Early => SessionVariant::Early,
                JobKind::Remux => SessionVariant::Remux {
                    audio_index: None,
                    keyframe_grid: true,
                },
            };
            let remux = job.remux.as_ref().map(|r| {
                let a = &r.view;
                let (start_seconds, frontier_seconds) = a.span();
                let last = viewers
                    .iter()
                    .filter_map(|v| v.last_requested_segment)
                    .max()
                    .unwrap_or(0);
                RunProgress::Remux {
                    start_seconds,
                    frontier_seconds,
                    boundaries: (0..=last as usize)
                        .map(|i| a.segment_content_seconds(i))
                        .collect(),
                }
            });
            let input = {
                let m = job.m.locked();
                ThrottleInput {
                    capability: cap,
                    enabled: streaming.throttle_enabled,
                    threshold_seconds: streaming.throttle_threshold_seconds,
                    paused: m.paused,
                    process_exited: job.proc.exited().is_some(),
                    variant: Some(variant),
                    viewers,
                    run: remux.unwrap_or_else(|| {
                        RunProgress::Transcode(TranscodeRun {
                            start_segment: job.start_segment,
                            frontier_hint: m.frontier_hint,
                            present: m.sets[0].clone(),
                            segment_duration: job.segment_duration,
                            fps: job.fps,
                        })
                    }),
                }
            };
            let out = core_session::throttle_step(&input);
            if out.frontier_segment.is_some() {
                job.m.locked().frontier_hint = out.frontier_segment;
            }
            match out.decision {
                ThrottleDecision::Pause => {
                    job.pause(cap, true).await;
                }
                ThrottleDecision::Resume => {
                    job.pause(cap, false).await;
                }
                ThrottleDecision::Noop => {}
            }
        }
    }

    pub(super) async fn scan_cache(&self) -> Vec<crate::cache::Scanned> {
        let root = self.cache.root.clone();
        tokio::task::spawn_blocking(move || Cache { root }.scan())
            .await
            .unwrap_or_default()
    }

    /// Entry dirs a job or session uses: never evicted.
    fn live_dirs(&self) -> Vec<PathBuf> {
        let reg = self.reg.locked();
        let sessions = reg.sessions.values().flat_map(|s| {
            [JobKind::Main, JobKind::Early, JobKind::Remux].map(|k| {
                self.cache
                    .entry_dir(s.owner.as_deref(), s.media_file_id, &s.cache_hash(k))
            })
        });
        let jobs = reg
            .jobs
            .values()
            .filter_map(|j| j.dir.parent().map(Path::to_path_buf));
        sessions.chain(jobs).collect()
    }

    pub async fn cache_summary(&self) -> fliks_engine_core::api::CacheSummary {
        let live = self.live_dirs();
        let entries: Vec<_> = self
            .scan_cache()
            .await
            .iter()
            .map(|e| e.entry(live.contains(&e.dir)))
            .collect();
        fliks_engine_core::api::CacheSummary {
            used_bytes: entries.iter().map(|e| e.bytes).sum(),
            entries,
        }
    }

    /// Removes the non-live entries matching the filters.
    pub async fn cache_delete(&self, file: Option<u32>, owner: Option<&str>) -> u32 {
        let live = self.live_dirs();
        let mut n = 0;
        for e in self.scan_cache().await {
            if live.contains(&e.dir)
                || file.is_some_and(|f| f != e.media_file_id)
                || owner.is_some_and(|o| e.owner.as_deref() != Some(o))
            {
                continue;
            }
            let _ = tokio::task::spawn_blocking(move || remove_dir(&e.dir)).await;
            n += 1;
        }
        n
    }

    /// TTL then LRU eviction over the rebuilt index. [ts:modules/streaming/transcoding/transcode-cache.service.ts:268-306]
    pub async fn cache_gc(&self) -> u32 {
        let live = self.live_dirs();
        let policy = CachePolicy::from_config(&self.current_config().streaming);
        let scanned = self.scan_cache().await;
        let index: Vec<CacheEntryInfo> = scanned
            .iter()
            .map(|e| CacheEntryInfo {
                key: e.dir.to_string_lossy().into_owned(),
                total_bytes: e.bytes,
                last_access_ms: e.last_access_ms,
                live: live.contains(&e.dir),
            })
            .collect();
        let gone = plan_gc(&index, now_ms(), &policy);
        for k in &gone {
            let dir = PathBuf::from(k);
            tracing::info!(dir = %dir.display(), "cache eviction");
            let _ = tokio::task::spawn_blocking(move || remove_dir(&dir)).await;
        }
        gone.len() as u32
    }
}

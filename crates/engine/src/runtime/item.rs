//! Serving an item: one run per part, bounded in length, started where a request lands.
//! Spec: `docs/spec/item.md`.

use super::fetch::{finish, ready};
use super::{Fetch, Job, Runtime, SEGMENT_WAIT, Session, layout_path};
use crate::cache::remove_dir;
use crate::lock::Locked;
use fliks_engine_core::item::RUN_SEGMENTS;
use fliks_engine_core::session::{self as core_session, ResolveInput, ResumeAction, SegmentName};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::Instant;

impl Runtime {
    fn part_file(&self, s: &Session, part: usize, name: SegmentName) -> PathBuf {
        let dir = self.cache.rung_dir(
            s.owner.as_deref(),
            s.media_file_id,
            &s.hash,
            &format!("p{part}"),
        );
        let ext = if s.plan.use_ts { "ts" } else { "m4s" };
        layout_path(&dir, false, 0, ext, name)
    }

    async fn present(path: &PathBuf) -> bool {
        tokio::fs::metadata(path).await.is_ok_and(|m| m.len() > 0)
    }

    /// The init or a segment of part `part` (`name` indexes the part's own segments). An init is
    /// any part's: every part of a session writes the same format.
    pub async fn item_file(
        self: &Arc<Self>,
        s: &Arc<Session>,
        part: usize,
        name: SegmentName,
    ) -> Fetch {
        let Some(item) = s.plan.item.as_ref() else {
            return Fetch::NotFound;
        };
        let key = s.item_key(part);
        let existing = self.job(&key);
        let path = self.part_file(s, part, name);
        // A live run's file past its budget is the cut tail, not a segment.
        let tail = |n| {
            existing
                .as_ref()
                .is_some_and(|e| !e.m.locked().settled && n >= e.start_segment + RUN_SEGMENTS)
        };
        let cut = matches!(name, SegmentName::Segment(n) if tail(n));
        if !cut && Self::present(&path).await {
            return ready(path, existing, 0.0, true);
        }
        let floor = |p: usize| {
            item.timeline
                .locate(s.position_s())
                .filter(|l| l.part == p)
                .map_or(0, |l| l.local)
        };
        if matches!(name, SegmentName::Init(_)) {
            for p in 0..item.parts.len() {
                let other = self.part_file(s, p, name);
                if Self::present(&other).await {
                    return ready(other, self.job(&s.item_key(p)), 0.0, true);
                }
            }
        }
        let start = match name {
            SegmentName::Init(_) => floor(part),
            SegmentName::Segment(n) => n,
        };
        let t0 = Instant::now();
        let Some(job) = self.ensure_item_job(s, part, start, false).await else {
            return Fetch::Failed;
        };
        let (job, landed) = self.wait_file(job, 0, name, SEGMENT_WAIT).await;
        finish(path, job, landed, t0).await
    }

    /// The part's run for a request at local segment `requested`: kept while it will reach the
    /// segment, else replaced by one starting there. A `warm` call only starts a run when the
    /// part has none, so it never moves the run a request already placed.
    async fn ensure_item_job(
        self: &Arc<Self>,
        s: &Arc<Session>,
        part: usize,
        mut requested: u32,
        warm: bool,
    ) -> Option<Arc<Job>> {
        let key = s.item_key(part);
        let lock = self.lock_for(&key);
        let _g = lock.lock().await;
        let mut restarts = 0;
        if let Some(e) = self.job(&key) {
            if warm {
                return Some(e);
            }
            restarts = e.restarts + 1;
            // The watcher's last listing of an exited run says which segments it wrote.
            if e.proc.exited().is_some() {
                e.settled().await;
            }
            let present = e.m.locked().sets[0].clone();
            // A run ends after `RUN_SEGMENTS`, so "nearby" says nothing past its end.
            let unreachable = requested >= e.start_segment + RUN_SEGMENTS
                || (e.proc.exited().is_some() && !present.contains(requested));
            if !unreachable {
                let input = ResolveInput {
                    run: e.existing_run(None),
                    requested_segment: requested,
                    quality_match: true,
                    present,
                };
                match core_session::resolve_existing(&input) {
                    ResumeAction::Serve | ResumeAction::Wait => {
                        self.resume_for_waiter(&e).await;
                        return Some(e);
                    }
                    ResumeAction::Respawn(r) => {
                        if r.kill {
                            e.kill_now().await;
                        }
                        if r.purge_cache {
                            let dir = e.dir.clone();
                            let _ = tokio::task::spawn_blocking(move || remove_dir(&dir)).await;
                        }
                        requested = r.start_segment.unwrap_or(e.start_segment);
                    }
                }
            }
        }
        self.spawn_item_job(s, part, requested, restarts).await
    }

    /// Starts the run of the segment a position falls in, unless one covers it.
    pub fn prewarm_item(self: &Arc<Self>, s: &Arc<Session>, at: f64) {
        let Some(l) = s.plan.item.as_ref().and_then(|i| i.timeline.locate(at)) else {
            return;
        };
        let (rt, s) = (self.clone(), s.clone());
        tokio::spawn(async move {
            rt.ensure_item_job(&s, l.part, l.local, true).await;
        });
    }
}

//! Serving a segment, init or audio file: from the cache, an early run or a fresh run.

use super::{Fetch, Job, JobKind, Runtime, SEGMENT_WAIT, Session, layout_path};
use crate::cache::remove_dir;
use crate::lock::Locked;
use fliks_engine_core::mp4::grid_segment_index;
use fliks_engine_core::session::{
    self as core_session, EARLY_PROBE_SEGMENTS, EARLY_WAIT_MS, ResolveInput, ResumeAction,
    SegmentName, SegmentSet, seconds_to_segment,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

impl Runtime {
    /// Waits up to `timeout` for a file of `job`, following the job that replaced it on the same
    /// rung (a CPU fallback).
    async fn wait_file(
        &self,
        mut job: Arc<Job>,
        track: usize,
        name: SegmentName,
        timeout: Duration,
    ) -> (Arc<Job>, bool) {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let ok = match name {
                SegmentName::Init(_) => job.wait_init(track, left).await,
                SegmentName::Segment(n) => job.wait_segment(track, n, left).await,
            };
            match self.job(&job.key) {
                Some(next)
                    if !ok
                        && !Arc::ptr_eq(&next, &job)
                        && next.quality == job.quality
                        && !left.is_zero() =>
                {
                    job = next
                }
                _ => return (job, ok),
            }
        }
    }

    /// The job that serves `quality` for a request at `requested`, spawning or restarting one as
    /// the seek/switch matrix says. [ts:modules/streaming/transcoding/transcoding.service.ts:743-812]
    async fn ensure_job(
        self: &Arc<Self>,
        s: &Arc<Session>,
        quality: &str,
        mut requested: u32,
    ) -> Option<Arc<Job>> {
        let key = s.job_key();
        let lock = self.lock_for(&key);
        let _g = lock.lock().await;
        let mut restarts = 0;
        let detected = self.detected_hw();
        let mut hw = (detected, detected);
        if let Some(e) = self.job(&key) {
            restarts = e.restarts + 1;
            // A run that fell back stays on the CPU.
            hw = (e.hw, e.requested_hw);
            let quality_match = e.quality == quality;
            // Another rung: start where this request is (an init already asks for the resume floor).
            if !quality_match {
                if e.producing() {
                    e.kill_now().await;
                }
            } else {
                let input = ResolveInput {
                    run: e.existing_run(None),
                    requested_segment: requested,
                    quality_match,
                    present: e.m.locked().sets[0].clone(),
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
        self.spawn_job(s, quality, requested, hw, restarts, JobKind::Main)
            .await
    }

    /// A request waits on a throttled run: wake it now, the next tick re-pauses it if still ahead.
    async fn resume_for_waiter(&self, e: &Job) {
        if e.m.locked().paused {
            e.pause(self.pause_capability(), false).await;
        }
    }

    /// The session's remux run for a request at `requested` (`audio`: the 1-based rendition),
    /// reused while the assembler can reach it. [ts:modules/streaming/transcoding/transcoding.service.ts:1447-1470]
    async fn ensure_remux(
        self: &Arc<Self>,
        s: &Arc<Session>,
        mut requested: u32,
        audio: Option<usize>,
    ) -> Option<Arc<Job>> {
        let key = s.key(JobKind::Remux);
        let lock = self.lock_for(&key);
        let _g = lock.lock().await;
        if let Some(e) = self.job(&key) {
            if e.proc.exited().is_some() {
                e.settled().await;
            }
            let input = ResolveInput {
                run: e.existing_run(audio),
                requested_segment: requested,
                quality_match: true,
                present: SegmentSet::default(),
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
                    requested = r.start_segment.unwrap_or(e.start_segment);
                }
            }
        }
        self.spawn_remux(s, requested).await
    }

    /// The early companion of `quality`: reused while it runs or once it ended cleanly, else
    /// killed, cleaned and spawned again. [ts:modules/streaming/transcoding/transcoding.service.ts:965-1060]
    pub(super) async fn early_job(
        self: &Arc<Self>,
        s: &Arc<Session>,
        quality: &str,
    ) -> Option<Arc<Job>> {
        let key = s.key(JobKind::Early);
        let lock = self.lock_for(&key);
        let _g = lock.lock().await;
        if let Some(e) = self.job(&key) {
            let clean = e.proc.exited().is_some_and(|x| x.code == Some(0));
            if e.quality == quality && (e.producing() || clean) {
                return Some(e);
            }
            self.unregister(&e);
            e.kill_now().await;
            let dir = e.dir.clone();
            let _ = tokio::task::spawn_blocking(move || remove_dir(&dir)).await;
        }
        let hw = self.detected_hw();
        self.spawn_job(s, quality, 0, (hw, hw), 0, JobKind::Early)
            .await
    }

    /// A file of the early companion within its short wait, `None` to fall through to the main.
    async fn early_file(&self, early: Arc<Job>, track: usize, name: SegmentName) -> Option<Fetch> {
        let t0 = Instant::now();
        let wait = Duration::from_millis(EARLY_WAIT_MS.into());
        let (job, landed) = self.wait_file(early, track, name, wait).await;
        if !landed {
            tracing::warn!(
                key = job.key,
                ?name,
                "early companion missed its wait, main instead"
            );
            return None;
        }
        let path = job.file_path(track, name);
        Some(finish(path, job, true, t0).await)
    }

    /// A video file of `quality` (`docs/spec/runtime.md` section 4).
    pub async fn video_file(
        self: &Arc<Self>,
        s: &Arc<Session>,
        quality: &str,
        name: SegmentName,
    ) -> Fetch {
        let key = s.job_key();
        let existing = self.job(&key);
        let same = existing.as_ref().filter(|j| j.quality == quality).cloned();
        let early = self
            .job(&s.key(JobKind::Early))
            .filter(|e| e.quality == quality);
        // The early companion's init is decode-compatible and lands first.
        // [ts:modules/streaming/streaming.controller.ts:2289-2345]
        if let (SegmentName::Init(_), Some(_), Some(e)) = (name, &same, &early)
            && let Some(f) = self.early_file(e.clone(), 0, name).await
        {
            return f;
        }
        if let (Some(j), SegmentName::Segment(n)) = (&same, name)
            && j.has(0, n)
        {
            return ready(j.file_path(0, name), Some(j.clone()), 0.0, true);
        }
        if same.is_none() {
            // An earlier run of this rung left the file: serve it without spawning. [new]
            let dir = self
                .cache
                .rung_dir(s.owner.as_deref(), s.media_file_id, &s.hash, quality);
            let ext = if s.plan.use_ts { "ts" } else { "m4s" };
            let path = layout_path(&dir, s.plan.multi_audio, 0, ext, name);
            if tokio::fs::metadata(&path).await.is_ok_and(|m| m.len() > 0) {
                return ready(path, existing, 0.0, true);
            }
        }
        // A resume's seg-0/seg-1 probe and an init go to the bounded early companion, never
        // relocating the main. [ts:modules/streaming/streaming.controller.ts:2409-2480]
        let floor = s.resume_floor(existing.as_deref());
        let probe = s.plan.probes_seg_zero
            && floor > EARLY_PROBE_SEGMENTS
            && match name {
                SegmentName::Init(_) => same.is_none(),
                SegmentName::Segment(n) => n < EARLY_PROBE_SEGMENTS,
            };
        if probe {
            let early = match early {
                Some(e) => Some(e),
                None => self.early_job(s, quality).await,
            };
            if let Some(e) = early.filter(|e| e.quality == quality)
                && let Some(f) = self.early_file(e, 0, name).await
            {
                return f;
            }
        }
        let t0 = Instant::now();
        let job = match (name, same) {
            (SegmentName::Init(_), Some(j)) => j,
            (SegmentName::Init(_), None) => match self.ensure_job(s, quality, floor).await {
                Some(j) => j,
                None => return Fetch::Failed,
            },
            (SegmentName::Segment(n), _) => match self.ensure_job(s, quality, n).await {
                Some(j) => j,
                None => return Fetch::Failed,
            },
        };
        let (job, landed) = self.wait_file(job, 0, name, SEGMENT_WAIT).await;
        let path = job.file_path(0, name);
        finish(path, job, landed, t0).await
    }

    /// Audio rendition `track` (1-based var_stream_map index) of the session's job, whatever rung
    /// it runs; spawns the top rung when none runs. [ts:modules/streaming/streaming.controller.ts:1909-2120]
    pub async fn audio_file(
        self: &Arc<Self>,
        s: &Arc<Session>,
        track: usize,
        name: SegmentName,
    ) -> Fetch {
        let key = s.job_key();
        let existing = self.job(&key);
        let job = match existing
            .filter(|j| j.producing() || matches!(name, SegmentName::Segment(n) if j.has(track, n)))
        {
            Some(j) => j,
            None => {
                let v = s.req.decide.info.video.first();
                let (w, h) = core_session::rung_source_size(v);
                let quality = core_session::top_rung(w, h, s.decision.use_hdr_ladder);
                let start = match name {
                    SegmentName::Init(_) => s.resume_floor(None),
                    SegmentName::Segment(n) => n,
                };
                match self.ensure_job(s, quality, start).await {
                    Some(j) => j,
                    None => return Fetch::Failed,
                }
            }
        };
        if track >= job.tracks {
            return Fetch::NotFound;
        }
        // A resume's rendition init and first segments come from the early companion.
        // [ts:modules/streaming/streaming.controller.ts:2008-2060]
        let want_early = job.start_segment > 0
            && match name {
                SegmentName::Init(_) => true,
                SegmentName::Segment(n) => n < EARLY_PROBE_SEGMENTS,
            };
        if want_early {
            let early = match self
                .job(&s.key(JobKind::Early))
                .filter(|e| e.quality == job.quality)
            {
                Some(e) => Some(e),
                None => self.early_job(s, &job.quality).await,
            };
            if let Some(e) = early.filter(|e| e.quality == job.quality)
                && let Some(f) = self.early_file(e, track, name).await
            {
                return f;
            }
        }
        let t0 = Instant::now();
        let (job, landed) = self.wait_file(job, track, name, SEGMENT_WAIT).await;
        let path = job.file_path(track, name);
        finish(path, job, landed, t0).await
    }

    /// A file of the remux run: `track` 0 is the video, `i > 0` audio rendition `i`. A file
    /// any run of the session assembled is served as is (same samples, same timeline).
    /// [ts:modules/streaming/streaming.controller.ts:1940-1975,2345-2380,2482-2500]
    pub async fn remux_file(
        self: &Arc<Self>,
        s: &Arc<Session>,
        track: usize,
        name: SegmentName,
    ) -> Fetch {
        let dir = self.cache.rung_dir(
            s.owner.as_deref(),
            s.media_file_id,
            &s.cache_hash(JobKind::Remux),
            "remux",
        );
        let path = layout_path(&dir, track > 0, track, "m4s", name);
        let existing = self.job(&s.key(JobKind::Remux));
        if tokio::fs::metadata(&path).await.is_ok_and(|m| m.len() > 0) {
            return ready(path, existing, 0.0, true);
        }
        let start = match name {
            SegmentName::Init(_) => s.remux_floor(existing.as_deref()),
            SegmentName::Segment(n) => n,
        };
        let audio = (track > 0).then_some(track);
        let t0 = Instant::now();
        let Some(job) = self.ensure_remux(s, start, audio).await else {
            return Fetch::Failed;
        };
        if track >= job.tracks {
            return Fetch::NotFound;
        }
        let (job, landed) = self.wait_file(job, track, name, SEGMENT_WAIT).await;
        finish(path, job, landed, t0).await
    }

    /// Starts the remux run at the segment a `startAt` falls in, unless one runs.
    /// [ts:modules/streaming/streaming.controller.ts:2155-2180]
    pub fn prewarm_remux(self: &Arc<Self>, s: &Arc<Session>, start_at: f64) {
        if self
            .job(&s.key(JobKind::Remux))
            .is_some_and(|j| j.producing())
        {
            return;
        }
        let start = match &s.plan.remux_grid {
            Some(g) => grid_segment_index(&g.boundaries, start_at, s.plan.timeline.origin) as u32,
            None => seconds_to_segment(start_at, s.plan.segment_duration, None),
        };
        let (rt, s) = (self.clone(), s.clone());
        tokio::spawn(async move {
            rt.ensure_remux(&s, start, None).await;
        });
    }
}

fn ready(path: PathBuf, job: Option<Arc<Job>>, waited_ms: f64, cache_hit: bool) -> Fetch {
    Fetch::Ready {
        path,
        job,
        waited_ms,
        cache_hit,
    }
}

/// The 503/404 rule after the wait. [ts:modules/streaming/streaming.controller.ts:2506-2545]
pub(super) async fn finish(path: PathBuf, job: Arc<Job>, landed: bool, t0: Instant) -> Fetch {
    let size = tokio::fs::metadata(&path).await.ok().map(|m| m.len());
    let probe = core_session::SegmentProbe {
        landed,
        size,
        producing: job.producing(),
        intentionally_killed: job.killed(),
    };
    match core_session::segment_response(&probe) {
        core_session::SegmentResponse::Serve => {
            ready(path, Some(job), t0.elapsed().as_secs_f64() * 1000.0, false)
        }
        core_session::SegmentResponse::Unavailable { .. } => Fetch::Retry,
        core_session::SegmentResponse::NotFound => Fetch::NotFound,
    }
}

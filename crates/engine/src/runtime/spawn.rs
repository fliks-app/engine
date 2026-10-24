//! Starting, watching and replacing the runs behind a session.

use super::{Job, JobKind, JobMut, Runtime, Session, WATCH_EVERY, kill_all, now_ms};
use crate::cache::remove_dir;
use crate::lock::Locked;
use crate::proc::Ffmpeg;
use crate::remux::Remux;
use fliks_engine_core::api::{EngineEvent, HwAccel, JobState};
use fliks_engine_core::mp4::{remux_assembly_plan, remux_audio_grid, remux_run_start};
use fliks_engine_core::session::{
    self as core_session, JobWatch, JobWatchInput, SegmentName, SegmentSet, early_run_seconds,
    job_watch, parse_segment_name,
};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

/// What a new job runs, before its process exists.
struct Run {
    pub(super) key: String,
    pub(super) kind: JobKind,
    pub(super) quality: String,
    pub(super) dir: PathBuf,
    pub(super) var_stream_map: bool,
    pub(super) tracks: usize,
    pub(super) ext: &'static str,
    pub(super) start: u32,
    /// (actual, requested)
    pub(super) hw: (HwAccel, HwAccel),
    pub(super) restarts: u32,
    pub(super) remux: Option<Remux>,
}

impl Runtime {
    /// Kills the key's previous run, purges the rung from `start` and spawns a main or early run.
    /// [ts:modules/streaming/transcoding/transcoding.service.ts:790-860]
    pub(super) async fn spawn_job(
        self: &Arc<Self>,
        s: &Arc<Session>,
        quality: &str,
        start: u32,
        (hw, requested_hw): (HwAccel, HwAccel),
        restarts: u32,
        kind: JobKind,
    ) -> Option<Arc<Job>> {
        let key = s.key(kind);
        if let Some(old) = self.job(&key)
            && old.producing()
        {
            old.kill().await;
        }
        let dir = self.cache.rung_dir(
            s.owner.as_deref(),
            s.media_file_id,
            &s.cache_hash(kind),
            quality,
        );
        let var_stream_map = s.plan.multi_audio;
        let tracks = if var_stream_map {
            s.req.decide.info.audio.len() + 1
        } else {
            1
        };
        let ext = if s.plan.use_ts { "ts" } else { "m4s" };
        {
            let (dir, ext) = (dir.clone(), ext);
            let _ = tokio::task::spawn_blocking(move || {
                purge_from(&dir, var_stream_map, tracks, start, ext)
            })
            .await;
        }
        let caps = self.caps().unwrap_or_default();
        let dev = self.device(&caps);
        let dir_s = dir.to_string_lossy().into_owned();
        let build = |hw| {
            let args = core_session::vod_args(&core_session::RunInput {
                req: &s.req,
                decision: &s.decision,
                streaming: &s.streaming,
                plan: &s.plan,
                input_path: &s.path,
                output_dir: &dir_s,
                quality,
                start_segment: start,
                hw,
                early: kind == JobKind::Early,
                encode_threads: crate::slots::cpu_quota() as u32,
            });
            fliks_engine_core::args::vod(&args, &caps, &dev)
        };
        let (argv, hw) = match build(hw) {
            Ok(a) => (a, hw),
            Err(e) if hw != HwAccel::None => {
                tracing::warn!(key, "argv on {hw:?}: {e}; CPU instead");
                (build(HwAccel::None).ok()?, HwAccel::None)
            }
            Err(e) => {
                tracing::error!(key, "argv: {e}");
                return None;
            }
        };
        let run = Run {
            key,
            kind,
            quality: quality.into(),
            dir,
            var_stream_map,
            tracks,
            ext,
            start,
            hw: (hw, requested_hw),
            restarts,
            remux: None,
        };
        self.launch(s, run, &argv).await
    }

    /// A copy-video run from `start`: one file per GOP into its own directory, assembled onto
    /// the session's remux directory. [ts:modules/streaming/transcoding/transcoding.service.ts:1447-1580]
    pub(super) async fn spawn_remux(
        self: &Arc<Self>,
        s: &Arc<Session>,
        start: u32,
    ) -> Option<Arc<Job>> {
        let key = s.key(JobKind::Remux);
        if let Some(old) = self.job(&key)
            && old.producing()
        {
            old.kill().await;
        }
        let caps = self.caps().unwrap_or_default();
        let plan = &s.plan;
        let grid = plan.remux_grid.as_ref();
        let audio_grid = remux_audio_grid(
            Some(core_session::remux_audio_basis(&s.decision)),
            caps.libfdk_aac,
        );
        let origin = plan.timeline.origin;
        let seg = plan.segment_duration;
        let run = match remux_run_start(grid, start as usize, seg, origin, audio_grid) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(key, "remux run: {e}");
                return None;
            }
        };
        let dir = self.cache.rung_dir(
            s.owner.as_deref(),
            s.media_file_id,
            &s.cache_hash(JobKind::Remux),
            "remux",
        );
        // Per run: a run still being reaped must not delete this one's GOPs.
        let gop_dir = dir.join(format!("gop-{}", crate::random_hex(4)));
        if let Err(e) = tokio::fs::create_dir_all(&gop_dir).await {
            tracing::error!(key, "mkdir {}: {e}", gop_dir.display());
            return None;
        }
        let gop_s = gop_dir.to_string_lossy().into_owned();
        let args = core_session::remux_args(
            &core_session::RunInput {
                req: &s.req,
                decision: &s.decision,
                streaming: &s.streaming,
                plan,
                input_path: &s.path,
                output_dir: &gop_s,
                quality: "remux",
                start_segment: start,
                hw: HwAccel::None,
                early: false,
                encode_threads: crate::slots::cpu_quota() as u32,
            },
            &run,
        );
        let argv = match fliks_engine_core::args::remux(&args, &caps) {
            Ok(a) => a,
            Err(e) => {
                tracing::error!(key, "remux argv: {e}");
                return None;
            }
        };
        let renditions = if plan.multi_audio {
            s.req.decide.info.audio.len()
        } else {
            0
        };
        let asm_plan = remux_assembly_plan(
            grid,
            start as usize,
            &run,
            origin,
            Some(seg),
            Some(renditions as u32),
        );
        let run = Run {
            key,
            kind: JobKind::Remux,
            quality: "remux".into(),
            dir: dir.clone(),
            var_stream_map: false,
            tracks: renditions + 1,
            ext: "m4s",
            start,
            hw: (HwAccel::None, HwAccel::None),
            restarts: 0,
            remux: Some(Remux::new(asm_plan, dir, gop_dir)),
        };
        self.launch(s, run, &argv).await
    }

    /// Spawns ffmpeg, registers the job and starts its watcher.
    async fn launch(
        self: &Arc<Self>,
        s: &Arc<Session>,
        r: Run,
        argv: &[String],
    ) -> Option<Arc<Job>> {
        let key = r.key;
        tracing::info!(key, quality = r.quality, start = r.start, hw = ?r.hw.0, "ffmpeg start");
        tracing::debug!(key, "ffmpeg {}", argv.join(" "));
        let slot = self.slots.playback(r.hw.0 == HwAccel::None);
        let proc = match Ffmpeg::spawn(&self.ffmpeg, argv).await {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(key, "spawn {}: {e}", self.ffmpeg);
                return None;
            }
        };
        let job = Arc::new(Job {
            key: key.clone(),
            kind: r.kind,
            hash: s.hash.clone(),
            media_file_id: s.media_file_id,
            owner: s.owner.clone(),
            quality: r.quality.clone(),
            dir: r.dir,
            var_stream_map: r.var_stream_map,
            tracks: r.tracks,
            ext: r.ext,
            start_segment: r.start,
            requested_hw: r.hw.1,
            hw: r.hw.0,
            segment_duration: s.plan.segment_duration,
            fps: s.plan.source_fps,
            duration: s.plan.duration,
            restarts: r.restarts,
            natural_end_ms: (r.kind == JobKind::Early)
                .then(|| (early_run_seconds(s.plan.segment_duration) * 1000.0) as i64),
            proc,
            m: Mutex::new(JobMut {
                sets: vec![SegmentSet::default(); r.tracks],
                inits: vec![false; r.tracks],
                settled: false,
                paused: false,
                frontier_hint: None,
                killed: false,
                pending: r.remux.is_some(),
                started_ms: now_ms(),
                last_progress_ms: now_ms(),
                seen_lines: 0,
                tmp_bytes: 0,
                cpu_ms: 0,
                reason: None,
            }),
            remux: r.remux,
            slot: Mutex::new(Some(slot)),
            changed: watch::channel(0).0,
        });
        job.scan().await;
        let replaced = self.reg.locked().jobs.insert(key.clone(), job.clone());
        if let Some(old) = replaced.filter(|o| o.producing()) {
            tokio::spawn(kill_all(vec![old]));
        }
        self.events.emit(EngineEvent::JobStarted {
            job_key: key,
            media_file_id: s.media_file_id,
            quality: r.quality,
            start_segment: r.start,
            hw_accel: r.hw.0,
        });
        tokio::spawn(self.clone().watch(job.clone(), s.clone()));
        Some(job)
    }

    /// Drops `job` from the registry unless another run replaced it.
    pub(super) fn unregister(&self, job: &Arc<Job>) {
        let mut reg = self.reg.locked();
        if reg.jobs.get(&job.key).is_some_and(|j| Arc::ptr_eq(j, job)) {
            reg.jobs.remove(&job.key);
        }
    }

    /// Kills a hung run or one past its natural end; it ends failed and the next request restarts it.
    async fn watchdog(&self, job: &Arc<Job>) {
        let (now, lines) = (now_ms(), job.proc.stderr_lines());
        let cpu = job.proc.cpu_time_ms();
        let verdict = {
            let mut m = job.m.locked();
            if m.seen_lines != lines {
                m.seen_lines = lines;
                m.last_progress_ms = now;
            }
            let cpu_delta_ms = cpu.map_or(0, |c| c.saturating_sub(m.cpu_ms));
            m.cpu_ms = cpu.unwrap_or(m.cpu_ms);
            if cpu_delta_ms > 0 {
                m.last_progress_ms = now;
            }
            job_watch(
                &JobWatchInput {
                    now_ms: now,
                    started_ms: m.started_ms,
                    last_progress_ms: m.last_progress_ms,
                    paused: m.paused,
                    cpu_delta_ms,
                    natural_end_ms: job.natural_end_ms,
                },
                &self.lifetime(),
            )
        };
        let reason = match verdict {
            JobWatch::Ok => return,
            JobWatch::Stalled => "stalled",
            JobWatch::Overran => "overran",
        };
        tracing::warn!(key = job.key, reason, "ffmpeg hung, killing it");
        job.m.locked().reason = Some(reason);
        job.proc.stop().await;
    }

    /// One assembler pass off the async threads; true when it gave up.
    async fn pump(job: &Job) -> bool {
        let Some(r) = job.remux.clone() else {
            return false;
        };
        tokio::task::spawn_blocking(move || {
            let mut a = r.asm.locked();
            a.pump();
            a.stopped()
        })
        .await
        .unwrap_or(true)
    }

    /// Follows the run's output until it exits, then falls back to the CPU when the hardware
    /// failed before the first segment. [ts:modules/streaming/transcoding/transcoding.service.ts:870-940]
    // A boxed `dyn Future`: the watcher respawns through `spawn_job`, which spawns a watcher.
    pub(super) fn watch(
        self: Arc<Self>,
        job: Arc<Job>,
        s: Arc<Session>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            loop {
                if Self::pump(&job).await && !job.killed() {
                    // Its output cannot be served: the next request starts over.
                    tracing::error!(key = job.key, "remux assembly gave up, stopping the run");
                    self.unregister(&job);
                    job.kill().await;
                }
                if job.scan().await {
                    job.changed.send_modify(|v| *v += 1);
                }
                if job.proc.exited().is_some() {
                    break;
                }
                self.watchdog(&job).await;
                if job.proc.exited().is_some() {
                    break;
                }
                tokio::select! {
                    _ = tokio::time::sleep(WATCH_EVERY) => {}
                    _ = job.proc.wait() => {}
                }
            }
            let exit = job.proc.exited().unwrap();
            job.slot.locked().take();
            if let Some(r) = job.remux.clone() {
                let clean = exit.code == Some(0) && !job.killed();
                let _ = tokio::task::spawn_blocking(move || r.asm.locked().finish(clean)).await;
            }
            job.scan().await;
            job.m.locked().pending = false;
            let state = job.state();
            if state == JobState::Failed {
                tracing::warn!(
                    key = job.key,
                    ?exit,
                    "ffmpeg failed:\n{}",
                    job.proc.stderr_tail().join("\n")
                );
            }
            self.events.emit(EngineEvent::JobEnded {
                job_key: job.key.clone(),
                state,
                exit_code: exit.code,
                reason: job.m.locked().reason.map(String::from),
            });
            // The replacement is registered before the waiters wake, so they move on to it.
            if state == JobState::Failed
                && !job.has(0, job.start_segment)
                && job.hw != HwAccel::None
            {
                self.clone().fall_back(&job, &s).await;
            }
            job.m.locked().settled = true;
            job.changed.send_modify(|v| *v += 1);
        })
    }

    async fn fall_back(self: Arc<Self>, job: &Arc<Job>, s: &Arc<Session>) {
        let lock = self.lock_for(&job.key);
        let _g = lock.lock().await;
        if !self.job(&job.key).is_some_and(|j| Arc::ptr_eq(&j, job)) {
            return;
        }
        tracing::warn!(key = job.key, from = ?job.hw, "hardware run failed, CPU fallback");
        *self.hw_failed.locked() = Some(self.detected_hw());
        self.events.emit(EngineEvent::HwFallback {
            job_key: job.key.clone(),
            from: job.hw,
            to: HwAccel::None,
            stderr_tail: job.proc.stderr_tail(),
        });
        let dir = job.dir.clone();
        let _ = tokio::task::spawn_blocking(move || remove_dir(&dir)).await;
        let hw = (HwAccel::None, job.requested_hw);
        self.spawn_job(
            s,
            &job.quality,
            job.start_segment,
            hw,
            job.restarts + 1,
            job.kind,
        )
        .await;
    }
}

/// Drops a previous run's segments at and after `start`: one timeline forward of the restart.
/// [ts:modules/streaming/transcoding/segment-utils.ts:127-170]
pub(super) fn purge_from(dir: &Path, var_stream_map: bool, tracks: usize, start: u32, ext: &str) {
    let dirs: Vec<PathBuf> = if var_stream_map {
        (0..tracks).map(|t| dir.join(t.to_string())).collect()
    } else {
        vec![dir.to_path_buf()]
    };
    for d in dirs {
        let _ = std::fs::create_dir_all(&d);
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let base = name.strip_suffix(".tmp").unwrap_or(&name);
            if let Some(SegmentName::Segment(n)) = parse_segment_name(base)
                && n >= start
                && base.ends_with(ext)
            {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

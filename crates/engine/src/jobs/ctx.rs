//! What a running job uses: the context, its process pipelines and the run supervisor.

use super::{BgJob, CANCELLED, PROGRESS_EVERY, PROMOTED, REQUEUED, TICK};
use crate::lock::Locked;
use crate::proc::{Ffmpeg, Pipes, background};
use crate::runtime::{Runtime, now_ms};
use crate::slots::Lane;
use fliks_engine_core::api::{EngineEvent, JobErrorClass, JobKind, PauseMode};
use fliks_engine_core::jobs::sprite::RUN_TIMEOUT_MS;
use fliks_engine_core::session::{JobWatch, JobWatchInput, job_watch};
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Instant;
use tokio::sync::watch;

/// What a running job uses: its ffmpeg runs, progress, cancellation and the disk check.
#[derive(Clone)]
pub struct Ctx {
    pub rt: Arc<Runtime>,
    pub job: Arc<BgJob>,
}

impl Ctx {
    pub fn cancelled(&self) -> bool {
        *self.job.cancel.borrow()
    }

    /// `BgJobProgress`, at most once per `PROGRESS_EVERY`.
    pub fn progress(&self, pct: f64) {
        let due = {
            let mut m = self.job.m.locked();
            m.pct = pct;
            let due = m
                .last_progress
                .is_none_or(|t| t.elapsed() >= PROGRESS_EVERY);
            if due {
                m.last_progress = Some(Instant::now());
            }
            due
        };
        if due {
            self.rt.events.emit(EngineEvent::BgJobProgress {
                job_id: self.job.id.clone(),
                pct,
            });
        }
    }

    /// Fails below `streaming.jobMinFreeBytes` free under `dir` (created first).
    pub async fn check_disk(&self, dir: &Path) -> Result<(), String> {
        self.rt.check_disk(dir).await
    }

    pub async fn check_disk_existing(&self, dir: &Path) -> Result<(), String> {
        self.rt.check_disk_existing(dir).await
    }

    pub fn media_path(&self) -> String {
        self.rt.map_media_path(&self.job.req.media.path)
    }

    /// Marks progress the watchdog cannot see (bytes read, lines parsed).
    pub fn pulse(&self) {
        self.job.activity.fetch_add(1, Relaxed);
    }

    /// Unpaused time one run may last: optimisations and checks take hours and only the stall
    /// watchdog stops them.
    pub fn run_timeout_ms(&self) -> u64 {
        match self.job.req.kind {
            JobKind::Optimize { .. } | JobKind::Verify { .. } => u64::MAX,
            _ => u64::from(RUN_TIMEOUT_MS),
        }
    }

    /// One ffmpeg run on a background slot: paused while preempted, killed when hung, past
    /// `run_timeout_ms` of unpaused time, or on cancel.
    pub async fn ffmpeg(&self, argv: &[String]) -> Result<(), String> {
        let cmd = Cmd::new("ffmpeg", &self.rt.ffmpeg, argv, Pipes::default());
        self.pipeline(self.run_timeout_ms(), vec![cmd], |p| async move {
            let p = &p[0];
            match p.wait().await.code {
                Some(0) => Ok(()),
                code => Err(format!("ffmpeg exited with {code:?}: {}", tail(p, 2))),
            }
        })
        .await?
    }

    /// Runs `cmds` as one unit on one slot (background, playback when urgent); `work` drives them,
    /// a watchdog verdict, the unpaused-time timeout or a cancel stops them all.
    pub async fn pipeline<T, F, Fut>(
        &self,
        timeout_ms: u64,
        cmds: Vec<Cmd>,
        work: F,
    ) -> Result<T, String>
    where
        F: FnOnce(Vec<Arc<Ffmpeg>>) -> Fut,
        Fut: Future<Output = T>,
    {
        let mut cancel = self.job.cancel.subscribe();
        let mut urgent = self.job.urgent.subscribe();
        let (_unpreempted, unpreempted) = watch::channel(false);
        // An urgent job's slot never waits nor is preempted, and its runs keep normal priority.
        self.job.m.locked().waiting += 1;
        let (_background, playback, mut preempted) = tokio::select! {
            biased;
            _ = urgent.wait_for(|u| *u) => (None, Some(self.rt.slots.playback_on(None)), unpreempted),
            s = self.slot() => {
                let p = s.preempted.clone();
                (Some(s), None, p)
            }
            _ = cancel.wait_for(|c| *c) => {
                self.job.m.locked().waiting -= 1;
                return Err(CANCELLED.into());
            }
        };
        self.job.m.locked().waiting -= 1;
        self.job.last_active_ms.store(0, Relaxed);
        let mut procs = vec![];
        for c in &cmds {
            let (bin, args) = match playback {
                Some(_) => (c.bin.clone(), c.argv.clone()),
                None => background(&c.bin, &c.argv),
            };
            let p = Ffmpeg::spawn_with(&bin, &args, c.pipes).await;
            procs.push(Arc::new(p.map_err(|e| format!("spawn {}: {e}", c.bin))?));
        }
        self.job.m.locked().runs += 1;
        let paused = AtomicBool::new(false);
        let work = work(procs.clone());
        let verdict = {
            let watch = self.supervise(
                &procs,
                &mut preempted,
                playback.is_none().then_some(&mut urgent),
                &mut cancel,
                timeout_ms,
                &paused,
            );
            tokio::pin!(watch, work);
            tokio::select! {
                out = &mut work => Ok(out),
                reason = &mut watch => Err(reason),
            }
        };
        {
            let mut m = self.job.m.locked();
            m.runs -= 1;
            m.paused_runs -= u32::from(paused.load(Relaxed));
        }
        // Nothing outlives the unit: a finished one may leave a producer behind (a pipe's writer).
        for p in &procs {
            match verdict {
                Ok(_) => p.kill().await,
                Err(_) => p.stop().await,
            };
        }
        let reason = match verdict {
            Ok(out) => return Ok(out),
            Err(r) => r,
        };
        if reason == PROMOTED {
            self.job.promoted.store(true, Relaxed);
        }
        Err(if reason == REQUEUED {
            JobErrorClass::Busy.message("stopped for playback, the job starts over")
        } else if reason == CANCELLED || reason == PROMOTED {
            reason.into()
        } else {
            format!("{} {reason}", cmds[0].name)
        })
    }

    /// The background slot of the job: an optimization's device lane, else its priority's queue.
    async fn slot(&self) -> crate::slots::BackgroundSlot {
        let lane = self.lane();
        match lane {
            Some(lane) => self.rt.slots.optimize(lane).await,
            None => self.rt.slots.background(self.job.priority(), 1).await,
        }
    }

    /// The optimization lane the job was leased on.
    pub fn lane(&self) -> Option<Lane> {
        self.job.lane.locked().clone()
    }

    /// Moves a leased job to another device lane; its next run takes a slot there.
    pub fn set_lane(&self, lane: Lane) {
        let mut l = self.job.lane.locked();
        if l.is_some() {
            *l = Some(lane);
        }
    }

    /// Unpaused milliseconds of the last pipeline run: what its work took, slot wait and pauses out.
    pub fn last_active_ms(&self) -> u64 {
        self.job.last_active_ms.load(Relaxed)
    }

    /// Applies the slot's pause verdict and returns why the runs must be stopped.
    async fn supervise(
        &self,
        procs: &[Arc<Ffmpeg>],
        preempted: &mut watch::Receiver<bool>,
        mut urgent: Option<&mut watch::Receiver<bool>>,
        cancel: &mut watch::Receiver<bool>,
        timeout_ms: u64,
        paused_out: &AtomicBool,
    ) -> &'static str {
        let cap = self.rt.pause_capability();
        let restart = self.lane().is_some() && self.rt.optimize_pause() == PauseMode::Restart;
        let lifetime = &self.rt.current_config().lifetime;
        let (mut paused, mut raised, mut active_ms, mut tick) =
            (false, false, 0u64, Instant::now());
        let (mut lines, mut cpu, mut last_progress) = (0, 0, now_ms());
        loop {
            tokio::select! {
                _ = preempted.changed() => {}
                _ = async {
                    match urgent.as_mut() {
                        Some(u) => u.changed().await.ok(),
                        None => std::future::pending().await,
                    }
                } => {}
                _ = cancel.changed() => {}
                _ = tokio::time::sleep(TICK) => {}
            }
            if !paused {
                active_ms += tick.elapsed().as_millis() as u64;
                self.job.last_active_ms.store(active_ms, Relaxed);
            }
            tick = Instant::now();
            if *cancel.borrow() {
                return CANCELLED;
            }
            let promoted = urgent.as_ref().is_some_and(|u| *u.borrow());
            if promoted && !raised {
                // Raised in place when the OS lets it, else stopped for `run` to redo on a playback slot.
                if !procs.iter().all(|p| p.restore_priority()) {
                    return PROMOTED;
                }
                raised = true;
            }
            let want = *preempted.borrow() && !promoted;
            if want && restart {
                return REQUEUED;
            }
            if want != paused {
                let mut sent = false;
                for p in procs {
                    sent |= p.pause(cap, want).await;
                }
                if sent {
                    paused = want;
                    paused_out.store(paused, Relaxed);
                    let mut m = self.job.m.locked();
                    if paused {
                        m.paused_runs += 1;
                    } else {
                        m.paused_runs -= 1;
                        last_progress = now_ms();
                    }
                }
            }
            let l = procs.iter().map(|p| p.stderr_lines()).sum::<u64>()
                + self.job.activity.load(Relaxed);
            let c = procs
                .iter()
                .filter_map(|p| p.cpu_time_ms())
                .reduce(|a, b| a + b);
            let cpu_delta_ms = c.map_or(0, |c| c.saturating_sub(cpu));
            cpu = c.unwrap_or(cpu);
            if l != lines || cpu_delta_ms > 0 {
                lines = l;
                last_progress = now_ms();
            }
            let watch = JobWatchInput {
                now_ms: now_ms(),
                started_ms: 0,
                last_progress_ms: last_progress,
                paused,
                cpu_delta_ms,
                natural_end_ms: None,
            };
            if job_watch(&watch, lifetime) != JobWatch::Ok {
                return "stalled";
            }
            if active_ms > timeout_ms {
                return "timed out";
            }
        }
    }
}

/// One process of a `Ctx::pipeline`; `name` labels its failures.
pub struct Cmd {
    pub name: &'static str,
    pub bin: String,
    pub argv: Vec<String>,
    pub pipes: Pipes,
}

impl Cmd {
    pub fn new(name: &'static str, bin: &str, argv: &[String], pipes: Pipes) -> Self {
        Self {
            name,
            bin: bin.into(),
            argv: argv.to_vec(),
            pipes,
        }
    }
}

/// The last `n` stderr lines of a run, on one line.
pub fn tail(p: &Ffmpeg, n: usize) -> String {
    let t = p.stderr_tail();
    t[t.len().saturating_sub(n)..].join(" ")
}

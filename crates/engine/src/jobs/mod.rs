//! Background jobs: queue, priority, cancellation, progress, disk check, and their ffmpeg runs on
//! the shared slot budget. Spec: `docs/spec/jobs.md`.
mod alass;
pub(crate) mod cover;
mod fingerprint;
mod merge;
mod ocr;
mod optimize;
mod scan;
mod sprite;
mod srt;
pub(crate) mod subtitles;
mod sync;
mod verify;

use crate::AppState;
use crate::lock::Locked;
use crate::proc::{Ffmpeg, Pipes, background};
use crate::runtime::{Runtime, now_ms};
use crate::slots::Lane;
use axum::{
    Json,
    extract::{Path as UrlPath, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use fliks_engine_core::api::{
    BgJobState, EngineEvent, FingerprintWindow, JobAccepted, JobErrorClass, JobKind, JobPriority,
    JobRequest, JobResult, JobSnapshot, MediaInfo, PauseMode,
};
use fliks_engine_core::jobs::sprite::RUN_TIMEOUT_MS;
use fliks_engine_core::session::{JobWatch, JobWatchInput, job_watch};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Jobs running at once. [ts:modules/streaming/thumbnail.service.ts:56]
const MAX_RUNNING: usize = 2;
const KEEP_ENDED: usize = 200;
const TICK: Duration = Duration::from_millis(200);
const PROGRESS_EVERY: Duration = Duration::from_secs(1);
const DEFAULT_MIN_FREE_BYTES: f64 = 1024.0 * 1024.0 * 1024.0;
pub const CANCELLED: &str = "cancelled";
const PROMOTED: &str = "promoted";
/// A run that must yield and cannot be paused is killed: the job goes back to its queue.
const REQUEUED: &str = "requeued";

struct JobMut {
    /// `Queued`, `Running` or an end state; `Paused` is derived from the runs.
    state: BgJobState,
    pct: f64,
    started_ms: Option<i64>,
    ended_ms: Option<i64>,
    result: Option<JobResult>,
    error: Option<String>,
    runs: u32,
    paused_runs: u32,
    /// Runs of a leased job waiting for a slot.
    waiting: u32,
    last_progress: Option<Instant>,
}

pub struct BgJob {
    pub id: String,
    pub req: JobRequest,
    seq: u64,
    dedupe: String,
    created_ms: i64,
    cancel: watch::Sender<bool>,
    /// A session open waits on it: past the queue, never preempted, its runs take playback slots.
    /// A background run in flight is raised in place when the OS allows, else stopped and redone.
    urgent: watch::Sender<bool>,
    /// A leased optimization: its own device slot, outside `MAX_RUNNING`; a software fallback on
    /// a GPU lane moves it to the CPU lane (`Ctx::set_lane`).
    lane: Mutex<Option<Lane>>,
    /// Unpaused milliseconds of the last pipeline run.
    last_active_ms: AtomicU64,
    /// A background run was stopped by the promotion: `run` redoes the job from its start.
    promoted: AtomicBool,
    /// Bumped by `Ctx::pulse`: work the tools' stderr and CPU do not show (reading a file).
    activity: AtomicU64,
    m: Mutex<JobMut>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl BgJob {
    fn priority(&self) -> JobPriority {
        self.req.priority.unwrap_or_default()
    }

    fn base_state(&self) -> BgJobState {
        self.m.locked().state
    }

    /// Every run of the job is held by playback right now.
    pub fn paused(&self) -> bool {
        self.snapshot().state == BgJobState::Paused
    }

    pub fn snapshot(&self) -> JobSnapshot {
        let m = self.m.locked();
        let held = m.runs > 0 && m.paused_runs == m.runs;
        let queued_for_slot = m.runs == 0 && m.waiting > 0 && self.lane.locked().is_some();
        let paused = m.state == BgJobState::Running && (held || queued_for_slot);
        JobSnapshot {
            job_id: self.id.clone(),
            kind: self.req.kind.name().into(),
            media_file_id: self.req.media.media_file_id,
            priority: self.priority(),
            state: if paused { BgJobState::Paused } else { m.state },
            pct: m.pct,
            created_ms: self.created_ms as f64,
            started_ms: m.started_ms.map(|t| t as f64),
            ended_ms: m.ended_ms.map(|t| t as f64),
            result: m.result.clone(),
            error: m.error.clone(),
        }
    }
}

/// Every job of the engine, in creation order (ended ones trimmed to the last `KEEP_ENDED`).
#[derive(Default)]
pub struct Jobs {
    q: Mutex<Queue>,
}

/// The jobs and the indexes `submit`, `pump` and `finish` keep in step, all under one lock.
#[derive(Default)]
struct Queue {
    next_seq: u64,
    all: BTreeMap<u64, Arc<BgJob>>,
    by_id: HashMap<String, u64>,
    /// Unfinished jobs by request, to answer an identical one with the same job.
    open: HashMap<String, u64>,
    /// Queued jobs, next to start first: by priority, then age.
    waiting: BTreeMap<(JobPriority, u64), Arc<BgJob>>,
    running: usize,
    ended: VecDeque<u64>,
}

impl Jobs {
    pub fn get(&self, id: &str) -> Option<Arc<BgJob>> {
        let q = self.q.locked();
        q.all.get(q.by_id.get(id)?).cloned()
    }

    pub fn snapshots(&self) -> Vec<JobSnapshot> {
        let all: Vec<_> = self.q.locked().all.values().cloned().collect();
        all.iter().map(|j| j.snapshot()).collect()
    }

    fn live(&self, keep: impl Fn(&BgJob) -> bool) -> Vec<Arc<BgJob>> {
        let q = self.q.locked();
        let open: Vec<_> = q
            .open
            .values()
            .filter_map(|s| q.all.get(s))
            .cloned()
            .collect();
        drop(q);
        open.into_iter()
            .filter(|j| !j.base_state().ended() && keep(j))
            .collect()
    }
}

fn validate(kind: &JobKind) -> Result<(), String> {
    if let Some(family) = kind.info().and_then(MediaInfo::family)
        && !kind.families().contains(&family)
    {
        return Err(format!(
            "{} does not apply to {} files",
            kind.name(),
            family.as_str()
        ));
    }
    match kind {
        JobKind::Sprite { info } => {
            if !info
                .duration_seconds
                .is_some_and(|d| d.is_finite() && d > 0.0)
            {
                return Err("Sprite needs a positive info.durationSeconds".into());
            }
        }
        JobKind::Scan { info } => {
            if info.video.is_empty() {
                return Err("Scan needs a video stream in info".into());
            }
        }
        JobKind::Fingerprint { window, .. } => {
            if let FingerprintWindow::Outro { duration_seconds } = window
                && !(duration_seconds.is_finite() && *duration_seconds > 0.0)
            {
                return Err("Fingerprint outro needs a positive durationSeconds".into());
            }
        }
        JobKind::SubtitleExtract { info } => subtitles::validate(info)?,
        JobKind::SubtitleOcr {
            info,
            stream_index,
            language,
        } => ocr::validate(info, *stream_index, language.as_deref())?,
        JobKind::SubtitleSync {
            info,
            subtitle,
            reference,
            max_offset_seconds,
            ..
        } => sync::validate(info, subtitle, reference, *max_offset_seconds)?,
        JobKind::SubtitleToSrt { info, source } => srt::validate(info, source)?,
        JobKind::Cover { info } => {
            fliks_engine_core::jobs::cover::validate(info)?;
        }
        JobKind::Merge { parts, output, .. } => {
            fliks_engine_core::jobs::merge::validate(parts, output)?;
        }
        JobKind::Optimize { payload } => optimize::validate(payload)?,
        JobKind::Verify { payload } => verify::validate(payload)?,
    }
    Ok(())
}

/// Optional kinds and routes the host may surface in `hello.features`.
pub fn optional_features() -> Vec<String> {
    let mut out = vec![];
    if ocr::announced() {
        out.push("subtitle-ocr".into());
    }
    if sync::announced() {
        out.push("subtitle-sync".into());
    }
    out
}

/// Queues a job, or answers the in-flight one of an identical request.
pub fn submit(rt: &Arc<Runtime>, req: JobRequest) -> Result<String, String> {
    submit_as(rt, req, false)
}

/// `submit` for work a session open waits on: the job, or the identical one in flight, runs at
/// once on a playback slot.
pub fn submit_urgent(rt: &Arc<Runtime>, req: JobRequest) -> Result<String, String> {
    submit_as(rt, req, true)
}

fn submit_as(rt: &Arc<Runtime>, req: JobRequest, urgent: bool) -> Result<String, String> {
    if !Path::new(&req.media.path).is_absolute() {
        return Err("media.path must be absolute".into());
    }
    validate(&req.kind)?;
    let dedupe = serde_json::to_string(&(&req.kind, &req.media)).unwrap_or_default();
    let (id, now) = {
        let mut q = rt.jobs.q.locked();
        if let Some(j) = q.open.get(&dedupe).and_then(|s| q.all.get(s)).cloned()
            && !j.base_state().ended()
        {
            if !urgent {
                return Ok(j.id.clone());
            }
            j.urgent.send_replace(true);
            let now = q.waiting.remove(&(j.priority(), j.seq)).is_some() && begin(&mut q, &j);
            (j.id.clone(), now.then_some(j))
        } else {
            let j = queue(&mut q, req, dedupe, urgent, None);
            let now = urgent && begin(&mut q, &j);
            if !now {
                q.waiting.insert((j.priority(), j.seq), j.clone());
            }
            (j.id.clone(), now.then_some(j))
        }
    };
    if let Some(j) = now {
        launch(rt, j);
    }
    pump(rt);
    Ok(id)
}

/// Starts the optimization or verification a worker leased, at once on `lane`: no wait for
/// `MAX_RUNNING`, the device's own limit applies in the slots. `lease` names it (never deduped).
pub fn submit_leased(
    rt: &Arc<Runtime>,
    req: JobRequest,
    lane: Lane,
    lease: &str,
) -> Result<Arc<BgJob>, String> {
    validate(&req.kind)?;
    let j = {
        let mut q = rt.jobs.q.locked();
        let j = queue(&mut q, req, lease.into(), false, Some(lane));
        begin(&mut q, &j);
        j
    };
    launch(rt, j.clone());
    Ok(j)
}

/// A new job, indexed but neither waiting nor started.
fn queue(
    q: &mut Queue,
    req: JobRequest,
    dedupe: String,
    urgent: bool,
    lane: Option<Lane>,
) -> Arc<BgJob> {
    let seq = q.next_seq;
    q.next_seq += 1;
    let j = Arc::new(BgJob {
        id: crate::random_hex(8),
        req,
        seq,
        dedupe: dedupe.clone(),
        lane: Mutex::new(lane),
        last_active_ms: AtomicU64::new(0),
        created_ms: now_ms(),
        cancel: watch::channel(false).0,
        urgent: watch::channel(urgent).0,
        promoted: AtomicBool::new(false),
        activity: AtomicU64::new(0),
        m: Mutex::new(JobMut {
            state: BgJobState::Queued,
            pct: 0.0,
            started_ms: None,
            ended_ms: None,
            result: None,
            error: None,
            runs: 0,
            paused_runs: 0,
            waiting: 0,
            last_progress: None,
        }),
        task: Mutex::new(None),
    });
    q.all.insert(seq, j.clone());
    q.by_id.insert(j.id.clone(), seq);
    q.open.insert(dedupe, seq);
    j
}

/// Marks a queued job running; false when it already left the queue.
fn begin(q: &mut Queue, j: &BgJob) -> bool {
    let mut m = j.m.locked();
    if m.state != BgJobState::Queued {
        return false;
    }
    m.state = BgJobState::Running;
    m.started_ms = Some(now_ms());
    q.running += usize::from(j.lane.locked().is_none());
    true
}

fn launch(rt: &Arc<Runtime>, j: Arc<BgJob>) {
    let task = tokio::spawn(run(rt.clone(), j.clone()));
    *j.task.locked() = Some(task);
}

/// Starts queued jobs by priority, then age, while fewer than `MAX_RUNNING` run.
fn pump(rt: &Arc<Runtime>) {
    let start: Vec<Arc<BgJob>> = {
        let mut q = rt.jobs.q.locked();
        let mut picked = vec![];
        while q.running < MAX_RUNNING {
            let Some((_, j)) = q.waiting.pop_first() else {
                break;
            };
            if begin(&mut q, &j) {
                picked.push(j);
            }
        }
        picked
    };
    for j in start {
        launch(rt, j);
    }
}

async fn run(rt: Arc<Runtime>, job: Arc<BgJob>) {
    let ctx = Ctx {
        rt: rt.clone(),
        job: job.clone(),
    };
    // A promotion that stopped a background run redoes the job once, now on playback slots.
    let out = loop {
        let out = exec(&ctx).await;
        if !job.promoted.swap(false, Relaxed) || ctx.cancelled() {
            break out;
        }
    };
    let (state, result, error) = match out {
        Ok(r) => (BgJobState::Done, Some(r), None),
        Err(_) if ctx.cancelled() => (BgJobState::Cancelled, None, Some(CANCELLED.into())),
        Err(e) => {
            tracing::warn!(job = job.id, kind = job.req.kind.name(), "job failed: {e}");
            (BgJobState::Failed, None, Some(e))
        }
    };
    finish(&rt, &job, state, result, error);
    pump(&rt);
}

async fn exec(ctx: &Ctx) -> Result<JobResult, String> {
    let job = &ctx.job;
    match &job.req.kind {
        JobKind::Sprite { info } => sprite::run(ctx, info).await,
        JobKind::Scan { info } => scan::run(ctx, info).await,
        JobKind::Cover { info } => cover::run(ctx, info).await,
        JobKind::Merge {
            parts,
            output,
            tags,
        } => merge::run(ctx, parts, output, tags.as_ref()).await,
        JobKind::Fingerprint {
            window,
            max_lookup_minutes,
        } => fingerprint::run(ctx, window, *max_lookup_minutes).await,
        JobKind::SubtitleExtract { info } => subtitles::run(ctx, info).await,
        JobKind::SubtitleOcr {
            info,
            stream_index,
            language,
        } => ocr::run(ctx, info, *stream_index, language.as_deref()).await,
        JobKind::SubtitleSync {
            info,
            subtitle,
            reference,
            max_offset_seconds,
            no_fix_framerate,
            gss,
        } => {
            sync::run(
                ctx,
                info,
                subtitle,
                reference,
                *max_offset_seconds,
                no_fix_framerate.unwrap_or(false),
                gss.unwrap_or(false),
            )
            .await
        }
        JobKind::SubtitleToSrt { info, source } => srt::run(ctx, info, source).await,
        JobKind::Optimize { payload } => optimize::run(ctx, payload).await,
        JobKind::Verify { payload } => verify::run(ctx, payload).await,
    }
}

fn finish(
    rt: &Runtime,
    job: &BgJob,
    state: BgJobState,
    result: Option<JobResult>,
    error: Option<String>,
) {
    let was = {
        let mut m = job.m.locked();
        if m.state.ended() {
            return;
        }
        let was = m.state;
        m.state = state;
        m.ended_ms = Some(now_ms());
        m.result = result.clone();
        m.error = error.clone();
        if state == BgJobState::Done {
            m.pct = 100.0;
        }
        was
    };
    rt.events.emit(EngineEvent::BgJobDone {
        job_id: job.id.clone(),
        state,
        result,
        error,
    });
    let mut q = rt.jobs.q.locked();
    if q.open.get(&job.dedupe) == Some(&job.seq) {
        q.open.remove(&job.dedupe);
    }
    match was {
        BgJobState::Queued => {
            q.waiting.remove(&(job.priority(), job.seq));
        }
        BgJobState::Running => q.running -= usize::from(job.lane.locked().is_none()),
        _ => {}
    }
    q.ended.push_back(job.seq);
    while q.ended.len() > KEEP_ENDED {
        if let Some(old) = q.ended.pop_front().and_then(|s| q.all.remove(&s)) {
            q.by_id.remove(&old.id);
        }
    }
}

/// Cancels `jobs` and waits until their runs are gone.
async fn stop(rt: &Arc<Runtime>, jobs: Vec<Arc<BgJob>>) {
    for j in &jobs {
        j.cancel.send_replace(true);
        if j.base_state() == BgJobState::Queued {
            finish(rt, j, BgJobState::Cancelled, None, Some(CANCELLED.into()));
        }
    }
    for j in jobs {
        let task = j.task.locked().take();
        if let Some(t) = task {
            let _ = t.await;
        }
    }
}

/// `DELETE /v1/jobs/{id}`: false when unknown.
pub async fn cancel(rt: &Arc<Runtime>, id: &str) -> bool {
    let Some(j) = rt.jobs.get(id) else {
        return false;
    };
    if !j.base_state().ended() {
        stop(rt, vec![j]).await;
    }
    true
}

/// Invalidate: every unfinished job of the file. Returns how many were cancelled.
pub async fn cancel_file(rt: &Arc<Runtime>, file: u32) -> u32 {
    let jobs = rt.jobs.live(|j| j.req.media.media_file_id == file);
    let n = jobs.len() as u32;
    stop(rt, jobs).await;
    n
}

pub async fn shutdown(rt: &Arc<Runtime>) {
    stop(rt, rt.jobs.live(|_| true)).await;
}

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
            _ = urgent.wait_for(|u| *u) => (None, Some(self.rt.slots.playback(true)), unpreempted),
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

fn disk_verdict(free: Option<u64>, min: f64, dir: &Path) -> Result<(), String> {
    match free {
        Some(f) if min > 0.0 && (f as f64) < min => Err(format!(
            "not enough disk space: {f} bytes free under {}, jobMinFreeBytes is {min}",
            dir.display()
        )),
        _ => Ok(()),
    }
}

#[cfg(unix)]
#[allow(clippy::useless_conversion)]
fn free_bytes(p: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a valid C string and a zeroed out-struct.
    let ok = unsafe { libc::statvfs(c.as_ptr(), &mut s) } == 0;
    ok.then(|| u64::from(s.f_bavail) * u64::from(s.f_frsize))
}

#[cfg(windows)]
fn free_bytes(p: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let w: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
    let mut free = 0u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            w.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(free)
}

#[cfg(not(any(unix, windows)))]
fn free_bytes(_: &Path) -> Option<u64> {
    None
}

impl Runtime {
    /// How an optimization yields to playback: suspended, or killed and requeued where the OS
    /// cannot suspend it (`FLIKS_OPTIMIZE_PAUSE=restart` forces it, for a GPU short of memory).
    pub fn optimize_pause(&self) -> PauseMode {
        let forced = std::env::var("FLIKS_OPTIMIZE_PAUSE").is_ok_and(|v| v == "restart");
        let none = self.pause_capability() == fliks_engine_core::session::PauseCapability::None;
        if forced || none {
            PauseMode::Restart
        } else {
            PauseMode::Suspend
        }
    }

    /// Fails below `streaming.jobMinFreeBytes` free under `dir` (created first): jobs and playback runs.
    pub async fn check_disk(&self, dir: &Path) -> Result<(), String> {
        self.check_disk_in(dir, true).await
    }

    /// `check_disk` on a directory that must already exist: nothing is created (a missing one passes).
    pub async fn check_disk_existing(&self, dir: &Path) -> Result<(), String> {
        self.check_disk_in(dir, false).await
    }

    async fn check_disk_in(&self, dir: &Path, create: bool) -> Result<(), String> {
        let min = self
            .current_config()
            .streaming
            .job_min_free_bytes
            .unwrap_or(DEFAULT_MIN_FREE_BYTES);
        let d = dir.to_path_buf();
        let free = tokio::task::spawn_blocking(move || {
            if create {
                let _ = std::fs::create_dir_all(&d);
            }
            free_bytes(&d)
        })
        .await
        .ok()
        .flatten();
        disk_verdict(free, min, dir)
    }
}

type S = State<Arc<AppState>>;

#[utoipa::path(post, path = "/v1/jobs", operation_id = "create_job", request_body = JobRequest, responses(
    (status = 200, body = JobAccepted, description = "queued, or the in-flight identical request"),
    (status = 400, description = "relative path or unusable request")))]
pub async fn create(State(s): S, Json(req): Json<JobRequest>) -> Response {
    match submit(&s.rt, req) {
        Ok(job_id) => Json(JobAccepted { job_id }).into_response(),
        Err(m) => (StatusCode::BAD_REQUEST, m).into_response(),
    }
}

#[utoipa::path(get, path = "/v1/jobs", operation_id = "list_jobs", responses((status = 200, body = Vec<JobSnapshot>, description = "jobs")))]
pub async fn list(State(s): S) -> Json<Vec<JobSnapshot>> {
    Json(s.rt.jobs.snapshots())
}

#[utoipa::path(get, path = "/v1/jobs/{id}", operation_id = "get_job", responses(
    (status = 200, body = JobSnapshot, description = "the job"), (status = 404, description = "unknown job")))]
pub async fn get_one(State(s): S, UrlPath(id): UrlPath<String>) -> Response {
    match s.rt.jobs.get(&id) {
        Some(j) => Json(j.snapshot()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[utoipa::path(delete, path = "/v1/jobs/{id}", operation_id = "cancel_job", responses(
    (status = 204, description = "cancelled (an ended job is left as is)"),
    (status = 404, description = "unknown job")))]
pub async fn delete(State(s): S, UrlPath(id): UrlPath<String>) -> StatusCode {
    if cancel(&s.rt, &id).await {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_check_fails_below_the_threshold_only() {
        let d = std::env::temp_dir();
        let free = free_bytes(&d).expect("free space of the temp dir");
        assert!(free > 0);
        assert!(disk_verdict(Some(free), 1.0, &d).is_ok());
        let e = disk_verdict(Some(10), 1e9, &d).unwrap_err();
        assert!(e.starts_with("not enough disk space: 10 bytes free"), "{e}");
        assert!(disk_verdict(Some(10), 0.0, &d).is_ok(), "0 disables");
        assert!(
            disk_verdict(None, 1e9, &d).is_ok(),
            "unknown is not a failure"
        );
    }
}

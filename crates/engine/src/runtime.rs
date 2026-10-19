//! Sessions and their ffmpeg jobs: open, heartbeat, seek and rung switch, HW fallback, throttle,
//! idle reaping, revocation and cache eviction. Spec: `docs/spec/runtime.md` sections 2-5.
use crate::cache::{Cache, remove_dir, valid_owner};
use crate::events::Events;
use crate::hw::Hw;
use crate::proc::{Exit, Ffmpeg};
use crate::remux::Remux;
use crate::resolve::Resolver;
use crate::slots::{PlaybackSlot, Slots};
use fliks_engine_core::api::{
    Decision, EngineConfig, EngineEvent, Hints, HostCaps, HwAccel, JobState, JobStats,
    LifetimeConfig, MEDIA_INFO_SCHEMA_VERSION, OpenSessionRequest, OpenSessionResponse, PlayMethod,
    SessionKind, SessionSnapshot, StreamingConfig,
};
use fliks_engine_core::mp4::{
    grid_segment_index, remux_assembly_plan, remux_audio_grid, remux_run_start,
};
use fliks_engine_core::session::{
    self as core_session, CacheEntryInfo, CachePolicy, EARLY_PROBE_SEGMENTS, EARLY_WAIT_MS,
    ExistingRun, JobWatch, JobWatchInput, PauseCapability, ResolveInput, ResumeAction, RunProgress,
    SegmentName, SegmentSet, SessionPlan, SessionVariant, ThrottleDecision, ThrottleInput,
    TranscodeRun, Viewer, early_run_seconds, job_watch, live_session_expired, parse_segment_name,
    plan_gc, seconds_to_segment, segment_index_to_seconds, session_key, session_to_evict,
    tombstone_revivable, variant_hash,
};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use tokio::time::Instant;

const WATCH_EVERY: Duration = Duration::from_millis(200);
const THROTTLE_EVERY: Duration = Duration::from_secs(5);
const SEGMENT_WAIT: Duration = Duration::from_millis(core_session::SEGMENT_WAIT_MS as u64);
const INIT_NON_EMPTY: Duration = Duration::from_millis(core_session::INIT_NON_EMPTY_WAIT_MS as u64);

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

struct SessionMut {
    last_beat_ms: i64,
    position_s: f64,
    state: String,
    quality: Option<String>,
    last_requested_segment: Option<u32>,
    last_segment_at_ms: Option<i64>,
    served: Option<(String, HwAccel)>,
    /// What the client plays now: an ABR switch moves a DirectStream session to a rung.
    kind: SessionKind,
}

/// Which run of a session a job is. [ts:modules/streaming/transcoding/variant.ts:11-27]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Main,
    /// Seg-0 and seg-1 of a resume, next to the main run.
    Early,
    Remux,
}

pub struct Session {
    pub sid: String,
    pub owner: Option<String>,
    pub media_file_id: u32,
    pub path: String,
    pub req: OpenSessionRequest,
    pub decision: Decision,
    pub streaming: StreamingConfig,
    pub config_revision: u32,
    pub plan: SessionPlan,
    /// Profile hash with the instance suffix of a concurrent playback.
    pub hash: String,
    pub opened_at_ms: i64,
    m: Mutex<SessionMut>,
}

impl Session {
    fn variant(&self, kind: JobKind) -> SessionVariant {
        match kind {
            JobKind::Main => SessionVariant::Main,
            JobKind::Early => SessionVariant::Early,
            JobKind::Remux => core_session::remux_variant_of(&self.req, &self.plan),
        }
    }

    /// Cache key of a run kind: the profile hash and its variant suffix.
    pub fn cache_hash(&self, kind: JobKind) -> String {
        variant_hash(&self.hash, &self.variant(kind))
    }

    pub fn key(&self, kind: JobKind) -> String {
        session_key(
            self.media_file_id,
            self.owner.as_deref(),
            &self.cache_hash(kind),
        )
    }

    pub fn job_key(&self) -> String {
        self.key(JobKind::Main)
    }

    pub fn served_kind(&self) -> SessionKind {
        self.m.lock().unwrap().kind
    }

    /// The run snapshots and job commands follow: the remux one while the client plays it.
    pub fn primary_key(&self) -> String {
        self.key(if self.served_kind() == SessionKind::Remux {
            JobKind::Remux
        } else {
            JobKind::Main
        })
    }

    pub fn pinned(&self) -> bool {
        self.req.pinned
    }

    pub fn position_s(&self) -> f64 {
        self.m.lock().unwrap().position_s
    }

    fn viewer(&self) -> Viewer {
        let m = self.m.lock().unwrap();
        Viewer {
            position: m.position_s,
            last_requested_segment: m.last_requested_segment,
            pinned: self.req.pinned,
        }
    }

    /// The resume floor of an init request: the job's start or the playhead's segment.
    /// [ts:modules/streaming/streaming.controller.ts:428-450]
    fn resume_floor(&self, job: Option<&Job>) -> u32 {
        let pos = seconds_to_segment(
            self.position_s(),
            self.plan.segment_duration,
            self.plan.source_fps,
        );
        pos.max(job.map_or(0, |j| j.start_segment))
    }

    /// The resume floor of a remux run, on its keyframe grid when it has one.
    /// [ts:modules/streaming/streaming.controller.ts:428-450]
    fn remux_floor(&self, job: Option<&Job>) -> u32 {
        let pos = match &self.plan.remux_grid {
            Some(g) => {
                grid_segment_index(&g.boundaries, self.position_s(), self.plan.timeline.origin)
                    as u32
            }
            None => seconds_to_segment(
                self.position_s(),
                self.plan.segment_duration,
                self.plan.source_fps,
            ),
        };
        pos.max(job.map_or(0, |j| j.start_segment))
    }
}

struct JobMut {
    sets: Vec<SegmentSet>,
    inits: Vec<bool>,
    /// The watcher listed the directories after the process exited: the sets are final.
    settled: bool,
    paused: bool,
    frontier_hint: Option<u32>,
    killed: bool,
    /// A remux run's assembler still builds segments after the process exited.
    pending: bool,
    /// Watchdog clocks; a resume restarts both.
    started_ms: i64,
    last_progress_ms: i64,
    seen_lines: u64,
    tmp_bytes: u64,
    cpu_ms: u64,
    reason: Option<&'static str>,
}

/// One ffmpeg run of a session key (`docs/spec/runtime.md` section 2).
pub struct Job {
    pub key: String,
    pub kind: JobKind,
    pub hash: String,
    pub media_file_id: u32,
    pub owner: Option<String>,
    pub quality: String,
    pub dir: PathBuf,
    pub var_stream_map: bool,
    pub tracks: usize,
    pub ext: &'static str,
    pub start_segment: u32,
    pub requested_hw: HwAccel,
    pub hw: HwAccel,
    pub segment_duration: f64,
    pub fps: Option<f64>,
    pub duration: f64,
    pub restarts: u32,
    /// Media time a run with a natural end produces (the early companion's `-to`), in ms.
    natural_end_ms: Option<i64>,
    proc: Ffmpeg,
    remux: Option<Remux>,
    /// Held while the process lives; lent back while throttled.
    slot: Mutex<Option<PlaybackSlot>>,
    m: Mutex<JobMut>,
    changed: watch::Sender<u64>,
}

impl Job {
    /// Track `t`'s own directory: var_stream_map's `<t>/`, a remux run's audio `<t>/`.
    fn split(&self, t: usize) -> bool {
        self.var_stream_map || (self.kind == JobKind::Remux && t > 0)
    }

    pub fn track_dir(&self, t: usize) -> PathBuf {
        if self.split(t) {
            self.dir.join(t.to_string())
        } else {
            self.dir.clone()
        }
    }

    pub fn init_path(&self, t: usize) -> PathBuf {
        let name = if self.split(t) {
            format!("init_{t}.mp4")
        } else {
            "init.mp4".into()
        };
        self.track_dir(t).join(name)
    }

    pub fn segment_path(&self, t: usize, n: u32) -> PathBuf {
        self.track_dir(t).join(format!("seg-{n:04}.{}", self.ext))
    }

    /// The process lives or its output is still being assembled.
    pub fn producing(&self) -> bool {
        self.proc.exited().is_none() || self.m.lock().unwrap().pending
    }

    pub fn killed(&self) -> bool {
        self.m.lock().unwrap().killed
    }

    pub fn has(&self, t: usize, n: u32) -> bool {
        self.m
            .lock()
            .unwrap()
            .sets
            .get(t)
            .is_some_and(|s| s.contains(n))
    }

    fn serves(&self, s: &Session) -> bool {
        s.media_file_id == self.media_file_id && s.owner == self.owner && s.hash == self.hash
    }

    /// Stops ffmpeg on purpose (seek, switch, idle, revoke), waits for it and frees its slot.
    pub async fn kill(&self) -> Exit {
        self.m.lock().unwrap().killed = true;
        let exit = self.proc.stop().await;
        self.slot.lock().unwrap().take();
        exit
    }

    pub async fn pause(&self, cap: PauseCapability, pause: bool) -> bool {
        let sent = self.proc.pause(cap, pause).await;
        if sent {
            {
                let mut m = self.m.lock().unwrap();
                m.paused = pause;
                if !pause {
                    m.started_ms = now_ms();
                    m.last_progress_ms = m.started_ms;
                }
            }
            if let Some(r) = &self.remux {
                r.view.pause(pause);
            }
            if let Some(s) = &*self.slot.lock().unwrap() {
                s.set_idle(pause);
            }
        }
        sent
    }

    fn state(&self) -> JobState {
        let m = self.m.lock().unwrap();
        match self.proc.exited() {
            None if m.paused => JobState::Paused,
            None => JobState::Running,
            Some(_) if m.killed => JobState::Killed,
            Some(e) if e.code == Some(0) => JobState::Done,
            Some(_) => JobState::Failed,
        }
    }

    pub fn stats(&self) -> JobStats {
        let state = self.state();
        let m = self.m.lock().unwrap();
        let latest = m.sets.first().and_then(SegmentSet::latest);
        let percent = match (latest, self.duration > 0.0) {
            (Some(l), true) => {
                let done = segment_index_to_seconds(l + 1, self.segment_duration, self.fps);
                (done / self.duration * 100.0).min(100.0)
            }
            _ => 0.0,
        };
        JobStats {
            state,
            quality: self.quality.clone(),
            requested_hw: self.requested_hw,
            actual_hw: self.hw,
            start_segment: self.start_segment,
            frontier_segment: latest,
            transcode_percent: percent,
            throttled: m.paused,
            restarts: self.restarts,
            exit_code: self.proc.exited().and_then(|e| e.code),
            stderr_tail: self.proc.stderr_tail(),
        }
    }

    /// Waits until `pred` holds, the run is over (false) or the timeout (false).
    async fn wait_for(&self, timeout: Duration, pred: impl Fn(&JobMut) -> bool) -> bool {
        let mut rx = self.changed.subscribe();
        let deadline = Instant::now() + timeout;
        loop {
            {
                let m = self.m.lock().unwrap();
                if pred(&m) {
                    return true;
                }
                if m.settled {
                    return false;
                }
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => {}
                _ => return false,
            }
        }
    }

    pub async fn wait_segment(&self, t: usize, n: u32, timeout: Duration) -> bool {
        self.wait_for(timeout, |m| m.sets.get(t).is_some_and(|s| s.contains(n)))
            .await
    }

    /// The init landed and has bytes (ffmpeg writes it without a temp file).
    pub async fn wait_init(&self, t: usize, timeout: Duration) -> bool {
        if !self
            .wait_for(timeout, |m| m.inits.get(t).copied().unwrap_or(false))
            .await
        {
            return false;
        }
        non_empty_within(&self.init_path(t), INIT_NON_EMPTY).await
    }

    /// One listing of the run's directories; true when something changed.
    async fn scan(&self) -> bool {
        let mut sets = vec![];
        let mut inits = vec![];
        let mut tmp_bytes = 0;
        for t in 0..self.tracks {
            let mut set = SegmentSet::default();
            let mut init = false;
            if let Ok(mut rd) = tokio::fs::read_dir(self.track_dir(t)).await {
                while let Ok(Some(e)) = rd.next_entry().await {
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    if name.contains(".tmp") {
                        tmp_bytes += e.metadata().await.map_or(0, |m| m.len());
                    }
                    let ext_ok = name.ends_with(self.ext) || name.ends_with(".mp4");
                    match parse_segment_name(&name) {
                        Some(SegmentName::Segment(n)) if ext_ok => set.insert(n),
                        Some(SegmentName::Init(_)) => init = true,
                        _ => {}
                    }
                }
            }
            sets.push(set);
            inits.push(init);
        }
        let mut m = self.m.lock().unwrap();
        let changed = m.sets != sets || m.inits != inits;
        if changed || m.tmp_bytes != tmp_bytes {
            m.last_progress_ms = now_ms();
        }
        m.sets = sets;
        m.inits = inits;
        m.tmp_bytes = tmp_bytes;
        changed
    }

    /// `audio`: the 1-based rendition a remux request is for.
    fn existing_run(&self, audio: Option<usize>) -> ExistingRun {
        let exit = self.proc.exited();
        ExistingRun {
            exit_code: exit.and_then(|e| e.code),
            signal: exit.is_some_and(|e| e.signal),
            output_pending: self.m.lock().unwrap().pending,
            start_segment: self.start_segment,
            remux: self.remux.is_some(),
            assembler: self.remux.as_ref().map(|r| r.view.progress(audio)),
        }
    }

    /// Waits until the watcher saw the run end and its output was assembled.
    async fn settled(&self) {
        self.wait_for(Duration::from_secs(3600), |_| false).await;
    }
}

pub async fn non_empty_within(p: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if tokio::fs::metadata(p).await.is_ok_and(|m| m.len() > 0) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// What a new job runs, before its process exists.
struct Run {
    key: String,
    kind: JobKind,
    quality: String,
    dir: PathBuf,
    var_stream_map: bool,
    tracks: usize,
    ext: &'static str,
    start: u32,
    /// (actual, requested)
    hw: (HwAccel, HwAccel),
    restarts: u32,
    remux: Option<Remux>,
}

#[derive(Default)]
struct Reg {
    sessions: HashMap<String, Arc<Session>>,
    tombstones: HashMap<String, Arc<Session>>,
    jobs: HashMap<String, Arc<Job>>,
}

/// Why a request could not be served.
#[derive(Debug, Clone, PartialEq)]
pub enum OpenError {
    BadRequest(String),
    NoConfig,
    Detecting,
}

/// What a segment request found.
pub enum Fetch {
    Ready {
        path: PathBuf,
        job: Option<Arc<Job>>,
        waited_ms: f64,
        cache_hit: bool,
    },
    Retry,
    NotFound,
    Failed,
}

pub struct Runtime {
    pub config: Mutex<Option<Arc<EngineConfig>>>,
    pub hw: Arc<Hw>,
    pub cache: Cache,
    pub events: Events,
    pub resolver: Resolver,
    pub ffmpeg: String,
    /// Background jobs' own tools: `FFPROBE_PATH` and `FPCALC_PATH`.
    pub ffprobe: String,
    pub fpcalc: String,
    /// The one ffmpeg budget: session runs, extracts, background jobs and probe samples.
    pub slots: Arc<Slots>,
    pub jobs: crate::jobs::Jobs,
    pub live: crate::live::Live,
    reg: Mutex<Reg>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Runtime {
    pub fn new(hw: Arc<Hw>, cache: Cache, ffmpeg: String) -> Arc<Self> {
        let tool = crate::hw::tool_path;
        let (ffprobe, fpcalc) = (
            tool("FFPROBE_PATH", "ffprobe"),
            tool("FPCALC_PATH", "fpcalc"),
        );
        Self::with_tools(hw, cache, ffmpeg, ffprobe, fpcalc)
    }

    pub fn with_tools(
        hw: Arc<Hw>,
        cache: Cache,
        ffmpeg: String,
        ffprobe: String,
        fpcalc: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            config: Mutex::new(None),
            hw,
            cache,
            events: Events::default(),
            resolver: Resolver::default(),
            ffmpeg,
            ffprobe,
            fpcalc,
            slots: Slots::new(crate::slots::auto_size()),
            jobs: Default::default(),
            live: Default::default(),
            reg: Mutex::new(Reg::default()),
            locks: Mutex::new(HashMap::new()),
        })
    }

    pub fn current_config(&self) -> Arc<EngineConfig> {
        self.config.lock().unwrap().clone().unwrap_or_default()
    }

    /// The host's media path as this engine opens it (`EngineConfig.pathMappings`).
    pub fn map_media_path(&self, path: &str) -> String {
        fliks_engine_core::api::apply_path_mappings(&self.current_config().path_mappings, path)
    }

    fn lifetime(&self) -> LifetimeConfig {
        self.config
            .lock()
            .unwrap()
            .as_ref()
            .map_or_else(Default::default, |c| c.lifetime.clone())
    }

    fn lock_for(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .unwrap()
            .entry(key.into())
            .or_default()
            .clone()
    }

    pub(crate) fn pause_capability(&self) -> PauseCapability {
        self.hw.snapshot().map_or(PauseCapability::None, |s| {
            PauseCapability::parse(&s.pause_capability)
        })
    }

    pub(crate) fn caps(&self) -> Option<HostCaps> {
        self.hw.snapshot().map(|s| s.host_caps())
    }

    // ---- sessions ----

    pub fn open(
        self: &Arc<Self>,
        req: OpenSessionRequest,
    ) -> Result<OpenSessionResponse, OpenError> {
        let config = self
            .config
            .lock()
            .unwrap()
            .clone()
            .ok_or(OpenError::NoConfig)?;
        let caps = self.caps().ok_or(OpenError::Detecting)?;
        if req.owner_id.as_deref().is_some_and(|o| !valid_owner(o)) {
            return Err(OpenError::BadRequest(
                "ownerId: [A-Za-z0-9_.-]{1,64}".into(),
            ));
        }
        if !Path::new(&req.decide.media.path).is_absolute() {
            return Err(OpenError::BadRequest("media.path must be absolute".into()));
        }
        let decision = fliks_engine_core::decide::decide(&req.decide, &caps, &config.streaming);
        let plan = core_session::plan(&req, &decision, &config.streaming, &caps);
        let sid = crate::random_hex(16);
        let now = now_ms();
        let lifetime = config.lifetime.clone();
        let owner = req.owner_id.clone();
        let file = req.decide.media.media_file_id;
        let mut evicted = None;
        let (session, orphans) = {
            let mut reg = self.reg.lock().unwrap();
            // [ts:modules/streaming/live-session.service.ts:409-432]
            let sibling = reg.sessions.values().any(|s| {
                s.owner == owner
                    && s.media_file_id == file
                    && s.plan.profile_hash == plan.profile_hash
                    && !live_session_expired(
                        s.m.lock().unwrap().last_beat_ms,
                        s.pinned(),
                        now,
                        &lifetime,
                    )
            });
            let hash = if sibling {
                let alnum: String = sid
                    .chars()
                    .filter(char::is_ascii_alphanumeric)
                    .take(8)
                    .collect();
                format!("{}~i{alnum}", plan.profile_hash)
            } else {
                plan.profile_hash.clone()
            };
            if owner.is_some() {
                let own: Vec<(String, i64)> = reg
                    .sessions
                    .values()
                    .filter(|s| s.owner == owner)
                    .map(|s| (s.sid.clone(), s.m.lock().unwrap().last_beat_ms))
                    .collect();
                let beats: Vec<i64> = own.iter().map(|o| o.1).collect();
                if let Some(i) = session_to_evict(&beats, &lifetime) {
                    reg.sessions.remove(&own[i].0);
                    evicted = Some(own[i].0.clone());
                }
            }
            let s = Arc::new(Session {
                sid: sid.clone(),
                owner: owner.clone(),
                media_file_id: file,
                path: self.map_media_path(&req.decide.media.path),
                decision: decision.clone(),
                streaming: config.streaming.clone(),
                config_revision: config.revision,
                hash,
                opened_at_ms: now,
                m: Mutex::new(SessionMut {
                    last_beat_ms: now,
                    position_s: req.start_at_s.unwrap_or(0.0),
                    state: "opened".into(),
                    quality: Some(decision.response.quality.clone()),
                    last_requested_segment: None,
                    last_segment_at_ms: None,
                    served: None,
                    kind: plan.kind,
                }),
                plan,
                req,
            });
            reg.sessions.insert(sid.clone(), s.clone());
            (s, take_orphans(&mut reg))
        };
        if !orphans.is_empty() {
            tokio::spawn(kill_all(orphans));
        }
        if let Some(e) = evicted {
            self.events.emit(EngineEvent::SessionEnded {
                session_id: e,
                reason: "evicted".into(),
            });
        }
        self.events.emit(EngineEvent::SessionOpened {
            session: Box::new(self.snapshot(&session)),
        });
        if decision.response.play_method == PlayMethod::Transcode {
            let q = session.req.decide.selection.requested_quality.clone();
            self.prewarm(&session, q.as_deref(), session.req.start_at_s);
        }
        let info = &session.req.decide.info;
        let report = core_session::open_report(
            &session.req.decide,
            &decision,
            &config.streaming,
            &caps,
            session.req.burn_in.as_ref().map(|b| b.kind),
        );
        Ok(OpenSessionResponse {
            session_id: sid.clone(),
            profile_hash: (session.plan.kind != SessionKind::Directplay)
                .then(|| session.hash.clone()),
            play_path: core_session::play_path(&decision.response.play_url, &sid),
            hints: Hints {
                probe_stale: info
                    .schema_version
                    .is_none_or(|v| v < MEDIA_INFO_SCHEMA_VERSION),
                scan_missing: session.req.decide.scan.is_none(),
            },
            decision,
            playback_clock_offset_s: Some(report.playback_clock_offset_s),
            tonemap_algo: report.tonemap_algo,
            tonemap_curve: report.tonemap_curve,
            qoe_pipeline: Some(report.qoe_pipeline),
        })
    }

    /// Warms the pinned rung at the resume segment. [ts:modules/streaming/streaming.controller.ts:612-690]
    pub fn prewarm(
        self: &Arc<Self>,
        s: &Arc<Session>,
        start_quality: Option<&str>,
        start_at: Option<f64>,
    ) {
        let Some(q) = start_quality.filter(|q| !matches!(*q, "" | "auto" | "remux" | "original"))
        else {
            return;
        };
        if s.plan.kind != SessionKind::Transcode {
            return;
        }
        let v = s.req.decide.info.video.first();
        let w = v
            .and_then(|v| v.crop.as_ref().map(|c| c.width).or(v.width))
            .unwrap_or(1920);
        let h = v
            .and_then(|v| v.crop.as_ref().map(|c| c.height).or(v.height))
            .unwrap_or(1080);
        let quality = fliks_engine_core::hls::pinned_rung_name(q, w, h, s.decision.use_hdr_ladder);
        if !core_session::is_valid_quality(&quality) {
            return;
        }
        let start = seconds_to_segment(
            start_at.unwrap_or(0.0),
            s.plan.segment_duration,
            s.plan.source_fps,
        );
        let (rt, s) = (self.clone(), s.clone());
        tokio::spawn(async move {
            let key = s.job_key();
            if rt.job(&key).is_some_and(|j| j.producing()) {
                return;
            }
            // Players that fetch seg-0 on a load-then-seek get it from the early companion.
            if start > 0 && s.plan.probes_seg_zero {
                rt.early_job(&s, &quality).await;
            }
            let lock = rt.lock_for(&key);
            let _g = lock.lock().await;
            if rt.job(&key).is_none_or(|j| !j.producing()) {
                let hw = rt.detected_hw();
                rt.spawn_job(&s, &quality, start, (hw, hw), 0, JobKind::Main)
                    .await;
            }
        });
    }

    pub fn job(&self, key: &str) -> Option<Arc<Job>> {
        self.reg.lock().unwrap().jobs.get(key).cloned()
    }

    /// A live session, never a tombstone.
    pub fn peek(&self, sid: &str) -> Option<Arc<Session>> {
        self.reg.lock().unwrap().sessions.get(sid).cloned()
    }

    /// A live session, or a tombstone revived within the revive window.
    pub fn session(&self, sid: &str) -> Option<Arc<Session>> {
        let mut reg = self.reg.lock().unwrap();
        if let Some(s) = reg.sessions.get(sid) {
            return Some(s.clone());
        }
        let buried = reg.tombstones.remove(sid)?;
        let lifetime = &self.current_config().lifetime;
        if !tombstone_revivable(buried.m.lock().unwrap().last_beat_ms, now_ms(), lifetime) {
            return None;
        }
        tracing::info!(sid, "revived a session");
        reg.sessions.insert(sid.into(), buried.clone());
        Some(buried)
    }

    /// The owner's most recently active session of a file (requests without a sid).
    pub fn current_session(&self, owner: Option<&str>, file: u32) -> Option<Arc<Session>> {
        let reg = self.reg.lock().unwrap();
        reg.sessions
            .values()
            .filter(|s| s.owner.as_deref() == owner && s.media_file_id == file)
            .max_by_key(|s| s.m.lock().unwrap().last_beat_ms)
            .cloned()
    }

    pub fn touch(&self, s: &Session, segment: Option<u32>) {
        let mut m = s.m.lock().unwrap();
        m.last_beat_ms = now_ms();
        if let Some(n) = segment {
            m.last_requested_segment = Some(n);
            m.last_segment_at_ms = Some(m.last_beat_ms);
        }
    }

    pub fn heartbeat(&self, s: &Session, hb: &fliks_engine_core::api::HeartbeatRequest) {
        let mut m = s.m.lock().unwrap();
        m.last_beat_ms = now_ms();
        if let Some(p) = hb.position_s.filter(|p| p.is_finite()) {
            m.position_s = p;
        }
        if let Some(st) = &hb.state {
            m.state = st.clone();
        }
        if hb.quality.is_some() {
            m.quality = hb.quality.clone();
        }
    }

    /// A video request names what the client plays: the remux variant or a rung.
    /// [ts:modules/streaming/streaming.controller.ts:2274-2282]
    pub fn note_kind(&self, s: &Session, remux: bool) {
        s.m.lock().unwrap().kind = if remux {
            SessionKind::Remux
        } else {
            SessionKind::Transcode
        };
    }

    /// Notes the rung and hardware a segment came from; a change is a `PipelineChanged`.
    pub fn note_served(&self, s: &Session, quality: &str, hw: HwAccel) {
        let changed = {
            let mut m = s.m.lock().unwrap();
            let now = (quality.to_string(), hw);
            let changed = m.served.as_ref() != Some(&now);
            m.served = Some(now);
            m.quality = Some(quality.into());
            changed
        };
        if changed {
            self.events.emit(EngineEvent::PipelineChanged {
                session_id: s.sid.clone(),
                quality: quality.into(),
                hw_accel: hw,
            });
        }
    }

    pub fn snapshot(&self, s: &Session) -> SessionSnapshot {
        let job = self.job(&s.primary_key()).map(|j| j.stats());
        let m = s.m.lock().unwrap();
        SessionSnapshot {
            session_id: s.sid.clone(),
            owner_id: s.owner.clone(),
            media_file_id: s.media_file_id,
            profile_hash: (s.plan.kind != SessionKind::Directplay).then(|| s.hash.clone()),
            kind: m.kind,
            quality: m.quality.clone(),
            client: s.req.client.clone(),
            tags: s.req.tags.clone(),
            opened_at_ms: s.opened_at_ms as f64,
            last_beat_ms: m.last_beat_ms as f64,
            last_segment_at_ms: m.last_segment_at_ms.map(|t| t as f64),
            position_s: m.position_s,
            state: m.state.clone(),
            pinned: s.req.pinned,
            config_revision: s.config_revision,
            decision: s.decision.clone(),
            job,
        }
    }

    pub fn list(&self, owner: Option<&str>, file: Option<u32>) -> Vec<SessionSnapshot> {
        let sessions: Vec<Arc<Session>> = {
            let reg = self.reg.lock().unwrap();
            reg.sessions.values().cloned().collect()
        };
        let mut out: Vec<SessionSnapshot> = sessions
            .iter()
            .filter(|s| owner.is_none_or(|o| s.owner.as_deref() == Some(o)))
            .filter(|s| file.is_none_or(|f| s.media_file_id == f))
            .map(|s| self.snapshot(s))
            .collect();
        out.sort_by(|a, b| a.opened_at_ms.total_cmp(&b.opened_at_ms));
        out
    }

    /// Idempotent; jobs no other session owns stop before it returns. [ts:modules/streaming/live-session.service.ts:553-558]
    pub async fn stop(&self, sid: &str, reason: &str) -> bool {
        let (known, orphans) = {
            let mut reg = self.reg.lock().unwrap();
            let a = reg.sessions.remove(sid).is_some();
            let known = reg.tombstones.remove(sid).is_some() || a;
            (known, take_orphans(&mut reg))
        };
        if known {
            self.events.emit(EngineEvent::SessionEnded {
                session_id: sid.into(),
                reason: reason.into(),
            });
        }
        kill_all(orphans).await;
        known
    }

    /// Sessions matching every given filter go, with their jobs and resolver answers.
    pub async fn revoke(&self, owner: Option<&str>, files: &[u32]) -> u32 {
        let matches = |s: &Session| {
            owner.is_none_or(|o| s.owner.as_deref() == Some(o))
                && (files.is_empty() || files.contains(&s.media_file_id))
        };
        let (gone, orphans) = {
            let mut reg = self.reg.lock().unwrap();
            let gone: Vec<Arc<Session>> = reg
                .sessions
                .values()
                .filter(|s| matches(s))
                .cloned()
                .collect();
            reg.sessions.retain(|_, s| !matches(s));
            reg.tombstones.retain(|_, s| !matches(s));
            (gone, take_orphans(&mut reg))
        };
        self.resolver.purge(owner, files);
        kill_all(orphans).await;
        for s in &gone {
            self.events.emit(EngineEvent::SessionEnded {
                session_id: s.sid.clone(),
                reason: "revoked".into(),
            });
        }
        // Live viewers belong to no media file: only an owner-wide revoke reaches them.
        let live = match owner {
            Some(o) if files.is_empty() => self.live.revoke(self, o),
            _ => 0,
        };
        gone.len() as u32 + live
    }

    /// Rescan or deletion: every job of the file dies, its cache and sprite go; sessions stay.
    pub async fn invalidate(self: &Arc<Self>, file: u32) -> (u32, u32) {
        let background = crate::jobs::cancel_file(self, file).await;
        let jobs: Vec<Arc<Job>> = {
            let mut reg = self.reg.lock().unwrap();
            let keys: Vec<String> = reg
                .jobs
                .iter()
                .filter(|(_, j)| j.media_file_id == file)
                .map(|(k, _)| k.clone())
                .collect();
            keys.iter().filter_map(|k| reg.jobs.remove(k)).collect()
        };
        kill_all(jobs.clone()).await;
        self.resolver.purge(None, &[file]);
        let entries = self.scan_cache().await;
        let mut removed = 0;
        for e in entries.iter().filter(|e| e.media_file_id == file) {
            let dir = e.dir.clone();
            let _ = tokio::task::spawn_blocking(move || remove_dir(&dir)).await;
            removed += 1;
        }
        let dirs = [self.cache.subtitles_dir(file), self.cache.sprite_dir(file)];
        let _ = tokio::task::spawn_blocking(move || dirs.iter().for_each(|d| remove_dir(d))).await;
        (jobs.len() as u32 + background, removed)
    }

    pub async fn pause_job(&self, s: &Session, pause: bool) -> bool {
        match self.job(&s.primary_key()) {
            Some(j) => j.pause(self.pause_capability(), pause).await,
            None => false,
        }
    }

    // ---- jobs ----

    fn detected_hw(&self) -> HwAccel {
        self.caps().map_or(HwAccel::None, |c| c.hw_accel)
    }

    pub(crate) fn device(&self, caps: &HostCaps) -> fliks_engine_core::hw::Device {
        fliks_engine_core::hw::Device {
            platform: caps.platform,
            render_node: self.hw.snapshot().map(|h| h.render_node),
            opencl_device: std::env::var("FLIKS_OPENCL_DEVICE").ok(),
        }
    }

    /// Kills the key's previous run, purges the rung from `start` and spawns a main or early run.
    /// [ts:modules/streaming/transcoding/transcoding.service.ts:790-860]
    async fn spawn_job(
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
    async fn spawn_remux(self: &Arc<Self>, s: &Arc<Session>, start: u32) -> Option<Arc<Job>> {
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
        let replaced = self
            .reg
            .lock()
            .unwrap()
            .jobs
            .insert(key.clone(), job.clone());
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
    fn unregister(&self, job: &Arc<Job>) {
        let mut reg = self.reg.lock().unwrap();
        if reg.jobs.get(&job.key).is_some_and(|j| Arc::ptr_eq(j, job)) {
            reg.jobs.remove(&job.key);
        }
    }

    /// Kills a hung run or one past its natural end; it ends failed and the next request restarts it.
    async fn watchdog(&self, job: &Arc<Job>) {
        let (now, lines) = (now_ms(), job.proc.stderr_lines());
        let cpu = job.proc.cpu_time_ms();
        let verdict = {
            let mut m = job.m.lock().unwrap();
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
        job.m.lock().unwrap().reason = Some(reason);
        job.proc.stop().await;
    }

    /// One assembler pass off the async threads; true when it gave up.
    async fn pump(job: &Job) -> bool {
        let Some(r) = job.remux.clone() else {
            return false;
        };
        tokio::task::spawn_blocking(move || {
            let mut a = r.asm.lock().unwrap();
            a.pump();
            a.stopped()
        })
        .await
        .unwrap_or(true)
    }

    /// Follows the run's output until it exits, then falls back to the CPU when the hardware
    /// failed before the first segment. [ts:modules/streaming/transcoding/transcoding.service.ts:870-940]
    // A boxed `dyn Future`: the watcher respawns through `spawn_job`, which spawns a watcher.
    fn watch(
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
            job.slot.lock().unwrap().take();
            if let Some(r) = job.remux.clone() {
                let clean = exit.code == Some(0) && !job.killed();
                let _ =
                    tokio::task::spawn_blocking(move || r.asm.lock().unwrap().finish(clean)).await;
            }
            job.scan().await;
            job.m.lock().unwrap().pending = false;
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
                reason: job.m.lock().unwrap().reason.map(String::from),
            });
            // The replacement is registered before the waiters wake, so they move on to it.
            if state == JobState::Failed
                && !job.has(0, job.start_segment)
                && job.hw != HwAccel::None
            {
                self.clone().fall_back(&job, &s).await;
            }
            job.m.lock().unwrap().settled = true;
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
                    e.kill().await;
                }
            } else {
                let input = ResolveInput {
                    run: e.existing_run(None),
                    requested_segment: requested,
                    quality_match,
                    present: e.m.lock().unwrap().sets[0].clone(),
                };
                match core_session::resolve_existing(&input) {
                    ResumeAction::Serve | ResumeAction::Wait => {
                        return Some(e);
                    }
                    ResumeAction::Respawn(r) => {
                        if r.kill {
                            e.kill().await;
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
                    return Some(e);
                }
                ResumeAction::Respawn(r) => {
                    if r.kill {
                        e.kill().await;
                    }
                    requested = r.start_segment.unwrap_or(e.start_segment);
                }
            }
        }
        self.spawn_remux(s, requested).await
    }

    /// The early companion of `quality`: reused while it runs or once it ended cleanly, else
    /// killed, cleaned and spawned again. [ts:modules/streaming/transcoding/transcoding.service.ts:965-1060]
    async fn early_job(self: &Arc<Self>, s: &Arc<Session>, quality: &str) -> Option<Arc<Job>> {
        let key = s.key(JobKind::Early);
        let lock = self.lock_for(&key);
        let _g = lock.lock().await;
        if let Some(e) = self.job(&key) {
            let clean = e.proc.exited().is_some_and(|x| x.code == Some(0));
            if e.quality == quality && (e.producing() || clean) {
                return Some(e);
            }
            self.unregister(&e);
            e.kill().await;
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
        let path = match name {
            SegmentName::Init(_) => job.init_path(track),
            SegmentName::Segment(n) => job.segment_path(track, n),
        };
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
            return ready(j.segment_path(0, n), Some(j.clone()), 0.0, true);
        }
        if same.is_none() {
            // An earlier run of this rung left the file: serve it without spawning. [new]
            let dir = self
                .cache
                .rung_dir(s.owner.as_deref(), s.media_file_id, &s.hash, quality);
            let path = match name {
                SegmentName::Init(_) if s.plan.multi_audio => dir.join("0/init_0.mp4"),
                SegmentName::Init(_) => dir.join("init.mp4"),
                SegmentName::Segment(n) => {
                    let sub = if s.plan.multi_audio {
                        dir.join("0")
                    } else {
                        dir
                    };
                    sub.join(format!(
                        "seg-{n:04}.{}",
                        if s.plan.use_ts { "ts" } else { "m4s" }
                    ))
                }
            };
            if tokio::fs::metadata(&path).await.is_ok_and(|m| m.len() > 0) {
                return ready(path, existing, 0.0, true);
            }
        }
        // A resume's seg-0/seg-1 probe and an init go to the bounded early companion, never
        // relocating the main. [ts:modules/streaming/streaming.controller.ts:2409-2480]
        let floor = s.resume_floor(existing.as_deref());
        let probe = s.plan.probes_seg_zero
            && match name {
                SegmentName::Init(_) => same.is_none(),
                SegmentName::Segment(n) => floor > EARLY_PROBE_SEGMENTS && n < EARLY_PROBE_SEGMENTS,
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
        let path = match name {
            SegmentName::Init(_) => job.init_path(0),
            SegmentName::Segment(n) => job.segment_path(0, n),
        };
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
                let (w, h) = (
                    v.and_then(|v| v.width).unwrap_or(1920),
                    v.and_then(|v| v.height).unwrap_or(1080),
                );
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
        let path = match name {
            SegmentName::Init(_) => job.init_path(track),
            SegmentName::Segment(n) => job.segment_path(track, n),
        };
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
        let path = match (track, name) {
            (0, SegmentName::Init(_)) => dir.join("init.mp4"),
            (0, SegmentName::Segment(n)) => dir.join(format!("seg-{n:04}.m4s")),
            (t, SegmentName::Init(_)) => dir.join(format!("{t}/init_{t}.mp4")),
            (t, SegmentName::Segment(n)) => dir.join(format!("{t}/seg-{n:04}.m4s")),
        };
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

    // ---- periodic work ----

    /// Session expiry, then the jobs no session owns are stopped, every `liveSessionGcIntervalMs`.
    pub async fn gc_tick(&self) {
        let now = now_ms();
        let lifetime = &self.current_config().lifetime;
        let (expired, orphans) = {
            let mut reg = self.reg.lock().unwrap();
            let expired: Vec<String> = reg
                .sessions
                .values()
                .filter(|s| {
                    live_session_expired(
                        s.m.lock().unwrap().last_beat_ms,
                        s.pinned(),
                        now,
                        lifetime,
                    )
                })
                .map(|s| s.sid.clone())
                .collect();
            for sid in &expired {
                if let Some(s) = reg.sessions.remove(sid) {
                    reg.tombstones.insert(sid.clone(), s);
                }
            }
            reg.tombstones.retain(|_, s| {
                tombstone_revivable(s.m.lock().unwrap().last_beat_ms, now, lifetime)
            });
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
            let reg = self.reg.lock().unwrap();
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
                let m = job.m.lock().unwrap();
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
                job.m.lock().unwrap().frontier_hint = out.frontier_segment;
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

    async fn scan_cache(&self) -> Vec<crate::cache::Scanned> {
        let root = self.cache.root.clone();
        tokio::task::spawn_blocking(move || Cache { root }.scan())
            .await
            .unwrap_or_default()
    }

    /// Entry dirs a job or session uses: never evicted.
    fn live_dirs(&self) -> Vec<PathBuf> {
        let reg = self.reg.lock().unwrap();
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

    /// Boot purge, then the GC, throttle and cache loops.
    pub fn start(self: &Arc<Self>) {
        // A crashed engine's live windows must not be appended to. [ts:modules/livetv/services/livetv-session.service.ts:258-262]
        remove_dir(&crate::live::root(&self.cache));
        let rt = self.clone();
        tokio::spawn(async move {
            let root = rt.cache.root.clone();
            let _ = tokio::task::spawn_blocking(move || Cache { root }.purge_old_layout()).await;
            loop {
                let ms = rt.current_config().lifetime.resolved().cache_gc_interval_ms;
                tokio::time::sleep(Duration::from_millis(ms.into())).await;
                rt.cache_gc().await;
            }
        });
        let rt = self.clone();
        tokio::spawn(async move {
            loop {
                let ms = rt
                    .current_config()
                    .lifetime
                    .resolved()
                    .live_session_gc_interval_ms;
                tokio::time::sleep(Duration::from_millis(ms.into())).await;
                rt.gc_tick().await;
            }
        });
        let rt = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(THROTTLE_EVERY).await;
                rt.throttle_tick().await;
            }
        });
    }

    /// Kills every job and background job (engine shutdown).
    pub async fn shutdown(self: &Arc<Self>) {
        crate::jobs::shutdown(self).await;
        crate::live::shutdown(self).await;
        let jobs: Vec<Arc<Job>> = self
            .reg
            .lock()
            .unwrap()
            .jobs
            .drain()
            .map(|(_, j)| j)
            .collect();
        kill_all(jobs).await;
    }
}

/// Removes and returns the jobs no live session serves (same owner, file and profile hash).
fn take_orphans(reg: &mut Reg) -> Vec<Arc<Job>> {
    let Reg { sessions, jobs, .. } = reg;
    let keys: Vec<String> = jobs
        .iter()
        .filter(|(_, j)| !sessions.values().any(|s| j.serves(s)))
        .map(|(k, _)| k.clone())
        .collect();
    keys.iter().filter_map(|k| jobs.remove(k)).collect()
}

/// Stops every running job at once and waits for them.
async fn kill_all(jobs: Vec<Arc<Job>>) {
    let tasks: Vec<_> = jobs
        .into_iter()
        .filter(|j| j.producing())
        .map(|j| {
            tokio::spawn(async move {
                tracing::info!(key = j.key, "no session left, stopping ffmpeg (cache kept)");
                j.kill().await
            })
        })
        .collect();
    for t in tasks {
        let _ = t.await;
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
async fn finish(path: PathBuf, job: Arc<Job>, landed: bool, t0: Instant) -> Fetch {
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

/// Drops a previous run's segments at and after `start`: one timeline forward of the restart.
/// [ts:modules/streaming/transcoding/segment-utils.ts:127-170]
fn purge_from(dir: &Path, var_stream_map: bool, tracks: usize, start: u32, ext: &str) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purge_keeps_segments_before_the_start() {
        let d = std::env::temp_dir().join(format!("fliks-purge-{}", std::process::id()));
        std::fs::create_dir_all(d.join("0")).unwrap();
        for n in [
            "seg-0001.m4s",
            "seg-0005.m4s",
            "seg-0006.m4s.tmp",
            "init_0.mp4",
        ] {
            std::fs::write(d.join("0").join(n), b"x").unwrap();
        }
        purge_from(&d, true, 2, 5, "m4s");
        let mut left: Vec<String> = std::fs::read_dir(d.join("0"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["init_0.mp4", "seg-0001.m4s"]);
        assert!(d.join("1").is_dir());
        remove_dir(&d);
    }
}

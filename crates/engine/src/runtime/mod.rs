//! Sessions and their ffmpeg jobs: open, heartbeat, seek and rung switch, HW fallback, throttle,
//! idle reaping, revocation and cache eviction. Spec: `docs/spec/runtime.md` sections 2-5.

mod fetch;
mod housekeeping;
mod job;
mod session;
mod spawn;

pub use self::job::{Job, non_empty_within};
pub use self::session::Session;

use self::job::{JobMut, layout_path};
use crate::cache::{Cache, remove_dir};
use crate::events::Events;
use crate::hw::Hw;
use crate::lock::Locked;
use crate::resolve::Resolver;
use crate::slots::Slots;
use fliks_engine_core::api::{EngineConfig, HostCaps, HwAccel, LifetimeConfig};
use fliks_engine_core::session::{self as core_session, PauseCapability};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Concurrent on-demand ffmpeg runs of the data plane; more wait for a permit.
const MAX_CAPTURES: usize = 4;
/// Size of the per-key lock map past which the unused locks are dropped.
const LOCKS_PRUNE_AT: usize = 256;
const WATCH_EVERY: Duration = Duration::from_millis(200);
const THROTTLE_EVERY: Duration = Duration::from_secs(5);
const SEGMENT_WAIT: Duration = Duration::from_millis(core_session::SEGMENT_WAIT_MS as u64);
const INIT_NON_EMPTY: Duration = Duration::from_millis(core_session::INIT_NON_EMPTY_WAIT_MS as u64);

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Which run of a session a job is. [ts:modules/streaming/transcoding/variant.ts:11-27]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Main,
    /// Seg-0 and seg-1 of a resume, next to the main run.
    Early,
    Remux,
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

pub(crate) type InitTracks = (
    PathBuf,
    (SystemTime, u64),
    fliks_engine_core::mp4::IdMap<fliks_engine_core::mp4::TrackInfo>,
);

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
    /// Bounds the on-demand ffmpeg runs of the data plane (trick play, subtitle extraction).
    pub capture: tokio::sync::Semaphore,
    pub jobs: crate::jobs::Jobs,
    pub live: crate::live::Live,
    /// Per rendition directory: its init file, that file's stamp and the tracks it declares.
    pub(crate) init_tracks: Mutex<HashMap<PathBuf, InitTracks>>,
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
            capture: tokio::sync::Semaphore::new(MAX_CAPTURES),
            jobs: Default::default(),
            live: Default::default(),
            init_tracks: Mutex::new(HashMap::new()),
            reg: Mutex::new(Reg::default()),
            locks: Mutex::new(HashMap::new()),
        })
    }

    pub fn current_config(&self) -> Arc<EngineConfig> {
        self.config.locked().clone().unwrap_or_default()
    }

    /// The host's media path as this engine opens it (`EngineConfig.pathMappings`).
    pub fn map_media_path(&self, path: &str) -> String {
        fliks_engine_core::api::apply_path_mappings(&self.current_config().path_mappings, path)
    }

    fn lifetime(&self) -> LifetimeConfig {
        self.config
            .locked()
            .as_ref()
            .map_or_else(Default::default, |c| c.lifetime.clone())
    }

    pub(crate) fn lock_for(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.locked();
        if locks.len() >= LOCKS_PRUNE_AT {
            // A lock nobody else holds is recreated on demand.
            locks.retain(|_, l| Arc::strong_count(l) > 1);
        }
        locks.entry(key.into()).or_default().clone()
    }

    pub(crate) fn pause_capability(&self) -> PauseCapability {
        self.hw.snapshot().map_or(PauseCapability::None, |s| {
            PauseCapability::parse(&s.pause_capability)
        })
    }

    pub(crate) fn caps(&self) -> Option<HostCaps> {
        self.hw.snapshot().map(|s| s.host_caps())
    }

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
        let jobs: Vec<Arc<Job>> = self.reg.locked().jobs.drain().map(|(_, j)| j).collect();
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

#[cfg(test)]
mod tests {
    use super::spawn::purge_from;
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

    #[tokio::test]
    async fn lock_map_drops_the_locks_nobody_holds() {
        let hw = Hw::new(Arc::new(|_| Box::pin(async { unreachable!() })));
        let rt = Runtime::new(hw, Cache::from_env(), "ffmpeg".into());
        let held = rt.lock_for("held");
        for i in 0..LOCKS_PRUNE_AT * 2 {
            rt.lock_for(&i.to_string());
        }
        assert!(rt.locks.locked().len() <= LOCKS_PRUNE_AT);
        assert!(Arc::ptr_eq(&held, &rt.lock_for("held")));
    }
}

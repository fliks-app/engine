//! Host capability detection at boot and on a GPU re-pin: the single source of `Capabilities`
//! and the `HostCaps` the decision reads. Pure logic lives in `fliks_engine_core::hw`.
mod io;

pub use io::{ProcRunner, enumerate_gpus, probe_stdin_pause};

use crate::lock::Locked;
use fliks_engine_core::api::{Capabilities, HostCaps};
use fliks_engine_core::hw::{
    DEFAULT_RENDER_NODE, Detected, Device, GpuInfo, detect, pause_capability, platform_from_os,
};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// One finished detection run.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub detected: Detected,
    pub gpus: Vec<GpuInfo>,
    pub pause_capability: String,
    /// Where `auto` resolves: the pinned node or the default.
    pub render_node: String,
}

impl Snapshot {
    pub fn capabilities(&self) -> Capabilities {
        self.detected
            .capabilities(&self.gpus, &self.render_node, &self.pause_capability)
    }

    pub fn host_caps(&self) -> HostCaps {
        self.detected.host_caps()
    }
}

/// Runs the whole detection for a pinned render node (`None` = auto).
pub type Detector =
    Arc<dyn Fn(Option<String>) -> Pin<Box<dyn Future<Output = Snapshot> + Send>> + Send + Sync>;

#[derive(Default)]
struct State {
    current: Option<Snapshot>,
    /// Latest node the host asked for.
    node: Option<String>,
    running: bool,
    rerun: bool,
}

/// Holds the latest detection. `None` until the first run ends; a re-run keeps serving the old one.
pub struct Hw {
    detector: Detector,
    state: Mutex<State>,
}

/// The binary named by env `var`, else `default` on PATH.
pub fn tool_path(var: &str, default: &str) -> String {
    std::env::var(var)
        .ok()
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| default.into())
}

/// `FFMPEG_PATH` or `ffmpeg` on PATH.
pub fn ffmpeg_path() -> String {
    tool_path("FFMPEG_PATH", "ffmpeg")
}

/// The real detection: ffmpeg runs, `/dev/dri` + sysfs, and the pause probe (node-independent, run once).
pub fn ffmpeg_detector(ffmpeg: String) -> Detector {
    let pause: Arc<tokio::sync::OnceCell<bool>> = Arc::new(tokio::sync::OnceCell::new());
    Arc::new(move |node| {
        let (ffmpeg, pause) = (ffmpeg.clone(), pause.clone());
        Box::pin(async move {
            let platform = platform_from_os(std::env::consts::OS);
            let dev = Device {
                platform,
                render_node: node.clone(),
                opencl_device: std::env::var("FLIKS_OPENCL_DEVICE").ok(),
            };
            let runner = ProcRunner {
                ffmpeg: ffmpeg.clone(),
            };
            let tmp = std::env::temp_dir().to_string_lossy().into_owned();
            let (detected, stdin) = tokio::join!(
                detect(&dev, &tmp, std::process::id(), &runner),
                pause.get_or_init(|| probe_stdin_pause(&ffmpeg))
            );
            Snapshot {
                pause_capability: pause_capability(*stdin, platform).into(),
                gpus: enumerate_gpus(Path::new("/dev/dri"), Path::new("/sys/class/drm")),
                render_node: node.unwrap_or_else(|| DEFAULT_RENDER_NODE.into()),
                detected,
            }
        })
    })
}

impl Hw {
    /// Idle: nothing detected until [`Hw::start`].
    pub fn new(detector: Detector) -> Arc<Self> {
        Arc::new(Self {
            detector,
            state: Mutex::new(State::default()),
        })
    }

    /// Already detected, never re-runs on its own (tests).
    pub fn fixed(snapshot: Snapshot) -> Arc<Self> {
        let hw = Self::new(Arc::new(|_| unreachable!("fixed host caps")));
        hw.state.locked().current = Some(snapshot);
        hw
    }

    pub fn snapshot(&self) -> Option<Snapshot> {
        self.state.locked().current.clone()
    }

    /// Boot detection in the background.
    pub fn start(self: &Arc<Self>) {
        self.schedule();
    }

    /// The host changed `gpuRenderNode`: detect again on the new node unless it is the one in use.
    pub fn set_render_node(self: &Arc<Self>, node: Option<String>) {
        {
            let mut s = self.state.locked();
            if s.node == node {
                return;
            }
            s.node = node;
        }
        self.schedule();
    }

    /// One run at a time; requests during a run coalesce into one follow-up on the latest node.
    fn schedule(self: &Arc<Self>) {
        {
            let mut s = self.state.locked();
            if s.running {
                s.rerun = true;
                return;
            }
            s.running = true;
        }
        let hw = self.clone();
        tokio::spawn(async move {
            loop {
                let node = {
                    let mut s = hw.state.locked();
                    s.rerun = false;
                    s.node.clone()
                };
                let snap = (hw.detector)(node).await;
                tracing::info!(
                    hw_accel = ?snap.detected.hw_accel,
                    encoders = snap.detected.encoders.len(),
                    decoders = snap.detected.decoders.len(),
                    probes = ?snap.detected.probes,
                    "host capabilities detected"
                );
                let mut s = hw.state.locked();
                s.current = Some(snap);
                if !s.rerun {
                    s.running = false;
                    return;
                }
            }
        });
    }
}

//! Host capability detection at boot and on a GPU re-pin: the single source of `Capabilities`
//! and the `HostCaps` the decision reads. Pure logic lives in `fliks_engine_core::hw`.
mod io;

pub use io::{ProcRunner, enumerate_gpus, probe_stdin_pause};

use crate::lock::Locked;
use fliks_engine_core::api::{Capabilities, HostCaps, HwAccel};
use fliks_engine_core::hw::{
    DEFAULT_RENDER_NODE, Detected, Device, Found, GpuInfo, assemble, cuda_index, detect_only,
    pause_capability, platform_from_os,
};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// One hardware device (`gpu0`, `gpu1`, ...) probed on its own render node.
#[derive(Debug, Clone)]
pub struct HwDevice {
    pub id: String,
    pub render_node: String,
    pub name: Option<String>,
    /// CUDA ordinal on a host with several NVIDIA cards.
    pub cuda_index: Option<u32>,
    pub detected: Detected,
}

impl HwDevice {
    /// What the argv builders vary on for a run placed on this device.
    pub fn device(&self) -> Device {
        Device {
            platform: self.detected.platform,
            render_node: Some(self.render_node.clone()),
            opencl_device: std::env::var("FLIKS_OPENCL_DEVICE").ok(),
            cuda_index: self.cuda_index,
        }
    }
}

/// One finished detection run. `detected` and `render_node` are `devices[0]`'s: what the playback
/// decision reads.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub detected: Detected,
    pub gpus: Vec<GpuInfo>,
    pub pause_capability: String,
    /// Where `auto` resolves: the pinned node or the default.
    pub render_node: String,
    /// Devices with a hardware encoder; empty on a CPU-only host.
    pub devices: Vec<HwDevice>,
}

impl Snapshot {
    pub fn capabilities(&self) -> Capabilities {
        let mut c =
            self.detected
                .capabilities(&self.gpus, &self.render_node, &self.pause_capability);
        c.devices = self
            .devices
            .iter()
            .map(|d| {
                d.detected
                    .device_caps(&d.id, &d.render_node, d.name.clone())
            })
            .collect();
        c
    }

    pub fn device(&self, id: &str) -> Option<&HwDevice> {
        self.devices.iter().find(|d| d.id == id)
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
            let runner = ProcRunner {
                ffmpeg: ffmpeg.clone(),
            };
            let tmp = std::env::temp_dir().to_string_lossy().into_owned();
            let gpus = enumerate_gpus(Path::new("/dev/dri"), Path::new("/sys/class/drm"));
            // Probes of different nodes run at once: each takes its own sample file names.
            let probe = |n: u32, node: Option<String>, only: Option<HwAccel>, cuda: Option<u32>| {
                let (tmp, runner) = (&tmp, &runner);
                async move {
                    let dev = Device {
                        platform,
                        render_node: node.clone(),
                        opencl_device: std::env::var("FLIKS_OPENCL_DEVICE").ok(),
                        cuda_index: cuda,
                    };
                    let detected =
                        detect_only(&dev, tmp, std::process::id().wrapping_add(n), runner, only)
                            .await;
                    Found {
                        render_node: node.unwrap_or_else(|| DEFAULT_RENDER_NODE.into()),
                        detected,
                        cuda_index: cuda,
                    }
                }
            };
            let (mut found, stdin) = tokio::join!(
                async {
                    if node.is_some() || gpus.is_empty() {
                        return vec![probe(0, node.clone(), None, None).await];
                    }
                    // NVIDIA cards test NVENC only (and any other node never reports it);
                    // the other nodes get the whole chain.
                    let mut all =
                        futures_util::future::join_all(gpus.iter().enumerate().map(|(i, g)| {
                            let nvidia = g.vendor == "nvidia";
                            probe(
                                i as u32,
                                Some(g.render_node.clone()),
                                nvidia.then_some(HwAccel::Nvenc),
                                nvidia.then(|| cuda_index(&gpus, &g.render_node)).flatten(),
                            )
                        }))
                        .await;
                    for f in &mut all {
                        let on_nvidia = gpus
                            .iter()
                            .any(|g| g.render_node == f.render_node && g.vendor == "nvidia");
                        if f.detected.hw_accel == HwAccel::Nvenc && !on_nvidia {
                            f.detected.hw_accel = HwAccel::None;
                        }
                    }
                    // An NVIDIA card with no render node (`runtime: nvidia`): one node-less test.
                    if !gpus.iter().any(|g| g.vendor == "nvidia") {
                        all.push(probe(gpus.len() as u32, None, Some(HwAccel::Nvenc), None).await);
                    }
                    all
                },
                pause.get_or_init(|| probe_stdin_pause(&ffmpeg))
            );
            let fallback = found[0].detected.clone();
            found.retain(|f| f.detected.hw_accel != HwAccel::None);
            let devices: Vec<HwDevice> = assemble(&gpus, found)
                .into_iter()
                .map(|d| HwDevice {
                    id: d.id,
                    render_node: d.render_node,
                    name: d.name,
                    cuda_index: d.cuda_index,
                    detected: d.detected,
                })
                .collect();
            let (detected, render_node) = devices.first().map_or_else(
                || {
                    let primary = node.clone().unwrap_or_else(|| DEFAULT_RENDER_NODE.into());
                    (fallback, primary)
                },
                |d| (d.detected.clone(), d.render_node.clone()),
            );
            Snapshot {
                pause_capability: pause_capability(*stdin, platform).into(),
                gpus,
                render_node,
                detected,
                devices,
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
                    devices = snap.devices.len(),
                    decoders = snap.detected.decoders.len(),
                    "host capabilities detected"
                );
                for d in &snap.devices {
                    tracing::info!(
                        id = d.id,
                        node = d.render_node,
                        hw_accel = ?d.detected.hw_accel,
                        encoders = d.detected.encoders.len(),
                        probes = ?d.detected.probes,
                        "hardware device"
                    );
                }
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

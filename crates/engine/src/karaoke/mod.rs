//! Karaoke: the model manager, the ONNX runtime behind a cargo feature, and the `Separate`,
//! `Align` and `Melody` jobs. Spec: `docs/spec/karaoke.md`.
mod align;
mod melody;
pub mod models;
mod onnx;
mod run;
mod separate;

pub use align::run as run_align;
pub use melody::run as run_melody;
pub use separate::run as run_separate;

use crate::AppState;
use crate::events::Events;
use crate::lock::Locked;
use axum::{
    Json,
    extract::{Path as UrlPath, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use fliks_engine_core::api::{KaraokeRuntime, ModelInfo, ModelsResponse};
use models::Manager;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::OnceCell;

/// A tensor of `f32`, what a network takes and gives.
pub struct Tensor {
    pub shape: Vec<i64>,
    pub data: Vec<f32>,
}

/// One loaded network.
pub trait Net: Send {
    fn run(&mut self, inputs: Vec<(&'static str, Tensor)>) -> Result<Vec<Tensor>, String>;
}

pub struct Loaded {
    pub net: Box<dyn Net>,
    /// `cuda`, `openvino` or `cpu`.
    pub provider: String,
}

/// What can run ONNX files.
pub trait Backend: Send + Sync {
    fn runtime(&self) -> BackendInfo;
    fn load(&self, file: &Path) -> Result<Loaded, String>;
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct BackendInfo {
    pub onnx: bool,
    pub error: Option<String>,
    /// Hardware execution providers available (`cuda`, `openvino`); the CPU is always there.
    pub providers: Vec<String>,
}

/// Tools the jobs spawn, looked up once.
pub struct Tools {
    /// The speech recogniser `Align` runs.
    pub whisper: Option<String>,
    /// An ffmpeg with the `rubberband` filter.
    pub pitch_ffmpeg: Option<String>,
}

struct Parts {
    models: Manager,
    backend: Arc<dyn Backend>,
}

pub struct Karaoke {
    parts: Mutex<Parts>,
    tools: OnceCell<Tools>,
}

fn on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| {
        std::env::split_paths(&p)
            .any(|d| d.join(name).is_file() || d.join(format!("{name}.exe")).is_file())
    })
}

fn exists(tool: &str) -> bool {
    if tool.contains(['/', '\\']) {
        Path::new(tool).is_file()
    } else {
        on_path(tool)
    }
}

/// Whether `ffmpeg` has the `rubberband` filter.
async fn has_rubberband(ffmpeg: &str) -> bool {
    let out = tokio::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-filters"])
        .stdin(std::process::Stdio::null())
        .output()
        .await;
    out.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(" rubberband "))
}

/// Threads a model run takes: `FLIKS_ORT_THREADS`, else half the cores within 2 and 8.
pub fn threads() -> usize {
    std::env::var("FLIKS_ORT_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| {
            let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
            (cores / 2).clamp(2, 8)
        })
}

pub fn models_dir(cache_root: &Path) -> PathBuf {
    std::env::var("FLIKS_MODELS_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map_or_else(|| cache_root.join("models"), PathBuf::from)
}

impl Karaoke {
    pub fn new(dir: PathBuf, events: Events) -> Self {
        Self {
            parts: Mutex::new(Parts {
                models: Manager::new(dir, models::manifest(), events),
                backend: onnx::default_backend(),
            }),
            tools: OnceCell::new(),
        }
    }

    /// Swaps the model manager and the network backend (tests).
    pub fn replace(&self, models: Manager, backend: Arc<dyn Backend>) {
        *self.parts.locked() = Parts { models, backend };
    }

    /// Fixes the tools the jobs find (tests); a no-op once they were looked up.
    pub fn set_tools(&self, tools: Tools) {
        let _ = self.tools.set(tools);
    }

    pub fn models(&self) -> Manager {
        self.parts.locked().models.clone()
    }

    pub fn backend(&self) -> Arc<dyn Backend> {
        self.parts.locked().backend.clone()
    }

    pub async fn tools(&self, ffmpeg: &str) -> &Tools {
        self.tools
            .get_or_init(|| async {
                let whisper = crate::tool_path("WHISPER_CLI_PATH", "whisper-cli");
                let mut pitch_ffmpeg = None;
                for c in [
                    crate::tool_path("FFMPEG_PITCH_PATH", "ffmpeg-pitch"),
                    ffmpeg.to_string(),
                ] {
                    if exists(&c) && has_rubberband(&c).await {
                        pitch_ffmpeg = Some(c);
                        break;
                    }
                }
                Tools {
                    whisper: exists(&whisper).then_some(whisper),
                    pitch_ffmpeg,
                }
            })
            .await
    }

    pub async fn runtime(&self, ffmpeg: &str) -> KaraokeRuntime {
        let b = self.backend().runtime();
        let t = self.tools(ffmpeg).await;
        let mut providers = b.providers;
        providers.push("cpu".into());
        KaraokeRuntime {
            onnx: b.onnx,
            onnx_error: b.error,
            providers,
            whisper: t.whisper.is_some(),
            rubberband: t.pitch_ffmpeg.is_some(),
        }
    }

    /// The entries of `hello.features` this build and host add.
    pub async fn features(&self, ffmpeg: &str) -> Vec<String> {
        let r = self.runtime(ffmpeg).await;
        let mut f = vec!["models".to_string()];
        if r.onnx {
            f.extend(["onnx", "karaoke-separate", "karaoke-melody"].map(String::from));
            for p in ["cuda", "openvino"] {
                if r.providers.iter().any(|x| x == p) {
                    f.push(format!("onnx-{p}"));
                }
            }
        }
        if r.whisper {
            f.push("karaoke-align".into());
        }
        if r.rubberband {
            f.push("pitch-shift".into());
        }
        f
    }
}

type S = State<Arc<AppState>>;

#[utoipa::path(get, path = "/v1/models", operation_id = "list_models", responses(
    (status = 200, body = ModelsResponse, description = "the manifest with each model's state, and what this engine can run")))]
pub async fn list(State(s): S) -> Json<ModelsResponse> {
    let k = &s.rt.karaoke;
    let models = k.models();
    Json(ModelsResponse {
        runtime: k.runtime(&s.rt.ffmpeg).await,
        models_dir: models.dir().to_string_lossy().into_owned(),
        models: models.list(),
    })
}

#[utoipa::path(post, path = "/v1/models/{id}/download", operation_id = "download_model", responses(
    (status = 200, body = ModelInfo, description = "installed already"),
    (status = 202, body = ModelInfo, description = "downloading (follow it with `GET /v1/models` or the `ModelChanged` events)"),
    (status = 404, description = "unknown model")))]
pub async fn download(State(s): S, UrlPath(id): UrlPath<String>) -> Response {
    match s.rt.karaoke.models().start(&id) {
        None => StatusCode::NOT_FOUND.into_response(),
        Some(m) if m.state == fliks_engine_core::api::ModelState::Installed => {
            Json(m).into_response()
        }
        Some(m) => (StatusCode::ACCEPTED, Json(m)).into_response(),
    }
}

#[utoipa::path(delete, path = "/v1/models/{id}", operation_id = "delete_model", responses(
    (status = 204, description = "removed, a download stopped"),
    (status = 404, description = "unknown model")))]
pub async fn delete(State(s): S, UrlPath(id): UrlPath<String>) -> StatusCode {
    if s.rt.karaoke.models().delete(&id).await {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

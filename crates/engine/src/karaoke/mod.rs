//! Karaoke: the model manager, the ONNX runtime behind a cargo feature, and the `Separate`,
//! `Align` and `Melody` jobs. Spec: `docs/spec/karaoke.md`.
mod align;
mod melody;
pub mod models;
mod onnx;
mod run;
mod separate;

pub use align::{run as run_align, whisper_argv};
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
    /// One of `PROVIDERS`, or `cpu`.
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
    /// Hardware execution providers available (`PROVIDERS`); the CPU is always there.
    pub providers: Vec<String>,
}

/// Hardware execution providers in the order a session tries them.
pub const PROVIDERS: [&str; 4] = ["cuda", "migraphx", "openvino", "coreml"];

/// Providers sessions never use: OpenVINO's GPU gives wrong Spleeter output (see the spec).
pub const UNUSED: [&str; 1] = ["openvino"];

/// The first provider that gives a net, else the CPU's; the name says which ran.
pub fn first_working<T>(
    providers: &[String],
    mut try_provider: impl FnMut(&str) -> Result<T, String>,
    cpu: impl FnOnce() -> Result<T, String>,
) -> Result<(T, String), String> {
    for name in providers.iter().filter(|p| !UNUSED.contains(&p.as_str())) {
        match try_provider(name) {
            Ok(net) => return Ok((net, name.clone())),
            Err(e) => tracing::warn!(provider = name, "execution provider unusable: {e}"),
        }
    }
    Ok((cpu()?, "cpu".into()))
}

/// `name` in the `karaoke/` directory next to the executable, where the desktop bundles keep
/// their tools and the ONNX library.
pub fn bundled(name: &str) -> Option<String> {
    let dir = std::env::current_exe().ok()?.parent()?.join("karaoke");
    beside(&dir, name)
}

fn beside(dir: &Path, name: &str) -> Option<String> {
    let p = dir.join(name);
    p.is_file().then(|| p.to_string_lossy().into_owned())
}

fn tool(var: &str, name: &str) -> String {
    let exe = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    std::env::var(var)
        .ok()
        .filter(|p| !p.trim().is_empty())
        .or_else(|| bundled(&exe))
        .unwrap_or_else(|| name.into())
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
                let whisper = tool("WHISPER_CLI_PATH", "whisper-cli");
                let mut pitch_ffmpeg = None;
                for c in [
                    tool("FFMPEG_PITCH_PATH", "ffmpeg-pitch"),
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
            for p in PROVIDERS {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_session_takes_the_first_provider_that_works_and_reports_it() {
        let got = first_working(
            &names(&["cuda", "migraphx", "coreml"]),
            |p| {
                if p == "cuda" {
                    Err("no".into())
                } else {
                    Ok(p.len())
                }
            },
            || Ok(0),
        );
        assert_eq!(got, Ok((8, "migraphx".to_string())));
    }

    #[test]
    fn a_session_falls_back_to_the_cpu_and_reports_cpu() {
        let got = first_working(&names(&["coreml"]), |_| Err::<u8, _>("no".into()), || Ok(1));
        assert_eq!(got, Ok((1, "cpu".to_string())));
        let none = first_working(&[], |_| Ok(2), || Ok(1));
        assert_eq!(none, Ok((1, "cpu".to_string())));
        let broken = first_working(
            &names(&["cuda"]),
            |_| Err::<u8, _>("no".into()),
            || Err("cpu too".into()),
        );
        assert_eq!(broken, Err("cpu too".to_string()));
    }

    #[test]
    fn an_unused_provider_is_listed_but_never_tried() {
        let got = first_working(&names(&["openvino"]), |_| Ok::<u8, String>(9), || Ok(1));
        assert_eq!(got, Ok((1, "cpu".to_string())));
    }

    #[test]
    fn providers_are_tried_cuda_then_migraphx_then_openvino() {
        assert_eq!(&PROVIDERS[..3], ["cuda", "migraphx", "openvino"]);
    }

    #[test]
    fn the_bundle_directory_is_searched_for_a_tool() {
        let d = std::env::temp_dir().join(format!("fliks-bundle-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("whisper-cli"), b"x").unwrap();
        assert!(beside(&d, "whisper-cli").is_some_and(|p| p.ends_with("whisper-cli")));
        assert_eq!(beside(&d, "ffmpeg-pitch"), None);
        std::fs::remove_dir_all(&d).unwrap();
    }

    struct Gpus;

    impl Backend for Gpus {
        fn runtime(&self) -> BackendInfo {
            BackendInfo {
                onnx: true,
                error: None,
                providers: names(&["migraphx", "coreml"]),
            }
        }

        fn load(&self, _: &Path) -> Result<Loaded, String> {
            Err("unused".into())
        }
    }

    #[tokio::test]
    async fn features_name_each_provider_the_library_has() {
        let dir = std::env::temp_dir().join(format!("fliks-feat-{}", std::process::id()));
        let k = Karaoke::new(dir.clone(), Events::default());
        k.replace(
            Manager::new(dir, models::manifest(), Events::default()),
            Arc::new(Gpus),
        );
        let f = k.features("no-such-ffmpeg").await;
        assert!(f.contains(&"onnx-migraphx".to_string()) && f.contains(&"onnx-coreml".to_string()));
        assert!(!f.contains(&"onnx-cuda".to_string()) && !f.contains(&"onnx-openvino".to_string()));
        assert_eq!(
            k.runtime("no-such-ffmpeg").await.providers,
            names(&["migraphx", "coreml", "cpu"])
        );
    }
}

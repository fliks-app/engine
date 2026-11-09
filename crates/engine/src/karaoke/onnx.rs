//! The ONNX runtime: `ort` with the library loaded at run time, in the builds that enable the
//! `karaoke` feature; elsewhere a backend that says it is absent.
use super::Backend;
use std::sync::Arc;

#[cfg_attr(not(feature = "karaoke"), allow(unused_variables))]
pub fn default_backend(models_dir: &std::path::Path) -> Arc<dyn Backend> {
    #[cfg(feature = "karaoke")]
    return Arc::new(imp::Ort::new(models_dir));
    #[cfg(not(feature = "karaoke"))]
    return Arc::new(Absent);
}

#[cfg(feature = "karaoke")]
pub use imp::{environment, providers, with_provider};

#[cfg(not(feature = "karaoke"))]
struct Absent;

#[cfg(not(feature = "karaoke"))]
impl Backend for Absent {
    fn runtime(&self) -> super::BackendInfo {
        super::BackendInfo {
            onnx: false,
            error: Some("this engine build has no ONNX runtime".into()),
            providers: vec![],
            windows_ml: None,
        }
    }

    fn load(
        &self,
        _: &std::path::Path,
        _: Option<&[String]>,
        _: &[super::Warm],
    ) -> Result<super::Loaded, String> {
        Err("this engine build has no ONNX runtime".into())
    }
}

#[cfg(feature = "karaoke")]
mod imp {
    use crate::karaoke::winml;
    use crate::karaoke::{
        Backend, BackendInfo, Loaded, Net, PROVIDERS, Tensor, Warm, first_working, warm_up,
    };
    use ort::{
        environment::Environment,
        ep::{self, ExecutionProvider},
        session::{Session, builder::SessionBuilder},
        value::Tensor as OrtTensor,
    };
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// ONNX Runtime from `ORT_DYLIB_PATH`, else the Windows ML runtime of the machine, else the bundle's,
    /// else `libonnxruntime` on the loader's path.
    pub struct Ort {
        env: OnceLock<Result<Environment, String>>,
        /// The environment runs the Windows ML runtime.
        system: AtomicBool,
        preparing: AtomicBool,
        models_dir: PathBuf,
    }

    /// Providers `ort` has a typed registration for (the runtime lists them when it has them).
    fn classic(name: &str) -> bool {
        match name {
            "cuda" => ep::CUDA::is_available(),
            "migraphx" => ep::MIGraphX::is_available(),
            "openvino" => ep::OpenVINO::is_available(),
            "coreml" => ep::CoreML::is_available(),
            "directml" => ep::DirectML::is_available(),
            _ => Ok(false),
        }
        .unwrap_or(false)
    }

    fn available(env: &Environment, name: &str) -> bool {
        winml::device(env, name).is_some() || classic(name)
    }

    pub fn with_provider(
        b: SessionBuilder,
        env: &Environment,
        name: &str,
    ) -> Result<SessionBuilder, String> {
        if let Some(d) = winml::device(env, name) {
            return b.with_devices([d], None).map_err(|e| e.to_string());
        }
        let p = match name {
            "cuda" => ep::CUDA::default().with_tf32(false).build(),
            "migraphx" => ep::MIGraphX::default().build(),
            "openvino" => ep::OpenVINO::default().with_device_type("GPU").build(),
            "directml" => ep::DirectML::default()
                .with_performance_preference(ep::directml::PerformancePreference::HighPerformance)
                .build(),
            _ => ep::CoreML::default().build(),
        };
        b.with_execution_providers([p.error_on_failure()])
            .map_err(|e| e.to_string())
    }

    fn overridden() -> Option<String> {
        std::env::var("ORT_DYLIB_PATH")
            .ok()
            .filter(|v| !v.is_empty())
    }

    /// The libraries to try in order, each with whether it is the Windows ML runtime's.
    fn libraries() -> Vec<(String, bool)> {
        if let Some(lib) = overridden() {
            return vec![(lib, false)];
        }
        let lib = format!(
            "{}onnxruntime{}",
            std::env::consts::DLL_PREFIX,
            std::env::consts::DLL_SUFFIX
        );
        let mut all: Vec<_> = winml::system()
            .map(|s| (s.onnxruntime(), true))
            .into_iter()
            .collect();
        all.push((crate::karaoke::bundled(&lib).unwrap_or(lib), false));
        all
    }

    /// An ONNX Runtime environment on the first library that loads, and whether that is the Windows
    /// ML runtime; with `download`, Windows fetches the execution providers it lacks.
    pub fn environment(download: bool) -> Result<(Environment, bool), String> {
        let mut last = String::new();
        for (lib, system) in libraries() {
            match ort::init_from(&lib) {
                Ok(b) => {
                    let env = b.build().map_err(|e| format!("start ONNX Runtime: {e}"))?;
                    if system {
                        winml::register(&env, download);
                    }
                    return Ok((env, system));
                }
                Err(e) => {
                    last = format!("load {lib}: {e}");
                    tracing::warn!("{last}");
                }
            }
        }
        Err(last)
    }

    impl Ort {
        pub fn new(models_dir: &Path) -> Self {
            Self {
                env: OnceLock::new(),
                system: AtomicBool::new(false),
                preparing: AtomicBool::new(false),
                models_dir: models_dir.to_owned(),
            }
        }

        fn windows_ml(&self) -> Option<fliks_engine_core::api::WindowsMl> {
            winml::status(self.system.load(Ordering::SeqCst), overridden().is_some())
        }

        fn env(&self) -> &Result<Environment, String> {
            self.env.get_or_init(|| {
                migraphx_cache(&self.models_dir);
                let (env, system) = environment(false)?;
                self.system.store(system, Ordering::SeqCst);
                Ok(env)
            })
        }
    }

    /// MIGraphX compiles a network per run shape: keep the programs on the models volume.
    fn migraphx_cache(models_dir: &Path) {
        const VAR: &str = "ORT_MIGRAPHX_MODEL_CACHE_PATH";
        if std::env::var_os(VAR).is_some_and(|v| !v.is_empty()) {
            return;
        }
        let dir = models_dir.join(".migraphx");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!("MIGraphX program cache {}: {e}", dir.display());
            return;
        }
        // Runs before the library loads, so no ONNX Runtime thread reads the environment yet.
        unsafe { std::env::set_var(VAR, dir) };
    }

    pub fn providers(env: &Environment) -> Vec<String> {
        PROVIDERS
            .iter()
            .filter(|p| available(env, p))
            .map(|p| p.to_string())
            .collect()
    }

    fn session(env: &Environment, file: &Path, provider: Option<&str>) -> Result<Session, String> {
        let mut b = Session::builder(env).map_err(|e| e.to_string())?;
        b = b
            .with_intra_threads(crate::karaoke::threads())
            .map_err(|e| e.to_string())?;
        if let Some(p) = provider {
            b = with_provider(b, env, p)?;
        }
        b.commit_from_file(file).map_err(|e| e.to_string())
    }

    impl Backend for Ort {
        fn runtime(&self) -> BackendInfo {
            match self.env() {
                Ok(env) => BackendInfo {
                    onnx: true,
                    error: None,
                    providers: providers(env),
                    windows_ml: self.windows_ml(),
                },
                Err(e) => BackendInfo {
                    onnx: false,
                    error: Some(e.clone()),
                    providers: vec![],
                    windows_ml: self.windows_ml(),
                },
            }
        }

        fn prepare(&self) {
            let Ok(env) = self.env() else { return };
            if !self.system.load(Ordering::SeqCst) || self.preparing.swap(true, Ordering::SeqCst) {
                return;
            }
            let env = env.clone();
            std::thread::spawn(move || winml::register(&env, true));
        }

        fn load(
            &self,
            file: &Path,
            allow: Option<&[String]>,
            warm: &[Warm],
        ) -> Result<Loaded, String> {
            let env = self.env().as_ref().map_err(Clone::clone)?;
            let (net, provider) = first_working(
                &providers(env),
                allow,
                |name| warm_up(OrtNet(session(env, file, Some(name))?), warm),
                || session(env, file, None).map(OrtNet),
            )?;
            Ok(Loaded {
                net: Box::new(net),
                provider,
            })
        }
    }

    struct OrtNet(Session);

    impl Net for OrtNet {
        fn run(&mut self, inputs: Vec<(&'static str, Tensor)>) -> Result<Vec<Tensor>, String> {
            let mut given = Vec::with_capacity(inputs.len());
            for (name, t) in inputs {
                let v = OrtTensor::from_array((t.shape, t.data)).map_err(|e| e.to_string())?;
                given.push((name, v));
            }
            let out = self.0.run(given).map_err(|e| e.to_string())?;
            out.iter()
                .map(|(_, v)| {
                    // SwiftF0 answers doubles, Spleeter floats.
                    if let Ok((shape, data)) = v.try_extract_tensor::<f32>() {
                        return Ok(Tensor {
                            shape: shape.iter().copied().collect(),
                            data: data.to_vec(),
                        });
                    }
                    let (shape, data) = v.try_extract_tensor::<f64>().map_err(|e| e.to_string())?;
                    Ok(Tensor {
                        shape: shape.iter().copied().collect(),
                        data: data.iter().map(|x| *x as f32).collect(),
                    })
                })
                .collect()
        }
    }
}

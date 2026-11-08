//! The ONNX runtime: `ort` with the library loaded at run time, in the builds that enable the
//! `karaoke` feature; elsewhere a backend that says it is absent.
use super::Backend;
use std::sync::Arc;

pub fn default_backend() -> Arc<dyn Backend> {
    #[cfg(feature = "karaoke")]
    return Arc::new(imp::Ort::default());
    #[cfg(not(feature = "karaoke"))]
    return Arc::new(Absent);
}

#[cfg(feature = "karaoke")]
pub use imp::{providers, with_provider};

#[cfg(not(feature = "karaoke"))]
struct Absent;

#[cfg(not(feature = "karaoke"))]
impl Backend for Absent {
    fn runtime(&self) -> super::BackendInfo {
        super::BackendInfo {
            onnx: false,
            error: Some("this engine build has no ONNX runtime".into()),
            providers: vec![],
        }
    }

    fn load(&self, _: &std::path::Path, _: Option<&[String]>) -> Result<super::Loaded, String> {
        Err("this engine build has no ONNX runtime".into())
    }
}

#[cfg(feature = "karaoke")]
mod imp {
    use crate::karaoke::winml;
    use crate::karaoke::{Backend, BackendInfo, Loaded, Net, PROVIDERS, Tensor, first_working};
    use ort::{
        environment::Environment,
        ep::{self, ExecutionProvider},
        session::{Session, builder::SessionBuilder},
        value::Tensor as OrtTensor,
    };
    use std::path::Path;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// ONNX Runtime from `ORT_DYLIB_PATH`, else the bundle's, else `libonnxruntime` on the loader's path.
    #[derive(Default)]
    pub struct Ort {
        env: OnceLock<Result<Environment, String>>,
        preparing: AtomicBool,
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
            "cuda" => ep::CUDA::default().build(),
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

    fn library() -> String {
        let lib = format!(
            "{}onnxruntime{}",
            std::env::consts::DLL_PREFIX,
            std::env::consts::DLL_SUFFIX
        );
        std::env::var("ORT_DYLIB_PATH")
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| crate::karaoke::bundled(&lib))
            .unwrap_or(lib)
    }

    impl Ort {
        fn env(&self) -> &Result<Environment, String> {
            self.env.get_or_init(|| {
                let lib = library();
                let env = ort::init_from(&lib)
                    .map_err(|e| format!("load {lib}: {e}"))?
                    .build()
                    .map_err(|e| format!("start ONNX Runtime: {e}"))?;
                winml::register(&env, false);
                Ok(env)
            })
        }
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
                },
                Err(e) => BackendInfo {
                    onnx: false,
                    error: Some(e.clone()),
                    providers: vec![],
                },
            }
        }

        fn prepare(&self) {
            let Ok(env) = self.env() else { return };
            if self.preparing.swap(true, Ordering::SeqCst) {
                return;
            }
            let env = env.clone();
            std::thread::spawn(move || winml::register(&env, true));
        }

        fn load(&self, file: &Path, allow: Option<&[String]>) -> Result<Loaded, String> {
            let env = self.env().as_ref().map_err(Clone::clone)?;
            let (net, provider) = first_working(
                &providers(env),
                allow,
                |name| session(env, file, Some(name)),
                || session(env, file, None),
            )?;
            Ok(Loaded {
                net: Box::new(OrtNet(net)),
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

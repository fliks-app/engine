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

    fn load(&self, _: &std::path::Path) -> Result<super::Loaded, String> {
        Err("this engine build has no ONNX runtime".into())
    }
}

#[cfg(feature = "karaoke")]
mod imp {
    use crate::karaoke::{Backend, BackendInfo, Loaded, Net, PROVIDERS, Tensor, first_working};
    use ort::{
        environment::Environment,
        ep::{self, ExecutionProvider},
        session::Session,
        value::Tensor as OrtTensor,
    };
    use std::path::Path;
    use std::sync::OnceLock;

    /// ONNX Runtime from `ORT_DYLIB_PATH`, else `libonnxruntime` on the loader's path.
    #[derive(Default)]
    pub struct Ort {
        env: OnceLock<Result<(Environment, Vec<String>), String>>,
    }

    fn available(name: &str) -> bool {
        match name {
            "cuda" => ep::CUDA::is_available(),
            "migraphx" => ep::MIGraphX::is_available(),
            "openvino" => ep::OpenVINO::is_available(),
            _ => ep::CoreML::is_available(),
        }
        .unwrap_or(false)
    }

    fn provider(name: &str) -> ep::ExecutionProviderDispatch {
        match name {
            "cuda" => ep::CUDA::default().build(),
            "migraphx" => ep::MIGraphX::default().build(),
            _ => ep::CoreML::default().build(),
        }
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
        fn env(&self) -> &Result<(Environment, Vec<String>), String> {
            self.env.get_or_init(|| {
                let lib = library();
                let env = ort::init_from(&lib)
                    .map_err(|e| format!("load {lib}: {e}"))?
                    .build()
                    .map_err(|e| format!("start ONNX Runtime: {e}"))?;
                let providers = PROVIDERS
                    .iter()
                    .filter(|p| available(p))
                    .map(|p| p.to_string())
                    .collect();
                Ok((env, providers))
            })
        }
    }

    fn session(
        env: &Environment,
        file: &Path,
        provider: Option<ep::ExecutionProviderDispatch>,
    ) -> Result<Session, String> {
        let mut b = Session::builder(env).map_err(|e| e.to_string())?;
        b = b
            .with_intra_threads(crate::karaoke::threads())
            .map_err(|e| e.to_string())?;
        if let Some(p) = provider {
            b = b
                .with_execution_providers([p.error_on_failure()])
                .map_err(|e| e.to_string())?;
        }
        b.commit_from_file(file).map_err(|e| e.to_string())
    }

    impl Backend for Ort {
        fn runtime(&self) -> BackendInfo {
            match self.env() {
                Ok((_, providers)) => BackendInfo {
                    onnx: true,
                    error: None,
                    providers: providers.clone(),
                },
                Err(e) => BackendInfo {
                    onnx: false,
                    error: Some(e.clone()),
                    providers: vec![],
                },
            }
        }

        fn load(&self, file: &Path) -> Result<Loaded, String> {
            let (env, providers) = self.env().as_ref().map_err(Clone::clone)?;
            let (net, provider) = first_working(
                providers,
                |name| session(env, file, Some(provider(name))),
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

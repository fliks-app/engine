//! Runs one model on a fixed input with the CPU and with a provider and prints how far the outputs
//! differ. `ORT_DYLIB_PATH=<lib> provider-check <coreml|cuda|migraphx|openvino> <model.onnx> <spleeter|swiftf0>`
use ort::{environment::Environment, ep, session::Session, value::Tensor};

fn noise(n: usize) -> Vec<f32> {
    let mut s = 0x2545_f491_u32;
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            (s as f32 / u32::MAX as f32) * 2.0 - 1.0
        })
        .collect()
}

fn e<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn run(
    env: &Environment,
    provider: Option<&str>,
    file: &str,
    kind: &str,
) -> Result<(Vec<Vec<f32>>, Vec<std::time::Duration>), String> {
    let mut b = Session::builder(env).map_err(e)?;
    if let Some(p) = provider {
        let d = match p {
            "coreml" => ep::CoreML::default().build(),
            "cuda" => ep::CUDA::default().build(),
            "migraphx" => ep::MIGraphX::default().build(),
            _ => ep::OpenVINO::default().with_device_type("GPU").build(),
        };
        b = b
            .with_execution_providers([d.error_on_failure()])
            .map_err(e)?;
    }
    let mut s = b.commit_from_file(file).map_err(e)?;
    let mut times = vec![];
    let mut last = vec![];
    for _ in 0..4 {
        let t = std::time::Instant::now();
        let out = if kind == "spleeter" {
            let x: Vec<f32> = noise(2 * 4 * 512 * 1024)
                .iter()
                .map(|v| v.abs() * 40.0)
                .collect();
            s.run(ort::inputs!["x" => Tensor::from_array(([2usize, 4, 512, 1024], x)).map_err(e)?])
        } else {
            let audio: Vec<f32> = noise(48000)
                .iter()
                .enumerate()
                .map(|(i, n)| {
                    0.4 * (i as f32 * 440.0 * std::f32::consts::TAU / 16000.0).sin() + 0.02 * n
                })
                .collect();
            s.run(ort::inputs![
                "audio" => Tensor::from_array(([1usize, 48000], audio)).map_err(e)?,
                "fmin" => Tensor::from_array(([0usize; 0], vec![46.875f32])).map_err(e)?,
                "fmax" => Tensor::from_array(([0usize; 0], vec![2093.75f32])).map_err(e)?
            ])
        }
        .map_err(e)?;
        times.push(t.elapsed());
        last = out
            .iter()
            .map(|(_, v)| {
                if let Ok((_, d)) = v.try_extract_tensor::<f32>() {
                    return Ok(d.to_vec());
                }
                let (_, d) = v.try_extract_tensor::<f64>().map_err(e)?;
                Ok(d.iter().map(|x| *x as f32).collect())
            })
            .collect::<Result<_, String>>()?;
    }
    Ok((last, times))
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let lib = std::env::var("ORT_DYLIB_PATH").expect("ORT_DYLIB_PATH");
    let env = ort::init_from(&lib).unwrap().build().unwrap();
    let (cpu, t_cpu) = run(&env, None, &a[2], &a[3]).unwrap();
    let (got, t_got) = run(&env, Some(&a[1]), &a[2], &a[3]).unwrap();
    println!(
        "{} {}\ncpu runs {t_cpu:?}\n{} runs {t_got:?}",
        a[3], a[2], a[1]
    );
    for (i, (c, g)) in cpu.iter().zip(&got).enumerate() {
        let diff = c.iter().zip(g).map(|(x, y)| (x - y).abs());
        let max = diff.clone().fold(0.0f32, f32::max);
        let share = |tol: f32| diff.clone().filter(|d| *d > tol).count() as f64 / c.len() as f64;
        let range = c.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        println!(
            "output {i}: {} elements, cpu max {range:.4}, max abs diff {max:.6}, differing >1e-3 {:.4}, >1e-2 {:.4}",
            c.len(),
            share(1e-3),
            share(1e-2)
        );
    }
}

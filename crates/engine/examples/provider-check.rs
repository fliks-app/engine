//! Runs one model on a fixed input with the CPU and with a provider and prints how far the outputs
//! differ. `provider-check <provider> <model.onnx> <spleeter|swiftf0>` on the engine's library (`ORT_DYLIB_PATH`, the Windows ML runtime, the bundle's), a provider
//! of `PROVIDERS` (`coreml`, `cuda`, `directml`, `nvtensorrtrtx`...); `list` instead prints them. Spleeter
//! runs on the production shape (one split) and on four splits. A provider that is not clearly faster
//! than the CPU may have given the whole graph to it: with MIGraphX, `ORT_MIGRAPHX_DUMP_MODEL_OPS=1`
//! lists the nodes it left out.
use fliks_engine::karaoke::onnx::{environment, providers, with_provider};
use ort::{environment::Environment, session::Session, value::Tensor};

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

type Case = (Vec<Vec<f32>>, Vec<std::time::Duration>);

fn run(
    env: &Environment,
    provider: Option<&str>,
    file: &str,
    kind: &str,
    splits: usize,
) -> Result<Case, String> {
    let mut b = Session::builder(env).map_err(e)?;
    if let Some(p) = provider {
        b = with_provider(b, env, p)?;
    }
    let mut s = b.commit_from_file(file).map_err(e)?;
    let mut times = vec![];
    let mut last = vec![];
    for _ in 0..4 {
        let t = std::time::Instant::now();
        let out = if kind == "spleeter" {
            let x: Vec<f32> = noise(2 * splits * 512 * 1024)
                .iter()
                .map(|v| v.abs() * 40.0)
                .collect();
            s.run(ort::inputs!["x" => Tensor::from_array(([2usize, splits, 512, 1024], x)).map_err(e)?])
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
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let a: Vec<String> = std::env::args().collect();
    let (env, system) = environment(true).unwrap();
    println!("windows ml runtime: {system}");
    if a[1] == "list" {
        for d in env.devices() {
            let hw = d.hardware_device();
            println!(
                "device {} {:?} {}",
                d.ep().unwrap_or("?"),
                hw.ty(),
                hw.vendor().unwrap_or_default()
            );
        }
        println!("providers {:?}", providers(&env));
        return;
    }
    let cases: &[usize] = if a[3] == "spleeter" { &[1, 4] } else { &[0] };
    for &n in cases {
        let (cpu, t_cpu) = run(&env, None, &a[2], &a[3], n).unwrap();
        let (got, t_got) = run(&env, Some(&a[1]), &a[2], &a[3], n).unwrap();
        println!(
            "{} {} splits {n}\ncpu runs {t_cpu:?}\n{} runs {t_got:?}",
            a[3], a[2], a[1]
        );
        let (c, g) = (steady(&t_cpu), steady(&t_got));
        println!("steady run: cpu {c:?}, {} {g:?}", a[1]);
        if g.as_secs_f64() > 0.7 * c.as_secs_f64() {
            println!(
                "WARNING: {} is not clearly faster than the CPU, it may run the whole graph there",
                a[1]
            );
        }
        compare(&cpu, &got);
    }
}

/// The median run after the first, which builds or compiles.
fn steady(times: &[std::time::Duration]) -> std::time::Duration {
    let mut t = times[1..].to_vec();
    t.sort();
    t[t.len() / 2]
}

fn compare(cpu: &[Vec<f32>], got: &[Vec<f32>]) {
    for (i, (c, g)) in cpu.iter().zip(got).enumerate() {
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

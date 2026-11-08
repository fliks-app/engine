//! The `Separate` job: the decoded track through a two-stem mask network in chunks, the stems
//! written as PCM, then encoded to Opus in one run. Spec: `docs/spec/karaoke.md` section 3.
use super::run::{Outputs, StopOnDrop, unsupported};
use super::{Net, Tensor};
use crate::jobs::{Cmd, Ctx, tail};
use crate::proc::Pipes;
use fliks_engine_core::api::{JobKind, JobResult, MediaInfo, ModelKind, SeparateMeta, StemFile};
use fliks_engine_core::jobs::separate::{
    CHUNK_FRAMES, CHUNK_SAMPLES, CHUNK_SPAN, DEFAULT_BITRATE_KBPS, DEFAULT_GUIDE_VOCALS_GAIN, LEAD,
    MODEL_BINS, Overlap, SAMPLE_RATE, Stft, chunk_count, soft_masks,
};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

/// One output: which stem and where.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stem {
    Vocals,
    Instrumental,
    Guide,
}

struct Pcm {
    stem: Stem,
    out: BufWriter<std::fs::File>,
}

pub async fn run(ctx: &Ctx, kind: &JobKind) -> Result<JobResult, String> {
    let JobKind::Separate {
        info,
        model,
        vocals,
        instrumental,
        guide,
        guide_vocals_gain,
        bitrate_kbps,
        providers,
    } = kind
    else {
        return Err("not a Separate job".into());
    };
    let (model, vocals, instrumental, guide) = (
        model.as_deref(),
        vocals.as_deref(),
        instrumental.as_deref(),
        guide.as_deref(),
    );
    let k = &ctx.rt.karaoke;
    let picked = k.models().pick(model, ModelKind::Separation)?;
    let backend = k.backend();
    let runtime = backend.runtime();
    if !runtime.onnx {
        return Err(unsupported(runtime.error.unwrap_or_default()));
    }
    let wanted: Vec<(Stem, PathBuf)> = [
        (Stem::Vocals, vocals),
        (Stem::Instrumental, instrumental),
        (Stem::Guide, guide),
    ]
    .into_iter()
    .filter_map(|(s, p)| Some((s, PathBuf::from(ctx.rt.map_media_path(p?)))))
    .collect();
    let outs = Outputs::claim(ctx, &wanted.iter().map(|w| w.1.clone()).collect::<Vec<_>>()).await?;
    let result = build(
        ctx,
        info,
        backend,
        &picked,
        &wanted,
        &outs,
        guide_vocals_gain.unwrap_or(DEFAULT_GUIDE_VOCALS_GAIN) as f32,
        bitrate_kbps.unwrap_or(DEFAULT_BITRATE_KBPS),
        providers.clone(),
    )
    .await;
    let published = match result {
        Ok(r) => outs.publish().await.map(|()| r),
        Err(e) => Err(e),
    };
    outs.cleanup().await;
    let (provider, seconds) = published?;
    let file = |stem: Stem| -> Option<StemFile> {
        let i = wanted.iter().position(|w| w.0 == stem)?;
        let req = match stem {
            Stem::Vocals => vocals,
            Stem::Instrumental => instrumental,
            Stem::Guide => guide,
        }?;
        let bytes = std::fs::metadata(&wanted[i].1).map_or(0, |m| m.len());
        Some(StemFile {
            path: req.into(),
            bytes: bytes as f64,
        })
    };
    Ok(JobResult::Separate {
        separate: SeparateMeta {
            model: picked.id.into(),
            model_version: picked.version.into(),
            provider,
            vocals: file(Stem::Vocals),
            instrumental: file(Stem::Instrumental),
            guide: file(Stem::Guide),
            duration_seconds: seconds,
        },
    })
}

#[allow(clippy::too_many_arguments)]
async fn build(
    ctx: &Ctx,
    info: &MediaInfo,
    backend: Arc<dyn super::Backend>,
    picked: &super::models::Picked,
    wanted: &[(Stem, PathBuf)],
    outs: &Outputs,
    gain: f32,
    kbps: u32,
    allow: Option<Vec<String>>,
) -> Result<(String, f64), String> {
    let (vocals_file, acc_file) = (
        picked.file("vocals.onnx"),
        picked.file("accompaniment.onnx"),
    );
    let (vocals_net, acc_net, provider) = tokio::task::spawn_blocking({
        move || -> Result<_, String> {
            let (v, a) = (
                backend.load(&vocals_file, allow.as_deref())?,
                backend.load(&acc_file, allow.as_deref())?,
            );
            let provider = v.provider.clone();
            Ok((v.net, a.net, provider))
        }
    })
    .await
    .map_err(|e| e.to_string())??;
    ctx.pulse();
    let pcm_path = |s: Stem| {
        outs.scratch().join(match s {
            Stem::Vocals => "vocals.pcm",
            Stem::Instrumental => "instrumental.pcm",
            Stem::Guide => "guide.pcm",
        })
    };
    let mut sinks = vec![];
    for (stem, _) in wanted {
        let f =
            std::fs::File::create(pcm_path(*stem)).map_err(|e| format!("write scratch: {e}"))?;
        sinks.push(Pcm {
            stem: *stem,
            out: BufWriter::new(f),
        });
    }
    let total = info.duration_seconds.filter(|d| *d > 0.0);
    let samples = decode_and_separate(ctx, vocals_net, acc_net, sinks, gain, total).await?;
    if samples == 0 {
        return Err(fliks_engine_core::api::JobErrorClass::Corrupt.message("no audio decoded"));
    }
    let seconds = samples as f64 / f64::from(SAMPLE_RATE);
    ctx.progress(95.0);
    let mut argv: Vec<String> = ["-hide_banner", "-loglevel", "warning", "-y"]
        .map(String::from)
        .to_vec();
    for (stem, _) in wanted {
        argv.extend(["-f", "s16le", "-ar", "44100", "-ac", "2", "-i"].map(String::from));
        argv.push(pcm_path(*stem).to_string_lossy().into_owned());
    }
    for i in 0..wanted.len() {
        argv.extend(["-map".into(), format!("{i}:a")]);
        argv.extend(["-c:a", "libopus", "-b:a"].map(String::from));
        argv.push(format!("{kbps}k"));
        argv.extend(["-vbr", "on", "-application", "audio", "-f", "ogg"].map(String::from));
        argv.push(outs.tmp(i).to_string_lossy().into_owned());
    }
    ctx.ffmpeg(&argv).await?;
    Ok((provider, seconds))
}

/// The decoder's stdout through the network, chunk by chunk, into the PCM sinks; the samples
/// written per channel.
async fn decode_and_separate(
    ctx: &Ctx,
    vocals_net: Box<dyn Net>,
    acc_net: Box<dyn Net>,
    sinks: Vec<Pcm>,
    gain: f32,
    total_seconds: Option<f64>,
) -> Result<u64, String> {
    let argv: Vec<String> = ["-hide_banner", "-loglevel", "warning", "-i"]
        .map(String::from)
        .into_iter()
        .chain([
            ctx.media_path(),
            "-map".into(),
            "0:a:0".into(),
            "-vn".into(),
            "-sn".into(),
            "-dn".into(),
            "-f".into(),
            "f32le".into(),
            "-ac".into(),
            "2".into(),
            "-ar".into(),
            SAMPLE_RATE.to_string(),
            "pipe:1".into(),
        ])
        .collect();
    let cmd = Cmd::new("ffmpeg", &ctx.rt.ffmpeg, &argv, Pipes::stdout());
    let c = ctx.clone();
    ctx.pipeline(u64::MAX, vec![cmd], |p| async move {
        let p = &p[0];
        let stop = Arc::new(AtomicBool::new(false));
        let _guard = StopOnDrop(stop.clone());
        let (tx, rx) = mpsc::channel::<Vec<u8>>(8);
        let driver = tokio::task::spawn_blocking({
            let (c, stop) = (c.clone(), stop.clone());
            move || {
                drive(rx, vocals_net, acc_net, sinks, gain, &stop, &|seconds| {
                    c.pulse();
                    if let Some(t) = total_seconds {
                        c.progress((seconds / t * 95.0).clamp(0.0, 95.0));
                    }
                })
            }
        });
        let mut out = p.take_stdout().ok_or("no decoder output")?;
        let mut eof = false;
        loop {
            let mut buf = vec![0u8; 1 << 20];
            let n = out
                .read(&mut buf)
                .await
                .map_err(|e| format!("decode: {e}"))?;
            if n == 0 {
                eof = true;
                break;
            }
            buf.truncate(n);
            if tx.send(buf).await.is_err() {
                break;
            }
        }
        drop(tx);
        if !eof {
            p.kill().await;
        }
        let samples = driver.await.map_err(|e| e.to_string())??;
        let exit = p.wait().await;
        match exit.code {
            Some(0) => Ok(samples),
            code => Err(format!("ffmpeg exited with {code:?}: {}", tail(p, 2))),
        }
    })
    .await?
}

/// Reads PCM from `rx`, separates it chunk by chunk and writes the stems; blocking.
fn drive(
    mut rx: mpsc::Receiver<Vec<u8>>,
    mut vocals_net: Box<dyn Net>,
    mut acc_net: Box<dyn Net>,
    mut sinks: Vec<Pcm>,
    gain: f32,
    stop: &AtomicBool,
    progress: &dyn Fn(f64),
) -> Result<u64, String> {
    let stft = Stft::default();
    let mut overlap = Overlap::default();
    // The signal in padded coordinates: `LEAD` zeros, then the samples; `base` is the padded
    // coordinate of `l[0]`.
    let (mut l, mut r) = (vec![0.0f32; LEAD], vec![0.0f32; LEAD]);
    let (mut base, mut leftover, mut eof) = (0usize, Vec::<u8>::new(), false);
    let mut written = 0u64;
    let mut k = 0usize;
    loop {
        let (from, upto) = (k * CHUNK_SAMPLES, k * CHUNK_SAMPLES + CHUNK_SPAN);
        while !eof && base + l.len() < upto {
            if stop.load(Relaxed) {
                return Err("stopped".into());
            }
            let Some(bytes) = rx.blocking_recv() else {
                eof = true;
                break;
            };
            leftover.extend_from_slice(&bytes);
            let frames = leftover.len() / 8;
            for f in leftover[..frames * 8].as_chunks::<8>().0 {
                l.push(f32::from_le_bytes([f[0], f[1], f[2], f[3]]));
                r.push(f32::from_le_bytes([f[4], f[5], f[6], f[7]]));
            }
            leftover.drain(..frames * 8);
        }
        if stop.load(Relaxed) {
            return Err("stopped".into());
        }
        let have = base + l.len();
        let samples = have.saturating_sub(LEAD);
        let at = from - base;
        let s = stft.analyse([&l[at.min(l.len())..], &r[at.min(r.len())..]]);
        progress(((from + CHUNK_SAMPLES).min(have)) as f64 / f64::from(SAMPLE_RATE));
        let mags = Tensor {
            shape: vec![2, 1, CHUNK_FRAMES as i64, MODEL_BINS as i64],
            data: s.magnitudes(),
        };
        let again = || Tensor {
            shape: mags.shape.clone(),
            data: mags.data.clone(),
        };
        let v = vocals_net.run(vec![("x", again())])?;
        progress(((from + CHUNK_SAMPLES).min(have)) as f64 / f64::from(SAMPLE_RATE));
        let a = acc_net.run(vec![("x", mags)])?;
        let (v, a) = (
            v.first().ok_or("no network output")?,
            a.first().ok_or("no network output")?,
        );
        if v.data.len() != 2 * CHUNK_FRAMES * MODEL_BINS || a.data.len() != v.data.len() {
            return Err("unexpected network output size".into());
        }
        let (vm, am) = soft_masks(&v.data, &a.data);
        stft.synthesise(&s, &vm, &am, &mut overlap);
        let done = overlap.finish();
        // Padded coordinates `from..from + CHUNK_SAMPLES` are samples `from - LEAD..`.
        let skip = LEAD.saturating_sub(from);
        let end = if eof {
            samples
                .saturating_sub(from.saturating_sub(LEAD))
                .min(CHUNK_SAMPLES - skip)
        } else {
            CHUNK_SAMPLES - skip
        };
        write(
            &mut sinks,
            &done.vocals,
            &done.instrumental,
            gain,
            skip,
            skip + end,
        )?;
        written += end as u64;
        let drop_to = (k + 1) * CHUNK_SAMPLES;
        let cut = drop_to.saturating_sub(base).min(l.len());
        l.drain(..cut);
        r.drain(..cut);
        base += cut;
        k += 1;
        if eof && k >= chunk_count(samples) {
            break;
        }
    }
    for s in &mut sinks {
        s.out.flush().map_err(|e| format!("write scratch: {e}"))?;
    }
    Ok(written)
}

fn write(
    sinks: &mut [Pcm],
    vocals: &[Vec<f32>; 2],
    instrumental: &[Vec<f32>; 2],
    gain: f32,
    from: usize,
    to: usize,
) -> Result<(), String> {
    let pcm = |x: f32| ((x.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes();
    for s in sinks {
        let mut buf = Vec::with_capacity((to - from) * 4);
        for i in from..to {
            for ch in 0..2 {
                let v = match s.stem {
                    Stem::Vocals => vocals[ch][i],
                    Stem::Instrumental => instrumental[ch][i],
                    Stem::Guide => instrumental[ch][i] + gain * vocals[ch][i],
                };
                buf.extend_from_slice(&pcm(v));
            }
        }
        s.out
            .write_all(&buf)
            .map_err(|e| format!("write scratch: {e}"))?;
    }
    Ok(())
}

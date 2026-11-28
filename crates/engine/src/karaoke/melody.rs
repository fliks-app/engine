//! The `Melody` job: the vocal stem at 16 kHz through the pitch network, the frames cut into
//! the notes of each span. Spec: `docs/spec/karaoke.md` section 5.
use super::run::{StopOnDrop, pcm_cmd, read_pcm, unsupported};
use super::{Net, Tensor};
use crate::jobs::Ctx;
use fliks_engine_core::api::{JobResult, MelodyMeta, MelodySpan, ModelKind};
use fliks_engine_core::jobs::key::analyse;
use fliks_engine_core::jobs::melody::{
    FMAX, FMIN, HOP, PitchTrack, SILENCE_PEAK, frames, normalise, spans_notes, window_samples,
    windows,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

pub async fn run(
    ctx: &Ctx,
    spans: &[MelodySpan],
    model: Option<&str>,
) -> Result<JobResult, String> {
    let k = &ctx.rt.karaoke;
    let picked = k.models().pick(model, ModelKind::Melody)?;
    let backend = k.backend();
    let runtime = backend.runtime();
    if !runtime.onnx {
        return Err(unsupported(runtime.error.unwrap_or_default()));
    }
    let file = picked.file("model.onnx");
    // SwiftF0 stays on the CPU: 10 ms a call, a GPU session costs more than it saves.
    let files = picked.files.clone();
    let loaded = super::run::low(move || {
        let _files = files.open();
        backend.load(&file, Some(&[]), &[])
    })
    .await
    .map_err(|e| e.to_string())??;
    ctx.pulse();
    let cmd = pcm_cmd(ctx, &ctx.media_path());
    let c = ctx.clone();
    let mut net = loaded.net;
    let track = ctx
        .pipeline(ctx.run_timeout_ms(), vec![cmd], |p| async move {
            let pcm = read_pcm(&c, &p[0]).await?;
            let stop = Arc::new(AtomicBool::new(false));
            let _guard = StopOnDrop(stop.clone());
            let c = c.clone();
            super::run::low(move || {
                let _busy = c.model_busy();
                track(
                    &mut *net,
                    &pcm,
                    &stop,
                    &|| c.wait_while_paused(&stop),
                    &|pct| {
                        c.pulse();
                        c.progress(pct);
                    },
                )
            })
            .await
            .map_err(|e| e.to_string())?
        })
        .await??;
    if track.hz.is_empty() {
        return Err(fliks_engine_core::api::JobErrorClass::Corrupt.message("no audio decoded"));
    }
    let notes = spans_notes(&track, spans);
    let analysis = analyse(&notes.concat());
    Ok(JobResult::Melody {
        melody: MelodyMeta {
            model: picked.id.into(),
            model_version: picked.version.clone(),
            spans: notes,
            analysis,
            revision: fliks_engine_core::api::REVISIONS.melody,
        },
    })
}

/// The pitch of every frame of `pcm`, one network call per overlapping window.
fn track(
    net: &mut dyn Net,
    pcm: &[f32],
    stop: &AtomicBool,
    wait: &dyn Fn(),
    report: &dyn Fn(f64),
) -> Result<PitchTrack, String> {
    let mut out = PitchTrack::default();
    if pcm.is_empty() {
        return Ok(out);
    }
    let calls = windows(frames(pcm.len()));
    let total = calls.len();
    for (i, (start, end, given)) in calls.into_iter().enumerate() {
        wait();
        if stop.load(Relaxed) {
            return Err("stopped".into());
        }
        let (a, b) = window_samples(pcm.len(), end, given);
        let audio = normalise(&pcm[a..b]);
        let scalar = |v: f32| Tensor {
            shape: vec![],
            data: vec![v],
        };
        let r = net.run(vec![
            (
                "audio",
                Tensor {
                    shape: vec![1, audio.len() as i64],
                    data: audio,
                },
            ),
            ("fmin", scalar(FMIN)),
            ("fmax", scalar(FMAX)),
        ])?;
        report((i + 1) as f64 / total as f64 * 95.0);
        let [pitch, conf] = &r[..] else {
            return Err("unexpected pitch network output".into());
        };
        let (lo, hi) = (start - given, end - given);
        if conf.data.len() < hi || pitch.data.len() < hi {
            return Err("pitch network answered too few frames".into());
        }
        for f in lo..hi {
            let at = (start + (f - lo)) * HOP;
            let peak = pcm[at..(at + HOP).min(pcm.len())]
                .iter()
                .fold(0.0f32, |m, v| m.max(v.abs()));
            out.hz.push(pitch.data[f]);
            out.confidence.push(if peak < SILENCE_PEAK {
                0.0
            } else {
                conf.data[f]
            });
        }
    }
    Ok(out)
}

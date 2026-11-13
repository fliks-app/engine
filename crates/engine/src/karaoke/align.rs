//! The `Align` job: a CTC network places the known lyrics on the vocal stem (`ctc.rs`), else
//! `whisper-cli` gives word times they are matched onto. Spec: `docs/spec/karaoke.md` section 4.
use super::ctc;
use super::models::Picked;
use super::run::unsupported;
use crate::jobs::{Cmd, Ctx, tail};
use crate::proc::Pipes;
use fliks_engine_core::api::{AlignLine, AlignMeta, JobResult, ModelKind};
use fliks_engine_core::jobs::align::{align, heard_from_json, words_of};

/// Characters of the lyrics given to the recogniser as its prompt: it then hears those words.
const PROMPT_CHARS: usize = 400;

/// Places the lyrics with the CTC model that serves the language when one is installed; the
/// speech recogniser stands in for a language with none and for a CTC run that fails.
pub async fn run(
    ctx: &Ctx,
    lines: &[AlignLine],
    language: Option<&str>,
    model: Option<&str>,
    providers: Option<&[String]>,
) -> Result<JobResult, String> {
    let k = &ctx.rt.karaoke;
    let models = k.models();
    let detected = ctc::language_of(language, lines);
    let picked = models.pick_aligner(model, detected.as_deref())?;
    if !picked.ctc {
        return recognise(ctx, &picked, lines, language, None).await;
    }
    let failure = match ctc::run(ctx, &picked, lines, detected.as_deref(), providers).await? {
        Ok(align) => return Ok(JobResult::Align { align }),
        Err(e) => e,
    };
    match models.pick(None, ModelKind::Alignment).ok() {
        Some(whisper) if k.tools(&ctx.rt.ffmpeg).await.whisper.is_some() => {
            tracing::warn!("CTC alignment failed, using the speech recogniser: {failure}");
            recognise(ctx, &whisper, lines, language, Some(failure)).await
        }
        _ => Err(failure),
    }
}

async fn recognise(
    ctx: &Ctx,
    picked: &Picked,
    lines: &[AlignLine],
    language: Option<&str>,
    fallback: Option<String>,
) -> Result<JobResult, String> {
    let k = &ctx.rt.karaoke;
    let tools = k.tools(&ctx.rt.ffmpeg).await;
    let whisper = tools
        .whisper
        .clone()
        .ok_or_else(|| unsupported("no speech recogniser (whisper-cli) is installed"))?;
    let scratch = ctx.rt.cache.merge_dir(&ctx.job.id);
    tokio::fs::create_dir_all(&scratch)
        .await
        .map_err(|e| format!("create scratch: {e}"))?;
    let result = heard(
        ctx,
        &whisper,
        &picked.file("model.bin"),
        &scratch,
        lines,
        language,
        tools.whisper_backend,
    )
    .await;
    let _ = tokio::fs::remove_dir_all(&scratch).await;
    let (heard, backend) = result?;
    let (aligned, matched) = align(lines, &heard);
    Ok(JobResult::Align {
        align: AlignMeta {
            model: picked.id.into(),
            model_version: picked.version.clone(),
            backend: backend.into(),
            lines: aligned,
            matched,
            fallback,
        },
    })
}

async fn heard(
    ctx: &Ctx,
    whisper: &str,
    model: &std::path::Path,
    scratch: &std::path::Path,
    lines: &[AlignLine],
    language: Option<&str>,
    backend: &'static str,
) -> Result<(Vec<fliks_engine_core::jobs::align::Heard>, &'static str), String> {
    let wav = scratch.join("vocals.wav");
    let decode: Vec<String> = [
        "-hide_banner",
        "-loglevel",
        "warning",
        "-y",
        "-i",
        &ctx.media_path(),
        "-map",
        "0:a:0",
        "-vn",
        "-sn",
        "-dn",
        "-ac",
        "1",
        "-ar",
        "16000",
        "-c:a",
        "pcm_s16le",
        &wav.to_string_lossy(),
    ]
    .map(String::from)
    .to_vec();
    ctx.ffmpeg(&decode).await?;
    let prefix = scratch.join("words");
    let run = |gpu: bool| {
        let argv = whisper_argv(model, &wav, &prefix, language, lines, gpu);
        let cmd = Cmd::new("whisper", whisper, &argv, Pipes::default());
        ctx.pipeline(ctx.run_timeout_ms(), vec![cmd], |p| async move {
            let p = &p[0];
            match p.wait().await.code {
                Some(0) => Ok(()),
                code => Err(format!("whisper exited with {code:?}: {}", tail(p, 3))),
            }
        })
    };
    let mut backend = backend;
    // A cancel or a stall is the pipeline's error and ends the job; only a failed run falls back.
    if let Err(e) = run(backend != "cpu").await? {
        if backend == "cpu" {
            return Err(e);
        }
        tracing::warn!(
            backend,
            "recogniser failed on the GPU, repeating on the CPU: {e}"
        );
        backend = "cpu";
        run(false).await??;
    }
    let json = tokio::fs::read_to_string(prefix.with_extension("json"))
        .await
        .map_err(|e| format!("whisper wrote no words: {e}"))?;
    Ok((heard_from_json(&json)?, backend))
}

/// The lyrics' first words, as one line.
fn prompt(lines: &[AlignLine]) -> String {
    let mut out = String::new();
    for c in words_of(lines).into_iter().filter(|c| c.repeat == 0) {
        for (s, e) in c.words {
            let w = &c.text[s as usize..e as usize];
            if out.chars().count() + w.chars().count() + 1 > PROMPT_CHARS {
                return out;
            }
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(w);
        }
    }
    out
}

/// The `whisper-cli` arguments of an `Align` run: words as JSON entries at `<prefix>.json`.
pub fn whisper_argv(
    model: &std::path::Path,
    wav: &std::path::Path,
    prefix: &std::path::Path,
    language: Option<&str>,
    lines: &[AlignLine],
    gpu: bool,
) -> Vec<String> {
    let mut argv: Vec<String> = [
        "-m",
        &model.to_string_lossy(),
        "-f",
        &wav.to_string_lossy(),
        "-l",
        language.unwrap_or("auto"),
        "-ml",
        "1",
        "-sow",
        "-oj",
        "-of",
        &prefix.to_string_lossy(),
        "-np",
        "-t",
        &super::threads().to_string(),
    ]
    .map(String::from)
    .to_vec();
    if !gpu {
        argv.push("-ng".into());
    }
    argv.extend(["--prompt".into(), prompt(lines)]);
    argv
}

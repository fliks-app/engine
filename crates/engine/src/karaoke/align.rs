//! The `Align` job: a CTC network places the known lyrics on the vocal stem (`ctc.rs`), else
//! `whisper-cli` gives word times they are matched onto. Spec: `docs/spec/karaoke.md` section 4.
use super::ctc;
use super::models::Picked;
use super::run::unsupported;
use crate::jobs::{Cmd, Ctx, tail};
use crate::proc::Pipes;
use fliks_engine_core::api::{AlignLine, AlignMeta, JobResult, ModelKind};
use fliks_engine_core::jobs::align::{align_fitted, heard_from_json, words_of};

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
    voicing: Option<&str>,
) -> Result<JobResult, String> {
    let k = &ctx.rt.karaoke;
    let models = k.models();
    let detected = ctc::language_of(language, lines);
    let picked = models.pick_aligner(model, detected.as_deref())?;
    // The recogniser takes two-letter codes only; any other language it detects itself.
    let heard = detected.as_deref().filter(|c| c.len() == 2);
    let Some(case) = picked.ctc else {
        return recognise(ctx, &picked, lines, heard, None).await;
    };
    let failure = match ctc::run(
        ctx,
        &picked,
        case,
        lines,
        detected.as_deref(),
        providers,
        voicing,
    )
    .await?
    {
        Ok(align) => return Ok(JobResult::Align { align }),
        Err(e) => e,
    };
    match models.pick(None, ModelKind::Alignment).ok() {
        Some(whisper) if k.tools(&ctx.rt.ffmpeg).await.whisper.is_some() => {
            tracing::warn!("CTC alignment failed, using the speech recogniser: {failure}");
            recognise(ctx, &whisper, lines, heard, Some(failure)).await
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
    let (heard, backend, duration_ms) = result?;
    let aligned = align_fitted(lines, &heard, duration_ms);
    Ok(JobResult::Align {
        align: AlignMeta {
            model: picked.id.into(),
            model_version: picked.version.clone(),
            backend: backend.into(),
            lines: aligned.lines,
            matched: aligned.matched,
            fallback,
            lrc_fit: aligned.lrc_fit,
            revision: fliks_engine_core::api::REVISIONS.align,
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
) -> Result<
    (
        Vec<fliks_engine_core::jobs::align::Heard>,
        &'static str,
        u32,
    ),
    String,
> {
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
    // 16 kHz mono s16le: 32 bytes a millisecond.
    let len = tokio::fs::metadata(&wav).await.map_or(0, |m| m.len());
    let duration_ms = u32::try_from(len / 32).unwrap_or(u32::MAX);
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
    Ok((heard_from_json(&json)?, backend, duration_ms))
}

/// The lyrics' first words, as one line.
fn prompt(lines: &[AlignLine]) -> String {
    let mut out = String::new();
    for c in words_of(lines).into_iter().filter(|c| c.repeat == 0) {
        let mut prev_end = None;
        for (s, e) in c.words {
            let w = &c.text[s as usize..e as usize];
            if out.chars().count() + w.chars().count() + 1 > PROMPT_CHARS {
                return out;
            }
            if !out.is_empty() && prev_end != Some(s) {
                out.push(' ');
            }
            out.push_str(w);
            prev_end = Some(e);
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

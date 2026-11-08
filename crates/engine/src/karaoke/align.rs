//! The `Align` job: the vocal stem at 16 kHz through `whisper-cli` for word times, the known
//! lyric words matched onto them. Spec: `docs/spec/karaoke.md` section 4.
use super::run::unsupported;
use crate::jobs::{Cmd, Ctx, tail};
use crate::proc::Pipes;
use fliks_engine_core::api::{AlignLine, AlignMeta, JobResult, ModelKind};
use fliks_engine_core::jobs::align::{align, heard_from_json, words_of};

/// Characters of the lyrics given to the recogniser as its prompt: it then hears those words.
const PROMPT_CHARS: usize = 400;

pub async fn run(
    ctx: &Ctx,
    lines: &[AlignLine],
    language: Option<&str>,
    model: Option<&str>,
) -> Result<JobResult, String> {
    let k = &ctx.rt.karaoke;
    let picked = k.models().pick(model, ModelKind::Alignment)?;
    let whisper = k
        .tools(&ctx.rt.ffmpeg)
        .await
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
    )
    .await;
    let _ = tokio::fs::remove_dir_all(&scratch).await;
    let (aligned, matched) = align(lines, &result?);
    Ok(JobResult::Align {
        align: AlignMeta {
            model: picked.id.into(),
            model_version: picked.version.into(),
            lines: aligned,
            matched,
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
) -> Result<Vec<fliks_engine_core::jobs::align::Heard>, String> {
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
    let argv: Vec<String> = [
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
        "--prompt",
        &prompt(lines),
    ]
    .map(String::from)
    .to_vec();
    let cmd = Cmd::new("whisper", whisper, &argv, Pipes::default());
    ctx.pipeline(ctx.run_timeout_ms(), vec![cmd], |p| async move {
        let p = &p[0];
        match p.wait().await.code {
            Some(0) => Ok(()),
            code => Err(format!("whisper exited with {code:?}: {}", tail(p, 3))),
        }
    })
    .await??;
    let json = tokio::fs::read_to_string(prefix.with_extension("json"))
        .await
        .map_err(|e| format!("whisper wrote no words: {e}"))?;
    heard_from_json(&json)
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

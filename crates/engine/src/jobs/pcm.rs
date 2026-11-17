//! The `Pcm` job: the audio of a file decoded to raw s16le at a host-named path, written beside
//! it and published without overwriting. Spec: `docs/spec/jobs.md` section 10b.
use super::Ctx;
use crate::safe_write::{exists, publish, taken};
use fliks_engine_core::api::{JobResult, PcmMeta};
use fliks_engine_core::jobs::pcm::*;
use std::path::PathBuf;

pub async fn run(
    ctx: &Ctx,
    output: &str,
    rate: Option<u32>,
    channels: Option<u32>,
) -> Result<JobResult, String> {
    let (rate, channels) = (
        rate.unwrap_or(DEFAULT_RATE),
        channels.unwrap_or(DEFAULT_CHANNELS),
    );
    let out = PathBuf::from(ctx.rt.map_media_path(output));
    let input = ctx.media_path();
    // The core accepts any platform's absolute form; the mapped paths must be this host's.
    if !out.is_absolute() {
        return Err(format!(
            "not an absolute path on this host: {}",
            out.display()
        ));
    }
    let (dir, name) = out
        .parent()
        .zip(out.file_name())
        .ok_or("output has no directory")?;
    if exists(&out).await {
        return Err(taken(&out));
    }
    ctx.check_disk(dir).await?;
    let tmp = dir.join(format!(
        ".{}.pcm-{}.tmp",
        name.to_string_lossy(),
        crate::random_hex(4)
    ));
    let made = tokio::fs::File::options()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .await
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    drop(made);
    let argv = decode_args(&input, rate, channels, &tmp.to_string_lossy());
    let published = match ctx.ffmpeg(&argv).await {
        Ok(()) => publish(&tmp, &out).await,
        Err(e) => Err(e),
    };
    let _ = tokio::fs::remove_file(&tmp).await;
    published?;
    let bytes = tokio::fs::metadata(&out).await.map_or(0, |m| m.len());
    Ok(JobResult::Pcm {
        pcm: PcmMeta {
            path: output.into(),
            bytes: bytes as f64,
            rate,
            channels,
        },
    })
}

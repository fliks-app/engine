//! The `SubtitleExtract` job: every embedded text track in one ffmpeg run, converted to WebVTT in
//! the engine cache, where the subtitle routes serve it. Spec: `docs/spec/jobs.md` section 8.
use super::{CANCELLED, Cmd, Ctx, tail};
use crate::proc::Pipes;
use fliks_engine_core::api::{JobResult, MediaInfo};
use fliks_engine_core::jobs::subtitles::*;
use std::path::Path;

pub fn validate(info: &MediaInfo) -> Result<(), String> {
    let streams = text_stream_indices(info);
    if streams.is_empty() {
        return Err("SubtitleExtract needs a text subtitle track in info".into());
    }
    if streams.iter().any(|s| *s > MAX_STREAM_INDEX) {
        return Err(format!("Invalid stream index: above {MAX_STREAM_INDEX}"));
    }
    Ok(())
}

/// The WebVTT of ffmpeg's ASS output, shared by the job and the subtitle routes.
pub fn raw_to_vtt(raw: &[u8], name: &str) -> Result<String, String> {
    check_extracted(raw.len() as u64, name)?;
    Ok(ass_to_vtt(&String::from_utf8_lossy(raw)))
}

pub fn vtt_path(dir: &Path, stream: u32) -> std::path::PathBuf {
    dir.join(format!("{stream}.vtt"))
}

/// Written beside, then renamed: a reader never sees half a file.
pub async fn store(dir: &Path, stream: u32, vtt: &str) -> std::io::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    let tmp = dir.join(format!("{stream}.vtt.{}", crate::random_hex(4)));
    tokio::fs::write(&tmp, vtt).await?;
    tokio::fs::rename(&tmp, vtt_path(dir, stream)).await
}

/// Where ffmpeg writes each stream's ASS before conversion; the tag keeps concurrent runs apart.
pub fn raw_files(dir: &Path, streams: &[u32]) -> Vec<(u32, String)> {
    let tag = crate::random_hex(4);
    streams
        .iter()
        .map(|s| {
            let p = dir.join(format!("{s}.ass.{tag}"));
            (*s, p.to_string_lossy().into_owned())
        })
        .collect()
}

/// Converts ffmpeg's ASS output of one stream and stores its WebVTT.
pub async fn convert_raw(dir: &Path, stream: u32, raw: &str) -> Result<(), String> {
    let bytes = tokio::fs::read(raw)
        .await
        .map_err(|e| format!("stream {stream}: read {raw}: {e}"))?;
    let vtt = raw_to_vtt(&bytes, raw).map_err(|e| format!("stream {stream}: {e}"))?;
    store(dir, stream, &vtt)
        .await
        .map_err(|e| format!("stream {stream}: write: {e}"))
}

pub async fn run(ctx: &Ctx, info: &MediaInfo) -> Result<JobResult, String> {
    let dir = ctx.rt.cache.subtitles_dir(ctx.job.req.media.media_file_id);
    let all = text_stream_indices(info);
    let pending: Vec<u32> = all
        .iter()
        .copied()
        .filter(|s| ctx.job.req.force || !vtt_path(&dir, *s).exists())
        .collect();
    if !pending.is_empty() {
        ctx.check_disk(&dir).await?;
        let raws = raw_files(&dir, &pending);
        let out = extract(ctx, &dir, &raws).await;
        for (_, raw) in &raws {
            let _ = tokio::fs::remove_file(raw).await;
        }
        out?;
    }
    Ok(JobResult::SubtitleExtract { streams: all })
}

async fn extract(ctx: &Ctx, dir: &Path, raws: &[(u32, String)]) -> Result<(), String> {
    let outputs: Vec<(u32, &str)> = raws.iter().map(|(s, r)| (*s, r.as_str())).collect();
    let argv = extract_args(&ctx.media_path(), &outputs);
    let cmd = Cmd::new("ffmpeg", &ctx.rt.ffmpeg, &argv, Pipes::default());
    ctx.pipeline(EXTRACT_TIMEOUT_MS, vec![cmd], |p| async move {
        match p[0].wait().await.code {
            Some(0) => Ok(()),
            code => Err(format!("ffmpeg exited with {code:?}: {}", tail(&p[0], 2))),
        }
    })
    .await??;
    // One bad track does not hold back the others.
    let mut errors = vec![];
    for (i, (stream, raw)) in raws.iter().enumerate() {
        if ctx.cancelled() {
            return Err(CANCELLED.into());
        }
        if let Err(e) = convert_raw(dir, *stream, raw).await {
            errors.push(e);
        }
        ctx.progress((i + 1) as f64 / raws.len() as f64 * 99.0);
    }
    match errors.is_empty() {
        true => Ok(()),
        false => Err(errors.join("; ")),
    }
}

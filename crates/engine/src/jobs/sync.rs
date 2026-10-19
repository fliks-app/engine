//! The `SubtitleSync` job: ffsubsync when it is on PATH, then alass.
use super::alass::{SAMPLE_RATE, Voice, sync_text};
use super::{Cmd, Ctx, tail};
use crate::proc::Pipes;
use fliks_engine_core::api::{JobResult, MediaInfo, SubtitleFormat, SubtitleText, SyncReference};
use fliks_engine_core::decide::timeline::parse_source_fps;
use fliks_engine_core::jobs::cues::cue_spans;
use fliks_engine_core::jobs::sync as pure;
use std::path::Path;
use std::sync::Arc;
use tokio::io::AsyncReadExt;

pub fn validate(
    info: &MediaInfo,
    subtitle: &SubtitleText,
    reference: &SyncReference,
    max_offset_seconds: Option<f64>,
) -> Result<(), String> {
    pure::validate_sync(info, subtitle, reference, max_offset_seconds)
}

fn alass_err(msg: impl AsRef<str>) -> String {
    let s: String = msg.as_ref().chars().take(300).collect();
    format!("alass error: {s}")
}

fn ref_fps(info: &MediaInfo) -> Option<f64> {
    info.video
        .first()
        .and_then(|v| parse_source_fps(v.frame_rate.as_deref()))
        .filter(|f| f.is_finite() && *f > 0.0)
}

async fn remove_tree(dir: &Path) {
    let _ = tokio::fs::remove_dir_all(dir).await;
}

pub async fn run(
    ctx: &Ctx,
    info: &MediaInfo,
    subtitle: &SubtitleText,
    reference: &SyncReference,
    max_offset_seconds: Option<f64>,
    no_fix_framerate: bool,
    gss: bool,
) -> Result<JobResult, String> {
    pure::validate_sync(info, subtitle, reference, None)?;
    let media = ctx.media_path();
    let resolved = pure::resolve_reference(info, reference, &media)?;
    let ext = pure::format_ext(subtitle.format);
    let dir = ctx.rt.cache.root.join("tmp").join("sync").join(&ctx.job.id);
    let in_path = dir.join(format!("in.{ext}"));
    let out_path = dir.join(format!("out.{ext}"));
    let ref_srt = dir.join("ref.srt");
    ctx.check_disk(&dir).await?;
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| e.to_string())?;
    tokio::fs::write(&in_path, &subtitle.text)
        .await
        .map_err(|e| e.to_string())?;
    let stage = Stage {
        input: &in_path,
        output: &out_path,
        reference_srt: &ref_srt,
    };
    let flags = FfsubsyncFlags {
        max_offset_seconds,
        no_fix_framerate,
        gss,
    };
    let out = run_inner(ctx, info, subtitle, reference, &resolved, &stage, &flags).await;
    remove_tree(&dir).await;
    out.map(|text| JobResult::SubtitleSync {
        subtitle: SubtitleText {
            format: subtitle.format,
            text,
        },
    })
}

struct Stage<'a> {
    input: &'a Path,
    output: &'a Path,
    reference_srt: &'a Path,
}

struct FfsubsyncFlags {
    max_offset_seconds: Option<f64>,
    no_fix_framerate: bool,
    gss: bool,
}

async fn run_inner(
    ctx: &Ctx,
    info: &MediaInfo,
    subtitle: &SubtitleText,
    reference: &SyncReference,
    resolved: &pure::ResolvedReference,
    stage: &Stage<'_>,
    flags: &FfsubsyncFlags,
) -> Result<String, String> {
    if announced() {
        match ffsubsync_text(ctx, info, reference, stage, flags).await {
            Ok(text) => return Ok(text),
            Err(ff) => {
                return match alass_text(ctx, info, subtitle, resolved, stage).await {
                    Ok(text) => Ok(text),
                    Err(alass) => Err(format!("ffsubsync stderr: {ff}\n{alass}")),
                };
            }
        }
    }
    alass_text(ctx, info, subtitle, resolved, stage).await
}

fn on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            #[cfg(windows)]
            {
                dir.join(format!("{name}.exe")).is_file() || dir.join(name).is_file()
            }
            #[cfg(not(windows))]
            {
                dir.join(name).is_file()
            }
        })
    })
}

pub fn announced() -> bool {
    on_path("ffsubsync")
}

fn bound(msg: &str) -> String {
    msg.chars().take(300).collect()
}

async fn ffsubsync_text(
    ctx: &Ctx,
    info: &MediaInfo,
    reference: &SyncReference,
    stage: &Stage<'_>,
    flags: &FfsubsyncFlags,
) -> Result<String, String> {
    let media = ctx.media_path();
    let reference_path = match reference {
        SyncReference::File { path } => path.clone(),
        _ => media,
    };
    let stream = match reference {
        SyncReference::AudioStream { index } | SyncReference::SubtitleStream { index } => {
            Some(*index)
        }
        _ => None,
    };
    let err = match ffsubsync_once(ctx, &reference_path, stream, stage, flags).await {
        Ok(()) => return read_subtitle(stage.output).await,
        Err(e) => e,
    };
    let retry =
        pure::speech_retry_stream(reference, &err, info.audio.first().map(|a| a.stream_index));
    if let Some(index) = retry
        && ffsubsync_once(ctx, &reference_path, Some(index), stage, flags)
            .await
            .is_ok()
    {
        return read_subtitle(stage.output).await;
    }
    Err(bound(&err))
}

async fn ffsubsync_once(
    ctx: &Ctx,
    reference: &str,
    stream: Option<u32>,
    stage: &Stage<'_>,
    flags: &FfsubsyncFlags,
) -> Result<(), String> {
    let argv = pure::ffsubsync_argv(
        reference,
        &stage.input.to_string_lossy(),
        &stage.output.to_string_lossy(),
        stream,
        flags.max_offset_seconds,
        flags.no_fix_framerate,
        flags.gss,
    );
    let cmd = Cmd::new("ffsubsync", "ffsubsync", &argv, Pipes::default());
    ctx.pipeline(pure::PCM_TIMEOUT_MS, vec![cmd], |p| async move {
        match p[0].wait().await.code {
            Some(0) => Ok(()),
            _ => Err(tail(&p[0], 8)),
        }
    })
    .await?
}

async fn read_subtitle(path: &Path) -> Result<String, String> {
    tokio::fs::read_to_string(path)
        .await
        .map_err(|e| e.to_string())
}

async fn alass_text(
    ctx: &Ctx,
    info: &MediaInfo,
    subtitle: &SubtitleText,
    resolved: &pure::ResolvedReference,
    stage: &Stage<'_>,
) -> Result<String, String> {
    let spans = reference_spans(ctx, resolved, stage.reference_srt)
        .await
        .map_err(|e| {
            if e.starts_with("alass error:") {
                e
            } else {
                alass_err(e)
            }
        })?;
    ctx.progress(50.0);
    let (text, format, fps) = (subtitle.text.clone(), subtitle.format, ref_fps(info));
    tokio::task::spawn_blocking(move || sync_text(&text, format, &spans, fps))
        .await
        .map_err(|e| alass_err(e.to_string()))
}

async fn reference_spans(
    ctx: &Ctx,
    resolved: &pure::ResolvedReference,
    ref_srt: &Path,
) -> Result<Vec<(i64, i64)>, String> {
    match resolved {
        pure::ResolvedReference::Pcm { path, map } => pcm_spans(ctx, path, &map.map_arg()).await,
        pure::ResolvedReference::SubtitleFile { path } => {
            subtitle_file_spans(Path::new(path), None).await
        }
        pure::ResolvedReference::ExtractSubtitleStream {
            media_path,
            stream_index,
        } => {
            extract_subtitle_stream(ctx, media_path.as_str(), *stream_index, ref_srt).await?;
            subtitle_file_spans(ref_srt, Some(SubtitleFormat::Srt)).await
        }
    }
}

/// Cue spans of a subtitle file; its format is the extension's unless given. Only the timestamps
/// are read, so the file's own encoding does not matter.
async fn subtitle_file_spans(
    path: &Path,
    format: Option<SubtitleFormat>,
) -> Result<Vec<(i64, i64)>, String> {
    let format = format
        .or_else(|| pure::format_of_path(&path.to_string_lossy()))
        .ok_or_else(|| {
            alass_err(format!(
                "subtitle format not supported ({})",
                path.display()
            ))
        })?;
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| alass_err(format!("{}: {e}", path.display())))?;
    Ok(cue_spans(&String::from_utf8_lossy(&bytes), format))
}

async fn extract_subtitle_stream(
    ctx: &Ctx,
    path: &str,
    stream_index: u32,
    out: &Path,
) -> Result<(), String> {
    let out_s = out.to_string_lossy();
    let argv = pure::subtitle_stream_extract_argv(path, stream_index, &out_s);
    let cmd = Cmd::new("ffmpeg", &ctx.rt.ffmpeg, &argv, Pipes::default());
    ctx.pipeline(pure::PCM_TIMEOUT_MS, vec![cmd], |p| async move {
        match p[0].wait().await.code {
            Some(0) => Ok(()),
            code => Err(format!("ffmpeg exited with {code:?}: {}", tail(&p[0], 2))),
        }
    })
    .await?
}

async fn pcm_spans(ctx: &Ctx, path: &str, map: &str) -> Result<Vec<(i64, i64)>, String> {
    let rate = SAMPLE_RATE;
    let argv = pure::pcm_extract_argv(path, map, rate);
    let cmd = Cmd::new("ffmpeg", &ctx.rt.ffmpeg, &argv, Pipes { stdout: true });
    let c = ctx.clone();
    ctx.pipeline(pure::PCM_TIMEOUT_MS, vec![cmd], |p| async move {
        let ff = Arc::clone(&p[0]);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<i16>>(64);
        // The detector is not Send, so voice activity runs on a blocking thread.
        let vad = tokio::task::spawn_blocking(move || {
            let mut voice = Voice::new();
            while let Some(samples) = rx.blocking_recv() {
                voice.push(&samples).map_err(alass_err)?;
            }
            voice.finish().map_err(alass_err)
        });
        let feed = feed_pcm(c, ff.clone(), tx);
        let (feed_out, exit, spans_out) = tokio::join!(feed, ff.wait(), vad);
        feed_out?;
        match exit.code {
            Some(0) => {}
            code => {
                return Err(format!(
                    "ffmpeg exited with {code:?}: {}",
                    tail(ff.as_ref(), 2)
                ));
            }
        }
        spans_out.map_err(|e| alass_err(e.to_string()))?
    })
    .await?
}

async fn feed_pcm(
    ctx: Ctx,
    ff: Arc<crate::proc::Ffmpeg>,
    tx: tokio::sync::mpsc::Sender<Vec<i16>>,
) -> Result<(), String> {
    let Some(mut out) = ff.take_stdout() else {
        return Err("ffmpeg produced no stdout".into());
    };
    let mut buf = vec![0u8; 64 * 1024];
    let mut carry: Option<u8> = None;
    loop {
        let n = out.read(&mut buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        let mut raw = Vec::with_capacity(n + usize::from(carry.is_some()));
        if let Some(b) = carry.take() {
            raw.push(b);
        }
        raw.extend_from_slice(&buf[..n]);
        let (pairs, rest) = raw.as_chunks::<2>();
        if let Some(b) = rest.first() {
            carry = Some(*b);
        }
        let samples: Vec<i16> = pairs.iter().map(|c| i16::from_le_bytes(*c)).collect();
        if !samples.is_empty() && tx.send(samples).await.is_err() {
            return Ok(());
        }
        ctx.pulse();
    }
    if carry.is_some() {
        return Err(alass_err("odd PCM byte count from ffmpeg"));
    }
    drop(tx);
    Ok(())
}

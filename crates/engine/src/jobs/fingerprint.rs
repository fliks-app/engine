//! The `Fingerprint` job: fpcalc on the head of the file, or on its tail through an ffmpeg window
//! piped into fpcalc (one slot for the pair). Spec: `docs/spec/jobs.md` section 7.
use super::{Cmd, Ctx};
use crate::proc::{Ffmpeg, Pipes};
use fliks_engine_core::api::{Fingerprint, FingerprintWindow, JobResult};
use fliks_engine_core::jobs::fingerprint::*;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub async fn run(
    ctx: &Ctx,
    window: &FingerprintWindow,
    max_lookup_minutes: Option<f64>,
) -> Result<JobResult, String> {
    let lookup = lookup_seconds(max_lookup_minutes);
    let path = ctx.media_path();
    let fingerprint = match window {
        FingerprintWindow::Head => head(ctx, &path, lookup).await?,
        FingerprintWindow::Outro { duration_seconds } => {
            let (start, length) =
                outro_window(*duration_seconds, lookup).ok_or("outro needs a duration")?;
            piped(ctx, &path, start, length).await?
        }
    };
    Ok(JobResult::Fingerprint { fingerprint })
}

fn fpcalc(ctx: &Ctx, argv: &[String], pipes: Pipes) -> Cmd {
    Cmd::new("fpcalc", &ctx.rt.fpcalc, argv, pipes)
}

async fn read_text(p: &Ffmpeg) -> String {
    let mut text = String::new();
    if let Some(mut out) = p.take_stdout() {
        let _ = out.read_to_string(&mut text).await;
    }
    text
}

fn stderr_text(p: &Ffmpeg) -> String {
    p.stderr_tail().join("\n")
}

async fn head(ctx: &Ctx, path: &str, lookup: f64) -> Result<Fingerprint, String> {
    let cmd = fpcalc(
        ctx,
        &head_args(path, lookup),
        Pipes {
            stdout: true,
            fds: 0,
        },
    );
    let (text, code, err) = ctx
        .pipeline(TIMEOUT_MS, vec![cmd], |p| async move {
            let p = &p[0];
            let (text, exit) = tokio::join!(read_text(p), p.wait());
            (text, exit.code, stderr_text(p))
        })
        .await?;
    match code {
        Some(0) => parse_fpcalc(&text, lookup, 0.0),
        c => Err(format!("fpcalc exited {}: {}", c.unwrap_or(-1), err.trim())),
    }
}

async fn piped(ctx: &Ctx, path: &str, start: f64, length: f64) -> Result<Fingerprint, String> {
    let pipes = Pipes {
        stdout: true,
        fds: 0,
    };
    let ffmpeg = Cmd::new(
        "ffmpeg",
        &ctx.rt.ffmpeg,
        &window_ffmpeg_args(path, start, length),
        pipes,
    );
    let fp = fpcalc(ctx, &window_fpcalc_args(length), pipes);
    let c = ctx.clone();
    let (text, code, errs) = ctx
        .pipeline(TIMEOUT_MS, vec![ffmpeg, fp], |p| async move {
            let (ff, fp): (&Arc<Ffmpeg>, &Arc<Ffmpeg>) = (&p[0], &p[1]);
            let feed = feed(c, ff.clone(), fp.clone(), length);
            let (text, exit, _) = tokio::join!(read_text(fp), fp.wait(), feed);
            (text, exit.code, (stderr_text(fp), stderr_text(ff)))
        })
        .await?;
    match code {
        Some(0) => parse_fpcalc(&text, length, start),
        c => Err(window_failure(c.unwrap_or(-1), &errs.0, &errs.1)),
    }
}

/// ffmpeg's WAV into fpcalc's stdin; fpcalc stopping early (it has its `-length`) ends the copy.
async fn feed(ctx: Ctx, ff: Arc<Ffmpeg>, fp: Arc<Ffmpeg>, length: f64) {
    let (Some(mut from), Some(mut to)) = (ff.take_stdout(), fp.take_stdin().await) else {
        return;
    };
    // 22.05 kHz, mono, 16-bit.
    let expected = (44_100.0 * length).max(1.0);
    let (mut buf, mut sent) = (vec![0u8; 64 * 1024], 0.0);
    while let Ok(n) = from.read(&mut buf).await {
        if n == 0 || to.write_all(&buf[..n]).await.is_err() {
            break;
        }
        sent += n as f64;
        ctx.pulse();
        ctx.progress((sent / expected * 100.0).min(99.0));
    }
}

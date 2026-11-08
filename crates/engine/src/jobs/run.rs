//! Helpers every ffmpeg-running job shares: the run supervisor, file error classes, file probe,
//! the pipeline error mapping.
use super::{CANCELLED, Cmd, Ctx, tail};
use crate::probe::run::probe_stamped;
use crate::proc::{Ffmpeg, Pipes};
use fliks_engine_core::api::{JobErrorClass as Class, MediaInfo};
use fliks_engine_core::jobs::merge::progress_seconds;
use fliks_engine_core::jobs::optimize as opt;
use fliks_engine_core::probe::ProbeOpts;
use std::io::ErrorKind;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Class of a failed file operation.
pub(super) fn io_class(e: &std::io::Error) -> Class {
    #[cfg(unix)]
    match e.raw_os_error() {
        Some(libc::EROFS) => return Class::Access,
        Some(libc::EIO) => return Class::Io,
        Some(libc::ENOSPC) => return Class::Nospace,
        _ => {}
    }
    match e.kind() {
        ErrorKind::StorageFull => Class::Nospace,
        ErrorKind::NotFound | ErrorKind::PermissionDenied => Class::Access,
        ErrorKind::AlreadyExists => Class::Engine,
        _ => Class::Io,
    }
}

pub(super) fn io_err(what: &Path, e: std::io::Error) -> String {
    io_class(&e).message(format!("{}: {e}", what.display()))
}

/// What an ffmpeg run left behind.
pub(super) struct Ran {
    pub errors: u32,
    pub excerpt: String,
    pub lines: Vec<String>,
}

/// One ffmpeg run on a background slot, `-progress` lines on stdout as job progress between the
/// two percentages. A non-zero exit fails with the class its stderr shows.
pub(super) async fn run_ffmpeg(
    ctx: &Ctx,
    argv: &[String],
    total: f64,
    (from, to): (f64, f64),
) -> Result<Ran, String> {
    let cmd = Cmd::new(
        "ffmpeg",
        &ctx.rt.ffmpeg,
        argv,
        Pipes {
            decode_errors: true,
            ..Pipes::stdout()
        },
    );
    let c = ctx.clone();
    ctx.pipeline(ctx.run_timeout_ms(), vec![cmd], |p| async move {
        let p = &p[0];
        let (_, exit) = tokio::join!(report(&c, p, total, from, to), p.wait());
        let lines = p.stderr_tail();
        match exit.code {
            Some(0) => Ok(Ran {
                errors: p.decode_errors(),
                excerpt: lines
                    .iter()
                    .filter(|l| opt::is_decode_error(l))
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" | "),
                lines,
            }),
            code => Err(opt::classify_stderr(&lines)
                .message(format!("ffmpeg exited with {code:?}: {}", tail(p, 2)))),
        }
    })
    .await
    .map_err(pipeline_err)?
}

async fn report(ctx: &Ctx, p: &Ffmpeg, total: f64, from: f64, to: f64) {
    let Some(out) = p.take_stdout() else {
        return;
    };
    let mut lines = BufReader::new(out).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        ctx.pulse();
        if let Some(s) = progress_seconds(&line)
            && total > 0.0
        {
            ctx.progress((from + (to - from) * (s / total).clamp(0.0, 1.0)).min(99.0));
        }
    }
}

pub(super) async fn probe_file(ctx: &Ctx, path: &str) -> Result<MediaInfo, String> {
    let opts = ProbeOpts {
        crop: false,
        mkv_tags: true,
        crop_decode: None,
    };
    let out = probe_stamped(path, ctx.rt.slots.clone(), &opts).await;
    if !out.ok {
        let why = out.error.unwrap_or_default();
        return Err(opt::classify_stderr(std::slice::from_ref(&why)).message(why));
    }
    Ok(out.info)
}

/// `Err` text of a run that did not end well: a cancel stays itself, the rest is an engine fault.
pub(super) fn pipeline_err(e: String) -> String {
    if e == CANCELLED || e == super::PROMOTED || is_busy(&e) {
        e
    } else {
        Class::Engine.message(e)
    }
}

/// A run stopped for playback: the job goes back to the queue, no failure.
pub(super) fn is_busy(e: &str) -> bool {
    Class::from_message(e).0 == Class::Busy
}

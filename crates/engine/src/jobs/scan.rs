//! The `Scan` job: ffprobe over every video packet (keyframes, end, clock break) and, on MPEG-TS
//! with AAC, the ADTS headers read from the same bytes. Spec: `docs/spec/jobs.md` section 6.
use super::{Cmd, Ctx};
use crate::proc::{Ffmpeg, Pipes};
use fliks_engine_core::api::{JobResult, MediaInfo, SOURCE_SCAN_SCHEMA_VERSION};
use fliks_engine_core::jobs::scan::*;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const CHUNK: usize = 1 << 20;

pub async fn run(ctx: &Ctx, info: &MediaInfo) -> Result<JobResult, String> {
    let path = ctx.media_path();
    let plan = ScanPlan::new(info, &path)?;
    let lead = match known_lead(&plan.video) {
        Some(l) => l,
        None => reorder_lead(ctx, &plan, &path).await?,
    };
    let size = tokio::fs::metadata(&path)
        .await
        .map_err(|e| format!("stat {path}: {e}"))?
        .len();
    let reader = PacketReader::new(lead, plan.mpeg_ts);
    let timeout = scan_timeout_ms(size as f64);
    let duration = info.duration_seconds.filter(|d| *d > 0.0);
    let (reader, changes) = if plan.piped() {
        piped(ctx, &plan, &path, size, reader, duration, timeout).await?
    } else {
        let cmd = ffprobe(ctx, &packet_args(&plan, &path));
        let c = ctx.clone();
        let reader = ctx
            .pipeline(timeout, vec![cmd], |p| async move {
                packets(&c, &p[0], reader, duration).await.map(|(r, _)| r)
            })
            .await??;
        (reader, audio_config_changes(&[], &[], true))
    };
    let mut scan = reader.finish(changes)?;
    scan.schema_version = Some(SOURCE_SCAN_SCHEMA_VERSION);
    Ok(JobResult::Scan { scan })
}

fn ffprobe(ctx: &Ctx, argv: &[String]) -> Cmd {
    let pipes = Pipes {
        stdout: true,
        fds: 0,
    };
    Cmd::new("ffprobe", &ctx.rt.ffprobe, argv, pipes)
}

/// ffprobe's stdout by line until `on_line` says stop (true, the run is stopped); an exit not asked for fails.
async fn lines(
    ctx: &Ctx,
    p: &Ffmpeg,
    mut on_line: impl FnMut(&str) -> bool,
) -> Result<bool, String> {
    let out = p.take_stdout().ok_or("ffprobe has no stdout")?;
    let mut lines = BufReader::new(out).lines();
    while let Ok(Some(l)) = lines.next_line().await {
        ctx.pulse();
        if !l.is_empty() && !on_line(&l) {
            p.stop().await;
            return Ok(true);
        }
    }
    match p.wait().await.code {
        Some(0) => Ok(false),
        code => Err(format!(
            "ffprobe exited {}: {}",
            code.map_or("null".into(), |c| c.to_string()),
            p.stderr_tail().join("\n").trim()
        )),
    }
}

async fn packets(
    ctx: &Ctx,
    p: &Ffmpeg,
    mut reader: PacketReader,
    duration: Option<f64>,
) -> Result<(PacketReader, bool), String> {
    let stopped = lines(ctx, p, |l| {
        let go = reader.feed_line(l);
        if let Some(d) = duration {
            ctx.progress((reader.end_seconds() / d * 100.0).clamp(0.0, 99.0));
        }
        go
    })
    .await?;
    Ok((reader, stopped))
}

async fn reorder_lead(ctx: &Ctx, plan: &ScanPlan, path: &str) -> Result<f64, String> {
    let cmd = ffprobe(ctx, &reorder_args(&plan.video, path));
    let c = ctx.clone();
    ctx.pipeline(REORDER_TIMEOUT_MS, vec![cmd], |p| async move {
        let mut lead = 0.0;
        lines(&c, &p[0], |l| {
            lead = lead_from_probe(l);
            false
        })
        .await
        .map(|_| lead)
    })
    .await?
}

/// The file's bytes into ffprobe and (unix) the ADTS ffmpeg; a sink whose tool ended is dropped.
async fn piped(
    ctx: &Ctx,
    plan: &ScanPlan,
    path: &str,
    size: u64,
    reader: PacketReader,
    duration: Option<f64>,
    timeout: u64,
) -> Result<(PacketReader, std::collections::BTreeMap<u32, bool>), String> {
    let mut cmds = vec![ffprobe(ctx, &packet_args(plan, "pipe:0"))];
    if cfg!(unix) {
        let argv = adts_args(plan, size);
        let pipes = Pipes {
            stdout: false,
            fds: plan.aac.len(),
        };
        cmds.push(Cmd::new("ffmpeg", &ctx.rt.ffmpeg, &argv, pipes));
    }
    let (c, file, tracks) = (ctx.clone(), path.to_string(), plan.aac.len());
    let (reader, walkers, ok) = ctx
        .pipeline(timeout, cmds, |p| async move {
            let adts = p.get(1).cloned();
            let mut walks = vec![];
            for i in 0..tracks {
                let fd = adts.as_ref().and_then(|a| a.take_fd(i));
                walks.push(tokio::spawn(walk(fd)));
            }
            let feeder = tokio::spawn(feed(c.clone(), file, p.clone()));
            let out = packets(&c, &p[0], reader, duration).await;
            match &out {
                Ok((_, true)) | Err(_) => feeder.abort(),
                Ok(_) => {
                    let _ = feeder.await;
                }
            }
            let ok = match &adts {
                Some(a) => a.wait().await.code == Some(0),
                None => false,
            };
            let mut walkers = vec![];
            for w in walks {
                walkers.push(w.await.unwrap_or_default());
            }
            out.map(|(r, _)| (r, walkers, ok))
        })
        .await??;
    Ok((reader, audio_config_changes(&plan.aac, &walkers, ok)))
}

async fn feed(ctx: Ctx, file: String, procs: Vec<Arc<Ffmpeg>>) {
    let Ok(mut f) = tokio::fs::File::open(&file).await else {
        return;
    };
    let mut sinks = vec![];
    for p in &procs {
        sinks.extend(p.take_stdin().await);
    }
    let mut buf = vec![0u8; CHUNK];
    while !sinks.is_empty() {
        let Ok(n @ 1..) = f.read(&mut buf).await else {
            return;
        };
        let mut alive = vec![];
        for mut s in sinks {
            if s.write_all(&buf[..n]).await.is_ok() {
                alive.push(s);
            }
        }
        sinks = alive;
        ctx.pulse();
    }
}

async fn walk(fd: Option<crate::proc::FdReader>) -> AdtsConfigWalker {
    let mut w = AdtsConfigWalker::default();
    if let Some(mut fd) = fd {
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n @ 1..) = fd.read(&mut buf).await {
            w.push(&buf[..n]);
        }
    }
    w
}

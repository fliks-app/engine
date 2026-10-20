//! The `Scan` job: ffprobe over every video packet (keyframes, end, clock break) and, on MPEG-TS
//! with AAC, the ADTS headers a `Demuxer` reads from the same bytes. Spec: `docs/spec/jobs.md` section 6.
use super::{Cmd, Ctx};
use crate::lock::Locked;
use crate::proc::{Ffmpeg, Pipes};
use fliks_engine_core::api::{JobResult, MediaInfo, SOURCE_SCAN_SCHEMA_VERSION};
use fliks_engine_core::jobs::scan::*;
use fliks_engine_core::mpegts::Demuxer;
use std::sync::{Arc, Mutex};
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
        piped(ctx, &plan, &path, reader, duration, timeout).await?
    } else {
        let cmd = ffprobe(ctx, &packet_args(&plan, &path));
        let c = ctx.clone();
        let reader = ctx
            .pipeline(timeout, vec![cmd], |p| async move {
                packets(&c, &p[0], reader, duration).await.map(|(r, _)| r)
            })
            .await??;
        (reader, Default::default())
    };
    let mut scan = reader.finish(changes)?;
    scan.schema_version = Some(SOURCE_SCAN_SCHEMA_VERSION);
    Ok(JobResult::Scan { scan })
}

fn ffprobe(ctx: &Ctx, argv: &[String]) -> Cmd {
    Cmd::new("ffprobe", &ctx.rt.ffprobe, argv, Pipes { stdout: true })
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

/// The PID of each AAC track from ffprobe; a track it names none for is read as possibly changing.
async fn aac_pids_of(ctx: &Ctx, plan: &ScanPlan, path: &str) -> Result<Vec<Option<u16>>, String> {
    let cmd = ffprobe(ctx, &pid_args(path));
    let c = ctx.clone();
    let out = ctx
        .pipeline(REORDER_TIMEOUT_MS, vec![cmd], |p| async move {
            let mut out = vec![];
            let r = lines(&c, &p[0], |l| {
                out.push(l.to_string());
                true
            })
            .await;
            r.map(|_| out)
        })
        .await?;
    Ok(aac_pids(plan, out.iter().flatten().map(String::as_str)))
}

/// The file's bytes into ffprobe and the demuxer; ffprobe stopping (clock break) stops the read.
async fn piped(
    ctx: &Ctx,
    plan: &ScanPlan,
    path: &str,
    reader: PacketReader,
    duration: Option<f64>,
    timeout: u64,
) -> Result<(PacketReader, std::collections::BTreeMap<u32, bool>), String> {
    let pids = aac_pids_of(ctx, plan, path).await?;
    let demux = Arc::new(Mutex::new(Demuxer::new(&pids)));
    let cmds = vec![ffprobe(ctx, &packet_args(plan, "pipe:0"))];
    let (c, file, d) = (ctx.clone(), path.to_string(), demux.clone());
    let (reader, walkers) = ctx
        .pipeline(timeout, cmds, |p| async move {
            let feeder = tokio::spawn(feed(c.clone(), file, p[0].clone(), d.clone()));
            let out = packets(&c, &p[0], reader, duration).await;
            match &out {
                Ok((_, true)) | Err(_) => feeder.abort(),
                Ok(_) => {
                    let _ = feeder.await;
                }
            }
            out.map(|(r, _)| (r, d.locked().finish()))
        })
        .await??;
    Ok((reader, audio_config_changes(&plan.aac, &walkers)))
}

async fn feed(ctx: Ctx, file: String, ffprobe: Arc<Ffmpeg>, demux: Arc<Mutex<Demuxer>>) {
    let (Ok(mut f), Some(mut sink)) = (
        tokio::fs::File::open(&file).await,
        ffprobe.take_stdin().await,
    ) else {
        return;
    };
    let mut buf = vec![0u8; CHUNK];
    while let Ok(n @ 1..) = f.read(&mut buf).await {
        demux.locked().push(&buf[..n]);
        if sink.write_all(&buf[..n]).await.is_err() {
            return;
        }
        ctx.pulse();
    }
}

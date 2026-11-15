//! The `Sprite` job: every frame in one decoding pass (one frame per seek when it fails), tiled
//! into `sprite.jpg`. Spec: `docs/spec/jobs.md` section 5.
use super::scan::{ffprobe, lines};
use super::{CANCELLED, Cmd, Ctx, PROMOTED, tail};
use crate::cache::remove_dir;
use crate::proc::{Ffmpeg, Pipes};
use fliks_engine_core::api::{HwAccel, JobResult, MediaInfo, Platform, SpriteDecode, SpriteMeta};
use fliks_engine_core::hw::platform_from_os;
use fliks_engine_core::jobs::merge::progress_seconds;
use fliks_engine_core::jobs::sprite::*;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed, Ordering::SeqCst};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::task::JoinSet;

pub async fn run(ctx: &Ctx, info: &MediaInfo) -> Result<JobResult, String> {
    let dir = ctx.rt.cache.sprite_dir(ctx.job.req.media.media_file_id);
    let existing = tokio::fs::read(dir.join("sprite.json")).await.ok();
    if !ctx.job.req.force
        && let Some(sprite) = existing.and_then(|b| serde_json::from_slice(&b).ok())
    {
        return Ok(JobResult::Sprite { sprite });
    }
    ctx.check_disk(&dir).await?;
    let frames = dir.join(format!("frames-{}", crate::random_hex(4)));
    tokio::fs::create_dir_all(&frames)
        .await
        .map_err(|e| format!("mkdir {}: {e}", frames.display()))?;
    let out = generate(ctx, info, &dir, &frames).await;
    let _ = tokio::task::spawn_blocking(move || remove_dir(&frames)).await;
    out.map(|sprite| JobResult::Sprite { sprite })
}

async fn generate(
    ctx: &Ctx,
    info: &MediaInfo,
    dir: &Path,
    frames: &Path,
) -> Result<SpriteMeta, String> {
    let l = layout(info.duration_seconds.unwrap_or(0.0));
    let pass = single_pass(ctx, info, l, frames).await;
    if ctx.cancelled() {
        return Err(CANCELLED.into());
    }
    let (done, failed) = match pass {
        Ok(done) if done > 0 => (done, l.count.saturating_sub(done)),
        // Promoted: the job starts over on a playback slot, the pass with it.
        Err(e) if ctx.job.promoted.load(Relaxed) => return Err(e),
        other => {
            let why = other.err().unwrap_or_else(|| "no frame written".into());
            tracing::warn!(
                job = ctx.job.id,
                "sprite single pass failed, extracting by seek: {why}"
            );
            extract(ctx, info, l, frames).await
        }
    };
    if ctx.cancelled() {
        return Err(CANCELLED.into());
    }
    if done == 0 {
        return Err(format!(
            "Sprite failed: no frame could be extracted ({failed} attempts)"
        ));
    }
    if failed > 0 {
        let present: Vec<bool> = (0..l.count)
            .map(|i| frames.join(frame_name(i)).exists())
            .collect();
        for (missing, src) in backfill(&present) {
            let (from, to) = (frame_name(src as u32), frame_name(missing as u32));
            tokio::fs::copy(frames.join(from), frames.join(to))
                .await
                .map_err(|e| format!("backfill: {e}"))?;
        }
    }
    let tmp = dir.join("sprite-tmp.jpg");
    let pattern = frames.join(FRAME_PATTERN);
    let argv = tile_args(&pattern.to_string_lossy(), &tmp.to_string_lossy(), l.rows);
    ctx.ffmpeg(&argv).await.map_err(|e| format!("tile: {e}"))?;
    let jpg = tokio::fs::read(&tmp)
        .await
        .map_err(|e| format!("tile: {e}"))?;
    let m = meta(l, thumb_height(jpeg_size(&jpg).map(|s| s.1), l.rows));
    let io = |e: std::io::Error| format!("write sprite: {e}");
    tokio::fs::rename(&tmp, dir.join("sprite.jpg"))
        .await
        .map_err(io)?;
    let json = serde_json::to_vec(&m).unwrap_or_default();
    let tmp = dir.join("sprite-tmp.json");
    tokio::fs::write(&tmp, json).await.map_err(io)?;
    tokio::fs::rename(&tmp, dir.join("sprite.json"))
        .await
        .map_err(io)?;
    Ok(m)
}

/// Every frame in one ffmpeg run on one slot, keyframes only when `spriteDecode` says so (`auto`:
/// when the keyframe probe finds one per interval); returns the frames written.
async fn single_pass(ctx: &Ctx, info: &MediaInfo, l: Layout, frames: &Path) -> Result<u32, String> {
    let input = ctx.media_path();
    let video = info.video.first();
    let codec = video.map_or("", |v| v.codec.as_str());
    let mode = ctx.rt.current_config().streaming.sprite_decode;
    // Keyframes alone only when they come out in display order (HEVC: short GOPs), even when
    // asked for: shuffled ones give a wrong sheet, not just a coarser one.
    let keyframes_only = match mode {
        SpriteDecode::Full => false,
        SpriteDecode::Keyframes if keyframes_in_order(codec, &[], None) => true,
        SpriteDecode::Keyframes | SpriteDecode::Auto => {
            let times = probe_keyframes(ctx, &input).await;
            (mode == SpriteDecode::Keyframes || keyframes_suffice(&times, l.interval))
                && keyframes_in_order(codec, &times, video.and_then(frames_per_second))
        }
    };
    if ctx.cancelled() {
        return Err(CANCELLED.into());
    }
    // A probe stopped by a promotion reads as no keyframe: redo the job rather than pick `full`.
    if ctx.job.promoted.load(Relaxed) {
        return Err(PROMOTED.into());
    }
    let (platform, hw, node) = host_hw(ctx);
    let crop = info.video.first().and_then(|v| v.crop.clone());
    // The GPU belongs to playback while it runs. [ts:modules/streaming/thumbnail.service.ts:103-113]
    let busy = ctx.rt.slots.playback_in_use() > 0;
    let ext = pick_extractor(platform, hw, &node, crop.is_some(), busy);
    let offset = seek_offset(info);
    let pattern = frames.join(FRAME_PATTERN);
    let argv = pass_args(
        &ext,
        &PassArgs {
            input_path: &input,
            frames_pattern: &pattern.to_string_lossy(),
            crop: crop.as_ref(),
            thumb_width: THUMB_WIDTH,
            hdr: is_hdr(info),
            layout: l,
            seek_offset: offset,
            keyframes_only,
            codec,
        },
    )?;
    tracing::debug!(job = ctx.job.id, keyframes_only, "sprite single pass");
    let cmd = Cmd::new("ffmpeg", &ctx.rt.ffmpeg, &argv, Pipes::stdout());
    let span = f64::from(l.count) * f64::from(l.interval);
    let c = ctx.clone();
    let timeout = pass_timeout_ms(info.duration_seconds.unwrap_or(0.0));
    ctx.pipeline(timeout, vec![cmd], |p| async move {
        let p = &p[0];
        let (_, exit) = tokio::join!(report(&c, p, offset, span), p.wait());
        match exit.code {
            Some(0) => Ok(()),
            code => Err(format!("ffmpeg exited with {code:?}: {}", tail(p, 2))),
        }
    })
    .await??;
    Ok((0..l.count)
        .filter(|i| frames.join(frame_name(*i)).exists())
        .count() as u32)
}

/// The pass's `-progress` lines: output time over the sheet's span, capped at 99. Only a moving
/// output time pulses the watchdog: ffmpeg keeps printing blocks while its input is stuck.
async fn report(ctx: &Ctx, p: &Ffmpeg, offset: f64, span: f64) {
    let Some(out) = p.take_stdout() else {
        return;
    };
    let mut lines = BufReader::new(out).lines();
    let mut last = None;
    while let Ok(Some(line)) = lines.next_line().await {
        let Some(s) = progress_seconds(&line) else {
            continue;
        };
        if last == Some(s) {
            continue;
        }
        last = Some(s);
        ctx.pulse();
        if span > 0.0 {
            ctx.progress(((s - offset) / span * 100.0).clamp(0.0, 99.0));
        }
    }
}

/// Keyframe times over the file's first seconds; empty when the probe fails (full decode then).
async fn probe_keyframes(ctx: &Ctx, input: &str) -> Vec<f64> {
    let cmd = ffprobe(ctx, &keyframe_probe_args(input));
    let c = ctx.clone();
    let out = ctx
        .pipeline(u64::from(RUN_TIMEOUT_MS), vec![cmd], |p| async move {
            let mut times = vec![];
            let read = lines(&c, &p[0], |l| {
                times.extend(keyframe_time(l));
                true
            })
            .await;
            read.map(|_| times)
        })
        .await;
    match out {
        Ok(Ok(times)) => times,
        Ok(Err(e)) | Err(e) => {
            tracing::warn!(job = ctx.job.id, "sprite keyframe probe failed: {e}");
            vec![]
        }
    }
}

/// The detected platform, acceleration and render node; software when detection has not run.
fn host_hw(ctx: &Ctx) -> (Platform, HwAccel, String) {
    ctx.rt.hw.snapshot().map_or_else(
        || {
            (
                platform_from_os(std::env::consts::OS),
                HwAccel::None,
                String::new(),
            )
        },
        |s| (s.detected.platform, s.detected.hw_accel, s.render_node),
    )
}

/// Frames by seek, `min(slots, count)` workers; returns (done, failed). The single pass's
/// fallback. [ts:modules/streaming/thumbnail.service.ts:495-570]
async fn extract(ctx: &Ctx, info: &MediaInfo, l: Layout, frames: &Path) -> (u32, u32) {
    let offset = seek_offset(info);
    let hdr = is_hdr(info);
    let crop = info.video.first().and_then(|v| v.crop.clone());
    let input = ctx.media_path();
    let (platform, hw, node) = host_hw(ctx);
    let next = Arc::new(AtomicU32::new(0));
    let counts = Arc::new((AtomicU32::new(0), AtomicU32::new(0)));
    let workers = ctx.rt.slots.size().min(l.count as usize).max(1);
    let mut set = JoinSet::new();
    for _ in 0..workers {
        let (ctx, next, counts) = (ctx.clone(), next.clone(), counts.clone());
        let (crop, input, node, frames) = (
            crop.clone(),
            input.clone(),
            node.clone(),
            frames.to_path_buf(),
        );
        set.spawn(async move {
            loop {
                let i = next.fetch_add(1, SeqCst);
                if i >= l.count || ctx.cancelled() {
                    return;
                }
                // The GPU belongs to playback while it runs. [ts:modules/streaming/thumbnail.service.ts:103-113]
                let busy = ctx.rt.slots.playback_in_use() > 0;
                let ext = pick_extractor(platform, hw, &node, crop.is_some(), busy);
                let out = frames.join(frame_name(i));
                let args = FrameArgs {
                    input_path: &input,
                    seek_seconds: f64::from(i) * f64::from(l.interval) + offset,
                    output_path: &out.to_string_lossy(),
                    crop: crop.as_ref(),
                    thumb_width: THUMB_WIDTH,
                    hdr,
                };
                let r = match frame_args(&ext, &args) {
                    Ok(argv) => ctx.ffmpeg(&argv).await,
                    Err(e) => Err(e),
                };
                match r {
                    Ok(()) => {
                        counts.0.fetch_add(1, SeqCst);
                    }
                    Err(_) if ctx.cancelled() => return,
                    Err(e) => {
                        let f = counts.1.fetch_add(1, SeqCst) + 1;
                        if f <= 3 {
                            tracing::warn!(
                                job = ctx.job.id,
                                frame = i + 1,
                                "sprite frame failed: {e}"
                            );
                        }
                    }
                }
                let n = counts.0.load(SeqCst) + counts.1.load(SeqCst);
                ctx.progress((f64::from(n) / f64::from(l.count) * 100.0).min(99.0));
            }
        });
    }
    while set.join_next().await.is_some() {}
    (counts.0.load(SeqCst), counts.1.load(SeqCst))
}

//! The `Sprite` job: one frame per seek on background slots, tiled into `sprite.jpg`.
//! Spec: `docs/spec/jobs.md` section 5.
use super::{CANCELLED, Ctx};
use crate::cache::remove_dir;
use fliks_engine_core::api::{HwAccel, JobResult, MediaInfo, SpriteMeta};
use fliks_engine_core::hw::platform_from_os;
use fliks_engine_core::jobs::sprite::*;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
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
    let (done, failed) = extract(ctx, info, l, frames).await;
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
    tokio::fs::write(dir.join("sprite.json"), json)
        .await
        .map_err(io)?;
    Ok(m)
}

/// Frames by seek, `min(slots, count)` workers; returns (done, failed).
/// [ts:modules/streaming/thumbnail.service.ts:495-570]
async fn extract(ctx: &Ctx, info: &MediaInfo, l: Layout, frames: &Path) -> (u32, u32) {
    let offset = seek_offset(info);
    let hdr = is_hdr(info);
    let crop = info.video.first().and_then(|v| v.crop.clone());
    let input = ctx.media_path();
    let (platform, hw, node) = ctx.rt.hw.snapshot().map_or_else(
        || {
            (
                platform_from_os(std::env::consts::OS),
                HwAccel::None,
                String::new(),
            )
        },
        |s| (s.detected.platform, s.detected.hw_accel, s.render_node),
    );
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

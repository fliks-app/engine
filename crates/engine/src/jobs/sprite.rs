//! The `Sprite` job: every frame in one decoding pass (one frame per seek when it fails, for what
//! it left out, or when seeks decode less), tiled into `sprite.jpg`. A run the hardware fails is
//! redone in software. Spec: `docs/spec/jobs.md` section 5.
use super::scan::{ffprobe, lines};
use super::{CANCELLED, Cmd, Ctx, PROMOTED, tail};
use crate::cache::remove_dir;
use crate::proc::{Ffmpeg, Pipes};
use fliks_engine_core::api::{
    HostCaps, JobResult, MediaInfo, SpriteDecode, SpriteMeta, VideoStreamInfo,
};
use fliks_engine_core::jobs::merge::progress_seconds;
use fliks_engine_core::jobs::sprite::*;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed, Ordering::SeqCst};
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
    if l.count == 0 {
        return Err("Sprite failed: no duration, no frame to extract".into());
    }
    let all: Vec<u32> = (0..l.count).collect();
    let decode = Arc::new(Decode::new(ctx, info));
    let (done, failed) = match plan(ctx, info, l).await? {
        Plan::Seeks => extract(ctx, info, l, frames, all, &decode).await,
        Plan::Pass { keyframes_only } => {
            let pass = decode
                .run(ctx, |ext| async move {
                    // A failed run may leave a half-written frame: each starts from an empty dir.
                    clear_frames(frames).await?;
                    single_pass(ctx, info, l, frames, keyframes_only, &ext).await
                })
                .await;
            if ctx.cancelled() {
                return Err(CANCELLED.into());
            }
            match pass {
                Ok(written) => {
                    // A pass that stopped early (truncated file, clock break): seek the rest.
                    let missing: Vec<u32> = all
                        .into_iter()
                        .filter(|i| !frames.join(frame_name(*i)).exists())
                        .collect();
                    if !missing.is_empty() {
                        tracing::warn!(
                            job = ctx.job.id,
                            "sprite single pass wrote {written} of {} frames, extracting the \
                             rest by seek",
                            l.count
                        );
                    }
                    let (more, failed) = extract(ctx, info, l, frames, missing, &decode).await;
                    (written + more, failed)
                }
                // Promoted: the job starts over on a playback slot, the pass with it.
                Err(e) if ctx.job.promoted.load(Relaxed) => return Err(e),
                Err(e) => {
                    tracing::warn!(
                        job = ctx.job.id,
                        "sprite single pass failed, extracting by seek: {e}"
                    );
                    clear_frames(frames).await?;
                    extract(ctx, info, l, frames, all, &decode).await
                }
            }
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

/// How the frames are extracted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// One ffmpeg run over the file.
    Pass { keyframes_only: bool },
    /// One run per frame: cheaper than a full pass when keyframes alone will not do.
    Seeks,
}

/// `spriteDecode` over the keyframe probe: keyframes only when every probe window has one per
/// interval (`auto`) and they come out in display order (HEVC: short GOPs, even in `keyframes`:
/// shuffled ones give a wrong sheet, not just a coarser one); else a full pass, or per-frame
/// seeks when those decode less (`seeks_cheaper`). `full` always decodes everything.
async fn plan(ctx: &Ctx, info: &MediaInfo, l: Layout) -> Result<Plan, String> {
    let video = info.video.first();
    let codec = video.map_or("", |v| v.codec.as_str());
    let mode = ctx.rt.current_config().streaming.sprite_decode;
    let plan = match mode {
        SpriteDecode::Full => Plan::Pass {
            keyframes_only: false,
        },
        SpriteDecode::Keyframes if keyframes_in_order(codec, &[], None) => Plan::Pass {
            keyframes_only: true,
        },
        SpriteDecode::Keyframes | SpriteDecode::Auto => {
            let duration = info.duration_seconds.unwrap_or(0.0);
            let stream = video.map_or(0, |v| v.stream_index);
            let mut windows = vec![];
            for w in keyframe_probe_windows(duration, l.interval) {
                if ctx.cancelled() {
                    break;
                }
                windows.push(probe_keyframes(ctx, stream, w).await);
            }
            let fps = video.and_then(frames_per_second);
            let suffice = mode == SpriteDecode::Keyframes
                || windows.iter().all(|t| keyframes_suffice(t, l.interval));
            let in_order = windows.iter().all(|t| keyframes_in_order(codec, t, fps));
            if suffice && in_order {
                Plan::Pass {
                    keyframes_only: true,
                }
            } else if seeks_cheaper(&windows, l.count, duration) {
                Plan::Seeks
            } else {
                Plan::Pass {
                    keyframes_only: false,
                }
            }
        }
    };
    if ctx.cancelled() {
        return Err(CANCELLED.into());
    }
    // A probe stopped by a promotion reads as no keyframe: redo the job rather than pick `full`.
    if ctx.job.promoted.load(Relaxed) {
        return Err(PROMOTED.into());
    }
    tracing::debug!(job = ctx.job.id, ?plan, "sprite plan");
    Ok(plan)
}

/// Every frame in one ffmpeg run of `ext` on one slot; returns the frames written.
async fn single_pass(
    ctx: &Ctx,
    info: &MediaInfo,
    l: Layout,
    frames: &Path,
    keyframes_only: bool,
    ext: &Extractor,
) -> Result<u32, String> {
    let input = ctx.media_path();
    let video = info.video.first();
    let codec = video.map_or("", |v| v.codec.as_str());
    let offset = seek_offset(info);
    let pattern = frames.join(FRAME_PATTERN);
    let argv = pass_args(
        ext,
        &PassArgs {
            input_path: &input,
            frames_pattern: &pattern.to_string_lossy(),
            crop: video.and_then(|v| v.crop.as_ref()),
            thumb_width: THUMB_WIDTH,
            hdr: is_hdr(info),
            layout: l,
            seek_offset: offset,
            keyframes_only,
            codec,
            stream_index: video.map_or(0, |v| v.stream_index),
        },
    )?;
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

/// Keyframe times over one probe window; empty when the probe fails (no keyframes only then).
async fn probe_keyframes(ctx: &Ctx, stream: u32, window: (f64, f64)) -> Vec<f64> {
    let cmd = ffprobe(ctx, &keyframe_probe_args(&ctx.media_path(), stream, window));
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

/// What a job's runs decode with: the host's extractor for the video, software once a hardware
/// run has failed (the boot probe tests a codec, not this file's profile or pixel format).
struct Decode {
    /// The detected host; a CPU-only one while detection has not run.
    caps: HostCaps,
    render_node: String,
    video: Option<VideoStreamInfo>,
    hw_failed: AtomicBool,
}

impl Decode {
    fn new(ctx: &Ctx, info: &MediaInfo) -> Self {
        let (caps, render_node) = ctx.rt.hw.host();
        Self {
            caps,
            render_node,
            video: info.video.first().cloned(),
            hw_failed: AtomicBool::new(false),
        }
    }

    fn extractor(&self, ctx: &Ctx) -> Extractor {
        // The GPU belongs to playback while it runs. [ts:modules/streaming/thumbnail.service.ts:103-113]
        let busy = ctx.rt.slots.playback_in_use() > 0;
        let software = busy || self.hw_failed.load(Relaxed);
        pick_extractor(&self.caps, &self.render_node, self.video.as_ref(), software)
    }

    /// `run` on the job's extractor. A hardware run that fails is redone in software, and so is
    /// every later run of the job: one ffmpeg per frame must not fail on the GPU frame after frame.
    async fn run<T, Fut>(&self, ctx: &Ctx, run: impl Fn(Extractor) -> Fut) -> Result<T, String>
    where
        Fut: Future<Output = Result<T, String>>,
    {
        let ext = self.extractor(ctx);
        let hardware = ext != Extractor::Software;
        match run(ext).await {
            // A cancelled or promoted run was stopped, not refused by the GPU.
            Err(e) if hardware && !ctx.cancelled() && !ctx.job.promoted.load(Relaxed) => {
                if !self.hw_failed.swap(true, Relaxed) {
                    tracing::warn!(
                        job = ctx.job.id,
                        "sprite hardware run failed, software from here: {e}"
                    );
                }
                run(Extractor::Software).await
            }
            out => out,
        }
    }
}

/// The frames a failed run left, removed before another redoes them.
async fn clear_frames(frames: &Path) -> Result<(), String> {
    let io = |e: std::io::Error| format!("clear {}: {e}", frames.display());
    let mut dir = tokio::fs::read_dir(frames).await.map_err(io)?;
    while let Some(entry) = dir.next_entry().await.map_err(io)? {
        tokio::fs::remove_file(entry.path()).await.map_err(io)?;
    }
    Ok(())
}

/// Frames `todo` by seek, `min(slots, todo)` workers; returns (done, failed). Long-GOP files,
/// what a pass left out, and its fallback. [ts:modules/streaming/thumbnail.service.ts:495-570]
/// Stops when its first `GIVE_UP_AFTER` runs all failed.
async fn extract(
    ctx: &Ctx,
    info: &MediaInfo,
    l: Layout,
    frames: &Path,
    todo: Vec<u32>,
    decode: &Arc<Decode>,
) -> (u32, u32) {
    if todo.is_empty() {
        return (0, 0);
    }
    let total = todo.len() as u32;
    let todo = Arc::new(todo);
    let offset = seek_offset(info);
    let hdr = is_hdr(info);
    let crop = info.video.first().and_then(|v| v.crop.clone());
    let input = ctx.media_path();
    let next = Arc::new(AtomicU32::new(0));
    let counts = Arc::new((AtomicU32::new(0), AtomicU32::new(0)));
    let workers = ctx.rt.slots.size().min(todo.len()).max(1);
    let mut set = JoinSet::new();
    for _ in 0..workers {
        let (ctx, next, counts, todo) = (ctx.clone(), next.clone(), counts.clone(), todo.clone());
        let (crop, input, decode, frames) = (
            crop.clone(),
            input.clone(),
            decode.clone(),
            frames.to_path_buf(),
        );
        set.spawn(async move {
            loop {
                let k = next.fetch_add(1, SeqCst);
                let hopeless = counts.0.load(SeqCst) == 0 && counts.1.load(SeqCst) >= GIVE_UP_AFTER;
                if k >= total || ctx.cancelled() || hopeless {
                    return;
                }
                let i = todo[k as usize];
                let out = frames.join(frame_name(i));
                let args = FrameArgs {
                    input_path: &input,
                    seek_seconds: f64::from(i) * f64::from(l.interval) + offset,
                    output_path: &out.to_string_lossy(),
                    crop: crop.as_ref(),
                    thumb_width: THUMB_WIDTH,
                    hdr,
                };
                let (c, args) = (&ctx, &args);
                let r = decode
                    .run(
                        c,
                        |ext| async move { c.ffmpeg(&frame_args(&ext, args)?).await },
                    )
                    .await;
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
                ctx.progress((f64::from(n) / f64::from(total) * 100.0).min(99.0));
            }
        });
    }
    while set.join_next().await.is_some() {}
    (counts.0.load(SeqCst), counts.1.load(SeqCst))
}

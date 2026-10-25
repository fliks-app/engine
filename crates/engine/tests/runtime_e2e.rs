//! Runtime against real ffmpeg (`FFMPEG_PATH`, `FFPROBE_PATH`: `tools/probe-tools.sh`). Ignored
//! unless `ENGINE_E2E` is set when building: `ENGINE_E2E=1 cargo test -p fliks-engine --test runtime_e2e`.
#![cfg(unix)]
mod runtime_common;
use fliks_engine_core::api::{EngineEvent, HwAccel, MediaInfo};
use fliks_engine_core::mp4::{first_tfdt, parse_init_tracks};
use runtime_common::*;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

struct Play {
    e: Engine,
    file: PathBuf,
    info: MediaInfo,
}

async fn play_with(e: Engine, id: u32, file: PathBuf) -> Play {
    e.stub.file(id, &file);
    e.stub.grant("alice-jwt", "alice");
    e.stub.grant("bob-jwt", "bob");
    let info = e.probe(id, &file).await;
    Play { e, file, info }
}

async fn play(file: PathBuf) -> Play {
    play_with(engine().await, 1, file).await
}

impl Play {
    async fn open(&self, owner: &str, extra: Value) -> String {
        let s = self.e.open(1, &self.file, &self.info, owner, extra).await;
        s["sessionId"].as_str().unwrap().to_string()
    }

    /// A segment GET that retries a 503 the way players do.
    async fn seg(&self, sid: &str, cred: &str, quality: &str, name: &str) -> Res {
        let path = format!("/api/stream/1/{quality}/{name}?sid={sid}");
        for _ in 0..5 {
            let r = self.e.get(&path, cred, &[]).await;
            if r.status != 503 {
                return r;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        self.e.get(&path, cred, &[]).await
    }

    async fn job(&self, sid: &str) -> Value {
        self.e
            .control("GET", &format!("/v1/sessions/{sid}"), None)
            .await
            .json()["job"]
            .clone()
    }
}

/// Decode time of a segment's first fragment of `track`, in seconds.
fn tfdt_seconds(init: &[u8], seg: &[u8], video: bool) -> f64 {
    let tracks = parse_init_tracks(init).unwrap();
    let (id, t) = tracks
        .iter()
        .find(|(_, t)| t.is_video == video)
        .copied()
        .unwrap();
    first_tfdt(seg, id).unwrap().unwrap() as f64 / f64::from(t.timescale)
}

fn seg_name(n: u32) -> String {
    format!("seg-{n:04}.m4s")
}

/// 24 fps, 3 s segments: 72 frames.
const SEG: f64 = 3.0;

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn master_variant_segments_with_tfdt_continuity() {
    let p = play(h264_clip(10)).await;
    let sid = p.open("alice", json!({})).await;
    let master =
        p.e.get(
            &format!("/api/stream/1/master.m3u8?sid={sid}"),
            "alice-jwt",
            &[],
        )
        .await;
    assert!(
        master.text().contains("720p/index.m3u8"),
        "{}",
        master.text()
    );
    let v =
        p.e.get(
            &format!("/api/stream/1/720p/index.m3u8?sid={sid}"),
            "alice-jwt",
            &[],
        )
        .await
        .text();
    assert_eq!(v.matches("#EXTINF").count(), 4, "{v}");
    let init = p.seg(&sid, "alice-jwt", "720p", "init.mp4").await;
    assert_eq!(init.status, 200);
    assert_eq!(init.header("content-type").as_deref(), Some("video/mp4"));
    for n in 0..4 {
        let s = p.seg(&sid, "alice-jwt", "720p", &seg_name(n)).await;
        assert_eq!(s.status, 200, "seg {n}: {}", s.text());
        assert_eq!(s.header("cache-control").as_deref(), Some("no-store"));
        assert_eq!(
            s.header("access-control-allow-origin").as_deref(),
            Some("*")
        );
        let t = tfdt_seconds(&init.body, &s.body, true);
        assert!(
            (t - f64::from(n) * SEG).abs() < 0.05,
            "seg {n} video tfdt {t}"
        );
        let a = tfdt_seconds(&init.body, &s.body, false);
        assert!(
            (a - f64::from(n) * SEG).abs() < 0.1,
            "seg {n} audio tfdt {a}"
        );
    }
    let job = p.job(&sid).await;
    assert_eq!(job["quality"], "720p");
    assert_eq!(job["actualHw"], "none");
    // A segment past the end of a finished run is a 404.
    until_async("the run ends", 60, || async {
        p.job(&sid).await["state"] == "done"
    })
    .await;
    assert_eq!(
        p.seg(&sid, "alice-jwt", "720p", &seg_name(9)).await.status,
        404
    );
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn seek_restarts_the_run_and_keeps_the_timeline() {
    let p = play(h264_clip(120)).await;
    let sid = p.open("alice", json!({})).await;
    let init = p.seg(&sid, "alice-jwt", "720p", "init.mp4").await;
    assert_eq!(
        p.seg(&sid, "alice-jwt", "720p", &seg_name(0)).await.status,
        200
    );
    // Freeze the frontier so the seek is out of reach.
    let cmd =
        p.e.control(
            "POST",
            &format!("/v1/sessions/{sid}/command"),
            Some(json!({ "action": "pauseJob" })),
        )
        .await;
    assert_eq!(cmd.status, 204);
    assert_eq!(p.job(&sid).await["state"], "paused");
    let s = p.seg(&sid, "alice-jwt", "720p", &seg_name(35)).await;
    assert_eq!(s.status, 200, "{}", s.text());
    let t = tfdt_seconds(&init.body, &s.body, true);
    assert!((t - 35.0 * SEG).abs() < 0.05, "tfdt {t}");
    let job = p.job(&sid).await;
    assert_eq!(
        (job["startSegment"].as_u64(), job["restarts"].as_u64()),
        (Some(35), Some(1))
    );
    assert_eq!(job["state"], "running");
    // The earlier segment is still served from the cache.
    assert_eq!(
        p.seg(&sid, "alice-jwt", "720p", &seg_name(0)).await.status,
        200
    );
    p.e.rt.shutdown().await;
    assert!(p.e.ffmpeg_pids().is_empty());
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn quality_switch_moves_the_job_and_reports_the_pipeline() {
    let p = play(h264_clip(60)).await;
    let mut ev = p.e.rt.events.subscribe();
    let sid = p.open("alice", json!({})).await;
    assert_eq!(
        p.seg(&sid, "alice-jwt", "720p", &seg_name(0)).await.status,
        200
    );
    let init = p.seg(&sid, "alice-jwt", "480p", "init.mp4").await;
    let s = p.seg(&sid, "alice-jwt", "480p", &seg_name(4)).await;
    assert_eq!(s.status, 200);
    let t = tfdt_seconds(&init.body, &s.body, true);
    assert!((t - 4.0 * SEG).abs() < 0.05, "tfdt {t}");
    assert_eq!(p.job(&sid).await["quality"], "480p");
    // Back to 720p: seg 0 of the earlier rung is still on disk.
    let back = p.seg(&sid, "alice-jwt", "720p", &seg_name(0)).await;
    assert_eq!(back.status, 200);
    assert_eq!(
        p.job(&sid).await["quality"],
        "480p",
        "a cached rung is served without a switch"
    );
    let mut rungs = vec![];
    while let Ok(e) = ev.try_recv() {
        if let EngineEvent::PipelineChanged { quality, .. } = e {
            rungs.push(quality);
        }
    }
    assert_eq!(rungs, ["720p", "480p", "720p"]);
    p.e.rt.shutdown().await;
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn a_killed_run_answers_503_then_the_retry_200() {
    // A slow ffmpeg start keeps the first request waiting while a rung switch kills its run.
    let dir = temp_dir("slow");
    let slow = dir.join("ffmpeg");
    std::fs::write(
        &slow,
        format!("#!/bin/sh\nsleep 2\nexec {} \"$@\"\n", ffmpeg()),
    )
    .unwrap();
    std::fs::set_permissions(&slow, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let e = engine_with(
        host(HwAccel::None, "/dev/dri/renderD128", "signal"),
        slow.to_str().unwrap(),
        json!({}),
    )
    .await;
    let p = play_with(e, 1, h264_clip(10)).await;
    let sid = p.open("alice", json!({})).await;
    let first = {
        let (sid, base) = (sid.clone(), p.e.base.clone());
        tokio::spawn(async move {
            http(
                "GET",
                &format!(
                    "{base}/api/stream/1/720p/{}?sid={sid}&token=alice-jwt",
                    seg_name(2)
                ),
                &[],
                None,
            )
            .await
        })
    };
    // The first request waits on its run: the run is registered.
    until_async("the 720p run", 60, || async {
        !p.job(&sid).await.is_null()
    })
    .await;
    let second = p.seg(&sid, "alice-jwt", "480p", &seg_name(0)).await;
    assert_eq!(second.status, 200, "{}", p.job(&sid).await);
    let r = first.await.unwrap();
    assert_eq!(r.status, 503);
    assert_eq!(r.header("retry-after").as_deref(), Some("2"));
    let again = p.seg(&sid, "alice-jwt", "720p", &seg_name(2)).await;
    assert_eq!(again.status, 200);
    p.e.rt.shutdown().await;
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn idle_gc_kills_the_job_and_leaves_no_ffmpeg() {
    let lifetime =
        json!({ "liveSessionTtlMs": 300, "jobGraceMs": 300, "sessionReviveTtlMs": 60000 });
    let e = engine_with(
        host(HwAccel::None, "/dev/dri/renderD128", "signal"),
        &ffmpeg(),
        lifetime,
    )
    .await;
    let p = play_with(e, 1, h264_clip(120)).await;
    let mut ev = p.e.rt.events.subscribe();
    let sid = p.open("alice", json!({})).await;
    assert_eq!(
        p.seg(&sid, "alice-jwt", "720p", &seg_name(0)).await.status,
        200
    );
    p.e.control(
        "POST",
        &format!("/v1/sessions/{sid}/command"),
        Some(json!({ "action": "pauseJob" })),
    )
    .await;
    assert_eq!(p.e.ffmpeg_pids().len(), 1);
    let hb = json!({ "positionS": 1.0, "state": "playing" });
    p.e.control("POST", &format!("/v1/sessions/{sid}/heartbeat"), Some(hb))
        .await;
    p.e.rt.gc_tick().await;
    // No heartbeat since: the session expires, then the job loses its last viewer and is reaped.
    until_async("session expired, job reaped", 60, || async {
        p.e.rt.gc_tick().await;
        p.e.ffmpeg_pids().is_empty()
    })
    .await;
    assert!(
        p.e.control("GET", "/v1/sessions", None)
            .await
            .json()
            .as_array()
            .unwrap()
            .is_empty()
    );
    let mut reasons = vec![];
    for _ in 0..2 {
        let e = event(&mut ev, 60, |e| {
            matches!(
                e,
                EngineEvent::SessionEnded { .. } | EngineEvent::JobEnded { .. }
            )
        })
        .await;
        match e {
            EngineEvent::SessionEnded { reason, .. } => reasons.push(reason),
            EngineEvent::JobEnded { state, .. } => reasons.push(format!("{state:?}")),
            _ => {}
        }
    }
    assert!(
        reasons.contains(&"expired".into()) && reasons.contains(&"Killed".into()),
        "{reasons:?}"
    );
    // The sid revives within the revive window and the next segment respawns.
    let s = p.seg(&sid, "alice-jwt", "720p", &seg_name(1)).await;
    assert_eq!(s.status, 200);
    p.e.rt.shutdown().await;
    assert!(p.e.ffmpeg_pids().is_empty());
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn revoke_answers_410_and_kills_the_job() {
    let p = play(h264_clip(120)).await;
    let sid = p.open("alice", json!({})).await;
    assert_eq!(
        p.seg(&sid, "alice-jwt", "720p", &seg_name(0)).await.status,
        200
    );
    assert_eq!(p.e.ffmpeg_pids().len(), 1);
    let r =
        p.e.control(
            "POST",
            "/v1/revoke",
            Some(json!({ "ownerId": "alice", "mediaFileIds": [1] })),
        )
        .await;
    assert_eq!(r.json()["sessions"], 1);
    let s = p.seg(&sid, "alice-jwt", "720p", &seg_name(1)).await;
    assert_eq!(s.status, 410);
    assert_eq!(s.json()["code"], "session_expired");
    assert!(p.e.ffmpeg_pids().is_empty());
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn hardware_failure_falls_back_to_the_cpu() {
    // A VAAPI host whose render node does not exist: the hardware run dies before seg 0.
    let e = engine_with(
        host(HwAccel::Vaapi, "/dev/dri/renderD199", "signal"),
        &ffmpeg(),
        json!({}),
    )
    .await;
    let p = play_with(e, 1, h264_clip(10)).await;
    let mut ev = p.e.rt.events.subscribe();
    let sid = p.open("alice", json!({})).await;
    let s = p.seg(&sid, "alice-jwt", "720p", &seg_name(0)).await;
    assert_eq!(s.status, 200, "{}", s.text());
    let job = p.job(&sid).await;
    assert_eq!(
        (job["requestedHw"].as_str(), job["actualHw"].as_str()),
        (Some("vaapi"), Some("none"))
    );
    let mut fell_back = false;
    while let Ok(e) = ev.try_recv() {
        fell_back |= matches!(
            e,
            EngineEvent::HwFallback {
                from: HwAccel::Vaapi,
                to: HwAccel::None,
                ..
            }
        );
    }
    assert!(fell_back);
    p.e.rt.shutdown().await;
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn hdr10_hevc_is_tonemapped_on_the_cpu() {
    let p = play(hevc_hdr_clip()).await;
    assert_eq!(
        p.info.video[0].hdr_format,
        Some(fliks_engine_core::api::HdrFormat::Hdr10)
    );
    let sid = p.open("alice", json!({})).await;
    let master =
        p.e.get(
            &format!("/api/stream/1/master.m3u8?sid={sid}"),
            "alice-jwt",
            &[],
        )
        .await
        .text();
    assert!(!master.contains("-hdr/"), "{master}");
    let init = p.seg(&sid, "alice-jwt", "720p", "init.mp4").await;
    assert_eq!(init.status, 200);
    let s = p.seg(&sid, "alice-jwt", "720p", &seg_name(1)).await;
    assert_eq!(s.status, 200);
    assert!((tfdt_seconds(&init.body, &s.body, true) - SEG).abs() < 0.05);
    p.e.rt.shutdown().await;
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn multi_audio_renditions_ride_the_video_run() {
    let p = play(multi_audio_clip()).await;
    let sid = p.open("alice", json!({})).await;
    let master =
        p.e.get(
            &format!("/api/stream/1/master.m3u8?sid={sid}"),
            "alice-jwt",
            &[],
        )
        .await
        .text();
    assert_eq!(master.matches("TYPE=AUDIO").count(), 2, "{master}");
    let audio =
        p.e.get(
            &format!("/api/stream/1/audio/1/index.m3u8?sid={sid}"),
            "alice-jwt",
            &[],
        )
        .await
        .text();
    assert!(
        audio.contains("/api/stream/1/audio/1/init_2.mp4?token=alice-jwt"),
        "{audio}"
    );
    let vinit = p.seg(&sid, "alice-jwt", "720p", "init_0.mp4").await;
    assert_eq!(vinit.status, 200);
    let v = p.seg(&sid, "alice-jwt", "720p", &seg_name(1)).await;
    assert_eq!(v.status, 200);
    let ainit =
        p.e.get(
            &format!("/api/stream/1/audio/1/init_2.mp4?sid={sid}"),
            "alice-jwt",
            &[],
        )
        .await;
    assert_eq!(ainit.status, 200);
    let a =
        p.e.get(
            &format!("/api/stream/1/audio/1/{}?sid={sid}", seg_name(1)),
            "alice-jwt",
            &[],
        )
        .await;
    assert_eq!(a.status, 200);
    let t = tfdt_seconds(&ainit.body, &a.body, false);
    assert!((t - SEG).abs() < 0.1, "audio tfdt {t}");
    p.e.rt.shutdown().await;
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn subtitles_iframes_and_sprites() {
    let p = play(h264_clip(10)).await;
    let sid = p
        .open(
            "alice",
            json!({ "device": { "supportsIFrameTrickPlay": true } }),
        )
        .await;
    let dir = temp_dir("subs");
    std::fs::write(
        dir.join("a.srt"),
        "1\n00:00:01,000 --> 00:00:02,000\nHello\n",
    )
    .unwrap();
    p.e.stub.subtitles.lock().unwrap().insert(
        5,
        (dir.join("a.srt").to_string_lossy().into(), "srt".into()),
    );
    let vtt =
        p.e.get(
            &format!("/api/stream/1/subtitles/5?sid={sid}"),
            "alice-jwt",
            &[],
        )
        .await;
    assert_eq!(vtt.status, 200);
    let text = vtt.text();
    assert!(
        text.starts_with("WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:00.000\n"),
        "{text}"
    );
    assert!(text.contains("Hello"));
    let dl =
        p.e.get(
            &format!("/api/stream/1/subtitles/5/download?sid={sid}"),
            "alice-jwt",
            &[],
        )
        .await;
    assert!(dl.text().contains("00:00:01,000"));
    let master =
        p.e.get(
            &format!("/api/stream/1/master.m3u8?sid={sid}"),
            "alice-jwt",
            &[],
        )
        .await
        .text();
    assert!(master.contains("iframe/index.m3u8"), "{master}");
    let i =
        p.e.get(
            &format!("/api/stream/1/iframe/{}?sid={sid}", "seg-0001.ts"),
            "alice-jwt",
            &[],
        )
        .await;
    assert_eq!(
        (i.status, i.header("content-type").as_deref()),
        (200, Some("video/mp2t"))
    );
    assert_eq!(i.body[0], 0x47, "MPEG-TS sync byte");
    let sprite_dir = temp_dir("sprite");
    p.e.stub
        .sprites
        .lock()
        .unwrap()
        .insert(1, sprite_dir.join("sprite.jpg").to_string_lossy().into());
    assert_eq!(
        p.e.get("/api/stream/1/thumbnails/sprite.jpg", "alice-jwt", &[])
            .await
            .status,
        404
    );
    std::fs::write(sprite_dir.join("sprite.json"), "{\"interval\":10}").unwrap();
    let meta =
        p.e.get("/api/stream/1/thumbnails/sprite.json", "alice-jwt", &[])
            .await;
    assert_eq!(
        (meta.status, meta.json()["interval"].as_u64()),
        (200, Some(10))
    );
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn sprite_job_on_real_clips_is_served() {
    // The tone-map chain's zscale keeps the height even (`h=-2`).
    for (clip, h) in [(h264_clip(10), 135), (hevc_hdr_clip(), 136)] {
        let p = play(clip).await;
        let mut rx = p.e.rt.events.subscribe();
        let r =
            p.e.control(
                "POST",
                "/v1/jobs",
                Some(json!({ "kind": { "type": "Sprite", "info": p.info },
                    "media": { "mediaFileId": 1, "path": p.file } })),
            )
            .await;
        let id = r.json()["jobId"].as_str().unwrap().to_string();
        let done = loop {
            match tokio::time::timeout(Duration::from_secs(120), rx.recv()).await {
                Ok(Ok(EngineEvent::BgJobDone {
                    job_id,
                    state,
                    result,
                    error,
                })) if job_id == id => {
                    break (state, result, error);
                }
                Ok(_) => {}
                Err(_) => panic!("no BgJobDone"),
            }
        };
        let count = (p.info.duration_seconds.unwrap() / 2.0).ceil() as u32;
        let meta = json!({ "interval": 2, "columns": 10, "thumbWidth": 240, "thumbHeight": h, "count": count });
        let v = serde_json::to_value(&done).unwrap();
        assert_eq!(
            v,
            json!(["done", { "type": "Sprite", "sprite": meta }, null]),
            "{:?}",
            p.file
        );
        let j =
            p.e.get("/api/stream/1/thumbnails/sprite.json", "alice-jwt", &[])
                .await;
        assert_eq!((j.status, j.json()), (200, meta));
        let jpg =
            p.e.get("/api/stream/1/thumbnails/sprite.jpg", "alice-jwt", &[])
                .await;
        assert_eq!(jpg.status, 200);
        assert_eq!(
            fliks_engine_core::jobs::sprite::jpeg_size(&jpg.body),
            Some((2400, h))
        );
        assert!(p.e.ffmpeg_pids().is_empty());
    }
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn direct_play_byte_ranges_of_a_real_file() {
    let p = play(h264_clip(10)).await;
    let bytes = std::fs::read(&p.file).unwrap();
    let r =
        p.e.get("/api/stream/1", "alice-jwt", &[("range", "bytes=1000-")])
            .await;
    assert_eq!(r.status, 206);
    assert_eq!(&r.body[..], &bytes[1000..]);
    let open = p.e.open(1, &p.file, &p.info, "alice", json!({
        "device": { "directPlayProfiles": [{ "containers": ["mkv", "matroska"], "videoCodecs": ["h264"], "audioCodecs": ["aac"] }],
            "rejectCopy": false, "supportsDirectPlay": true }
    })).await;
    assert_eq!(
        open["decision"]["response"]["playMethod"], "DirectPlay",
        "{open}"
    );
    let sid = open["sessionId"].as_str().unwrap();
    let r =
        p.e.get(
            &format!("/api/stream/1?sid={sid}"),
            "alice-jwt",
            &[("range", "bytes=0-9")],
        )
        .await;
    let d = fliks_engine_core::jsnum::js_string(p.info.duration_seconds.unwrap());
    assert_eq!(r.header("x-content-duration"), Some(d));
}

// ---- the six streaming-lifecycle scenarios, against real runs ----

/// GC passes until every session is gone and no run is left.
async fn reap(p: &Play) {
    until_async("sessions expired, runs reaped", 60, || async {
        p.e.rt.gc_tick().await;
        let live = p.e.control("GET", "/v1/sessions", None).await.json();
        live.as_array().unwrap().is_empty() && p.e.ffmpeg_pids().is_empty()
    })
    .await;
}

fn short_life() -> Value {
    json!({ "liveSessionTtlMs": 100, "jobGraceMs": 100, "jobFallbackTimeoutMs": 100 })
}

async fn short_play(file: PathBuf) -> Play {
    let e = engine_with(
        host(HwAccel::None, "/dev/dri/renderD128", "signal"),
        &ffmpeg(),
        short_life(),
    )
    .await;
    play_with(e, 1, file).await
}

fn cache_hits(ev: &mut tokio::sync::broadcast::Receiver<EngineEvent>) -> (usize, usize) {
    let (mut hits, mut starts) = (0, 0);
    while let Ok(e) = ev.try_recv() {
        match e {
            EngineEvent::SegmentServed {
                cache_hit: true, ..
            } => hits += 1,
            EngineEvent::JobStarted { .. } => starts += 1,
            _ => {}
        }
    }
    (hits, starts)
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn lifecycle_pause_then_resume_hits_the_cache() {
    let p = short_play(h264_clip(10)).await;
    let sid = p.open("alice", json!({})).await;
    for n in 0..2 {
        assert_eq!(
            p.seg(&sid, "alice-jwt", "720p", &seg_name(n)).await.status,
            200
        );
    }
    // The pause outlives the session and the job; the cache stays.
    reap(&p).await;
    assert!(
        p.e.control("GET", "/v1/sessions", None)
            .await
            .json()
            .as_array()
            .unwrap()
            .is_empty()
    );
    let mut ev = p.e.rt.events.subscribe();
    let resumed = p.open("alice", json!({})).await;
    assert_ne!(resumed, sid);
    assert_eq!(
        p.seg(&resumed, "alice-jwt", "720p", &seg_name(1))
            .await
            .status,
        200
    );
    assert_eq!(
        cache_hits(&mut ev),
        (1, 0),
        "served from the cache, no new run"
    );
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn lifecycle_profiles_coexist_on_one_owner_and_file() {
    let p = play(h264_clip(10)).await;
    let browser = p.open("alice", json!({})).await;
    let cast = p
        .open("alice", json!({ "device": { "useTs": true } }))
        .await;
    assert_eq!(
        p.seg(&browser, "alice-jwt", "720p", &seg_name(0))
            .await
            .status,
        200
    );
    let ts = p.seg(&cast, "alice-jwt", "720p", "seg-0000.ts").await;
    assert_eq!(
        (ts.status, ts.header("content-type").as_deref()),
        (200, Some("video/MP2T"))
    );
    let cache = p.e.control("GET", "/v1/cache", None).await.json();
    assert_eq!(cache["entries"].as_array().unwrap().len(), 2, "{cache}");
    p.e.control("DELETE", &format!("/v1/sessions/{cast}"), None)
        .await;
    let list =
        p.e.control("GET", "/v1/sessions?ownerId=alice", None)
            .await
            .json();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["sessionId"], browser.as_str());
    assert_eq!(
        p.seg(&browser, "alice-jwt", "720p", &seg_name(1))
            .await
            .status,
        200
    );
    p.e.rt.shutdown().await;
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn lifecycle_close_and_reopen_reattaches_the_cache() {
    let p = play(h264_clip(10)).await;
    let first = p.open("alice", json!({})).await;
    for n in 0..4 {
        assert_eq!(
            p.seg(&first, "alice-jwt", "720p", &seg_name(n))
                .await
                .status,
            200
        );
    }
    p.e.control("DELETE", &format!("/v1/sessions/{first}"), None)
        .await;
    let mut ev = p.e.rt.events.subscribe();
    let again = p.open("alice", json!({})).await;
    assert_ne!(again, first);
    for n in 0..2 {
        assert_eq!(
            p.seg(&again, "alice-jwt", "720p", &seg_name(n))
                .await
                .status,
            200
        );
    }
    assert_eq!(cache_hits(&mut ev).1, 0, "no new run");
}

fn age(dir: &Path, hours: u64) {
    let t = std::time::SystemTime::now() - Duration::from_secs(hours * 3600);
    for e in walk(dir) {
        let f = std::fs::File::options().write(true).open(&e).unwrap();
        f.set_modified(t).unwrap();
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        if e.file_type().unwrap().is_dir() {
            out.extend(walk(&e.path()));
        } else {
            out.push(e.path());
        }
    }
    out
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn lifecycle_ttl_then_lru_eviction() {
    let e = engine_with(
        host(HwAccel::None, "/dev/dri/renderD128", "signal"),
        &ffmpeg(),
        short_life(),
    )
    .await;
    let file = h264_clip(10);
    let p = play_with(e, 1, file.clone()).await;
    for id in [2, 3] {
        p.e.stub.file(id, &file);
    }
    for id in [1u32, 2, 3] {
        let s = p.e.open(id, &file, &p.info, "alice", json!({})).await;
        let sid = s["sessionId"].as_str().unwrap();
        let r =
            p.e.get(
                &format!("/api/stream/{id}/720p/{}?sid={sid}", seg_name(0)),
                "alice-jwt",
                &[],
            )
            .await;
        assert_eq!(r.status, 200);
        p.e.control("DELETE", &format!("/v1/sessions/{sid}"), None)
            .await;
    }
    reap(&p).await;
    let entries = p.e.control("GET", "/v1/cache", None).await.json()["entries"].clone();
    assert_eq!(entries.as_array().unwrap().len(), 3);
    let dir = |id: u32| {
        p.e.rt.cache.entry_dir(
            Some("alice"),
            id,
            entries[0]["profileHash"].as_str().unwrap(),
        )
    };
    age(&dir(2), 5);
    age(&dir(3), 5);
    assert_eq!(p.e.rt.cache_gc().await, 2);
    let left = p.e.control("GET", "/v1/cache", None).await.json()["entries"].clone();
    assert_eq!(
        (
            left.as_array().unwrap().len(),
            left[0]["mediaFileId"].as_u64()
        ),
        (1, Some(1))
    );
    // Over budget: the least recently used non-live entry goes.
    let s = p.e.open(2, &file, &p.info, "alice", json!({})).await;
    let sid = s["sessionId"].as_str().unwrap();
    p.e.get(
        &format!("/api/stream/2/720p/{}?sid={sid}", seg_name(0)),
        "alice-jwt",
        &[],
    )
    .await;
    p.e.control("DELETE", &format!("/v1/sessions/{sid}"), None)
        .await;
    reap(&p).await;
    age(&dir(1), 1);
    let cfg = json!({ "revision": 2, "hostUrl": "http://127.0.0.1:9", "streaming": { "cacheMaxBytes": 1.0 }, "lifetime": short_life() });
    p.e.control("PUT", "/v1/config", Some(cfg)).await;
    assert_eq!(p.e.rt.cache_gc().await, 2);
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn lifecycle_ffmpeg_crash_keeps_the_session_and_respawns() {
    let p = play(h264_clip(120)).await;
    let sid = p.open("alice", json!({})).await;
    assert_eq!(
        p.seg(&sid, "alice-jwt", "720p", &seg_name(0)).await.status,
        200
    );
    let pids = p.e.ffmpeg_pids();
    assert_eq!(pids.len(), 1);
    unsafe { libc::kill(pids[0] as i32, libc::SIGKILL) };
    until_async("the crash is seen", 60, || async {
        p.job(&sid).await["state"] == "failed"
    })
    .await;
    let s = p.seg(&sid, "alice-jwt", "720p", &seg_name(1)).await;
    assert_eq!(s.status, 200);
    let job = p.job(&sid).await;
    assert_eq!(
        (job["state"].as_str(), job["restarts"].as_u64()),
        (Some("running"), Some(1))
    );
    assert_eq!(
        p.e.control("GET", "/v1/cache", None).await.json()["entries"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    p.e.rt.shutdown().await;
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn lifecycle_two_owners_are_isolated() {
    let p = play(h264_clip(10)).await;
    let alice = p.open("alice", json!({})).await;
    let bob = p.open("bob", json!({})).await;
    assert_eq!(
        p.seg(&alice, "alice-jwt", "720p", &seg_name(0))
            .await
            .status,
        200
    );
    assert_eq!(
        p.seg(&bob, "bob-jwt", "720p", &seg_name(0)).await.status,
        200
    );
    assert_eq!(
        p.seg(&bob, "alice-jwt", "720p", &seg_name(0)).await.status,
        403
    );
    let cache = p.e.control("GET", "/v1/cache", None).await.json();
    let mut owners: Vec<&str> = cache["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["ownerId"].as_str().unwrap())
        .collect();
    owners.sort();
    assert_eq!(owners, ["alice", "bob"]);
    p.e.control("DELETE", &format!("/v1/sessions/{alice}"), None)
        .await;
    assert_eq!(
        p.seg(&bob, "bob-jwt", "720p", &seg_name(1)).await.status,
        200
    );
    assert_eq!(
        p.e.control("GET", "/v1/sessions?ownerId=bob", None)
            .await
            .json()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    p.e.rt.shutdown().await;
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn throttle_pauses_a_run_far_ahead_and_resumes_it() {
    let p = play(h264_clip(120)).await;
    let cfg = json!({ "revision": 2, "hostUrl": p.e.rt.current_config().host_url, "hostToken": HOST_TOKEN,
        "streaming": { "throttleThresholdSeconds": 9.0 } });
    assert_eq!(
        p.e.control("PUT", "/v1/config", Some(cfg)).await.status,
        200
    );
    let sid = p.open("alice", json!({})).await;
    assert_eq!(
        p.seg(&sid, "alice-jwt", "720p", &seg_name(0)).await.status,
        200
    );
    // Let the run get more than 9 s ahead of the playhead (segment 0).
    until_async("the frontier is 6 segments ahead", 60, || async {
        p.job(&sid).await["frontierSegment"].as_u64().unwrap_or(0) >= 6
    })
    .await;
    p.e.rt.throttle_tick().await;
    let job = p.job(&sid).await;
    assert_eq!(
        (job["state"].as_str(), job["throttled"].as_bool()),
        (Some("paused"), Some(true)),
        "{job}"
    );
    let frontier = job["frontierSegment"].as_u64().unwrap();
    // The viewer catches up: under half the threshold ahead resumes the run.
    let hb = json!({ "positionS": frontier as f64 * SEG, "state": "playing" });
    p.e.control("POST", &format!("/v1/sessions/{sid}/heartbeat"), Some(hb))
        .await;
    p.e.rt.throttle_tick().await;
    assert_eq!(p.job(&sid).await["state"], "running");
    p.e.rt.shutdown().await;
}

/// A desktop client that copies H.264/AAC but cannot direct play Matroska: DirectStream.
fn copy_client(file: &Path) -> Value {
    json!({
        "device": {
            "directPlayProfiles": [{ "containers": ["mp4"], "videoCodecs": ["h264"], "audioCodecs": ["aac"] }],
            "codecConditions": [{ "codec": "h264", "maxBitDepth": 8, "maxLevel": 52, "profiles": ["baseline", "main", "high"] }],
            "rejectCopy": false, "maxAudioChannels": 2, "deviceType": "desktop",
        },
        "scan": scan(file),
    })
}

fn extinf(playlist: &str) -> Vec<f64> {
    playlist
        .lines()
        .filter_map(|l| l.strip_prefix("#EXTINF:"))
        .map(|l| l.trim_end_matches(',').parse().unwrap())
        .collect()
}

/// Start time of each segment: the running sum of the playlist's durations.
fn starts(durations: &[f64]) -> Vec<f64> {
    durations
        .iter()
        .scan(0.0, |t, d| {
            let s = *t;
            *t += d;
            Some(s)
        })
        .collect()
}

impl Play {
    async fn text(&self, path: &str) -> String {
        let r = self.e.get(path, "alice-jwt", &[]).await;
        assert_eq!(r.status, 200, "{path}: {}", r.text());
        r.text()
    }

    async fn open_remux(&self) -> String {
        let s = self
            .e
            .open(1, &self.file, &self.info, "alice", copy_client(&self.file))
            .await;
        assert_eq!(s["decision"]["response"]["playMethod"], "DirectStream");
        s["sessionId"].as_str().unwrap().to_string()
    }
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn remux_variant_and_audio_renditions_on_the_keyframe_grid() {
    let p = play(multi_audio_clip()).await;
    let sid = p.open_remux().await;
    let master = p
        .text(&format!("/api/stream/1/master.m3u8?sid={sid}&remux=1"))
        .await;
    assert!(
        master.contains("/api/stream/1/remux/index.m3u8"),
        "{master}"
    );
    assert!(
        master.contains("/api/stream/1/audio/1/index.m3u8"),
        "{master}"
    );
    let video = extinf(
        &p.text(&format!("/api/stream/1/remux/index.m3u8?sid={sid}"))
            .await,
    );
    assert!((video.iter().sum::<f64>() - 10.0).abs() < 0.1, "{video:?}");
    let init = p.seg(&sid, "alice-jwt", "remux", "init.mp4").await;
    assert_eq!(init.status, 200);
    for (n, t0) in starts(&video).into_iter().enumerate() {
        let s = p.seg(&sid, "alice-jwt", "remux", &seg_name(n as u32)).await;
        assert_eq!(s.status, 200, "seg {n}: {}", s.text());
        let t = tfdt_seconds(&init.body, &s.body, true);
        assert!((t - t0).abs() < 0.05, "seg {n} video tfdt {t}, want {t0}");
    }
    for a in 0..2 {
        let list = p
            .text(&format!("/api/stream/1/audio/{a}/index.m3u8?sid={sid}"))
            .await;
        assert_eq!(extinf(&list), video, "audio {a} rides the video grid");
        let ainit = p
            .seg(
                &sid,
                "alice-jwt",
                &format!("audio/{a}"),
                &format!("init_{}.mp4", a + 1),
            )
            .await;
        assert_eq!(ainit.status, 200);
        for (n, t0) in starts(&video).into_iter().enumerate() {
            let s = p
                .seg(
                    &sid,
                    "alice-jwt",
                    &format!("audio/{a}"),
                    &seg_name(n as u32),
                )
                .await;
            assert_eq!(s.status, 200, "audio {a} seg {n}: {}", s.text());
            let t = tfdt_seconds(&ainit.body, &s.body, false);
            assert!(
                (t - t0).abs() < 0.1,
                "audio {a} seg {n} tfdt {t}, want {t0}"
            );
        }
    }
    let job = p.job(&sid).await;
    assert_eq!(job["quality"], "remux");
    p.e.rt.shutdown().await;
    assert!(p.e.ffmpeg_pids().is_empty());
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn remux_seek_restarts_on_the_grid_and_keeps_the_timeline() {
    let p = play(h264_clip(120)).await;
    let sid = p.open_remux().await;
    let video = extinf(
        &p.text(&format!("/api/stream/1/remux/index.m3u8?sid={sid}"))
            .await,
    );
    let at = starts(&video);
    // A copy outruns any player: seek forward first, then back, each a run of its own.
    let far = 25;
    let mut segs = vec![];
    for (n, start) in [(far, far), (far + 1, far), (0, 0), (1, 0)] {
        let s = p.seg(&sid, "alice-jwt", "remux", &seg_name(n)).await;
        assert_eq!(s.status, 200, "seg {n}: {}", s.text());
        assert_eq!(p.job(&sid).await["startSegment"], start, "seg {n}");
        segs.push((n, s.body));
    }
    let init = p.seg(&sid, "alice-jwt", "remux", "init.mp4").await;
    assert_eq!(init.status, 200);
    for (n, body) in segs {
        let want = at[n as usize];
        let t = tfdt_seconds(&init.body, &body, true);
        assert!((t - want).abs() < 0.05, "seg {n} tfdt {t}, want {want}");
        let a = tfdt_seconds(&init.body, &body, false);
        assert!(
            (a - want).abs() < 0.1,
            "seg {n} audio tfdt {a}, want {want}"
        );
    }
    // The first run's segments stay served from disk.
    assert_eq!(
        p.seg(&sid, "alice-jwt", "remux", &seg_name(far))
            .await
            .status,
        200
    );
    assert_eq!(p.job(&sid).await["startSegment"], 0);
    let past = video.len() as u32;
    assert_eq!(
        p.e.get(
            &format!("/api/stream/1/remux/{}?sid={sid}", seg_name(past)),
            "alice-jwt",
            &[]
        )
        .await
        .status,
        404
    );
    p.e.rt.shutdown().await;
    assert!(p.e.ffmpeg_pids().is_empty());
}

/// First video PTS of a TS segment, in seconds.
fn ts_first_video_pts(body: &[u8]) -> f64 {
    let f = std::env::temp_dir().join(format!("fliks-ts-seg-{}.ts", std::process::id()));
    std::fs::write(&f, body).unwrap();
    let ffprobe = std::env::var("FFPROBE_PATH").unwrap_or_else(|_| "ffprobe".into());
    let out = std::process::Command::new(ffprobe)
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "packet=pts_time", "-of", "csv=p=0"])
        .arg(&f)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&f);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().trim_end_matches(',').parse::<f64>().ok())
        .fold(f64::MAX, f64::min)
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn ts_remux_seek_keeps_the_timeline_of_every_segment() {
    let p = play(h264_clip(120)).await;
    let mut client = copy_client(&p.file);
    client["device"]["useTsOnSingleAudio"] = json!(true);
    client["device"]["supportsTsRemux"] = json!(true);
    let s = p.e.open(1, &p.file, &p.info, "alice", client).await;
    assert_eq!(s["decision"]["response"]["playMethod"], "DirectStream");
    assert_eq!(s["decision"]["muxFlavour"], "ts");
    let sid = s["sessionId"].as_str().unwrap().to_string();
    let list = p
        .text(&format!("/api/stream/1/remux/index.m3u8?sid={sid}"))
        .await;
    assert!(
        !list.contains("EXT-X-MAP") && list.contains(".ts"),
        "{list}"
    );
    let at = starts(&extinf(&list));
    let far = 25;
    for (n, start) in [(far, far), (far + 1, far), (0, 0), (1, 0)] {
        let s = p
            .seg(&sid, "alice-jwt", "remux", &format!("seg-{n:04}.ts"))
            .await;
        assert_eq!(s.status, 200, "seg {n}: {}", s.text());
        assert_eq!(p.job(&sid).await["startSegment"], start, "seg {n}");
        // Every run puts a segment at its source time plus the 1 s headroom.
        let pts = ts_first_video_pts(&s.body);
        assert!(
            (pts - (at[n] + 1.0)).abs() < 0.1,
            "seg {n} pts {pts}, want {}",
            at[n] + 1.0
        );
    }
    p.e.rt.shutdown().await;
    assert!(p.e.ffmpeg_pids().is_empty());
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn resume_seg_zero_probe_is_served_by_the_early_companion() {
    let p = play(h264_clip(120)).await;
    let mut ev = p.e.rt.events.subscribe();
    let sid = p
        .open(
            "alice",
            json!({ "startAtS": 60.0, "selection": { "requestedQuality": "720p" } }),
        )
        .await;
    let init = p.seg(&sid, "alice-jwt", "720p", "init.mp4").await;
    assert_eq!(init.status, 200);
    for n in 0..2 {
        let s = p.seg(&sid, "alice-jwt", "720p", &seg_name(n)).await;
        assert_eq!(s.status, 200, "seg {n}: {}", s.text());
        let t = tfdt_seconds(&init.body, &s.body, true);
        assert!((t - f64::from(n) * SEG).abs() < 0.05, "seg {n} tfdt {t}");
    }
    // The main run kept its resume start; the probe never relocated it.
    let resume = p.seg(&sid, "alice-jwt", "720p", &seg_name(20)).await;
    assert_eq!(resume.status, 200);
    assert_eq!(p.job(&sid).await["startSegment"], 20);
    let mut starts = vec![];
    while let Ok(e) = ev.try_recv() {
        if let EngineEvent::JobStarted {
            job_key,
            start_segment,
            ..
        } = e
        {
            starts.push((job_key.ends_with("-early"), start_segment));
        }
    }
    starts.sort();
    assert_eq!(starts, [(false, 20), (true, 0)], "{starts:?}");
    p.e.rt.shutdown().await;
    assert!(p.e.ffmpeg_pids().is_empty());
}

/// The `BgJobDone` of a job on the engine of `p`, as JSON.
async fn run_job(p: &Play, kind: Value) -> Value {
    let mut rx = p.e.rt.events.subscribe();
    let body = json!({ "kind": kind, "media": { "mediaFileId": 1, "path": p.file } });
    let r = p.e.control("POST", "/v1/jobs", Some(body)).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let id = r.json()["jobId"].as_str().unwrap().to_string();
    loop {
        match tokio::time::timeout(Duration::from_secs(120), rx.recv()).await {
            Ok(Ok(ev @ EngineEvent::BgJobDone { .. })) => {
                let v = serde_json::to_value(&ev).unwrap();
                if v["jobId"] == id {
                    return v;
                }
            }
            Ok(_) => {}
            Err(_) => panic!("no BgJobDone"),
        }
    }
}

fn close(a: &Value, b: &Value, what: &str) {
    let (x, y) = (a.as_f64().unwrap(), b.as_f64().unwrap());
    assert!((x - y).abs() < 1e-3, "{what}: {x} vs {y}");
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn scan_job_matches_ffprobe_on_real_files() {
    // The mkv is scanned by ffprobe on the file, the MPEG-TS from stdin with its ADTS headers demuxed.
    for (file, adts) in [(h264_clip(10), false), (ts_clip(), true)] {
        let p = play(file.clone()).await;
        let v = run_job(&p, json!({ "type": "Scan", "info": p.info })).await;
        assert_eq!(v["state"], "done", "{file:?}: {v}");
        let (got, want) = (&v["result"]["scan"], scan(&file));
        assert_eq!(got["schemaVersion"], 1);
        close(&got["end"], &want["end"], "end");
        let (g, w) = (
            got["keyframes"].as_array().unwrap(),
            want["keyframes"].as_array().unwrap(),
        );
        assert_eq!(g.len(), w.len(), "{file:?}");
        assert!(g.len() >= 2);
        for (a, b) in g.iter().zip(w) {
            close(&a["pts"], &b["pts"], "pts");
            close(&a["dts"], &b["dts"], "dts");
        }
        let audio = p.info.audio[0].stream_index.to_string();
        let changes = if adts {
            json!({ audio: false })
        } else {
            json!({})
        };
        assert_eq!(got["audioConfigChanges"], changes, "{file:?}");
        assert!(p.e.ffmpeg_pids().is_empty());
    }
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn scan_job_of_mpeg_ts_with_two_aac_tracks_matches_the_ffmpeg_read() {
    // The demuxer's verdict per track equals ffmpeg's ADTS copy; a changed track is found mid-stream.
    for (change, want) in [(false, [false, false]), (true, [true, false])] {
        let file = ts_two_aac_clip(change);
        let p = play(file.clone()).await;
        let v = run_job(&p, json!({ "type": "Scan", "info": p.info })).await;
        assert_eq!(v["state"], "done", "{change}: {v}");
        let tracks: Vec<u32> = p.info.audio.iter().map(|a| a.stream_index).collect();
        assert_eq!(tracks.len(), 2);
        let got = &v["result"]["scan"]["audioConfigChanges"];
        for (i, t) in tracks.iter().enumerate() {
            assert_eq!(got[t.to_string()], adts_changes(&file, *t), "{change}: {t}");
            assert_eq!(got[t.to_string()], want[i], "{change}: {t}");
        }
        let (g, w) = (&v["result"]["scan"], scan(&file));
        close(&g["end"], &w["end"], "end");
        assert_eq!(
            g["keyframes"].as_array().unwrap().len(),
            w["keyframes"].as_array().unwrap().len()
        );
        assert!(p.e.ffmpeg_pids().is_empty());
    }
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn fingerprint_job_matches_fpcalc_on_a_real_file() {
    use fliks_engine_core::jobs::fingerprint::parse_fpcalc;
    let p = play(h264_clip(10)).await;
    let head = run_job(
        &p,
        json!({ "type": "Fingerprint", "window": { "type": "head" } }),
    )
    .await;
    assert_eq!(head["state"], "done", "{head}");
    let fpcalc = fliks_engine::hw::tool_path("FPCALC_PATH", "fpcalc");
    let out = std::process::Command::new(&fpcalc)
        .args(["-raw", "-length", "600"])
        .arg(&p.file)
        .output()
        .expect("fpcalc");
    let want = parse_fpcalc(&String::from_utf8_lossy(&out.stdout), 600.0, 0.0).unwrap();
    assert!(want.hashes.len() > 20);
    assert_eq!(
        head["result"]["fingerprint"],
        serde_json::to_value(&want).unwrap()
    );
    // The last 6 s (0.1 min) of the 10 s file: ffmpeg's window piped into fpcalc.
    let outro = json!({ "type": "Fingerprint", "maxLookupMinutes": 0.1,
        "window": { "type": "outro", "durationSeconds": p.info.duration_seconds.unwrap() } });
    let v = run_job(&p, outro).await;
    assert_eq!(v["state"], "done", "{v}");
    let fp = &v["result"]["fingerprint"];
    close(
        &fp["baseOffsetSec"],
        &json!(p.info.duration_seconds.unwrap() - 6.0),
        "offset",
    );
    let n = fp["hashes"].as_array().unwrap().len();
    assert!((15..=60).contains(&n), "chromaprint hashes of 6 s: {n}");
    assert!(p.e.ffmpeg_pids().is_empty());
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn subtitle_job_and_route_serve_the_same_vtt_from_a_real_mkv() {
    let p = play(subs_clip()).await;
    let streams: Vec<u32> = p.info.subtitles.iter().map(|s| s.stream_index).collect();
    assert_eq!(streams.len(), 2, "{:?}", p.info.subtitles);
    let v = run_job(&p, json!({ "type": "SubtitleExtract", "info": p.info })).await;
    assert_eq!(
        v["result"],
        json!({ "type": "SubtitleExtract", "streams": streams }),
        "{v}"
    );
    // A second engine on the same file serves them through the route's own extraction.
    let q = play(subs_clip()).await;
    for idx in &streams {
        let path = format!("/api/stream/1/subtitles/embedded/{idx}");
        let (job, route) = (
            p.e.get(&path, "alice-jwt", &[]).await,
            q.e.get(&path, "alice-jwt", &[]).await,
        );
        assert_eq!((job.status, route.status), (200, 200));
        assert_eq!(job.text(), route.text(), "stream {idx}");
        let cached =
            std::fs::read_to_string(p.e.root.join(format!("subtitles/v2/1/{idx}.vtt"))).unwrap();
        assert_eq!(
            std::fs::read_to_string(q.e.root.join(format!("subtitles/v2/1/{idx}.vtt"))).unwrap(),
            cached
        );
        assert!(job.text().starts_with("WEBVTT\nX-TIMESTAMP-MAP="));
    }
    let srt = std::fs::read_to_string(p.e.root.join(format!("subtitles/v2/1/{}.vtt", streams[0])))
        .unwrap();
    assert!(
        srt.contains("00:00:01.000 --> 00:00:02.500") && srt.contains("<i>Hello</i>\nworld"),
        "{srt}"
    );
    let ass = std::fs::read_to_string(p.e.root.join(format!("subtitles/v2/1/{}.vtt", streams[1])))
        .unwrap();
    // `\an8` becomes a line position, `\i`/`\b` overrides tags, `\N` a line break.
    assert!(
        ass.contains("line:5%")
            && ass.contains("<b>Bold</b> and plain")
            && ass.contains("Hello\nworld"),
        "{ass}"
    );
    assert!(p.e.ffmpeg_pids().is_empty() && q.e.ffmpeg_pids().is_empty());
}

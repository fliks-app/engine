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
    tokio::time::sleep(Duration::from_millis(500)).await;
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
    tokio::time::sleep(Duration::from_millis(500)).await;
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
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        p.e.rt.gc_tick().await;
    }
    assert!(
        p.e.control("GET", "/v1/sessions", None)
            .await
            .json()
            .as_array()
            .unwrap()
            .is_empty()
    );
    let left = p.e.ffmpeg_pids();
    assert!(
        left.is_empty(),
        "ffmpeg left: {left:?} {:?}",
        p.e.control("GET", "/v1/sessions", None).await.json()
    );
    let mut reasons = vec![];
    while let Ok(e) = ev.try_recv() {
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

async fn reap(p: &Play) {
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        p.e.rt.gc_tick().await;
    }
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
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(p.job(&sid).await["state"], "failed");
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
    for _ in 0..50 {
        if p.job(&sid).await["frontierSegment"].as_u64().unwrap_or(0) >= 6 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
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

//! Background jobs against the `fake-ffmpeg` test binary: a sprite job end to end, dedupe,
//! progress, cancel, shutdown, failures, the disk check and playback preempting a job; scan,
//! fingerprint and subtitle extraction jobs (success, failure, cancel, slots, pause).
mod runtime_common;
use fliks_engine_core::api::{EngineEvent, HwAccel};
use runtime_common::*;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

fn info() -> Value {
    json!({
        "video": [{ "streamIndex": 0, "codec": "h264", "profile": "High", "level": 31, "width": 1280,
            "height": 720, "frameRate": "24/1", "avgFrameRate": "24/1", "pixelFormat": "yuv420p",
            "bitDepth": 8, "startTimeSeconds": 0.0, "firstFrameSeconds": 0.0 }],
        "audio": [{ "streamIndex": 1, "codec": "aac", "language": "eng", "channels": 2, "sampleRate": 48000 }],
        "subtitles": [],
        "formatName": "matroska,webm", "formatStartSeconds": 0.0, "durationSeconds": 120.0, "schemaVersion": 1
    })
}

/// An engine whose ffmpeg, ffprobe and fpcalc are `mode`'s fake, with `streaming` merged into its
/// config, and a media file per id.
async fn engine_of(mode: &str, streaming: Value) -> (Engine, PathBuf) {
    let pause = if cfg!(unix) { "signal" } else { "stdin" };
    let f = fake(mode);
    let e = engine_with_tools(
        host(HwAccel::None, "/dev/dri/renderD128", pause),
        &f,
        &f,
        &f,
        json!({}),
    )
    .await;
    let mut config = e.control("GET", "/v1/config", None).await.json();
    config["revision"] = json!(2);
    for (k, v) in streaming.as_object().unwrap() {
        config["streaming"][k] = v.clone();
    }
    assert_eq!(
        e.control("PUT", "/v1/config", Some(config)).await.status,
        200
    );
    let file = temp_dir("media").join("movie.mkv");
    std::fs::write(&file, b"x").unwrap();
    for id in [7, 8, 9] {
        e.stub.file(id, &file);
    }
    e.stub.grant("alice-jwt", "alice");
    (e, file)
}

async fn submit(e: &Engine, id: u32, file: &std::path::Path, force: bool) -> String {
    let r = e
        .control(
            "POST",
            "/v1/jobs",
            Some(json!({
                "kind": { "type": "Sprite", "info": info() },
                "media": { "mediaFileId": id, "path": file }, "force": force,
            })),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    r.json()["jobId"].as_str().unwrap().into()
}

async fn job(e: &Engine, id: &str) -> Value {
    e.control("GET", &format!("/v1/jobs/{id}"), None)
        .await
        .json()
}

/// The `BgJobProgress` events of `id`, then its `BgJobDone`.
async fn done(rx: &mut broadcast::Receiver<EngineEvent>, id: &str) -> (Vec<f64>, EngineEvent) {
    let mut progress = vec![];
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("BgJobDone")
            .unwrap();
        match &ev {
            EngineEvent::BgJobProgress { job_id, pct } if job_id == id => progress.push(*pct),
            EngineEvent::BgJobDone { job_id, .. } if job_id == id => return (progress, ev),
            _ => {}
        }
    }
}

fn done_json(ev: &EngineEvent) -> Value {
    serde_json::to_value(ev).unwrap()
}

#[tokio::test]
async fn sprite_job_runs_dedupes_reports_progress_and_is_served() {
    let (e, file) = engine_of("sprite", json!({ "ffmpegSlots": 3 })).await;
    let mut rx = e.rt.events.subscribe();
    let t0 = Instant::now();
    let id = submit(&e, 7, &file, false).await;
    assert_eq!(submit(&e, 7, &file, true).await, id, "in-flight dedupe");
    let (progress, ev) = done(&mut rx, &id).await;
    let meta =
        json!({ "interval": 2, "columns": 10, "thumbWidth": 240, "thumbHeight": 135, "count": 60 });
    assert_eq!(
        done_json(&ev),
        json!({ "type": "BgJobDone", "jobId": id, "state": "done",
            "result": { "type": "Sprite", "sprite": meta } })
    );
    assert!(!progress.is_empty() && progress.iter().all(|p| (0.0..=99.0).contains(p)));
    assert!(
        progress.len() as f64 <= t0.elapsed().as_secs_f64() + 1.0,
        "at most 1/s"
    );
    let snap = job(&e, &id).await;
    assert_eq!(
        (snap["state"].as_str(), snap["pct"].as_f64()),
        (Some("done"), Some(100.0))
    );
    assert_eq!(snap["kind"], "Sprite");
    let list = e.control("GET", "/v1/jobs", None).await.json();
    assert_eq!(list.as_array().unwrap().len(), 1);
    let j = e
        .get("/api/stream/7/thumbnails/sprite.json", "alice-jwt", &[])
        .await;
    assert_eq!((j.status, j.json()), (200, meta.clone()));
    let jpg = e
        .get("/api/stream/7/thumbnails/sprite.jpg", "alice-jwt", &[])
        .await;
    assert_eq!(jpg.header("content-type").as_deref(), Some("image/jpeg"));
    assert_eq!(&jpg.body[..2], &[0xFF, 0xD8]);
    let dir = e.root.join("sprites/v1/7");
    let left: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|d| d.unwrap().file_name())
        .collect();
    assert_eq!(left.len(), 2, "frames removed: {left:?}");
    // Already generated: answered from sprite.json without a run.
    let again = submit(&e, 7, &file, false).await;
    assert_ne!(again, id);
    let (_, ev) = done(&mut rx, &again).await;
    assert_eq!(done_json(&ev)["result"]["sprite"], meta);
    let r = e
        .control("POST", "/v1/invalidate", Some(json!({ "mediaFileId": 7 })))
        .await;
    assert_eq!(r.status, 200);
    let gone = e
        .get("/api/stream/7/thumbnails/sprite.json", "alice-jwt", &[])
        .await;
    assert_eq!(gone.status, 404);
    assert!(e.ffmpeg_pids().is_empty());
}

#[tokio::test]
async fn cancel_a_queued_and_a_running_job_then_shutdown() {
    let (e, file) = engine_of("hangsprite", json!({ "ffmpegSlots": 2 })).await;
    let mut rx = e.rt.events.subscribe();
    let a = submit(&e, 7, &file, false).await;
    let b = submit(&e, 8, &file, false).await;
    let c = submit(&e, 9, &file, false).await;
    until("frame runs", 10, || !e.ffmpeg_pids().is_empty()).await;
    assert_eq!(job(&e, &c).await["state"], "queued", "two jobs run at once");
    assert_eq!(
        e.control("DELETE", &format!("/v1/jobs/{c}"), None)
            .await
            .status,
        204
    );
    assert_eq!(job(&e, &c).await["state"], "cancelled");
    assert_eq!(
        e.control("DELETE", &format!("/v1/jobs/{a}"), None)
            .await
            .status,
        204
    );
    let (_, ev) = done(&mut rx, &a).await;
    assert_eq!(done_json(&ev)["state"], "cancelled");
    assert_eq!(job(&e, &a).await["error"], "cancelled");
    assert_eq!(e.control("DELETE", "/v1/jobs/nope", None).await.status, 404);
    e.rt.shutdown().await;
    assert_eq!(job(&e, &b).await["state"], "cancelled");
    assert!(e.ffmpeg_pids().is_empty(), "runs killed on shutdown");
}

#[tokio::test]
async fn failing_frames_and_a_full_disk_fail_the_job() {
    let (e, file) = engine_of("failsprite", json!({ "ffmpegSlots": 2 })).await;
    let mut rx = e.rt.events.subscribe();
    let id = submit(&e, 7, &file, false).await;
    let (_, ev) = done(&mut rx, &id).await;
    let v = done_json(&ev);
    assert_eq!(v["state"], "failed");
    assert_eq!(
        v["error"], "Sprite failed: no frame could be extracted (60 attempts)",
        "{v}"
    );
    let (e, file) = engine_of("sprite", json!({ "jobMinFreeBytes": 1e18 })).await;
    let mut rx = e.rt.events.subscribe();
    let id = submit(&e, 7, &file, false).await;
    let (_, ev) = done(&mut rx, &id).await;
    let err = done_json(&ev)["error"].as_str().unwrap().to_string();
    assert!(err.starts_with("not enough disk space: "), "{err}");
    let bad = e
        .control(
            "POST",
            "/v1/jobs",
            Some(json!({
                "kind": { "type": "Sprite", "info": { "video": [], "audio": [], "subtitles": [] } },
                "media": { "mediaFileId": 7, "path": file },
            })),
        )
        .await;
    assert_eq!(bad.status, 400, "no duration");
}

/// `stat` of the sprite frame runs of `e`.
#[cfg(unix)]
fn frame_states(e: &Engine) -> Vec<String> {
    let root = e.root.to_string_lossy().into_owned();
    let out = std::process::Command::new("ps")
        .args(["-axo", "stat=,command="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains(&root) && l.contains("-frames:v") && !l.starts_with('Z'))
        .map(|l| l.split_whitespace().next().unwrap().to_string())
        .collect()
}

#[cfg(unix)]
#[tokio::test]
async fn playback_preempts_a_background_job_then_it_resumes() {
    let (e, file) = engine_of("slowsprite", json!({ "ffmpegSlots": 1 })).await;
    let id = submit(&e, 7, &file, false).await;
    until("a frame run", 10, || frame_states(&e).len() == 1).await;
    // A resume on a pinned rung: the main run and its early companion take playback slots.
    let info = serde_json::from_value(info()).unwrap();
    let open = json!({ "startAtS": 60.0, "selection": { "requestedQuality": "720p" } });
    let sid = e.open(7, &file, &info, "alice", open).await["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    until("frame paused", 10, || {
        frame_states(&e).iter().all(|s| s.starts_with('T'))
    })
    .await;
    assert_eq!(job(&e, &id).await["state"], "paused");
    assert_eq!(e.rt.slots.playback_in_use(), 2);
    let r = e
        .control("DELETE", &format!("/v1/sessions/{sid}"), None)
        .await;
    assert_eq!(r.status, 204);
    until("frame resumed", 10, || {
        let s = frame_states(&e);
        !s.is_empty() && s.iter().all(|s| !s.starts_with('T'))
    })
    .await;
    assert_eq!(job(&e, &id).await["state"], "running");
    assert_eq!(
        e.control("DELETE", &format!("/v1/jobs/{id}"), None)
            .await
            .status,
        204
    );
    assert!(e.ffmpeg_pids().is_empty());
}

#[tokio::test]
async fn failed_frames_are_backfilled_and_the_sprite_still_tiles() {
    // Every 4th frame fails; the fake tiler refuses a gap in the frame sequence.
    let (e, file) = engine_of("flakysprite", json!({ "ffmpegSlots": 3 })).await;
    let mut rx = e.rt.events.subscribe();
    let id = submit(&e, 7, &file, false).await;
    let (_, ev) = done(&mut rx, &id).await;
    let v = done_json(&ev);
    assert_eq!(v["state"], "done", "{v}");
    assert_eq!(v["result"]["sprite"]["count"], 60);
    assert!(e.ffmpeg_pids().is_empty());
}

fn tool_info(extra: Value) -> Value {
    let mut i = info();
    for (k, v) in extra.as_object().unwrap() {
        i[k] = v.clone();
    }
    i
}

fn text_track(index: u32) -> Value {
    json!({ "streamIndex": index, "codec": "subrip", "language": "eng", "forced": false,
        "hearingImpaired": false, "isImageBased": false })
}

async fn post(e: &Engine, id: u32, file: &std::path::Path, kind: Value) -> Res {
    let body = json!({ "kind": kind, "media": { "mediaFileId": id, "path": file } });
    e.control("POST", "/v1/jobs", Some(body)).await
}

/// Submits `kind` and returns the `BgJobDone` of the job as JSON.
async fn run_kind(e: &Engine, id: u32, file: &std::path::Path, kind: Value) -> Value {
    let mut rx = e.rt.events.subscribe();
    let r = post(e, id, file, kind).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let job_id = r.json()["jobId"].as_str().unwrap().to_string();
    done_json(&done(&mut rx, &job_id).await.1)
}

/// Cancels a job whose tool hangs, and checks nothing is left running.
async fn hang_then_cancel(e: &Engine, file: &std::path::Path, kind: Value) {
    let mut rx = e.rt.events.subscribe();
    let id = post(e, 7, file, kind).await.json()["jobId"]
        .as_str()
        .unwrap()
        .to_string();
    until("a tool run", 10, || !e.ffmpeg_pids().is_empty()).await;
    let r = e.control("DELETE", &format!("/v1/jobs/{id}"), None).await;
    assert_eq!(r.status, 204);
    let v = done_json(&done(&mut rx, &id).await.1);
    assert_eq!(
        (v["state"].as_str(), v["error"].as_str()),
        (Some("cancelled"), Some("cancelled"))
    );
    assert!(
        e.ffmpeg_pids().is_empty(),
        "every process of the job is gone"
    );
}

fn near(v: &Value, want: f64) {
    assert!((v.as_f64().unwrap() - want).abs() < 1e-5, "{v} vs {want}");
}

#[tokio::test]
async fn scan_job_reads_packets_and_reports_progress() {
    let (e, file) = engine_of("scan", json!({})).await;
    let kind = json!({ "type": "Scan", "info": info() });
    let v = run_kind(&e, 7, &file, kind).await;
    assert_eq!(v["state"], "done", "{v}");
    let scan = &v["result"]["scan"];
    assert_eq!(v["result"]["type"], "Scan");
    assert_eq!(
        (
            scan["schemaVersion"].as_u64(),
            scan["audioConfigChanges"].clone()
        ),
        (Some(1), json!({}))
    );
    // The lead was read from the file (2 frames at 24 fps) because the probe did not know it.
    let keys = scan["keyframes"].as_array().unwrap();
    assert_eq!(keys.len(), 2);
    near(&keys[0]["dts"], -0.083333);
    near(&keys[1]["pts"], 0.125);
    near(&scan["end"], 0.166667);
    assert!(scan.get("breakSeconds").is_none());
    let snap = e.control("GET", "/v1/jobs", None).await.json();
    assert_eq!(
        (snap[0]["kind"].as_str(), snap[0]["pct"].as_f64()),
        (Some("Scan"), Some(100.0))
    );
    assert!(e.ffmpeg_pids().is_empty());
}

#[tokio::test]
async fn scan_job_of_mpeg_ts_reads_the_adts_headers_from_the_same_bytes() {
    let ts = json!({ "formatName": "mpegts" });
    for (mode, changed) in [("scan", false), ("scanchange", true)] {
        let (e, file) = engine_of(mode, json!({})).await;
        let kind = json!({ "type": "Scan", "info": tool_info(ts.clone()) });
        let v = run_kind(&e, 7, &file, kind).await;
        assert_eq!(v["state"], "done", "{mode}: {v}");
        let want = if cfg!(unix) { changed } else { true };
        assert_eq!(
            v["result"]["scan"]["audioConfigChanges"],
            json!({ "1": want }),
            "{mode}"
        );
        assert!(e.ffmpeg_pids().is_empty());
    }
}

#[tokio::test]
async fn scan_job_fails_cancels_and_is_validated() {
    let (e, file) = engine_of("failscan", json!({})).await;
    let kind = json!({ "type": "Scan", "info": info() });
    let v = run_kind(&e, 7, &file, kind).await;
    let err = v["error"].as_str().unwrap();
    assert_eq!(v["state"], "failed");
    assert!(
        err.starts_with("ffprobe exited 1:") && err.contains("fake ffprobe: boom"),
        "{err}"
    );
    let none = json!({ "type": "Scan", "info": { "video": [], "audio": [], "subtitles": [] } });
    assert_eq!(
        post(&e, 7, &file, none).await.status,
        400,
        "no video stream"
    );
    let (e, file) = engine_of("hangscan", json!({})).await;
    hang_then_cancel(&e, &file, json!({ "type": "Scan", "info": info() })).await;
    hang_then_cancel(
        &e,
        &file,
        json!({ "type": "Scan", "info": tool_info(json!({ "formatName": "mpegts" })) }),
    )
    .await;
}

#[tokio::test]
async fn fingerprint_job_head_and_outro_windows() {
    let (e, file) = engine_of("fp", json!({})).await;
    let hashes = json!([1, -2, -1294967296]);
    let head = json!({ "type": "Fingerprint", "window": { "type": "head" } });
    let v = run_kind(&e, 7, &file, head).await;
    let want = json!({ "type": "Fingerprint", "fingerprint":
        { "hashes": hashes, "durationSeconds": 12.5, "baseOffsetSec": 0.0 } });
    assert_eq!(
        (v["state"].as_str(), &v["result"]),
        (Some("done"), &want),
        "{v}"
    );
    // The last 5 minutes of a 25 minute file: ffmpeg piped into fpcalc, offset kept.
    let outro = json!({ "type": "Fingerprint", "maxLookupMinutes": 5,
        "window": { "type": "outro", "durationSeconds": 1500.0 } });
    let v = run_kind(&e, 7, &file, outro).await;
    assert_eq!(v["result"]["fingerprint"]["baseOffsetSec"], 1200.0, "{v}");
    assert_eq!(v["result"]["fingerprint"]["hashes"], hashes);
    let no_duration =
        json!({ "type": "Fingerprint", "window": { "type": "outro", "durationSeconds": 0.0 } });
    assert_eq!(post(&e, 7, &file, no_duration).await.status, 400);
    assert!(e.ffmpeg_pids().is_empty());
}

#[tokio::test]
async fn fingerprint_job_fails_and_cancels() {
    let (e, file) = engine_of("failfp", json!({})).await;
    let head = json!({ "type": "Fingerprint", "window": { "type": "head" } });
    let v = run_kind(&e, 7, &file, head).await;
    assert_eq!(
        (v["state"].as_str(), v["error"].as_str()),
        (Some("failed"), Some("fpcalc exited 2: fake fpcalc: boom"))
    );
    let outro =
        json!({ "type": "Fingerprint", "window": { "type": "outro", "durationSeconds": 900.0 } });
    let v = run_kind(&e, 7, &file, outro.clone()).await;
    assert_eq!(v["error"], "fpcalc exited 2: fake fpcalc: boom", "{v}");
    let (e, file) = engine_of("hangfp", json!({})).await;
    hang_then_cancel(
        &e,
        &file,
        json!({ "type": "Fingerprint", "window": { "type": "head" } }),
    )
    .await;
    hang_then_cancel(&e, &file, outro).await;
}

fn subtitle_kind(streams: &[u32]) -> Value {
    let tracks: Vec<Value> = streams.iter().map(|s| text_track(*s)).collect();
    json!({ "type": "SubtitleExtract", "info": tool_info(json!({ "subtitles": tracks })) })
}

/// `fixtures/jobs/subtitles/served.vtt`: the VTT both paths must produce from `served.ass`
/// (`BLESS=1` rewrites it).
fn golden_vtt() -> String {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/jobs/subtitles");
    let want = fliks_engine_core::jobs::subtitles::ass_to_vtt(
        &std::fs::read_to_string(dir.join("served.ass")).unwrap(),
    );
    if std::env::var("BLESS").is_ok_and(|v| v == "1") {
        std::fs::write(dir.join("served.vtt"), &want).unwrap();
    }
    let golden = std::fs::read_to_string(dir.join("served.vtt")).unwrap();
    assert_eq!(
        golden, want,
        "served.vtt is stale: BLESS=1 and review the diff"
    );
    golden
}

#[tokio::test]
async fn subtitle_job_stores_the_vtt_the_routes_serve() {
    let golden = golden_vtt();
    let (e, file) = engine_of("sub", json!({})).await;
    let v = run_kind(&e, 7, &file, subtitle_kind(&[2, 3])).await;
    assert_eq!(v["state"], "done", "{v}");
    assert_eq!(
        v["result"],
        json!({ "type": "SubtitleExtract", "streams": [2, 3] })
    );
    for idx in [2, 3] {
        let path = e.root.join(format!("subtitles/v2/7/{idx}.vtt"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            golden,
            "job output {idx}"
        );
        let r = e
            .get(
                &format!("/api/stream/7/subtitles/embedded/{idx}"),
                "alice-jwt",
                &[],
            )
            .await;
        let served = fliks_engine::serve::with_timestamp_map(&golden, 0.0);
        assert_eq!(
            (r.status, r.text()),
            (200, served),
            "served from the job's file {idx}"
        );
    }
    let left: Vec<_> = std::fs::read_dir(e.root.join("subtitles/v2/7"))
        .unwrap()
        .map(|d| d.unwrap().file_name())
        .collect();
    assert_eq!(left.len(), 2, "raw ASS removed: {left:?}");
    // The route's own extraction (no job ran in this engine) is the same text.
    let (e2, _) = engine_of("sub", json!({})).await;
    let r = e2
        .get("/api/stream/7/subtitles/embedded/2", "alice-jwt", &[])
        .await;
    assert_eq!(
        r.text(),
        fliks_engine::serve::with_timestamp_map(&golden, 0.0)
    );
    assert_eq!(
        std::fs::read_to_string(e2.root.join("subtitles/v2/7/2.vtt")).unwrap(),
        golden
    );
    assert!(e.ffmpeg_pids().is_empty() && e2.ffmpeg_pids().is_empty());
}

#[tokio::test]
async fn subtitle_job_skips_cached_tracks_fails_and_cancels() {
    // `failsub` fails any ffmpeg run: a cached track needs none.
    let (e, file) = engine_of("failsub", json!({})).await;
    let dir = e.root.join("subtitles/v2/7");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("2.vtt"), "WEBVTT\n").unwrap();
    let v = run_kind(&e, 7, &file, subtitle_kind(&[2])).await;
    assert_eq!(v["state"], "done", "{v}");
    let v = run_kind(&e, 7, &file, subtitle_kind(&[2, 3])).await;
    assert_eq!(v["state"], "failed");
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .starts_with("ffmpeg exited with Some(1)"),
        "{v}"
    );
    let no_text = tool_info(
        json!({ "subtitles": [{ "streamIndex": 4, "codec": "hdmv_pgs_subtitle",
        "language": "eng", "forced": false, "hearingImpaired": false, "isImageBased": true }] }),
    );
    let bad = json!({ "type": "SubtitleExtract", "info": no_text });
    assert_eq!(post(&e, 7, &file, bad).await.status, 400, "no text track");
    assert_eq!(
        post(&e, 7, &file, subtitle_kind(&[1000])).await.status,
        400,
        "index above 999"
    );
    let (e, file) = engine_of("emptysub", json!({})).await;
    let v = run_kind(&e, 7, &file, subtitle_kind(&[2])).await;
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .starts_with("stream 2: ffmpeg wrote an empty subtitle file"),
        "{v}"
    );
    assert!(!e.root.join("subtitles/v2/7/2.vtt").exists());
    let (e, file) = engine_of("sub", json!({ "jobMinFreeBytes": 1e18 })).await;
    let v = run_kind(&e, 7, &file, subtitle_kind(&[2])).await;
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .starts_with("not enough disk space: "),
        "{v}"
    );
    let (e, file) = engine_of("hangsub", json!({})).await;
    hang_then_cancel(&e, &file, subtitle_kind(&[2])).await;
}

#[tokio::test]
async fn new_kinds_share_the_slot_budget() {
    // One slot: the second job waits for the first one's run.
    let (e, file) = engine_of("hangfp", json!({ "ffmpegSlots": 1 })).await;
    let head = json!({ "type": "Fingerprint", "window": { "type": "head" } });
    let a = post(&e, 7, &file, head.clone()).await.json()["jobId"]
        .as_str()
        .unwrap()
        .to_string();
    let b = post(&e, 8, &file, head).await.json()["jobId"]
        .as_str()
        .unwrap()
        .to_string();
    until("the first run", 10, || !e.ffmpeg_pids().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(e.ffmpeg_pids().len(), 1, "one tool run for one slot");
    assert_eq!(
        (
            job(&e, &a).await["state"].as_str(),
            job(&e, &b).await["state"].as_str()
        ),
        (Some("running"), Some("running"))
    );
    e.rt.shutdown().await;
    assert!(e.ffmpeg_pids().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn playback_pauses_a_fingerprint_job() {
    let (e, file) = engine_of("hangfp", json!({ "ffmpegSlots": 1 })).await;
    let head = json!({ "type": "Fingerprint", "window": { "type": "head" } });
    let id = post(&e, 8, &file, head).await.json()["jobId"]
        .as_str()
        .unwrap()
        .to_string();
    until("the tool run", 10, || !e.ffmpeg_pids().is_empty()).await;
    let info = serde_json::from_value(info()).unwrap();
    let open = json!({ "startAtS": 60.0, "selection": { "requestedQuality": "720p" } });
    let sid = e.open(7, &file, &info, "alice", open).await["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    until("job paused", 10, || e.rt.slots.playback_in_use() > 0).await;
    for _ in 0..200 {
        if job(&e, &id).await["state"] == "paused" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(job(&e, &id).await["state"], "paused");
    let r = e
        .control("DELETE", &format!("/v1/sessions/{sid}"), None)
        .await;
    assert_eq!(r.status, 204);
    e.rt.shutdown().await;
    assert!(e.ffmpeg_pids().is_empty());
}

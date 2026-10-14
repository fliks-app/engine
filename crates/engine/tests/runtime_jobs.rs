//! Background jobs against the `fake-ffmpeg` test binary: a sprite job end to end, dedupe,
//! progress, cancel, shutdown, failures, the disk check and playback preempting a job.
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

/// An engine on `mode`'s fake with `streaming` merged into its config, and a media file per id.
async fn engine_of(mode: &str, streaming: Value) -> (Engine, PathBuf) {
    let pause = if cfg!(unix) { "signal" } else { "stdin" };
    let e = engine_with(
        host(HwAccel::None, "/dev/dri/renderD128", pause),
        &fake(mode),
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
    assert!(
        v["error"].as_str().unwrap().starts_with("Sprite aborted: "),
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

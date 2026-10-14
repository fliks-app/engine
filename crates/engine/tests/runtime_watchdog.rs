//! Job ownership and the stall watchdog, against the `fake-ffmpeg` test binary (no real ffmpeg).
mod runtime_common;
use fliks_engine::proc::{Ffmpeg, STOP_GRACE};
use fliks_engine::runtime::JobKind;
use fliks_engine_core::api::{EngineEvent, HwAccel};
use runtime_common::*;
use serde_json::{Value, json};
use std::time::Duration;

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

async fn engine_of(ffmpeg: &str, lifetime: Value) -> (Engine, std::path::PathBuf) {
    let e = engine_with(
        // Windows has no SIGSTOP: a pause goes through stdin there.
        host(
            HwAccel::None,
            "/dev/dri/renderD128",
            if cfg!(unix) { "signal" } else { "stdin" },
        ),
        ffmpeg,
        lifetime,
    )
    .await;
    let file = temp_dir("media").join("movie.mkv");
    std::fs::write(&file, b"x").unwrap();
    e.stub.file(7, &file);
    e.stub.grant("alice-jwt", "alice");
    (e, file)
}

/// A resume at 60 s on a pinned rung: `open` prewarms the main run and its early companion.
async fn open(e: &Engine, file: &std::path::Path, extra: Value) -> String {
    let info = serde_json::from_value(info()).unwrap();
    let s = e.open(7, file, &info, "alice", extra).await;
    s["sessionId"].as_str().unwrap().into()
}

fn resume() -> Value {
    json!({ "startAtS": 60.0, "selection": { "requestedQuality": "720p" } })
}

/// A session with a running main, early and remux job.
async fn with_all_jobs(lifetime: Value) -> (Engine, String) {
    let (e, file) = engine_of(&fake("hang"), lifetime).await;
    let sid = open(&e, &file, resume()).await;
    let s = e.rt.peek(&sid).unwrap();
    e.rt.prewarm_remux(&s, 0.0);
    until("three runs", 10, || e.ffmpeg_pids().len() == 3).await;
    for k in [JobKind::Main, JobKind::Early, JobKind::Remux] {
        assert!(e.rt.job(&s.key(k)).is_some_and(|j| j.producing()), "{k:?}");
    }
    (e, sid)
}

async fn gone(e: &Engine, why: &str) {
    until(why, 10, || e.ffmpeg_pids().is_empty()).await;
}

#[tokio::test]
async fn every_job_kind_stops_when_the_session_is_deleted() {
    let (e, sid) = with_all_jobs(json!({})).await;
    let r = e
        .control("DELETE", &format!("/v1/sessions/{sid}"), None)
        .await;
    assert_eq!(r.status, 204);
    // The call returns once the processes are gone.
    assert!(e.ffmpeg_pids().is_empty());
}

#[tokio::test]
async fn every_job_kind_stops_when_the_stop_command_ends_the_session() {
    let (e, sid) = with_all_jobs(json!({})).await;
    let body = json!({ "action": "stop" });
    let r = e
        .control("POST", &format!("/v1/sessions/{sid}/command"), Some(body))
        .await;
    assert_eq!(r.status, 204);
    assert!(e.ffmpeg_pids().is_empty());
}

#[tokio::test]
async fn every_job_kind_stops_when_the_session_expires() {
    let (e, _) = with_all_jobs(json!({ "liveSessionTtlMs": 200 })).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    e.rt.gc_tick().await;
    assert!(e.ffmpeg_pids().is_empty());
}

#[tokio::test]
async fn every_job_kind_stops_when_the_session_is_revoked() {
    let (e, _) = with_all_jobs(json!({})).await;
    let body = json!({ "ownerId": "alice" });
    let r = e.control("POST", "/v1/revoke", Some(body)).await;
    assert_eq!(r.json()["sessions"], 1);
    assert!(e.ffmpeg_pids().is_empty());
}

#[tokio::test]
async fn every_job_kind_stops_when_the_file_is_invalidated() {
    let (e, _) = with_all_jobs(json!({})).await;
    let r = e
        .control("POST", "/v1/invalidate", Some(json!({ "mediaFileId": 7 })))
        .await;
    assert_eq!(r.json()["jobs"], 3);
    assert!(e.ffmpeg_pids().is_empty());
}

#[tokio::test]
async fn every_job_kind_stops_when_the_owner_cap_evicts_the_session() {
    let (e, sid) = with_all_jobs(json!({ "maxSessionsPerOwner": 1 })).await;
    let mut ev = e.rt.events.subscribe();
    let file = e.stub.paths.lock().unwrap()[&7].clone();
    let second = open(&e, std::path::Path::new(&file), json!({})).await;
    assert_ne!(second, sid);
    gone(&e, "evicted session's jobs").await;
    let reasons: Vec<_> = std::iter::from_fn(|| ev.try_recv().ok())
        .filter_map(|x| match x {
            EngineEvent::SessionEnded { reason, .. } => Some(reason),
            _ => None,
        })
        .collect();
    assert_eq!(reasons, ["evicted"]);
}

#[tokio::test]
async fn every_job_kind_stops_on_engine_shutdown() {
    let (e, _) = with_all_jobs(json!({})).await;
    e.rt.shutdown().await;
    assert!(e.ffmpeg_pids().is_empty());
}

#[tokio::test]
async fn a_job_whose_session_is_gone_is_reaped_on_the_next_gc_tick() {
    let (e, file) = engine_of(&fake("hang"), json!({})).await;
    let sid = open(&e, &file, json!({})).await;
    let s = e.rt.peek(&sid).unwrap();
    e.rt.stop(&sid, "stopped").await;
    // A request racing the end of its session registers a run nobody owns.
    e.rt.prewarm_remux(&s, 0.0);
    until("the orphan run", 10, || {
        e.ffmpeg_pids().len() == 1 && e.rt.job(&s.key(JobKind::Remux)).is_some()
    })
    .await;
    e.rt.gc_tick().await;
    gone(&e, "the reaped orphan").await;
    assert!(e.rt.job(&s.key(JobKind::Remux)).is_none());
}

#[tokio::test]
async fn a_hung_run_is_killed_after_the_stall_timeout_and_restarts_on_the_next_request() {
    let (e, file) = engine_of(&fake("hang"), json!({ "jobStallTimeoutMs": 1000 })).await;
    let mut ev = e.rt.events.subscribe();
    let sid = open(&e, &file, json!({})).await;
    let s = e.rt.peek(&sid).unwrap();
    e.rt.prewarm_remux(&s, 0.0);
    until("the run", 10, || {
        e.ffmpeg_pids().len() == 1 && e.rt.job(&s.key(JobKind::Remux)).is_some()
    })
    .await;
    let first = e.rt.job(&s.key(JobKind::Remux)).unwrap();
    // 1 s of silence, then the polite stop is ignored and the 2 s grace ends in a kill.
    until("the stall kill", 10, || !first.producing()).await;
    gone(&e, "no process left").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let ended: Vec<_> = std::iter::from_fn(|| ev.try_recv().ok())
        .filter_map(|x| match x {
            EngineEvent::JobEnded {
                state,
                exit_code,
                reason,
                ..
            } => Some((format!("{state:?}"), exit_code, reason)),
            _ => None,
        })
        .collect();
    // A kill is a signal on unix and exit code 1 on windows.
    let code = if cfg!(unix) { None } else { Some(1) };
    assert_eq!(ended, [("Failed".into(), code, Some("stalled".into()))]);
    // The next request goes through the normal path and starts a new run.
    e.rt.prewarm_remux(&s, 0.0);
    until("the restart", 10, || e.ffmpeg_pids().len() == 1).await;
    e.rt.stop(&sid, "stopped").await;
    assert!(e.ffmpeg_pids().is_empty());
}

#[tokio::test]
async fn a_run_burning_cpu_without_output_is_not_killed_until_it_goes_idle() {
    // Busy for ~4 s (past the 1 s timeout) with no file and no stderr, then hangs like the real bug.
    let (e, file) = engine_of(&fake("busy"), json!({ "jobStallTimeoutMs": 1000 })).await;
    let sid = open(&e, &file, json!({})).await;
    let s = e.rt.peek(&sid).unwrap();
    e.rt.prewarm_remux(&s, 0.0);
    until("the run", 10, || {
        e.ffmpeg_pids().len() == 1 && e.rt.job(&s.key(JobKind::Remux)).is_some()
    })
    .await;
    let job = e.rt.job(&s.key(JobKind::Remux)).unwrap();
    tokio::time::sleep(Duration::from_millis(3000)).await;
    assert!(
        job.producing(),
        "a working encode must survive the stall timeout"
    );
    // Once the CPU goes flat the usual stall kill applies.
    until("the stall kill", 10, || !job.producing()).await;
    gone(&e, "no process left").await;
}

#[tokio::test]
async fn a_run_that_keeps_talking_is_not_stalled_and_a_paused_one_is_exempt() {
    let (e, file) = engine_of(&fake("talk"), json!({ "jobStallTimeoutMs": 1000 })).await;
    let sid = open(&e, &file, json!({})).await;
    let s = e.rt.peek(&sid).unwrap();
    e.rt.prewarm_remux(&s, 0.0);
    until("the run", 10, || e.ffmpeg_pids().len() == 1).await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(e.ffmpeg_pids().len(), 1);
    e.rt.shutdown().await;

    let (e, file) = engine_of(&fake("hang"), json!({ "jobStallTimeoutMs": 1000 })).await;
    let pinned = json!({ "selection": { "requestedQuality": "720p" } });
    let sid = open(&e, &file, pinned).await;
    until("the run", 10, || e.ffmpeg_pids().len() == 1).await;
    let r = e
        .control(
            "POST",
            &format!("/v1/sessions/{sid}/command"),
            Some(json!({ "action": "pauseJob" })),
        )
        .await;
    assert_eq!(r.status, 204);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(
        e.ffmpeg_pids().len(),
        1,
        "a paused run is not a stalled one"
    );
    e.rt.shutdown().await;
    assert!(e.ffmpeg_pids().is_empty());
}

#[tokio::test]
async fn an_early_companion_that_outlives_its_media_time_is_killed() {
    let (e, file) = engine_of(&fake("early"), json!({ "jobStallTimeoutMs": 1000 })).await;
    let mut ev = e.rt.events.subscribe();
    let sid = open(&e, &file, resume()).await;
    let s = e.rt.peek(&sid).unwrap();
    until("both runs", 10, || {
        e.ffmpeg_pids().len() == 2 && e.rt.job(&s.key(JobKind::Early)).is_some()
    })
    .await;
    let early = e.rt.job(&s.key(JobKind::Early)).unwrap();
    // 2 segments + 1 s of media, then the 1 s grace.
    until("the early kill", 20, || !early.producing()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut reasons: Vec<_> = std::iter::from_fn(|| ev.try_recv().ok())
        .filter_map(|x| match x {
            EngineEvent::JobEnded {
                job_key, reason, ..
            } => Some((job_key, reason?)),
            _ => None,
        })
        .map(|(k, r)| (k.ends_with("-early"), r))
        .collect();
    reasons.sort();
    assert_eq!(
        reasons,
        [
            (false, "stalled".to_string()),
            (true, "overran".to_string())
        ]
    );
    gone(&e, "no process left").await;
}

#[tokio::test]
async fn cpu_time_grows_for_a_busy_process_and_stays_flat_for_a_hung_one() {
    let busy = Ffmpeg::spawn(&fake("busy"), &[]).await.unwrap();
    let idle = Ffmpeg::spawn(&fake("hang"), &[]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(busy.cpu_time_ms().unwrap() >= 300);
    assert!(idle.cpu_time_ms().unwrap() < 150);
    busy.stop().await;
    idle.stop().await;
}

#[tokio::test]
async fn a_process_deaf_to_stop_is_killed_after_the_grace() {
    let p = Ffmpeg::spawn(&fake("hang"), &[]).await.unwrap();
    // Let it install its signal handling first.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t = std::time::Instant::now();
    p.stop().await;
    assert!(t.elapsed() >= STOP_GRACE);
    assert!(p.exited().is_some());
    // Dropping a running handle kills it too.
    let p = Ffmpeg::spawn(&fake("hang"), &[]).await.unwrap();
    let cpu = p.cpu_time_ms();
    assert!(cpu.is_some());
    drop(p);
}

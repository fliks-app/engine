//! Job ownership and the stall watchdog, against the `fake-ffmpeg` test binary (no real ffmpeg).
mod runtime_common;
use fliks_engine::proc::{Ffmpeg, STOP_GRACE};
use fliks_engine::runtime::{Job, JobKind};
use fliks_engine_core::api::{EngineEvent, HwAccel};
use runtime_common::*;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
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
    // A process is listed before its job is registered: wait for the registry.
    until("three producing jobs", 30, || {
        [JobKind::Main, JobKind::Early, JobKind::Remux]
            .iter()
            .all(|k| e.rt.job(&s.key(*k)).is_some_and(|j| j.producing()))
    })
    .await;
    assert_eq!(e.ffmpeg_pids().len(), 3);
    (e, sid)
}

async fn gone(e: &Engine, why: &str) {
    until(why, 30, || e.ffmpeg_pids().is_empty()).await;
}

/// The next `JobEnded`, as (job key, state, exit code, reason).
async fn job_ended(
    rx: &mut tokio::sync::broadcast::Receiver<EngineEvent>,
) -> (String, String, Option<i32>, Option<String>) {
    match event(rx, 60, |e| matches!(e, EngineEvent::JobEnded { .. })).await {
        EngineEvent::JobEnded {
            job_key,
            state,
            exit_code,
            reason,
        } => (job_key, format!("{state:?}"), exit_code, reason),
        _ => unreachable!(),
    }
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
    let e = &e;
    until_async("the expired session's jobs", 30, || async move {
        e.rt.gc_tick().await;
        e.ffmpeg_pids().is_empty()
    })
    .await;
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
    until("the orphan run", 30, || {
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
    until("the run", 30, || {
        e.ffmpeg_pids().len() == 1 && e.rt.job(&s.key(JobKind::Remux)).is_some()
    })
    .await;
    let first = e.rt.job(&s.key(JobKind::Remux)).unwrap();
    // 1 s of silence, then the polite stop is ignored and the 2 s grace ends in a kill.
    let (_, state, exit_code, reason) = job_ended(&mut ev).await;
    // A kill is a signal on unix and exit code 1 on windows.
    let code = if cfg!(unix) { None } else { Some(1) };
    assert_eq!(
        (state, exit_code, reason),
        ("Failed".into(), code, Some("stalled".into()))
    );
    assert!(!first.producing());
    gone(&e, "no process left").await;
    // The next request goes through the normal path and starts a new run.
    e.rt.prewarm_remux(&s, 0.0);
    until("the restart", 30, || {
        e.ffmpeg_pids().len() == 1
            && e.rt
                .job(&s.key(JobKind::Remux))
                .is_some_and(|j| !Arc::ptr_eq(&j, &first) && j.producing())
    })
    .await;
    e.rt.stop(&sid, "stopped").await;
    assert!(e.ffmpeg_pids().is_empty());
}

/// The job `sid` opened, once registered: a process is listed before its job is.
async fn primary(e: &Engine, sid: &str) -> Arc<Job> {
    let key = e.rt.peek(sid).unwrap().primary_key();
    until("the job", 30, || {
        e.rt.job(&key).is_some_and(|j| j.producing())
    })
    .await;
    e.rt.job(&key).unwrap()
}

#[tokio::test]
async fn a_run_burning_cpu_without_output_is_not_killed_until_it_goes_idle() {
    // Busy, with no file and no stderr, until told to rest; then it hangs like the real bug.
    let busy = fake("busy");
    let (e, file) = engine_of(&busy, json!({ "jobStallTimeoutMs": 3000 })).await;
    let sid = open(&e, &file, json!({})).await;
    let s = e.rt.peek(&sid).unwrap();
    e.rt.prewarm_remux(&s, 0.0);
    until("the run", 30, || {
        e.ffmpeg_pids().len() == 1 && e.rt.job(&s.key(JobKind::Remux)).is_some()
    })
    .await;
    let job = e.rt.job(&s.key(JobKind::Remux)).unwrap();
    tokio::time::sleep(Duration::from_millis(4500)).await;
    assert!(
        job.producing(),
        "a working encode must survive the stall timeout"
    );
    // Once the CPU goes flat the usual stall kill applies.
    std::fs::write(Path::new(&busy).with_extension("rest"), b"").unwrap();
    until("the stall kill", 60, || !job.producing()).await;
    gone(&e, "no process left").await;
}

#[tokio::test]
async fn a_run_that_keeps_talking_is_not_stalled_and_a_paused_one_is_exempt() {
    let (e, file) = engine_of(&fake("talk"), json!({ "jobStallTimeoutMs": 3000 })).await;
    let sid = open(&e, &file, json!({})).await;
    let s = e.rt.peek(&sid).unwrap();
    e.rt.prewarm_remux(&s, 0.0);
    until("the run", 30, || {
        e.ffmpeg_pids().len() == 1 && e.rt.job(&s.key(JobKind::Remux)).is_some()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(4500)).await;
    assert_eq!(e.ffmpeg_pids().len(), 1);
    e.rt.shutdown().await;

    let (e, file) = engine_of(&fake("hang"), json!({ "jobStallTimeoutMs": 1000 })).await;
    let pinned = json!({ "selection": { "requestedQuality": "720p" } });
    let sid = open(&e, &file, pinned).await;
    primary(&e, &sid).await;
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
    // The early run talks every 200 ms: only its natural end may stop it.
    let (e, file) = engine_of(&fake("early"), json!({ "jobStallTimeoutMs": 3000 })).await;
    let mut ev = e.rt.events.subscribe();
    let sid = open(&e, &file, resume()).await;
    let s = e.rt.peek(&sid).unwrap();
    until("both runs", 30, || {
        e.ffmpeg_pids().len() == 2 && e.rt.job(&s.key(JobKind::Early)).is_some()
    })
    .await;
    // The main run stalls, the early one overruns its 2 segments + 1 s of media plus the grace.
    let mut reasons = vec![];
    for _ in 0..2 {
        let (key, _, _, reason) = job_ended(&mut ev).await;
        reasons.push((key.ends_with("-early"), reason.unwrap()));
    }
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
    let rest = fake("busy");
    let busy = Ffmpeg::spawn(&rest, &[]).await.unwrap();
    let idle = Ffmpeg::spawn(&fake("hang"), &[]).await.unwrap();
    until("the busy process burns CPU", 60, || {
        busy.cpu_time_ms().unwrap() >= 300
    })
    .await;
    assert!(idle.cpu_time_ms().unwrap() < 150);
    std::fs::write(Path::new(&rest).with_extension("rest"), b"").unwrap();
    busy.stop().await;
    idle.stop().await;
}

#[tokio::test]
async fn a_process_deaf_to_stop_is_killed_after_the_grace() {
    let p = Ffmpeg::spawn(&fake("hang"), &[]).await.unwrap();
    // It says so once its signal handling is in place.
    until("the handler", 30, || p.stderr_lines() > 0).await;
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

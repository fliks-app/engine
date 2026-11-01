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
    engine_on(&fake(mode), streaming).await
}

/// `engine_of` over any tool binary, used for all three tools.
async fn engine_on(f: &str, streaming: Value) -> (Engine, PathBuf) {
    let pause = if cfg!(unix) { "signal" } else { "stdin" };
    let e = engine_with_tools(
        host(HwAccel::None, "/dev/dri/renderD128", pause),
        f,
        f,
        f,
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
    let file = e.root.join("movie.mkv");
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
        let ev = tokio::time::timeout(Duration::from_secs(120), rx.recv())
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
    assert_eq!(
        jpg.header("cache-control").as_deref(),
        Some("private, max-age=86400")
    );
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
    until("frame runs", 30, || !e.ffmpeg_pids().is_empty()).await;
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
async fn a_remux_open_runs_the_queued_scan_it_needs_at_once() {
    // Two fingerprints whose fpcalc hangs hold both job places and the one background slot.
    let (e, file) = engine_of("hangfp", json!({ "ffmpegSlots": 1 })).await;
    let head = json!({ "type": "Fingerprint", "window": { "type": "head" } });
    let a = post(&e, 7, &file, head.clone()).await.json()["jobId"].clone();
    let b = post(&e, 8, &file, head).await.json()["jobId"].clone();
    until("fingerprint runs", 30, || !e.ffmpeg_pids().is_empty()).await;
    let scan = post(&e, 9, &file, json!({ "type": "Scan", "info": info() })).await;
    let scan = scan.json()["jobId"].as_str().unwrap().to_string();
    assert_eq!(
        job(&e, &scan).await["state"],
        "queued",
        "behind two jobs and the slot"
    );
    let info = serde_json::from_value(info()).unwrap();
    let copy = json!({ "device": {
        "directPlayProfiles": [{ "containers": ["mp4"], "videoCodecs": ["h264"], "audioCodecs": ["aac"] }],
        "rejectCopy": false } });
    let open = e.open(9, &file, &info, "alice", copy).await;
    assert_eq!(open["decision"]["response"]["playMethod"], "DirectStream");
    assert_eq!(open["hints"]["scanMissing"], true);
    assert_eq!(open["scan"]["keyframes"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        job(&e, &scan).await["state"],
        "done",
        "the queued job itself ran"
    );
    for id in [a, b] {
        let state = job(&e, id.as_str().unwrap()).await["state"].clone();
        assert!(state == "running" || state == "paused", "{state}");
    }
    e.rt.shutdown().await;
    assert!(e.ffmpeg_pids().is_empty());
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

/// Waits for the main and early runs of `sid`: they launch after `open` returns, and a stop only
/// reaches the registered ones.
#[cfg(unix)]
async fn playback_runs(e: &Engine, sid: &str) {
    use fliks_engine::runtime::JobKind;
    let s = e.rt.peek(sid).unwrap();
    until("both playback runs", 30, || {
        [JobKind::Main, JobKind::Early]
            .iter()
            .all(|k| e.rt.job(&s.key(*k)).is_some_and(|j| j.producing()))
    })
    .await;
}

/// `ps` state of the sprite frame runs of `e`.
#[cfg(unix)]
fn frame_states(e: &Engine) -> Vec<String> {
    e.ffmpeg_procs()
        .into_iter()
        .filter(|(_, _, cmd)| cmd.contains("-frames:v"))
        .map(|(_, stat, _)| stat)
        .collect()
}

#[cfg(unix)]
#[tokio::test]
async fn playback_preempts_a_background_job_then_it_resumes() {
    // A frame run that never ends: the preemption always meets a live run.
    let (e, file) = engine_of("hangsprite", json!({ "ffmpegSlots": 1 })).await;
    let id = submit(&e, 7, &file, false).await;
    until("a frame run", 30, || frame_states(&e).len() == 1).await;
    // A resume on a pinned rung: the main run and its early companion take playback slots.
    let info = serde_json::from_value(info()).unwrap();
    let open = json!({ "startAtS": 60.0, "selection": { "requestedQuality": "720p" } });
    let sid = e.open(7, &file, &info, "alice", open).await["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    playback_runs(&e, &sid).await;
    assert_eq!(e.rt.slots.playback_in_use(), 2);
    until("frame paused", 30, || {
        let s = frame_states(&e);
        !s.is_empty() && s.iter().all(|s| s.starts_with('T'))
    })
    .await;
    until_async("job paused", 30, || async {
        job(&e, &id).await["state"] == "paused"
    })
    .await;
    let r = e
        .control("DELETE", &format!("/v1/sessions/{sid}"), None)
        .await;
    assert_eq!(r.status, 204);
    assert_eq!(
        e.rt.slots.playback_in_use(),
        0,
        "stop returns with the slots freed"
    );
    until("frame resumed", 30, || {
        let s = frame_states(&e);
        !s.is_empty() && s.iter().all(|s| !s.starts_with('T'))
    })
    .await;
    until_async("job running", 30, || async {
        job(&e, &id).await["state"] == "running"
    })
    .await;
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
    until("a tool run", 30, || !e.ffmpeg_pids().is_empty()).await;
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

/// A MPEG-TS of 188-byte packets: PAT, a PMT with H.264 on 0x100 and ADTS on 0x101, and one PES
/// of 0x101 holding two frames (sampling index 4, then `second`).
fn ts_bytes(second: u8) -> Vec<u8> {
    let packet = |pid: u16, pusi: bool, cc: u8, payload: &[u8]| {
        let mut p = vec![0x47, u8::from(pusi) << 6 | (pid >> 8) as u8, pid as u8];
        let stuff = 183 - payload.len();
        p.extend([0x30 | cc, stuff as u8]);
        if stuff > 0 {
            p.extend([0]);
            p.extend(vec![0xff; stuff - 1]);
        }
        p.extend_from_slice(payload);
        p
    };
    let pat = [0, 0, 0xb0, 13, 0, 1, 0xc1, 0, 0, 0, 1, 0xf0, 0, 0, 0, 0, 0];
    let pmt = [
        0, 2, 0xb0, 23, 0, 1, 0xc1, 0, 0, 0xe1, 0, 0xf0, 0, 0x1b, 0xe1, 0, 0xf0, 0, 0x0f, 0xe1, 1,
        0xf0, 0, 0, 0, 0, 0,
    ];
    let frame = |sf: u8| [0xff, 0xf1, 0x40 | sf << 2, 0x80, 0x01, 0x1f, 0xfc, 0];
    let mut pes = vec![0, 0, 1, 0xc0, 0, 0, 0x80, 0, 0];
    pes.extend([frame(4), frame(second)].concat());
    [
        packet(0, true, 0, &pat),
        packet(0x1000, true, 0, &pmt),
        packet(0x101, true, 0, &pes),
    ]
    .concat()
}

#[tokio::test]
async fn scan_job_of_mpeg_ts_reads_the_adts_headers_from_the_same_bytes() {
    let ts = json!({ "formatName": "mpegts" });
    let (e, file) = engine_of("scan", json!({})).await;
    // Stream 2 has no PID in ffprobe's answer: it may change, whatever the bytes say.
    let mut info = tool_info(ts);
    let fra = json!({ "streamIndex": 2, "codec": "aac", "language": "fra" });
    info["audio"].as_array_mut().unwrap().push(fra);
    for (second, changed) in [(4, false), (3, true)] {
        std::fs::write(&file, ts_bytes(second)).unwrap();
        let kind = json!({ "type": "Scan", "info": info });
        let v = run_kind(&e, 7, &file, kind).await;
        assert_eq!(v["state"], "done", "{second}: {v}");
        assert_eq!(
            v["result"]["scan"]["audioConfigChanges"],
            json!({ "1": changed, "2": true }),
            "{second}"
        );
        assert!(e.ffmpeg_pids().is_empty());
    }
}

/// The nice values the fake packet run logged, one per run.
#[cfg(target_os = "linux")]
fn runs_of(file: &std::path::Path) -> Vec<String> {
    let log = format!("{}.runs", file.display());
    std::fs::read_to_string(log)
        .map(|t| t.lines().map(String::from).collect())
        .unwrap_or_default()
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn promoting_a_running_scan_redoes_it_on_a_playback_slot_without_the_wrapper() {
    // SAFETY: geteuid has no preconditions. A privileged engine renices in place instead.
    if unsafe { libc::geteuid() } == 0 || !fliks_engine::proc::background("x", &[]).0.eq("ionice") {
        return;
    }
    let (e, file) = engine_of("idlescan", json!({ "ffmpegSlots": 1 })).await;
    let req = |_| {
        serde_json::from_value(json!({
            "kind": { "type": "Scan", "info": info() },
            "media": { "mediaFileId": 7, "path": file },
        }))
        .unwrap()
    };
    let mut rx = e.rt.events.subscribe();
    let id = fliks_engine::jobs::submit(&e.rt, req(())).unwrap();
    until("the idle-class packet run", 30, || runs_of(&file) == ["19"]).await;
    assert_eq!(e.rt.slots.playback_in_use(), 0);
    let again = fliks_engine::jobs::submit_urgent(&e.rt, req(())).unwrap();
    assert_eq!(again, id, "one job, one run key");
    let (_, ev) = done(&mut rx, &id).await;
    let v = done_json(&ev);
    assert_eq!(v["state"], "done", "{v}");
    assert_eq!(
        v["result"]["scan"]["keyframes"].as_array().map(Vec::len),
        Some(2)
    );
    assert_eq!(runs_of(&file), ["19", "0"], "redone once, without nice");
    assert_eq!(
        e.control("GET", "/v1/jobs", None)
            .await
            .json()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(e.ffmpeg_pids().is_empty());
    until("slots released", 30, || e.rt.slots.playback_in_use() == 0).await;
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
async fn concurrent_embedded_requests_extract_once() {
    let (e, file) = engine_of("slowsub", json!({})).await;
    let rs = futures_util::future::join_all(
        (0..6).map(|_| e.get("/api/stream/7/subtitles/embedded/2", "alice-jwt", &[])),
    )
    .await;
    assert!(
        rs.iter()
            .all(|r| r.status == 200 && r.text() == rs[0].text())
    );
    let runs = std::fs::read_to_string(format!("{}.runs", file.display())).unwrap();
    assert_eq!(runs.lines().count(), 1, "one ffmpeg run for six requests");
}

#[tokio::test]
async fn one_embedded_request_extracts_every_text_track() {
    let (e, file) = engine_of("slowsub", json!({})).await;
    for idx in [2, 3] {
        let r = e
            .get(
                &format!("/api/stream/7/subtitles/embedded/{idx}"),
                "alice-jwt",
                &[],
            )
            .await;
        assert_eq!(r.status, 200, "track {idx}");
    }
    let runs = std::fs::read_to_string(format!("{}.runs", file.display())).unwrap();
    assert_eq!(runs.lines().count(), 1, "one ffmpeg run for two tracks");
    let r = e
        .get("/api/stream/7/subtitles/embedded/4", "alice-jwt", &[])
        .await;
    assert_eq!(r.status, 404, "a bitmap track is not extracted");
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
    until("the first run", 30, || !e.ffmpeg_pids().is_empty()).await;
    until("the second waits for the slot", 30, || {
        e.rt.slots.waiting() == 1
    })
    .await;
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

const OCR_SRT: &str = "1\n00:00:00,000 --> 00:00:01,000\nhello\n";

fn pgs_track(index: u32) -> Value {
    json!({ "streamIndex": index, "codec": "hdmv_pgs_subtitle", "language": "und",
        "forced": false, "hearingImpaired": false, "isImageBased": true })
}

fn vobsub_track(index: u32) -> Value {
    json!({ "streamIndex": index, "codec": "dvd_subtitle", "language": "en",
        "forced": false, "hearingImpaired": false, "isImageBased": true })
}

async fn engine_ocr() -> (Engine, PathBuf) {
    let fake = fake("ocr");
    let dir = PathBuf::from(&fake).parent().unwrap().to_path_buf();
    for name in ["pgsrip", "mkvextract", "subtile-ocr"] {
        let link = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        if !link.exists() {
            std::fs::hard_link(&fake, &link).unwrap();
        }
    }
    let sep = if cfg!(windows) { ";" } else { ":" };
    unsafe {
        std::env::set_var(
            "PATH",
            format!(
                "{}{}{}",
                dir.display(),
                sep,
                std::env::var("PATH").unwrap_or_default()
            ),
        );
    }
    let pause = if cfg!(unix) { "signal" } else { "stdin" };
    let e = engine_with_tools(
        host(HwAccel::None, "/dev/dri/renderD128", pause),
        &fake,
        &fake,
        &fake,
        json!({}),
    )
    .await;
    let file = e.root.join("movie.mkv");
    std::fs::write(&file, b"x").unwrap();
    e.stub.file(7, &file);
    e.stub.grant("alice-jwt", "alice");
    (e, file)
}

#[tokio::test]
async fn subtitle_ocr_job_pgs_and_vobsub() {
    let (e, file) = engine_ocr().await;
    let pgs = json!({ "type": "SubtitleOcr", "info": tool_info(json!({ "subtitles": [pgs_track(4)] })),
        "streamIndex": 4 });
    let v = run_kind(&e, 7, &file, pgs).await;
    assert_eq!(v["state"], "done", "{v}");
    assert_eq!(v["result"]["type"], "SubtitleOcr");
    assert_eq!(v["result"]["srt"], OCR_SRT);
    let vob = json!({ "type": "SubtitleOcr", "info": tool_info(json!({ "subtitles": [vobsub_track(6)] })),
        "streamIndex": 6 });
    let v = run_kind(&e, 7, &file, vob).await;
    assert_eq!(v["state"], "done", "{v}");
    assert_eq!(v["result"]["srt"], OCR_SRT);
    assert!(e.ffmpeg_pids().is_empty());
}

async fn engine_sync() -> (Engine, PathBuf) {
    let fake_bin = fake("sync");
    let dir = PathBuf::from(&fake_bin).parent().unwrap().to_path_buf();
    let link = dir.join(format!("ffsubsync{}", std::env::consts::EXE_SUFFIX));
    if !link.exists() {
        std::fs::hard_link(&fake_bin, &link).unwrap();
    }
    let sep = if cfg!(windows) { ";" } else { ":" };
    unsafe {
        std::env::set_var(
            "PATH",
            format!(
                "{}{}{}",
                dir.display(),
                sep,
                std::env::var("PATH").unwrap_or_default()
            ),
        );
    }
    let pause = if cfg!(unix) { "signal" } else { "stdin" };
    let e = engine_with_tools(
        host(HwAccel::None, "/dev/dri/renderD128", pause),
        &fake_bin,
        &fake_bin,
        &fake_bin,
        json!({}),
    )
    .await;
    let file = e.root.join("movie.mkv");
    std::fs::write(&file, b"x").unwrap();
    e.stub.file(7, &file);
    e.stub.grant("alice-jwt", "alice");
    (e, file)
}

fn sync_kind(text: &str) -> Value {
    json!({
        "type": "SubtitleSync",
        "info": info(),
        "subtitle": { "format": "srt", "text": text },
        "reference": { "type": "auto" },
        "maxOffsetSeconds": 12,
        "noFixFramerate": true,
        "gss": true
    })
}

#[tokio::test]
async fn subtitle_sync_ffsubsync_then_speech_retry() {
    let (e, file) = engine_sync().await;
    let late = "1\n00:00:10,000 --> 00:00:12,000\nLATE\n";
    let v = run_kind(&e, 7, &file, sync_kind(late)).await;
    assert_eq!(v["state"], "done", "{v}");
    assert_eq!(
        v["result"]["subtitle"]["text"],
        "1\n00:00:10,000 --> 00:00:12,000\nSYNCED\n"
    );
    let noisy = "1\n00:00:10,000 --> 00:00:12,000\nNOSPEECH LATE\n";
    let v = run_kind(&e, 7, &file, sync_kind(noisy)).await;
    assert_eq!(v["state"], "done", "{v}");
    assert!(
        v["result"]["subtitle"]["text"]
            .as_str()
            .unwrap()
            .contains("SYNCED"),
        "{v}"
    );
    let v = run_kind(
        &e,
        7,
        &file,
        sync_kind("1\n00:00:01,000 --> 00:00:02,000\nFAIL\n"),
    )
    .await;
    assert_eq!(v["state"], "failed", "{v}");
    let err = v["error"].as_str().unwrap();
    assert!(err.contains("ffsubsync stderr:"), "{err}");
    assert!(err.contains("ffsubsync boom"), "{err}");
    assert!(err.contains("alass error:"), "{err}");
}

#[tokio::test]
async fn subtitle_to_srt_reads_ffmpeg_stdout() {
    let (e, file) = engine_of("srt", json!({})).await;
    let mut probed = info();
    probed["subtitles"] = json!([{
        "streamIndex": 3, "codec": "ass", "language": "eng",
        "forced": false, "hearingImpaired": false, "isImageBased": false
    }]);
    let kind = json!({
        "type": "SubtitleToSrt",
        "info": probed,
        "source": { "type": "stream", "index": 3 }
    });
    let v = run_kind(&e, 7, &file, kind).await;
    assert_eq!(v["state"], "done", "{v}");
    assert!(
        v["result"]["srt"].as_str().unwrap().contains("hello"),
        "{v}"
    );
    let file_kind = json!({
        "type": "SubtitleToSrt",
        "info": info(),
        "source": { "type": "file", "path": file.with_extension("ass").display().to_string() }
    });
    let v = run_kind(&e, 7, &file, file_kind).await;
    assert_eq!(v["state"], "done", "{v}");
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
    until("the tool run", 30, || !e.ffmpeg_pids().is_empty()).await;
    let info = serde_json::from_value(info()).unwrap();
    let open = json!({ "startAtS": 60.0, "selection": { "requestedQuality": "720p" } });
    let sid = e.open(7, &file, &info, "alice", open).await["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    playback_runs(&e, &sid).await;
    until_async("job paused", 30, || async {
        job(&e, &id).await["state"] == "paused"
    })
    .await;
    let r = e
        .control("DELETE", &format!("/v1/sessions/{sid}"), None)
        .await;
    assert_eq!(r.status, 204);
    e.rt.shutdown().await;
    assert!(e.ffmpeg_pids().is_empty());
}

fn audio_book() -> Value {
    json!({
        "video": [],
        "audio": [{ "streamIndex": 0, "codec": "aac", "language": "und", "channels": 1, "sampleRate": 48000 }],
        "subtitles": [], "formatName": "mov,mp4,m4a,3gp,3g2,mj2", "durationSeconds": 3.0,
        "cover": { "streamIndex": 2, "codec": "mjpeg", "width": 64, "height": 64 },
    })
}

#[tokio::test]
async fn cover_job_stores_the_picture_and_the_control_plane_reads_it() {
    let (e, file) = engine_of("cover", json!({})).await;
    let kind = json!({ "type": "Cover", "info": audio_book() });
    let v = run_kind(&e, 7, &file, kind.clone()).await;
    let want = json!({ "type": "Cover", "cover": { "mime": "image/jpeg", "bytes": 9 } });
    assert_eq!(
        (v["state"].as_str(), &v["result"]),
        (Some("done"), &want),
        "{v}"
    );
    let r = e.control("GET", "/v1/covers/7", None).await;
    assert_eq!(
        (r.status, r.header("content-type")),
        (200, Some("image/jpeg".into()))
    );
    assert_eq!(&r.body[..], b"\xFF\xD8cover\xFF\xD9");
    // A stored cover answers a second job without a run.
    let v = run_kind(&e, 7, &file, kind).await;
    assert_eq!(v["result"], want, "{v}");
    assert_eq!(e.control("GET", "/v1/covers/8", None).await.status, 404);
    let r = e
        .control("POST", "/v1/invalidate", Some(json!({ "mediaFileId": 7 })))
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(e.control("GET", "/v1/covers/7", None).await.status, 404);
    assert!(e.ffmpeg_pids().is_empty());
}

#[tokio::test]
async fn cover_job_fails_on_an_empty_picture() {
    let (e, file) = engine_of("emptycover", json!({})).await;
    let kind = json!({ "type": "Cover", "info": audio_book() });
    let v = run_kind(&e, 7, &file, kind).await;
    assert_eq!(
        (v["state"].as_str(), v["error"].as_str()),
        (Some("failed"), Some("ffmpeg wrote no picture"))
    );
    assert_eq!(e.control("GET", "/v1/covers/7", None).await.status, 404);
}

#[tokio::test]
async fn cover_job_refuses_a_picture_over_the_cap() {
    let (e, file) = engine_of("bigcover", json!({})).await;
    let kind = json!({ "type": "Cover", "info": audio_book() });
    let v = run_kind(&e, 7, &file, kind).await;
    assert_eq!(
        (v["state"].as_str(), v["error"].as_str()),
        (Some("failed"), Some("cover too large"))
    );
    assert_eq!(e.control("GET", "/v1/covers/7", None).await.status, 404);
    let dir = std::fs::read_dir(e.root.join("covers/v1/7"));
    assert_eq!(dir.map_or(0, |d| d.count()), 0, "no temporary file is left");
}

/// The real ffmpeg on a BMP picture, which is neither copied nor stored as is: skipped without ffmpeg.
#[tokio::test]
async fn cover_job_re_encodes_a_bmp_picture_to_jpeg() {
    let dir = temp_dir("cover-real");
    let mp3 = dir.join("bmp.mp3");
    let made = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=duration=1",
            "-f",
            "lavfi",
        ])
        .args([
            "-i",
            "color=c=green:size=16x16:duration=1:rate=1",
            "-map",
            "0:a",
        ])
        .args([
            "-map",
            "1:v",
            "-frames:v",
            "1",
            "-c:a",
            "libmp3lame",
            "-c:v",
            "bmp",
        ])
        .args(["-disposition:v", "attached_pic"])
        .arg(&mp3)
        .status();
    if !made.is_ok_and(|s| s.success()) {
        eprintln!("ffmpeg with libmp3lame not available: skipped");
        return;
    }
    let (e, _) = engine_on("ffmpeg", json!({})).await;
    e.stub.file(7, &mp3);
    let mut info = audio_book();
    info["cover"] = json!({ "streamIndex": 1, "codec": "bmp", "width": 16, "height": 16 });
    let v = run_kind(&e, 7, &mp3, json!({ "type": "Cover", "info": info })).await;
    assert_eq!(v["state"], "done", "{v}");
    assert_eq!(v["result"]["cover"]["mime"], "image/jpeg");
    let r = e.control("GET", "/v1/covers/7", None).await;
    assert_eq!(&r.body[..3], b"\xFF\xD8\xFF", "a JPEG");
}

#[tokio::test]
async fn jobs_are_filtered_by_the_family_of_the_file() {
    let (e, file) = engine_of("cover", json!({})).await;
    let book = audio_book();
    let mut with_cover = info();
    with_cover["cover"] = book["cover"].clone();
    let empty = json!({ "video": [], "audio": [], "subtitles": [] });
    for (kind, why) in [
        (
            json!({ "type": "Sprite", "info": book }),
            "Sprite does not apply to audio files",
        ),
        (
            json!({ "type": "Scan", "info": book }),
            "Scan does not apply to audio files",
        ),
        (
            json!({ "type": "SubtitleExtract", "info": book }),
            "SubtitleExtract does not apply to audio files",
        ),
        (
            json!({ "type": "Cover", "info": with_cover }),
            "Cover does not apply to video files",
        ),
        (
            json!({ "type": "Cover", "info": empty }),
            "Cover needs info.cover",
        ),
    ] {
        let r = post(&e, 7, &file, kind).await;
        assert_eq!((r.status, r.text().as_str()), (400, why));
    }
    let hello = e.control("GET", "/v1/hello", None).await.json();
    let has = |v: Value| hello["jobFamilies"].as_array().unwrap().contains(&v);
    assert!(
        hello["features"]
            .as_array()
            .unwrap()
            .contains(&json!("cover"))
    );
    assert!(has(json!({ "kind": "Sprite", "families": ["video"] })));
    assert!(has(json!({ "kind": "Cover", "families": ["audio"] })));
}

fn aac_part(path: &std::path::Path, seconds: f64, title: &str) -> Value {
    json!({ "path": path, "info": {
        "video": [], "audio": [{ "streamIndex": 0, "codec": "aac", "language": "und", "channels": 1,
            "sampleRate": 44100, "profile": "LC" }],
        "subtitles": [], "durationSeconds": seconds, "formatName": "mov,mp4,m4a,3gp,3g2,mj2",
        "tags": { "title": title, "album": "Synthetic Book", "narrator": "Nar Rator" } } })
}

fn merge_kind(dir: &std::path::Path, output: &std::path::Path) -> Value {
    json!({ "type": "Merge", "output": output, "parts": [
        aac_part(&dir.join("a.m4a"), 100.0, "One"), aac_part(&dir.join("b.m4a"), 50.5, "Two")] })
}

fn files_in(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|d| {
            d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[tokio::test]
async fn merge_job_publishes_the_m4b_reports_progress_and_never_overwrites() {
    let (e, file) = engine_of("merge", json!({})).await;
    let out = e.root.join("books/Book.m4b");
    let mut rx = e.rt.events.subscribe();
    let r = post(&e, 7, &file, merge_kind(&e.root, &out)).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let id = r.json()["jobId"].as_str().unwrap().to_string();
    let (progress, ev) = done(&mut rx, &id).await;
    let v = done_json(&ev);
    let bytes = std::fs::metadata(&out).unwrap().len() as f64;
    let want = json!({ "type": "Merge", "merge": { "path": out, "bytes": bytes,
        "durationSeconds": 150.5, "reencoded": false, "chapters": [
            { "startSeconds": 0.0, "endSeconds": 100.0, "title": "One" },
            { "startSeconds": 100.0, "endSeconds": 150.5, "title": "Two" }] } });
    assert_eq!(
        (v["state"].as_str(), &v["result"]),
        (Some("done"), &want),
        "{v}"
    );
    assert!(!progress.is_empty() && progress.iter().all(|p| (0.0..=99.0).contains(p)));
    let written = std::fs::read(&out).unwrap();
    assert!(
        written.windows(8).any(|w| w == b"narrator") && written.ends_with(b"Nar Rator"),
        "the free-form tags are in the moov"
    );
    assert_eq!(
        files_in(&e.root.join("books")),
        ["Book.m4b"],
        "no temporary file"
    );
    assert_eq!(
        files_in(&e.root.join("merge/v1")),
        Vec::<String>::new(),
        "no scratch"
    );
    // The name is taken: the job fails and the file is untouched.
    let v = run_kind(&e, 7, &file, merge_kind(&e.root, &out)).await;
    assert_eq!(v["state"], "failed", "{v}");
    assert!(
        v["error"].as_str().unwrap().starts_with("output exists: "),
        "{v}"
    );
    assert_eq!(std::fs::read(&out).unwrap(), written);
    assert_eq!(files_in(&e.root.join("books")), ["Book.m4b"]);
}

#[tokio::test]
async fn merge_job_leaves_nothing_behind_when_ffmpeg_fails_or_it_is_cancelled() {
    let (e, file) = engine_of("failmerge", json!({})).await;
    let out = e.root.join("books/Book.m4b");
    let v = run_kind(&e, 7, &file, merge_kind(&e.root, &out)).await;
    assert_eq!(v["state"], "failed", "{v}");
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .starts_with("ffmpeg exited with Some(1)"),
        "{v}"
    );
    assert_eq!(files_in(&e.root.join("books")), Vec::<String>::new());
    let (e, file) = engine_of("hangmerge", json!({})).await;
    let out = e.root.join("books/Book.m4b");
    hang_then_cancel(&e, &file, merge_kind(&e.root, &out)).await;
    assert_eq!(files_in(&e.root.join("books")), Vec::<String>::new());
    assert_eq!(files_in(&e.root.join("merge/v1")), Vec::<String>::new());
}

#[tokio::test]
async fn merge_job_respects_the_disk_threshold_and_validates_its_request() {
    let (e, file) = engine_of("merge", json!({ "jobMinFreeBytes": 1e18 })).await;
    let out = e.root.join("books/Book.m4b");
    let v = run_kind(&e, 7, &file, merge_kind(&e.root, &out)).await;
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .starts_with("not enough disk space: "),
        "{v}"
    );
    let book = |f: &dyn Fn(&mut Value)| {
        let mut k = merge_kind(&e.root, &out);
        f(&mut k);
        k
    };
    let video = info();
    for (kind, why) in [
        (
            book(&|k| k["parts"] = json!([])),
            "Merge needs 1 to 500 parts",
        ),
        (
            book(&|k| k["output"] = json!("/books/Book.mp3")),
            "Merge output must be an absolute .m4b path without control characters",
        ),
        (
            book(&|k| k["parts"][1]["path"] = json!("b.m4a")),
            "Merge part 2 path must be absolute, without control characters",
        ),
        (
            book(&|k| k["parts"][1]["path"] = json!("/x/b.m4a\nfile '/etc/passwd'")),
            "Merge part 2 path must be absolute, without control characters",
        ),
        (
            book(&|k| k["parts"][0]["info"] = video.clone()),
            "Merge part 1 is not an audio file",
        ),
        (
            book(&|k| k["parts"][1]["info"]["durationSeconds"] = json!(0)),
            "Merge part 2 has no duration",
        ),
    ] {
        let r = post(&e, 7, &file, kind).await;
        assert_eq!((r.status, r.text().as_str()), (400, why));
    }
    let hello = e.control("GET", "/v1/hello", None).await.json();
    assert!(
        hello["features"]
            .as_array()
            .unwrap()
            .contains(&json!("merge"))
    );
    assert!(
        hello["jobFamilies"]
            .as_array()
            .unwrap()
            .contains(&json!({ "kind": "Merge", "families": ["audio"] }))
    );
}

fn tool_ok(bin: &str, args: &[&std::ffi::OsStr]) -> bool {
    std::process::Command::new(bin)
        .args(["-v", "error"])
        .args(args)
        .stdout(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Synthetic sine-tone parts of an audiobook, probed by the engine: a cover and tags, own
/// chapters, a plain AAC part and a WAV in another format.
async fn real_parts(e: &Engine, dir: &std::path::Path) -> Option<Vec<Value>> {
    let ff = |args: &[&str]| {
        let args: Vec<&std::ffi::OsStr> = args.iter().map(std::ffi::OsStr::new).collect();
        tool_ok(&e.ffmpeg, &args)
    };
    let p = |n: &str| dir.join(n).to_string_lossy().into_owned();
    let chapters = dir.join("chapters.txt");
    std::fs::write(
        &chapters,
        ";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=1000\ntitle=Inner A\n\
         [CHAPTER]\nTIMEBASE=1/1000\nSTART=1000\nEND=2500\ntitle=Inner B\n",
    )
    .unwrap();
    let sine = |hz: &str, secs: &str, rate: &str| {
        format!("sine=frequency={hz}:duration={secs}:sample_rate={rate}")
    };
    let made = ff(&[
        "-y",
        "-f",
        "lavfi",
        "-i",
        "color=c=red:size=64x64:rate=1:duration=1",
        "-frames:v",
        "1",
        &p("cover.jpg"),
    ]) && ff(&[
        "-y",
        "-f",
        "lavfi",
        "-i",
        &sine("440", "2", "44100"),
        "-i",
        &p("cover.jpg"),
        "-map",
        "0:a",
        "-map",
        "1:v",
        "-c:a",
        "aac",
        "-b:a",
        "64k",
        "-c:v",
        "copy",
        "-disposition:v",
        "attached_pic",
        "-metadata",
        "title=Part One",
        "-metadata",
        "album=Synth Book",
        "-metadata",
        "artist=Au Thor",
        "-metadata",
        "date=2020",
        &p("p1.m4a"),
    ]) && ff(&[
        "-y",
        "-f",
        "lavfi",
        "-i",
        &sine("550", "2.5", "44100"),
        "-i",
        &chapters.to_string_lossy(),
        "-map",
        "0:a",
        "-map_chapters",
        "1",
        "-c:a",
        "aac",
        "-b:a",
        "64k",
        "-movflags",
        "use_metadata_tags",
        "-metadata",
        "asin=B000TEST00",
        &p("p2.m4a"),
    ]) && ff(&[
        "-y",
        "-f",
        "lavfi",
        "-i",
        &sine("660", "1.5", "44100"),
        "-c:a",
        "aac",
        "-b:a",
        "64k",
        &p("p3.m4a"),
    ]) && ff(&[
        "-y",
        "-f",
        "lavfi",
        "-i",
        &sine("770", "1.5", "22050"),
        "-c:a",
        "pcm_s16le",
        &p("p4.wav"),
    ]);
    if !made {
        return None;
    }
    let mut parts = vec![];
    for (i, n) in ["p1.m4a", "p2.m4a", "p3.m4a", "p4.wav"].iter().enumerate() {
        let body = json!({ "media": { "mediaFileId": i + 1, "path": p(n) } });
        let probed = e.control("POST", "/v1/probe", Some(body)).await.json();
        assert_eq!(probed["ok"], true, "{probed}");
        parts.push(json!({ "path": p(n), "info": probed["info"] }));
    }
    Some(parts)
}

fn ffprobe_json(bin: &str, file: &std::path::Path) -> Value {
    let out = std::process::Command::new(bin)
        .args([
            "-v",
            "error",
            "-show_chapters",
            "-show_entries",
            "format=duration:format_tags",
        ])
        .args(["-of", "json"])
        .arg(file)
        .output()
        .unwrap();
    serde_json::from_slice(&out.stdout).unwrap()
}

/// The real ffmpeg on synthetic parts, once copied and once encoded: skipped without ffmpeg.
#[tokio::test]
async fn merge_job_with_the_real_ffmpeg_copies_or_encodes_and_chapters_the_parts() {
    let ffprobe = fliks_engine::hw::tool_path("FFPROBE_PATH", "ffprobe");
    if !tool_ok(&ffmpeg(), &[std::ffi::OsStr::new("-version")])
        || !tool_ok(&ffprobe, &[std::ffi::OsStr::new("-version")])
    {
        eprintln!("ffmpeg or ffprobe not available: skipped");
        return;
    }
    let pause = if cfg!(unix) { "signal" } else { "stdin" };
    let hw = host(HwAccel::None, "/dev/dri/renderD128", pause);
    let e = engine_with_tools(hw, &ffmpeg(), &ffprobe, "fpcalc", json!({})).await;
    let dir = temp_dir("merge-real");
    let Some(parts) = real_parts(&e, &dir).await else {
        eprintln!("ffmpeg cannot make the synthetic parts: skipped");
        return;
    };
    let first = PathBuf::from(parts[0]["path"].as_str().unwrap());
    let tags = json!({ "narrator": "Nar Rator", "series": "Series X" });
    let cases = [
        (
            "copy.m4b",
            &parts[..3],
            false,
            6.0,
            vec![
                ("Part One", 0.0),
                ("Inner A", 2.0),
                ("Inner B", 3.0),
                ("Part 3", 4.5),
            ],
        ),
        (
            "encode.m4b",
            &[&parts[..2], &parts[3..]].concat()[..],
            true,
            6.0,
            vec![
                ("Part One", 0.0),
                ("Inner A", 2.0),
                ("Inner B", 3.0),
                ("Part 3", 4.5),
            ],
        ),
    ];
    for (name, set, reencoded, total, chapters) in cases {
        let out = dir.join(name);
        let kind = json!({ "type": "Merge", "parts": set, "output": out, "tags": tags });
        let v = run_kind(&e, 1, &first, kind).await;
        assert_eq!(v["state"], "done", "{name}: {v}");
        assert_eq!(v["result"]["merge"]["reencoded"], reencoded, "{name}");
        let probed = ffprobe_json(&ffprobe, &out);
        let got: Vec<(String, f64)> = probed["chapters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                (
                    c["tags"]["title"].as_str().unwrap().into(),
                    c["start_time"].as_str().unwrap().parse().unwrap(),
                )
            })
            .collect();
        assert_eq!(got.len(), chapters.len(), "{name}: {probed}");
        for ((title, at), (want, want_at)) in got.iter().zip(&chapters) {
            assert!(
                title == want && (at - want_at).abs() < 0.05,
                "{name}: {title} {at}"
            );
        }
        let seconds: f64 = probed["format"]["duration"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!((seconds - total).abs() < 0.15, "{name}: {seconds}s");
        let t = &probed["format"]["tags"];
        let tag = |k: &str| t[k].as_str().unwrap_or_default().to_string();
        assert_eq!(
            [
                tag("title"),
                tag("album"),
                tag("artist"),
                tag("date"),
                tag("narrator"),
                tag("series"),
                tag("asin")
            ],
            [
                "Synth Book",
                "Synth Book",
                "Au Thor",
                "2020",
                "Nar Rator",
                "Series X",
                "B000TEST00"
            ],
            "{name}: {t}"
        );
        let body = json!({ "media": { "mediaFileId": 9, "path": out } });
        let info = e.control("POST", "/v1/probe", Some(body)).await.json()["info"].clone();
        assert_eq!(info["cover"]["codec"], "mjpeg", "{name}: the cover is kept");
        assert_eq!(info["audio"][0]["codec"], "aac", "{name}");
    }
    assert!(e.ffmpeg_pids().is_empty());
}

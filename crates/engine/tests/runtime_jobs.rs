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

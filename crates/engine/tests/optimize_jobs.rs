//! The `Optimize` and `Verify` jobs on lavfi-generated media with the real ffmpeg: remux with
//! retagging, verification of the output, refusal of a bad source, staged files, validation.
//! Skipped without ffmpeg; the re-encode needs libx265 and runs where it exists (CI).
mod runtime_common;
use fliks_engine_core::api::{EngineEvent, HwAccel};
use runtime_common::*;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn ok(bin: &str, args: &[&str]) -> bool {
    std::process::Command::new(bin)
        .args(["-v", "error"])
        .args(args)
        .stdout(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn encoder(bin: &str, name: &str) -> bool {
    std::process::Command::new(bin)
        .args(["-hide_banner", "-encoders"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(name))
}

struct Fixture {
    e: Engine,
    dir: TempDir,
    src: PathBuf,
    info: Value,
}

/// A 30 s MKV (H.264, one AAC track, one SRT) made by ffmpeg and probed by the engine.
async fn fixture(seconds: u32) -> Option<Fixture> {
    let ffprobe = fliks_engine::hw::tool_path("FFPROBE_PATH", "ffprobe");
    if !ok(&ffmpeg(), &["-version"]) || !ok(&ffprobe, &["-version"]) {
        eprintln!("ffmpeg or ffprobe not available: skipped");
        return None;
    }
    let pause = if cfg!(unix) { "signal" } else { "stdin" };
    let e = engine_with_tools(
        host(HwAccel::None, "/dev/dri/renderD128", pause),
        &ffmpeg(),
        &ffprobe,
        "fpcalc",
        json!({}),
    )
    .await;
    let dir = temp_dir("optimize");
    let srt = dir.join("in.srt");
    std::fs::write(&srt, "1\n00:00:01,000 --> 00:00:02,000\nhello\n").unwrap();
    let src = dir.join("movie.mkv");
    let d = seconds.to_string();
    let made = ok(
        &ffmpeg(),
        &[
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc2=size=320x180:rate=24:duration={d}"),
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:duration={d}"),
            "-i",
            srt.to_str().unwrap(),
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-map",
            "2:s",
            "-c:v",
            "libx264",
            "-g",
            "24",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-c:s",
            "copy",
            "-metadata:s:a:0",
            "language=eng",
            src.to_str().unwrap(),
        ],
    );
    if !made {
        eprintln!("ffmpeg cannot make the synthetic file: skipped");
        return None;
    }
    let body = json!({ "media": { "mediaFileId": 1, "path": src } });
    let info = e.control("POST", "/v1/probe", Some(body)).await.json()["info"].clone();
    assert_eq!(info["audio"][0]["language"], "eng", "{info}");
    Some(Fixture { e, dir, src, info })
}

async fn run_kind(f: &Fixture, kind: Value) -> Value {
    let mut rx = f.e.rt.events.subscribe();
    let body = json!({ "kind": kind, "media": { "mediaFileId": 1, "path": f.src } });
    let r = f.e.control("POST", "/v1/jobs", Some(body)).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let id = r.json()["jobId"].as_str().unwrap().to_string();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(300), rx.recv())
            .await
            .expect("BgJobDone")
            .unwrap();
        if let EngineEvent::BgJobDone { job_id, .. } = &ev
            && *job_id == id
        {
            return serde_json::to_value(&ev).unwrap();
        }
    }
}

fn optimize(f: &Fixture, plan: Value, staged: &Path) -> Value {
    json!({ "type": "Optimize", "payload": {
        "info": f.info, "plan": plan, "stagedPath": staged } })
}

fn dir_names(d: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(d)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[tokio::test]
async fn a_remux_retags_a_track_and_hands_back_a_verified_hashed_output() {
    let Some(f) = fixture(8).await else { return };
    let staged = f.dir.join(".fliks-opt-1-1.tmp");
    let plan = json!({ "level": "remux", "streams": [
        { "streamIndex": 1, "keep": true, "language": "fre", "title": "Français" }] });
    let before = std::fs::read(&f.src).unwrap();
    let v = run_kind(&f, optimize(&f, plan, &staged)).await;
    assert_eq!(v["state"], "done", "{v}");
    let m = &v["result"]["optimize"];
    assert_eq!(m["level"], "remux");
    assert_eq!(m["stagedPath"], staged.to_str().unwrap());
    assert_eq!(
        m["outSize"],
        std::fs::metadata(&staged).unwrap().len() as f64
    );
    assert_eq!(m["outSha256"].as_str().unwrap().len(), 64);
    assert_eq!(m["outInfo"]["audio"][0]["language"], "fre");
    assert_eq!(m["outInfo"]["audio"][0]["title"], "Français");
    assert_eq!(m["outInfo"]["subtitles"].as_array().unwrap().len(), 1);
    assert_eq!(
        std::fs::read(&f.src).unwrap(),
        before,
        "the source is untouched"
    );
    assert!(f.e.ffmpeg_pids().is_empty());
}

#[tokio::test]
async fn dropping_a_track_leaves_the_others_and_a_taken_staged_name_is_never_replaced() {
    let Some(f) = fixture(8).await else { return };
    let staged = f.dir.join(".fliks-opt-2-1.tmp");
    let plan = json!({ "level": "remux", "streams": [{ "streamIndex": 2, "keep": false }] });
    let v = run_kind(&f, optimize(&f, plan.clone(), &staged)).await;
    assert_eq!(v["state"], "done", "{v}");
    let out = &v["result"]["optimize"]["outInfo"];
    assert_eq!(out["subtitles"].as_array().unwrap().len(), 0, "{out}");
    assert_eq!(out["audio"].as_array().unwrap().len(), 1);
    // The name is now taken: the job fails and leaves that file alone.
    let size = std::fs::metadata(&staged).unwrap().len();
    let v = run_kind(&f, optimize(&f, plan, &staged)).await;
    assert_eq!(v["state"], "failed", "{v}");
    assert_eq!(std::fs::metadata(&staged).unwrap().len(), size);
}

#[tokio::test]
async fn a_failing_run_leaves_no_staged_file() {
    let Some(f) = fixture(8).await else { return };
    let staged = f.dir.join(".fliks-opt-3-1.tmp");
    // A subtitle file that does not exist makes ffmpeg fail after the staged file was created.
    let plan = json!({ "level": "remux", "extraSubtitles": [{ "path": f.dir.join("none.srt") }] });
    let before = dir_names(&f.dir);
    let v = run_kind(&f, optimize(&f, plan, &staged)).await;
    assert_eq!(v["state"], "failed", "{v}");
    assert!(v["error"].as_str().unwrap().contains(": "), "{v}");
    assert_eq!(dir_names(&f.dir), before);
}

#[tokio::test]
async fn an_ocr_srt_is_added_as_a_text_track() {
    let Some(f) = fixture(8).await else { return };
    let staged = f.dir.join(".fliks-opt-4-1.tmp");
    let srt = f.dir.join("ocr.srt");
    std::fs::write(&srt, "1\n00:00:01,000 --> 00:00:02,000\nbonjour\n").unwrap();
    let plan = json!({ "level": "remux", "streams": [{ "streamIndex": 2, "keep": false }],
        "extraSubtitles": [{ "path": srt, "language": "fre", "title": "OCR" }] });
    let v = run_kind(&f, optimize(&f, plan, &staged)).await;
    assert_eq!(v["state"], "done", "{v}");
    let subs = &v["result"]["optimize"]["outInfo"]["subtitles"];
    assert_eq!(subs[0]["language"], "fre", "{subs}");
    assert_eq!(subs[0]["title"], "OCR");
}

#[tokio::test]
async fn the_metadata_level_edits_a_copy_with_mkvpropedit() {
    let Some(f) = fixture(8).await else { return };
    if !ok("mkvpropedit", &["--version"]) {
        eprintln!("mkvpropedit not available: skipped");
        return;
    }
    let staged = f.dir.join(".fliks-opt-5-1.tmp");
    let plan = json!({ "level": "metadata", "stripStatisticsTags": true, "streams": [
        { "streamIndex": 1, "keep": true, "language": "spa", "default": true }] });
    let v = run_kind(&f, optimize(&f, plan, &staged)).await;
    assert_eq!(v["state"], "done", "{v}");
    let m = &v["result"]["optimize"];
    assert_eq!(m["level"], "metadata");
    assert_eq!(m["outInfo"]["audio"][0]["language"], "spa");
    assert_eq!(m["outInfo"]["video"][0]["hasStatisticsTags"], true, "{m}");
    assert_eq!(
        f.info["audio"][0]["language"], "eng",
        "the source keeps its tag"
    );
}

#[tokio::test]
async fn a_hevc_reencode_is_verified_and_a_demanding_threshold_keeps_the_video() {
    let Some(f) = fixture(30).await else { return };
    if !encoder(&ffmpeg(), "libx265") {
        eprintln!("libx265 not available: skipped");
        return;
    }
    let staged = f.dir.join(".fliks-opt-6-1.tmp");
    let plan =
        json!({ "level": "reencode", "codec": "hevc", "software": true, "quality": "small" });
    let v = run_kind(&f, optimize(&f, plan, &staged)).await;
    assert_eq!(v["state"], "done", "{v}");
    let m = &v["result"]["optimize"];
    assert_eq!(m["level"], "reencode");
    assert_eq!(m["outInfo"]["video"][0]["codec"], "hevc");
    assert_eq!(m["cpuFallback"], false);
    let staged = f.dir.join(".fliks-opt-6-2.tmp");
    let plan = json!({ "level": "reencode", "codec": "hevc", "software": true,
        "minSavingsPct": 99.99 });
    let v = run_kind(&f, optimize(&f, plan, &staged)).await;
    assert_eq!(v["state"], "done", "{v}");
    let m = &v["result"]["optimize"];
    assert_eq!(
        (m["level"].as_str(), m["copyReason"].as_str()),
        (Some("remux"), Some("low_savings"))
    );
    assert_eq!(m["outInfo"]["video"][0]["codec"], "h264");
}

#[tokio::test]
async fn verify_passes_a_clean_file_and_refuses_a_corrupt_one() {
    let Some(f) = fixture(8).await else { return };
    let v = run_kind(
        &f,
        json!({ "type": "Verify", "payload": { "info": f.info } }),
    )
    .await;
    assert_eq!(v["state"], "done", "{v}");
    assert!(v["result"]["verify"]["bytes"].as_f64().unwrap() > 0.0);
    std::fs::write(&f.src, vec![0x5au8; 4096]).unwrap();
    let v = run_kind(
        &f,
        json!({ "type": "Verify", "payload": { "info": f.info } }),
    )
    .await;
    assert_eq!(v["state"], "failed", "{v}");
    assert!(v["error"].as_str().unwrap().starts_with("corrupt: "), "{v}");
}

#[tokio::test]
async fn a_plan_the_executor_cannot_run_is_refused_at_submission() {
    let Some(f) = fixture(8).await else { return };
    let post = |plan: Value| {
        let body = json!({ "kind": optimize(&f, plan, &f.dir.join("x.tmp")),
            "media": { "mediaFileId": 1, "path": f.src } });
        f.e.control("POST", "/v1/jobs", Some(body))
    };
    assert_eq!(post(json!({ "level": "reencode" })).await.status, 400);
    let drop_audio =
        json!({ "level": "metadata", "streams": [{ "streamIndex": 1, "keep": false }] });
    assert_eq!(post(drop_audio).await.status, 400);
    let relative = json!({ "level": "remux", "extraSubtitles": [{ "path": "ocr.srt" }] });
    assert_eq!(post(relative).await.status, 400);
}

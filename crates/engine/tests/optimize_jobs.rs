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

/// Every MKV output goes through `mkvpropedit` (`MKVPROPEDIT_PATH`).
fn has_mkvpropedit() -> bool {
    let bin = std::env::var("MKVPROPEDIT_PATH").unwrap_or("mkvpropedit".into());
    ok(&bin, &["--version"])
}

struct Fixture {
    e: Engine,
    dir: TempDir,
    src: PathBuf,
    info: Value,
}

/// A 30 s MKV (H.264, one AAC track, one SRT) made by ffmpeg and probed by the engine.
async fn fixture(seconds: u32) -> Option<Fixture> {
    fixture_with(seconds, "mkv", false).await
}

/// `cover`: a JPEG picture embedded as MP4 `covr` or MKV image attachment.
async fn fixture_with(seconds: u32, ext: &str, cover: bool) -> Option<Fixture> {
    let ffprobe = fliks_engine::hw::tool_path("FFPROBE_PATH", "ffprobe");
    if !ok(&ffmpeg(), &["-version"]) || !ok(&ffprobe, &["-version"]) {
        eprintln!("ffmpeg or ffprobe not available: skipped");
        return None;
    }
    if ext == "mkv" && !has_mkvpropedit() {
        eprintln!("mkvpropedit not available: skipped");
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
    let src = dir.join(format!("movie.{ext}"));
    let jpg = dir.join("cover.jpg");
    let d = seconds.to_string();
    let mkv = ext == "mkv";
    let picture = !cover
        || ok(
            &ffmpeg(),
            &[
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:size=64x64:rate=1",
                "-frames:v",
                "1",
                jpg.to_str().unwrap(),
            ],
        );
    let (video, tone) = (
        format!("testsrc2=size=320x180:rate=24:duration={d}"),
        format!("sine=frequency=440:duration={d}"),
    );
    let (srt, jpg) = (srt.to_str().unwrap(), jpg.to_str().unwrap());
    let mut args = vec![
        "-y", "-f", "lavfi", "-i", &video, "-f", "lavfi", "-i", &tone,
    ];
    args.extend(["-i", srt]);
    if cover && !mkv {
        args.extend(["-i", jpg]);
    }
    args.extend(["-map", "0:v", "-map", "1:a", "-map", "2:s"]);
    if cover && !mkv {
        args.extend([
            "-map",
            "3:v",
            "-c:v:1",
            "copy",
            "-disposition:v:1",
            "attached_pic",
        ]);
    }
    args.extend([
        "-c:v:0", "libx264", "-g", "24", "-pix_fmt", "yuv420p", "-c:a", "aac",
    ]);
    args.extend(["-c:s", if mkv { "copy" } else { "mov_text" }]);
    args.extend(["-metadata:s:a:0", "language=eng"]);
    if cover && mkv {
        args.extend(["-attach", jpg, "-metadata:s:t", "mimetype=image/jpeg"]);
    }
    let out = src.to_str().unwrap();
    args.push(out);
    let made = picture && ok(&ffmpeg(), &args);
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

async fn cover_survives(ext: &str, id: u32) {
    let Some(f) = fixture_with(4, ext, true).await else {
        return;
    };
    assert_eq!(f.info["cover"]["codec"], "mjpeg", "{}", f.info);
    let staged = f.dir.join(format!(".fliks-opt-{id}-1.tmp"));
    let plan = json!({ "level": "remux" });
    let v = run_kind(&f, optimize(&f, plan, &staged)).await;
    assert_eq!(v["state"], "done", "{v}");
    let out = &v["result"]["optimize"]["outInfo"];
    assert_eq!(out["video"].as_array().unwrap().len(), 1, "{out}");
    let pictures =
        std::process::Command::new(fliks_engine::hw::tool_path("FFPROBE_PATH", "ffprobe"))
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_name,codec_type",
                "-of",
                "csv=p=0",
            ])
            .arg(&staged)
            .output()
            .unwrap();
    let pictures = String::from_utf8_lossy(&pictures.stdout);
    assert_eq!(pictures.matches("mjpeg").count(), 1, "{pictures}");
}

#[tokio::test]
async fn an_mp4_cover_survives_a_remux() {
    cover_survives("mp4", 20).await;
}

#[tokio::test]
async fn an_mkv_image_attachment_survives_a_remux() {
    cover_survives("mkv", 21).await;
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

async fn probe(f: &Fixture) -> Value {
    let body = json!({ "media": { "mediaFileId": 1, "path": f.src } });
    f.e.control("POST", "/v1/probe", Some(body)).await.json()["info"].clone()
}

#[tokio::test]
async fn a_demoted_default_keeps_forced_and_clear_title_empties_the_title() {
    let Some(mut f) = fixture(8).await else {
        return;
    };
    let tagged = f.dir.join("tagged.mkv");
    let tagged_ok = ok(
        &ffmpeg(),
        &[
            "-y",
            "-i",
            f.src.to_str().unwrap(),
            "-map",
            "0",
            "-c",
            "copy",
            "-disposition:s:0",
            "+forced+default",
            "-metadata",
            "title=Movie.2024.1080p.x264-GRP",
            tagged.to_str().unwrap(),
        ],
    );
    assert!(tagged_ok, "ffmpeg tags the fixture");
    std::fs::rename(&tagged, &f.src).unwrap();
    f.info = probe(&f).await;
    assert_eq!(f.info["title"], "Movie.2024.1080p.x264-GRP", "{}", f.info);
    assert_eq!(f.info["subtitles"][0]["forced"], true);
    let staged = f.dir.join(".fliks-opt-7-1.tmp");
    let plan = json!({ "level": "remux", "clearTitle": true, "streams": [
        { "streamIndex": 2, "keep": true, "default": false }] });
    let v = run_kind(&f, optimize(&f, plan, &staged)).await;
    assert_eq!(v["state"], "done", "{v}");
    let out = &v["result"]["optimize"]["outInfo"];
    assert_eq!(out["subtitles"][0]["forced"], true, "{out}");
    let flags = std::process::Command::new(fliks_engine::hw::tool_path("FFPROBE_PATH", "ffprobe"))
        .args(["-v", "error", "-select_streams", "s:0", "-show_entries"])
        .args(["stream_disposition=default,forced", "-of", "csv=p=0"])
        .arg(&staged)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&flags.stdout).trim(), "0,1");
    assert!(out.get("title").is_none(), "{out}");
}

#[tokio::test]
async fn a_missing_staged_directory_is_not_created() {
    let Some(f) = fixture(8).await else { return };
    let gone = f.dir.join("renamed");
    let staged = gone.join(".fliks-opt-8-1.tmp");
    let v = run_kind(&f, optimize(&f, json!({ "level": "remux" }), &staged)).await;
    assert_eq!(v["state"], "failed", "{v}");
    assert!(v["error"].as_str().unwrap().starts_with("access: "), "{v}");
    assert!(!gone.exists(), "no create_dir_all in the library");
}

#[tokio::test]
async fn sdr_ten_bit_is_encoded_and_av1_is_always_ten_bit() {
    let Some(mut f) = fixture(10).await else {
        return;
    };
    let ten = f.dir.join("ten.mkv");
    let made = encoder(&ffmpeg(), "libx265")
        && ok(
            &ffmpeg(),
            &[
                "-y",
                "-i",
                f.src.to_str().unwrap(),
                "-map",
                "0",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p10le",
                "-c:a",
                "copy",
                "-c:s",
                "copy",
                ten.to_str().unwrap(),
            ],
        );
    if !made {
        eprintln!("10-bit x264 or libx265 not available: skipped");
        return;
    }
    std::fs::rename(&ten, &f.src).unwrap();
    f.info = probe(&f).await;
    assert_eq!(f.info["video"][0]["bitDepth"], 10, "{}", f.info);
    let staged = f.dir.join(".fliks-opt-9-1.tmp");
    let plan =
        json!({ "level": "reencode", "codec": "hevc", "software": true, "quality": "small" });
    let v = run_kind(&f, optimize(&f, plan, &staged)).await;
    assert_eq!(v["state"], "done", "{v}");
    let m = &v["result"]["optimize"];
    assert_eq!(
        m["level"], "reencode",
        "10-bit SDR is no longer copied: {m}"
    );
    assert_eq!(m["outInfo"]["video"][0]["codec"], "hevc");
    assert_eq!(
        m["outInfo"]["video"][0]["pixelFormat"], "yuv420p10le",
        "{m}"
    );
    if encoder(&ffmpeg(), "libsvtav1") {
        let staged = f.dir.join(".fliks-opt-9-2.tmp");
        let plan = json!({ "level": "reencode", "codec": "av1", "software": true });
        let v = run_kind(&f, optimize(&f, plan, &staged)).await;
        assert_eq!(v["state"], "done", "{v}");
        let out = &v["result"]["optimize"]["outInfo"]["video"][0];
        assert_eq!(
            (out["codec"].as_str(), out["pixelFormat"].as_str()),
            (Some("av1"), Some("yuv420p10le"))
        );
    }
}

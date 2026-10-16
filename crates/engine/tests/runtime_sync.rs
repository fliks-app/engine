//! SubtitleSync e2e against real ffmpeg. Build with `ENGINE_E2E=1`.
#![cfg(unix)]
mod runtime_common;
use runtime_common::*;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;

const LATE_SRT: &str = "1\n00:00:10,000 --> 00:00:12,000\nsync me\n";

fn ffmpeg_bin() -> String {
    std::env::var("FFMPEG_PATH").unwrap_or_else(|_| "ffmpeg".into())
}

/// 20 s WAV: silence, then full-scale noise from 5 s to 8 s. Samples are written
/// at those offsets so the burst is not shifted by a filter graph.
fn burst_media(dir: &Path) -> PathBuf {
    const RATE: u32 = 8000;
    let pcm = dir.join("burst.pcm");
    let out = dir.join("burst.wav");
    let mut samples = vec![0i16; (RATE * 20) as usize];
    let mut state = 0x1234_5678u32;
    for s in &mut samples[(RATE * 5) as usize..(RATE * 8) as usize] {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *s = (state >> 16) as i16;
    }
    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    std::fs::write(&pcm, &bytes).unwrap();
    let status = Command::new(ffmpeg_bin())
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "s16le",
            "-ar",
            "8000",
            "-ac",
            "1",
            "-i",
        ])
        .arg(&pcm)
        .args(["-c:a", "pcm_s16le"])
        .arg(&out)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg burst wav failed");
    out
}

fn sync_info(audio_index: u32) -> Value {
    json!({
        "video": [],
        "audio": [{ "streamIndex": audio_index, "codec": "aac", "language": "eng", "channels": 1,
            "sampleRate": 8000 }],
        "subtitles": [],
        "durationSeconds": 20.0,
        "schemaVersion": 1
    })
}

fn parse_srt_ts(s: &str) -> f64 {
    let s = s.trim().replace(',', ".");
    let p: Vec<&str> = s.split(':').collect();
    let f = |x: &str| {
        x.parse::<f64>()
            .unwrap_or_else(|e| panic!("parse {x:?} in timestamp {s:?}: {e}"))
    };
    match p.len() {
        3 => f(p[0]) * 3600.0 + f(p[1]) * 60.0 + f(p[2]),
        2 => f(p[0]) * 60.0 + f(p[1]),
        _ => panic!("bad SRT timestamp {s:?}"),
    }
}

fn first_cue_start_seconds(srt: &str) -> f64 {
    let line = srt
        .lines()
        .find(|l| l.contains("-->"))
        .unwrap_or_else(|| panic!("no SRT timing in {srt:?}"));
    let start = line
        .split_whitespace()
        .next()
        .unwrap_or_else(|| panic!("no start time in {line:?}"));
    parse_srt_ts(start)
}

async fn submit_sync(e: &Engine, file: &Path, info: Value, text: &str) -> String {
    let r = e
        .control(
            "POST",
            "/v1/jobs",
            Some(json!({
                "kind": {
                    "type": "SubtitleSync",
                    "info": info,
                    "subtitle": { "format": "srt", "text": text },
                    "reference": { "type": "auto" },
                },
                "media": { "mediaFileId": 42, "path": file },
            })),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    r.json()["jobId"].as_str().unwrap().into()
}

async fn job_snap(e: &Engine, id: &str) -> Value {
    e.control("GET", &format!("/v1/jobs/{id}"), None)
        .await
        .json()
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn subtitle_sync_alass_moves_late_cue_toward_burst() {
    let dir = temp_dir("sync-e2e");
    let file = burst_media(&dir);
    let e = engine().await;
    e.stub.file(42, &file);
    let id = submit_sync(&e, &file, sync_info(0), LATE_SRT).await;
    until_async("SubtitleSync finished", 120, || async {
        let snap = job_snap(&e, &id).await;
        let s = snap["state"].as_str().unwrap_or("");
        s == "done" || s == "failed"
    })
    .await;
    let snap = job_snap(&e, &id).await;
    assert_eq!(snap["state"], "done", "{}", snap["error"]);
    let out = snap["result"]["subtitle"]["text"].as_str().unwrap();
    let start = first_cue_start_seconds(out);
    assert!(
        (start - 5.0).abs() < 1.5,
        "cue start {start}s expected ~5s (was 10s late); full srt:\n{out}"
    );
}

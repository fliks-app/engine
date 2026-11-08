//! Runs the engine's `Align` on a clip whose word times are known and prints how far they are, as
//! one JSON line. `WHISPER_CLI_PATH=<whisper-cli> align-check <model.bin> <clip.wav> <truth.json> [lang]`
//! The clip is a 16 kHz mono WAV and the truth is `tools/align-check/gen.py`'s JSON.
use fliks_engine::karaoke::whisper_argv;
use fliks_engine_core::api::AlignLine;
use fliks_engine_core::jobs::align::{align, heard_from_json};
use serde_json::{Value, json};
use std::{path::PathBuf, process::Command, time::Instant};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (model, wav, truth) = (PathBuf::from(&a[1]), PathBuf::from(&a[2]), &a[3]);
    let truth: Value = serde_json::from_str(&std::fs::read_to_string(truth).unwrap()).unwrap();
    let lines: Vec<AlignLine> = truth["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| AlignLine {
            text: l["text"].as_str().unwrap().into(),
            start_ms: None,
            end_ms: None,
        })
        .collect();
    let dir = std::env::temp_dir().join(format!("align-check-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let prefix = dir.join("words");
    let argv = whisper_argv(&model, &wav, &prefix, a.get(4).map(String::as_str), &lines);
    let tool = std::env::var("WHISPER_CLI_PATH").unwrap_or("whisper-cli".into());
    let t = Instant::now();
    let out = Command::new(&tool).args(&argv).output().unwrap();
    let wall_ms = t.elapsed().as_millis();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let heard =
        heard_from_json(&std::fs::read_to_string(prefix.with_extension("json")).unwrap()).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    let (aligned, matched) = align(&lines, &heard);

    let mut errs: Vec<i64> = vec![];
    let mut line_errs: Vec<i64> = vec![];
    for (l, t) in aligned.iter().zip(truth["lines"].as_array().unwrap()) {
        let tw = t["words"].as_array().unwrap();
        line_errs.push(l.start_ms as i64 - tw[0]["startMs"].as_i64().unwrap());
        for (w, tw) in l.words.iter().zip(tw) {
            errs.push(w.start_ms as i64 - tw["startMs"].as_i64().unwrap());
        }
    }
    let n = errs.len();
    let abs: Vec<i64> = errs.iter().map(|e| e.abs()).collect();
    let within = |ms: i64| abs.iter().filter(|e| **e <= ms).count() as f64 / n as f64;
    let mut sorted = abs.clone();
    sorted.sort();
    println!(
        "{}",
        json!({
            "wallMs": wall_ms, "words": n, "heard": heard.len(), "matched": matched,
            "missing": n - (matched * n as f64).round() as usize,
            "misplaced": abs.iter().filter(|e| **e > 1000).count(),
            "meanAbsMs": abs.iter().sum::<i64>() as f64 / n as f64,
            "medianAbsMs": sorted[n / 2],
            "within100": within(100), "within250": within(250), "within500": within(500),
            "lineMeanAbsMs": line_errs.iter().map(|e| e.abs()).sum::<i64>() as f64 / line_errs.len() as f64,
            "errs": errs,
        })
    );
}

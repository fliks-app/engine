//! `fixtures/jobs/sprite/*.json`: layout, frame and tile argv, seek offset and missing-frame
//! backfill of the sprite job, recorded once from the host's thumbnail service.
mod common;

use common::{Fixture, blessing, diff};
use fliks_engine_core::api::{CropInfo, MediaInfo};
use fliks_engine_core::jobs::sprite::*;
use serde_json::{Value, json};
use std::path::PathBuf;

fn replay(name: &str, run: impl Fn(&Value) -> Value) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/jobs/sprite")
        .join(format!("{name}.json"));
    let mut fx = Fixture::load(&path);
    let cases = fx.root["cases"].as_array().unwrap().clone();
    let mut failed = vec![];
    for (i, c) in cases.iter().enumerate() {
        let got = run(&c["input"]);
        let mut d = vec![];
        diff("", &got, &c["expected"], &mut d);
        if !d.is_empty() && blessing() {
            fx.bless(Some(i), got);
        } else if !d.is_empty() {
            failed.push(format!("{name}[{i}] {}: {}", c["input"], d.join("; ")));
        }
    }
    fx.save();
    assert!(!cases.is_empty());
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

#[test]
fn layout_matches() {
    replay("layout", |i| {
        let l = layout(i["durationSeconds"].as_f64().unwrap());
        json!({ "interval": l.interval, "count": l.count, "rows": l.rows, "columns": COLUMNS, "thumbWidth": THUMB_WIDTH })
    });
}

#[test]
fn frame_args_match() {
    replay("frame-args", |i| {
        let device = i["device"].as_str().unwrap_or_default().to_string();
        let e = match i["extractor"].as_str().unwrap() {
            "sw" => Extractor::Software,
            "vaapi" => Extractor::Vaapi { device },
            "qsv" => Extractor::Qsv { device },
            "cuda" => Extractor::Cuda,
            _ => Extractor::VideoToolbox,
        };
        let crop: Option<CropInfo> = serde_json::from_value(i["crop"].clone()).ok();
        let a = FrameArgs {
            input_path: i["inputPath"].as_str().unwrap(),
            seek_seconds: i["seekSeconds"].as_f64().unwrap(),
            output_path: i["outputPath"].as_str().unwrap(),
            crop: crop.as_ref(),
            thumb_width: i["thumbWidth"].as_u64().unwrap() as u32,
            hdr: i["hdr"].as_bool().unwrap(),
        };
        match frame_args(&e, &a) {
            Ok(argv) => json!({ "argv": argv }),
            Err(error) => json!({ "error": error }),
        }
    });
}

#[test]
fn tile_args_match() {
    replay("tile-args", |i| {
        let pattern = format!("{}/{FRAME_PATTERN}", i["framesDir"].as_str().unwrap());
        let argv = tile_args(
            &pattern,
            i["spritePath"].as_str().unwrap(),
            i["rows"].as_u64().unwrap() as u32,
        );
        json!({ "cmd": "ffmpeg", "argv": argv, "background": true })
    });
}

#[test]
fn seek_offset_matches() {
    replay("seek-offset", |i| {
        let info: MediaInfo = serde_json::from_value(i["info"].clone()).unwrap();
        json!({ "seekOffsetSeconds": seek_offset(&info) })
    });
}

#[test]
fn backfill_matches() {
    replay("backfill", |i| {
        let present: Vec<bool> = serde_json::from_value(i["present"].clone()).unwrap();
        let mut source: Vec<Option<usize>> = present
            .iter()
            .enumerate()
            .map(|(k, p)| p.then_some(k + 1))
            .collect();
        for (missing, src) in backfill(&present) {
            source[missing] = source[src];
        }
        json!({ "source": source })
    });
}

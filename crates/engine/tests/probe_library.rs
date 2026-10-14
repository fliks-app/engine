//! Differential probe on a real library: replays the host harness NDJSON (`PROBE_LIBRARY` +
//! `PROBE_OUT` of `probe-vectors.spec.ts`) through the engine; a no-op unless `PROBE_LIBRARY_NDJSON`
//! is set (tools: `FFPROBE_PATH`, `FFMPEG_PATH`, `MKVMERGE_PATH`). `PROBE_LIBRARY_EXAMPLES` = examples per group.
#[path = "../../core/tests/common/mod.rs"]
mod cmp;
mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::cpu_host;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Instant;
use tower::ServiceExt;

/// Accepted deviation (docs/README.md): the host writes NaN as `null`, the engine leaves it absent.
fn without_nan_nulls(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.retain(|_, x| !x.is_null());
            m.values_mut().for_each(without_nan_nulls);
        }
        Value::Array(a) => a.iter_mut().for_each(without_nan_nulls),
        _ => {}
    }
}

/// Accepted deviation (docs/README.md): the host's moviehash is wrong when bit 31 of the size is set.
fn moviehash_bit31(host: &Value) -> bool {
    host["bytesize"]
        .as_u64()
        .is_some_and(|s| s & 0x8000_0000 != 0)
}

#[tokio::test]
async fn library_matches_the_host() {
    let Ok(file) = std::env::var("PROBE_LIBRARY_NDJSON") else {
        eprintln!("PROBE_LIBRARY_NDJSON not set: skipped");
        return;
    };
    let examples: usize = std::env::var("PROBE_LIBRARY_EXAMPLES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let app = fliks_engine::app("secret".into(), cpu_host());
    let (mut n, mut host_ms, mut engine_ms) = (0, 0u64, 0u128);
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in std::fs::read_to_string(file).unwrap().lines() {
        let h: Value = serde_json::from_str(line).unwrap();
        let path = h["path"].as_str().unwrap();
        let body = json!({ "media": { "mediaFileId": 1, "path": path }, "crop": true, "mkvTags": true, "moviehash": true });
        let req = Request::builder()
            .method("POST")
            .uri("/v1/probe")
            .header("content-type", "application/json")
            .header("authorization", "Bearer secret")
            .body(Body::from(body.to_string()))
            .unwrap();
        let t = Instant::now();
        let res = app.clone().oneshot(req).await.unwrap();
        engine_ms += t.elapsed().as_millis();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let mut got: Value = serde_json::from_slice(&bytes).unwrap();
        got["info"].as_object_mut().unwrap().remove("schemaVersion");
        let mut want = h["info"].clone();
        without_nan_nulls(&mut want);
        let mut diffs = vec![];
        cmp::diff("info", &got["info"], &want, &mut diffs);
        let gh = got.get("moviehash").cloned().unwrap_or(Value::Null);
        let hh = h["moviehash"].clone();
        if !moviehash_bit31(&hh) && !cmp::same(&gh, &hh) {
            diffs.push(format!("moviehash: got {gh}, expected {hh}"));
        }
        host_ms += h["ms"].as_u64().unwrap_or(0);
        n += 1;
        if let Some(d) = diffs.first() {
            // Group by the first differing path, array indices masked.
            let key = d.split(':').next().unwrap();
            let key = regex_lite_mask(key);
            groups.entry(key).or_default().push(format!("#{n} {d}"));
        }
    }
    eprintln!("probed {n} files; host {host_ms} ms, engine {engine_ms} ms");
    for (k, v) in &groups {
        eprintln!("GROUP {k}: {} files", v.len());
        v.iter().take(examples).for_each(|e| eprintln!("  {e}"));
    }
    assert!(groups.is_empty(), "{} mismatch groups", groups.len());
}

fn regex_lite_mask(s: &str) -> String {
    let mut out = String::new();
    let mut in_idx = false;
    for c in s.chars() {
        match c {
            '[' => {
                in_idx = true;
                out.push_str("[]");
            }
            ']' => in_idx = false,
            _ if !in_idx => out.push(c),
            _ => {}
        }
    }
    out
}

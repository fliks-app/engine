//! Golden size estimates (`fixtures/estimate-sizes/*.json`, from the host's real controller method).
mod common;

use common::diff;
use fliks_engine_core::api::{EstimateSizesRequest, MediaInfo};
use fliks_engine_core::decide::estimate_sizes;
use serde_json::Value;
use std::path::PathBuf;

#[test]
fn estimate_sizes_reproduces_expected() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/estimate-sizes");
    let (mut n, mut failed) = (0, vec![]);
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        let f: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let _: MediaInfo = serde_json::from_value(f["input"]["info"].clone()).unwrap();
        let req: EstimateSizesRequest = serde_json::from_value(f["input"].clone())
            .unwrap_or_else(|e| panic!("{name}: input: {e}"));
        let got = serde_json::to_value(estimate_sizes(&req)).unwrap();
        let mut d = vec![];
        diff("", &got, &f["expected"], &mut d);
        if !d.is_empty() {
            failed.push(format!("{name}\n  {}", d.join("\n  ")));
        }
        n += 1;
    }
    assert!(n > 0, "fixtures/estimate-sizes is empty");
    for f in failed.iter().take(20) {
        eprintln!("FAIL {f}");
    }
    assert!(
        failed.is_empty(),
        "estimate-sizes: {} of {n} fail",
        failed.len()
    );
}

//! Golden item decisions (`fixtures/item/*.json`, `BLESS=1` rewrites `expected`).
mod common;

use common::{Fixture, blessing, diff};
use fliks_engine_core::api::{DecideRequest, Decision, HostCaps, StreamingConfig};
use fliks_engine_core::decide::decide;
use std::path::PathBuf;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/item")
}

#[test]
fn decide_reproduces_expected() {
    let mut names: Vec<_> = std::fs::read_dir(dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    names.sort();
    assert!(!names.is_empty(), "fixtures/item is empty");
    let mut failed = vec![];
    for path in &names {
        let mut fx = Fixture::load(path);
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let f = &fx.root;
        let req: DecideRequest = serde_json::from_value(f["input"].clone())
            .unwrap_or_else(|e| panic!("{name}: input: {e}"));
        let caps: HostCaps = serde_json::from_value(f["hostCaps"].clone()).unwrap();
        let config: StreamingConfig = serde_json::from_value(f["config"].clone()).unwrap();
        let got = serde_json::to_value::<Decision>(decide(&req, &caps, &config)).unwrap();
        let mut d = vec![];
        diff("", &got, &f["expected"], &mut d);
        if d.is_empty() {
            continue;
        }
        if blessing() {
            fx.bless(None, got);
            fx.save();
        } else {
            failed.push(format!("{name}\n  {}", d.join("\n  ")));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

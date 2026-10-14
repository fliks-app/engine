//! Golden decision vectors (`fixtures/decide/*.json`, `BLESS=1` rewrites `expected`): every
//! input round-trips the wire types and `decide()` reproduces `expected`.
mod common;

use common::{Fixture, blessing, diff, same};
use fliks_engine_core::api::{
    DecideRequest, Decision, DeviceProfile, HostCaps, MediaInfo, SourceScan, StreamingConfig,
};
use fliks_engine_core::decide::decide;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/decide")
}

fn fixtures() -> Vec<(String, Value)> {
    let mut out: Vec<_> = std::fs::read_dir(dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .map(|p| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            (
                name,
                serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap(),
            )
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn roundtrip<T: Serialize + for<'a> Deserialize<'a>>(name: &str, what: &str, v: &Value) {
    let t: T = serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("{name}: {what}: {e}"));
    let back = serde_json::to_value(t).unwrap();
    assert!(
        same(&back, v),
        "{name}: {what} changed on round trip\n in: {v}\nout: {back}"
    );
}

#[test]
fn fixture_inputs_roundtrip_wire_types() {
    let all = fixtures();
    assert!(!all.is_empty(), "fixtures/decide is empty");
    for (name, f) in &all {
        let input = &f["input"];
        roundtrip::<MediaInfo>(name, "input.info", &input["info"]);
        roundtrip::<DeviceProfile>(name, "input.device", &input["device"]);
        if !input["scan"].is_null() {
            roundtrip::<SourceScan>(name, "input.scan", &input["scan"]);
        }
        serde_json::from_value::<StreamingConfig>(f["config"].clone())
            .unwrap_or_else(|e| panic!("{name}: config: {e}"));
    }
}

/// Vectors that replicate a host bug the port must not copy, by file name; see docs/README.md
/// "Deviations".
const SKIPPED: &[&str] = &[];

#[test]
fn decide_reproduces_expected() {
    let (mut pass, mut skipped, mut failed) = (0, 0, vec![]);
    for (name, f) in &fixtures() {
        if SKIPPED.contains(&name.as_str()) {
            skipped += 1;
            continue;
        }
        let input = &f["input"];
        let req: DecideRequest =
            serde_json::from_value(input.clone()).unwrap_or_else(|e| panic!("{name}: input: {e}"));
        let caps: HostCaps = serde_json::from_value(f["hostCaps"].clone())
            .unwrap_or_else(|e| panic!("{name}: hostCaps: {e}"));
        let config: StreamingConfig = serde_json::from_value(f["config"].clone())
            .unwrap_or_else(|e| panic!("{name}: config: {e}"));
        let got = serde_json::to_value::<Decision>(decide(&req, &caps, &config)).unwrap();
        let mut d = vec![];
        diff("", &got, &f["expected"], &mut d);
        if d.is_empty() {
            pass += 1;
        } else if blessing() {
            let mut fx = Fixture::load(&dir().join(name));
            fx.bless(None, got);
            fx.save();
        } else {
            failed.push(format!("{name}\n  {}", d.join("\n  ")));
        }
    }
    let total = pass + skipped + failed.len();
    for f in failed.iter().take(40) {
        eprintln!("FAIL {f}");
    }
    assert!(
        failed.is_empty(),
        "decide: {pass}/{total} pass, {} fail, {skipped} skipped",
        failed.len()
    );
    eprintln!("decide: {pass}/{total} pass, {skipped} skipped");
}

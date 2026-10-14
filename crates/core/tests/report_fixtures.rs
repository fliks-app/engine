//! `fixtures/report/open-report.json`: what the host's playback-info derives beside the decision
//! (clock offset, tone-map report, QoE pipeline) for every decide fixture's input.
mod common;

use common::diff;
use fliks_engine_core::api::{BurnInKind, DecideRequest, HostCaps, StreamingConfig};
use fliks_engine_core::decide::decide;
use fliks_engine_core::session::open_report;
use serde_json::{Value, json};
use std::path::PathBuf;

#[test]
fn open_report_reproduces_the_host() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    let all: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("report/open-report.json")).unwrap(),
    )
    .unwrap();
    let (mut n, mut failed) = (0, vec![]);
    for c in all["cases"].as_array().unwrap() {
        let name = c["decide"].as_str().unwrap();
        let f: Value = serde_json::from_str(
            &std::fs::read_to_string(root.join(format!("decide/{name}.json"))).unwrap(),
        )
        .unwrap();
        let req: DecideRequest = serde_json::from_value(f["input"].clone()).unwrap();
        let caps: HostCaps = serde_json::from_value(f["hostCaps"].clone()).unwrap();
        let mut config = f["config"].clone();
        config["tonemapCurve"] = c["tonemapCurve"].clone();
        let config: StreamingConfig = serde_json::from_value(config).unwrap();
        let sel = &req.selection;
        let burn_in = sel.burn_in_subtitle_id.map(|_| {
            if sel.burn_in_is_text == Some(true) {
                BurnInKind::Text
            } else {
                BurnInKind::Image
            }
        });
        let d = decide(&req, &caps, &config);
        let r = open_report(&req, &d, &config, &caps, burn_in);
        let got = json!({
            "playbackClockOffsetS": r.playback_clock_offset_s,
            "tonemapAlgo": r.tonemap_algo,
            "tonemapCurve": r.tonemap_curve,
            "qoePipeline": r.qoe_pipeline,
        });
        let mut dd = vec![];
        diff("", &got, &c["expected"], &mut dd);
        if !dd.is_empty() {
            failed.push(format!("{name}: {}", dd.join("; ")));
        }
        n += 1;
    }
    for f in failed.iter().take(10) {
        eprintln!("FAIL {f}");
    }
    assert!(n > 400, "{n} cases");
    assert!(failed.is_empty(), "{} of {n} fail", failed.len());
}

//! Golden intro/outro vectors (`fixtures/intro/*.json`, `BLESS=1` rewrites `expected`).
mod common;

use common::{Fixture, blessing, diff, same};
use fliks_engine_core::api::{IntroDetectKind, IntroDetectRequest};
use fliks_engine_core::intro::detect;
use serde_json::Value;
use std::path::PathBuf;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/intro")
}

fn fixtures() -> Vec<(String, Value)> {
    let mut out: Vec<_> = std::fs::read_dir(dir())
        .unwrap_or_else(|_| panic!("missing fixtures/intro"))
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

#[test]
fn intro_request_roundtrips() {
    for (name, f) in fixtures() {
        let v = &f["input"];
        let back = serde_json::to_value(
            serde_json::from_value::<IntroDetectRequest>(v.clone())
                .unwrap_or_else(|e| panic!("{name}: {e}")),
        )
        .unwrap();
        assert!(same(&back, v), "{name}: input round trip\n{v}\n{back}");
    }
}

#[test]
fn validate_rejects_bad_requests() {
    let base = IntroDetectRequest {
        kind: IntroDetectKind::Intro,
        lookup_seconds: 600.0,
        min_segment_seconds: None,
        episodes: vec![fliks_engine_core::api::IntroEpisode {
            media_file_id: 1,
            duration_seconds: 3600.0,
            chapters: None,
            intro_fingerprint: Some(fliks_engine_core::api::Fingerprint {
                hashes: vec![1],
                duration_seconds: 600.0,
                base_offset_sec: 0.0,
            }),
            outro_fingerprint: None,
        }],
    };
    let mut bad = base.clone();
    bad.lookup_seconds = 0.0;
    assert!(detect(bad).is_err());

    let mut outro = base.clone();
    outro.kind = IntroDetectKind::Outro;
    outro.episodes[0].outro_fingerprint = outro.episodes[0].intro_fingerprint.take();
    outro.episodes[0].duration_seconds = 0.0;
    assert!(detect(outro).is_err());

    let mut missing = base;
    missing.episodes[0].intro_fingerprint = None;
    assert!(detect(missing).is_err());
}

#[test]
fn intro_reproduces_expected() {
    let (mut pass, mut failed) = (0, vec![]);
    for (name, f) in fixtures() {
        let req: IntroDetectRequest =
            serde_json::from_value(f["input"].clone()).unwrap_or_else(|e| panic!("{name}: {e}"));
        let got = serde_json::to_value(detect(req).expect("detect")).unwrap();
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
    for f in failed.iter().take(20) {
        eprintln!("FAIL {f}");
    }
    assert!(
        failed.is_empty(),
        "intro: {pass}/{} pass, {} fail",
        pass + failed.len(),
        failed.len()
    );
}

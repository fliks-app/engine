//! Every `fixtures/session/*.json` (a vector, or a matrix of cases) through `core::session`; the
//! result must equal `expected` (`BLESS=1` rewrites it), see `docs/spec/session.md`.
mod common;
mod session_common;

use common::{Fixture, blessing, diff};
use serde_json::Value;
use std::path::PathBuf;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/session")
}

/// The mismatch text, or `None` when the case passes; `Some(got)` is what a bless stores.
fn check(file: &str, kind: &str, name: &str, case: &Value) -> Result<(), (String, Value)> {
    let got = session_common::run(kind, &case["input"]);
    let mut d = vec![];
    diff("", &got, &case["expected"], &mut d);
    if d.is_empty() {
        return Ok(());
    }
    Err((format!("{file}: {name}: {}", d.join("; ")), got))
}

#[test]
fn session_vectors_match_the_host() {
    let mut files: Vec<_> = std::fs::read_dir(dir())
        .expect("fixtures/session")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no fixtures/session/*.json");
    let (mut bad, mut cases) = (vec![], 0);
    for f in &files {
        let mut fx = Fixture::load(f);
        let v = fx.root.clone();
        let file = f.file_name().unwrap().to_string_lossy().to_string();
        let kind = v["kind"].as_str().unwrap();
        match v["cases"].as_array() {
            Some(list) => {
                for (i, c) in list.iter().enumerate() {
                    cases += 1;
                    if let Err((m, got)) = check(&file, kind, c["name"].as_str().unwrap(), c) {
                        if blessing() {
                            fx.bless(Some(i), got);
                        } else {
                            bad.push(m);
                        }
                    }
                }
            }
            None => {
                cases += 1;
                if let Err((m, got)) = check(&file, kind, v["name"].as_str().unwrap(), &v) {
                    if blessing() {
                        fx.bless(None, got);
                    } else {
                        bad.push(m);
                    }
                }
            }
        }
        fx.save();
    }
    assert!(
        bad.is_empty(),
        "{} of {cases} cases differ:\n{}",
        bad.len(),
        bad.iter().take(20).cloned().collect::<Vec<_>>().join("\n")
    );
}

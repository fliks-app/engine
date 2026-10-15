//! Every `fixtures/jobs/sync/*.json` through `core::jobs::sync`.
mod common;

use common::{Fixture, blessing, diff};
use fliks_engine_core::jobs::sync;
use serde_json::{Value, json};

fn run(kind: &str, v: &Value) -> Value {
    match kind {
        "ffsubsync" => {
            let i = &v["input"];
            json!({
                "argv": sync::ffsubsync_argv(
                    i["reference"].as_str().unwrap(),
                    i["input"].as_str().unwrap(),
                    i["output"].as_str().unwrap(),
                    i["stream"].as_u64().map(|n| n as u32),
                    i["maxOffsetSeconds"].as_f64(),
                    i["noFixFramerate"].as_bool().unwrap_or(false),
                    i["gss"].as_bool().unwrap_or(false),
                )
            })
        }
        k => panic!("unknown kind {k}"),
    }
}

fn replay() -> (usize, Vec<String>) {
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/jobs/sync");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no fixtures in {dir:?}");
    let (mut n, mut bad) = (0, vec![]);
    for f in &files {
        let mut fx = Fixture::load(f);
        let root = fx.root.clone();
        let file = f.file_name().unwrap().to_string_lossy().to_string();
        let kind = root["kind"].as_str().unwrap().to_string();
        let mut check = |idx: Option<usize>, case: &Value, name: &str| {
            n += 1;
            let got = run(&kind, case);
            let mut d = vec![];
            diff("", &got, &case["expected"], &mut d);
            if d.is_empty() {
                return;
            }
            if blessing() {
                fx.bless(idx, got);
            } else {
                bad.push(format!("{file}: {name}: {}", d.join("; ")));
            }
        };
        match root["cases"].as_array() {
            Some(list) => list
                .iter()
                .enumerate()
                .for_each(|(i, c)| check(Some(i), c, c["name"].as_str().unwrap())),
            None => check(None, &root, root["name"].as_str().unwrap()),
        }
        fx.save();
    }
    (n, bad)
}

#[test]
fn sync_matches_fixtures() {
    let (n, bad) = replay();
    assert!(
        bad.is_empty(),
        "{n} cases, {} mismatches:\n{}",
        bad.len(),
        bad.join("\n")
    );
    assert!(n > 0);
}

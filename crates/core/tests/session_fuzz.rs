//! Differential fuzz: replays the host's random vectors (`$SESSION_FUZZ_DIR/*.ndjson`, written by
//! `backend/test/engine-vectors` with `SESSION_FUZZ_OUT`) through `core::session`. No-op when the
//! variable is unset.
mod common;
mod session_common;

use common::diff;
use serde_json::Value;
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

#[test]
fn session_matches_host_fuzz() {
    let Some(dir) = std::env::var_os("SESSION_FUZZ_DIR") else {
        return;
    };
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "ndjson"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no *.ndjson in {dir:?}");
    let show: usize = std::env::var("SESSION_FUZZ_EXAMPLES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    let mut totals: BTreeMap<String, usize> = BTreeMap::new();
    // kind -> (count, examples)
    let mut groups: BTreeMap<String, (usize, Vec<String>)> = BTreeMap::new();
    for f in &files {
        for line in std::fs::read_to_string(f).unwrap().lines() {
            let v: Value = serde_json::from_str(line).unwrap();
            let kind = v["kind"].as_str().unwrap().to_string();
            *totals.entry(kind.clone()).or_default() += 1;
            let got = catch_unwind(AssertUnwindSafe(|| session_common::run(&kind, &v["input"])));
            let mut d = vec![];
            match got {
                Ok(got) => diff("", &got, &v["expected"], &mut d),
                Err(_) => d.push("<panic>".into()),
            }
            if !d.is_empty() {
                let g = groups
                    .entry(format!("{kind} {}", d[0].split(':').next().unwrap()))
                    .or_default();
                g.0 += 1;
                if g.1.len() < show {
                    g.1.push(format!(
                        "seed {} i {}: {}\n    input {}",
                        v["seed"],
                        v["i"],
                        d.join("; "),
                        v["input"]
                    ));
                }
            }
        }
    }
    let total: usize = totals.values().sum();
    eprintln!("session fuzz: {total} inputs {totals:?}");
    for (g, (n, ex)) in &groups {
        eprintln!("MISMATCH {g}: {n}");
        for e in ex {
            eprintln!("  {e}");
        }
    }
    assert!(groups.is_empty(), "{} mismatching classes", groups.len());
}

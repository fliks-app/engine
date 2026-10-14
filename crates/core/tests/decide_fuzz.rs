//! Differential fuzz: replays the host's random vectors (`$DECIDE_FUZZ_DIR/*.ndjson`, written by
//! `backend/test/engine-vectors` with `FUZZ_OUT`) through `decide()`. No-op when the variable is unset.
mod common;

use common::diff;
use fliks_engine_core::api::{DecideRequest, Decision, HostCaps, StreamingConfig};
use fliks_engine_core::decide::decide;
use serde_json::Value;
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

/// Named classes of TS behaviour the port must not copy (docs/README.md "Deviations"); a line is
/// excluded when its predicate holds on the whole vector.
type Predicate = fn(&Value) -> bool;
const EXCLUDED: &[(&str, Predicate)] = &[];

/// Path with array indices and map keys collapsed, so one class groups every occurrence.
fn class(path: &str) -> String {
    let mut out = String::new();
    let mut skip = false;
    for c in path.chars() {
        match c {
            '[' => {
                skip = true;
                out.push_str("[]");
            }
            ']' => skip = false,
            _ if !skip => out.push(c),
            _ => {}
        }
    }
    out
}

fn run(v: &Value) -> Result<Value, String> {
    let req: DecideRequest = serde_json::from_value(v["input"].clone())
        .map_err(|e| format!("<deserialize input>: {e}"))?;
    let caps: HostCaps =
        serde_json::from_value(v["hostCaps"].clone()).map_err(|e| format!("<hostCaps>: {e}"))?;
    let cfg: StreamingConfig =
        serde_json::from_value(v["config"].clone()).map_err(|e| format!("<config>: {e}"))?;
    catch_unwind(AssertUnwindSafe(|| {
        serde_json::to_value::<Decision>(decide(&req, &caps, &cfg)).unwrap()
    }))
    .map_err(|_| "<panic>".to_string())
}

#[test]
fn decide_matches_host_fuzz() {
    let Some(dir) = std::env::var_os("DECIDE_FUZZ_DIR") else {
        return;
    };
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "ndjson"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no *.ndjson in {dir:?}");

    let show: usize = std::env::var("DECIDE_FUZZ_EXAMPLES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let (mut total, mut excluded) = (0usize, BTreeMap::<&str, usize>::new());
    // class -> (count, examples: "seed i" + diff lines)
    let mut groups: BTreeMap<String, (usize, Vec<String>)> = BTreeMap::new();
    for f in &files {
        for line in std::fs::read_to_string(f).unwrap().lines() {
            let v: Value = serde_json::from_str(line).unwrap();
            if let Some((name, _)) = EXCLUDED.iter().find(|(_, p)| p(&v)) {
                *excluded.entry(name).or_default() += 1;
                continue;
            }
            total += 1;
            let mut d = vec![];
            match run(&v) {
                Ok(got) => diff("", &got, &v["expected"], &mut d),
                Err(e) => d.push(e),
            }
            if d.is_empty() {
                continue;
            }
            let key = class(d[0].split(": got").next().unwrap_or(&d[0]));
            let g = groups.entry(key).or_default();
            g.0 += 1;
            if g.1.len() < show {
                g.1.push(format!(
                    "seed {} i {}\n    {}",
                    v["seed"],
                    v["i"],
                    d.iter().take(8).cloned().collect::<Vec<_>>().join("\n    ")
                ));
            }
        }
    }
    let failed: usize = groups.values().map(|g| g.0).sum();
    let mut classes: Vec<_> = groups.iter().collect();
    classes.sort_by_key(|(_, g)| std::cmp::Reverse(g.0));
    for (key, (n, ex)) in classes {
        eprintln!("CLASS {n:>5}  {key}");
        for e in ex {
            eprintln!("  {e}");
        }
    }
    eprintln!(
        "decide fuzz: {}/{total} pass, excluded {excluded:?}",
        total - failed
    );
    assert_eq!(failed, 0, "decide fuzz: {failed}/{total} mismatches");
}

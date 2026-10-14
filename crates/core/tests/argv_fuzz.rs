//! Differential fuzz over `$ARGV_FUZZ_DIR/*.ndjson` (host `ARGV_FUZZ_OUT`); no-op when unset.
mod argv_common;

use argv_common::{first_diff, run};
use serde_json::Value;
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

#[test]
fn argv_matches_host_fuzz() {
    let Some(dir) = std::env::var_os("ARGV_FUZZ_DIR") else {
        return;
    };
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "ndjson"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no *.ndjson in {dir:?}");
    let examples: usize = std::env::var("ARGV_FUZZ_EXAMPLES")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(2);
    let (mut total, mut groups) = (0, BTreeMap::<String, Vec<String>>::new());
    for f in &files {
        for line in std::fs::read_to_string(f).unwrap().lines() {
            let v: Value = serde_json::from_str(line).unwrap();
            total += 1;
            let id = format!("seed {} #{}", v["seed"], v["i"]);
            let got = catch_unwind(AssertUnwindSafe(|| run(&v)))
                .unwrap_or_else(|_| Err("<panic>".into()));
            let (class, detail) = match got {
                Ok(g) if g == v["expected"] => continue,
                Ok(g) => {
                    let d = first_diff(&g, &v["expected"]);
                    let class = d.split(": got").next().unwrap_or("").to_string();
                    (
                        format!(
                            "{} {}",
                            v["kind"],
                            class.split("] ").nth(1).unwrap_or(&class)
                        ),
                        d,
                    )
                }
                Err(e) => (format!("{} {}", v["kind"], e.split(':').next().unwrap()), e),
            };
            groups
                .entry(class)
                .or_default()
                .push(format!("{id}: {detail}"));
        }
    }
    let mismatches: usize = groups.values().map(Vec::len).sum();
    let mut report = format!("{mismatches} of {total} fuzz vectors differ\n");
    for (class, v) in &groups {
        report.push_str(&format!("[{}] {class}\n", v.len()));
        for e in v.iter().take(examples) {
            report.push_str(&format!("    {}\n", &e[..e.len().min(400)]));
        }
    }
    eprintln!("{report}");
    assert_eq!(mismatches, 0, "{report}");
}

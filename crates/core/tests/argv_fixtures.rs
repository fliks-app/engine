//! Every `fixtures/argv/*.json` must give the host's argv element by element, or its error.
mod argv_common;

use argv_common::{first_diff, run};
use serde_json::Value;

#[test]
fn argv_matches_host() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/argv");
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no fixtures in {dir}");
    let mut failed = vec![];
    for f in &files {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(f).unwrap()).unwrap();
        let name = f.file_name().unwrap().to_string_lossy().into_owned();
        match run(&v) {
            Ok(got) if got == v["expected"] => {}
            Ok(got) => failed.push(format!("{name}: {}", first_diff(&got, &v["expected"]))),
            Err(e) => failed.push(format!("{name}: {e}")),
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {} argv fixtures differ:\n{}",
        failed.len(),
        files.len(),
        failed.join("\n")
    );
}

//! `fixtures/jobs/merge/merge.json`: the plan, argv, chapters, ffmetadata and free-form tags of a
//! `Merge`, from synthetic parts.
mod common;

use common::{Fixture, blessing, diff};
use fliks_engine_core::api::{MediaTags, MergePart};
use fliks_engine_core::jobs::merge::*;
use serde_json::{Value, json};
use std::path::PathBuf;

fn run(i: &Value) -> Value {
    let parts: Vec<MergePart> = serde_json::from_value(i["parts"].clone()).unwrap();
    let over: Option<MediaTags> = serde_json::from_value(i["tags"].clone()).ok();
    let paths: Vec<String> = parts.iter().map(|p| p.path.clone()).collect();
    if let Err(e) = validate(&parts, i["output"].as_str().unwrap()) {
        return json!({ "error": e });
    }
    let files = MergeFiles {
        list: "/s/list.txt".into(),
        meta: "/s/meta.txt".into(),
        graph: "/s/graph.txt".into(),
    };
    let enc = encoding(&parts);
    let tags = merged_tags(&parts, over.as_ref());
    let chapters = chapters(&parts);
    json!({
        "encoding": enc,
        "starts": part_starts(&parts),
        "chapters": chapters,
        "list": concat_list(&paths),
        "ffmetadata": ffmetadata(&tags, &chapters),
        "freeform": freeform_tags(&tags),
        "graph": match enc {
            Encoding::Aac { sample_rate, channels, .. } => json!(filter_graph(parts.len(), sample_rate, channels)),
            Encoding::Copy => Value::Null,
        },
        "argv": merge_args(&parts, &paths, &enc, &files, "/books/.Synthetic Saga.m4b.merge-ab12.tmp"),
    })
}

#[test]
fn merge_plans_match() {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/jobs/merge/merge.json");
    let mut fx = Fixture::load(&path);
    let cases = fx.root["cases"].as_array().unwrap().clone();
    let mut failed = vec![];
    for (i, c) in cases.iter().enumerate() {
        let got = run(&c["input"]);
        let mut d = vec![];
        diff("", &got, &c["expected"], &mut d);
        if !d.is_empty() && blessing() {
            fx.bless(Some(i), got);
        } else if !d.is_empty() {
            failed.push(format!("[{i}] {}: {}", c["input"]["name"], d.join("; ")));
        }
    }
    fx.save();
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

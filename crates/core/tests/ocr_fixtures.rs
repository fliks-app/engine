//! Every `fixtures/jobs/ocr/*.json` through `core::jobs::ocr`.
mod common;

use common::{Fixture, blessing, diff};
use fliks_engine_core::api::MediaInfo;
use fliks_engine_core::jobs::ocr;
use serde_json::{Value, json};

fn run(kind: &str, v: &Value) -> Value {
    match kind {
        "pgs-argv" => {
            let i = &v["input"];
            let base = i["base"].as_str().unwrap();
            let stream = i["streamIndex"].as_u64().unwrap() as u32;
            let path = i["path"].as_str().unwrap();
            let ietf = ocr::resolved_language(
                i["language"].as_str(),
                i["trackLanguage"].as_str().unwrap_or("und"),
            );
            let sup = ocr::pgs_sup_path(base, &ietf);
            json!({
                "ietf": ietf,
                "extract": ocr::pgs_extract_argv(path, stream, &sup),
                "pgsrip": ocr::pgsrip_argv(&ietf, &sup),
            })
        }
        "vobsub-argv" => {
            let i = &v["input"];
            let base = i["base"].as_str().unwrap();
            let stream = i["streamIndex"].as_u64().unwrap() as u32;
            let path = i["path"].as_str().unwrap();
            let ietf = ocr::resolved_language(
                i["language"].as_str(),
                i["trackLanguage"].as_str().unwrap_or("und"),
            );
            let pack = ocr::tesseract_pack(&ietf);
            json!({
                "ietf": ietf,
                "pack": pack,
                "extract": ocr::mkvextract_argv(path, stream, &format!("{base}.idx")),
                "ocr": ocr::subtile_ocr_argv(pack, base),
            })
        }
        "language" => {
            let i = &v["input"];
            json!({ "ietf": ocr::resolved_language(i["request"].as_str(), i["track"].as_str().unwrap()) })
        }
        "tess-pack" => {
            let ietf = v["input"]["ietf"].as_str().unwrap();
            json!({ "pack": ocr::tesseract_pack(ietf) })
        }
        "vobsub-mkv" => match ocr::require_matroska_vobsub(v["input"]["path"].as_str().unwrap()) {
            Ok(()) => json!({ "ok": true }),
            Err(e) => json!({ "error": e }),
        },
        "validate" => {
            let i = &v["input"];
            let info: MediaInfo = serde_json::from_value(i["info"].clone()).unwrap();
            let stream = i["streamIndex"].as_u64().unwrap() as u32;
            match ocr::validate_ocr(&info, stream, i["language"].as_str()) {
                Ok(t) => json!({ "codec": t.codec }),
                Err(e) => json!({ "error": e }),
            }
        }
        "pgs-missing-srt" => {
            let i = &v["input"];
            json!({ "error": ocr::pgs_missing_srt_error(
                i["ietf"].as_str().unwrap(),
                i["stderr"].as_str().unwrap_or(""),
                i["stdout"].as_str().unwrap_or(""),
            ) })
        }
        k => panic!("unknown kind {k}"),
    }
}

fn replay() -> (usize, Vec<String>) {
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/jobs/ocr");
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
fn ocr_matches_host() {
    let (n, bad) = replay();
    assert!(n >= 10, "only {n} ocr cases");
    assert!(
        bad.is_empty(),
        "{} of {n} ocr cases differ:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

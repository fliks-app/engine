//! Every `fixtures/jobs/{scan,fingerprint,subtitles}/*.json` through `core::jobs`: the argv and the
//! parsed result must equal the host's, see `docs/spec/jobs-pure.md`.
mod common;

use common::{Fixture, blessing, diff};
use fliks_engine_core::api::MediaInfo;
use fliks_engine_core::jobs::{fingerprint as fp, scan, subtitles as sub};
use serde_json::{Map, Value, json};
use std::path::PathBuf;

fn unb64(s: &str) -> Vec<u8> {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let (mut out, mut acc, mut bits) = (vec![], 0u32, 0);
    for c in s.bytes().filter(|c| *c != b'=') {
        acc = acc << 6 | T.iter().position(|x| *x == c).expect("base64") as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

fn strs(v: &Value) -> Vec<&str> {
    v.as_array()
        .map_or(vec![], |a| a.iter().filter_map(Value::as_str).collect())
}

fn scan_run(v: &Value) -> Value {
    let input = &v["input"];
    let info: MediaInfo = serde_json::from_value(input["info"].clone()).unwrap();
    let file = input["filePath"].as_str().unwrap();
    let plan = match scan::ScanPlan::new(&info, file) {
        Ok(p) => p,
        Err(e) => return json!({ "error": e }),
    };
    let tools = &v["tools"];
    let mut argv = Map::new();
    let lead = scan::known_lead(&plan.video).unwrap_or_else(|| {
        argv.insert(
            "reorder".into(),
            json!(scan::reorder_args(&plan.video, file)),
        );
        strs(&tools["stream"])
            .first()
            .map_or(0.0, |l| scan::lead_from_probe(l))
    });
    argv.insert("packets".into(), json!(scan::packet_args(&plan, file)));
    let mut reader = scan::PacketReader::new(lead, plan.mpeg_ts);
    for line in strs(&tools["packets"]) {
        if !reader.feed_line(line) {
            break;
        }
    }
    let mut changes = Default::default();
    if plan.piped() {
        // The host's ffmpeg argv of the ADTS read (no longer ours: the walkers take the bytes of a TS demux).
        argv.insert("adts".into(), v["expected"]["argv"]["adts"].clone());
        let walkers: Vec<_> = (0..plan.aac.len())
            .map(|i| {
                let mut w = scan::AdtsConfigWalker::default();
                strs(&tools["adts"][i])
                    .iter()
                    .for_each(|c| w.push(&unb64(c)));
                (tools["adtsExit"] == 0).then_some(w)
            })
            .collect();
        changes = scan::audio_config_changes(&plan.aac, &walkers);
    }
    match reader.finish(changes) {
        Ok(s) => json!({ "argv": argv, "scan": s }),
        Err(e) => json!({ "error": e }),
    }
}

fn adts_run(v: &Value) -> Value {
    let mut w = scan::AdtsConfigWalker::default();
    strs(&v["input"]["chunks"])
        .iter()
        .for_each(|c| w.push(&unb64(c)));
    json!({ "changed": w.changed })
}

fn fingerprint_run(v: &Value) -> Value {
    let (i, t) = (&v["input"], &v["tools"]);
    let path = i["path"].as_str().unwrap();
    let stdout = t["fpcalcStdout"].as_str().unwrap_or("");
    let mut argv = Map::new();
    let res = if i["kind"] == "head" {
        let max = i["maxSeconds"].as_f64().unwrap();
        argv.insert("fpcalc".into(), json!(fp::head_args(path, max)));
        fp::parse_fpcalc(stdout, max, 0.0)
    } else {
        let (start, len) = (
            i["startSeconds"].as_f64().unwrap(),
            i["lengthSeconds"].as_f64().unwrap(),
        );
        argv.insert(
            "ffmpeg".into(),
            json!(fp::window_ffmpeg_args(path, start, len)),
        );
        argv.insert("fpcalc".into(), json!(fp::window_fpcalc_args(len)));
        match t["fpcalcExit"].as_i64().unwrap() {
            0 => fp::parse_fpcalc(stdout, len, start),
            c => Err(fp::window_failure(
                c as i32,
                t["fpcalcStderr"].as_str().unwrap(),
                t["ffmpegStderr"].as_str().unwrap(),
            )),
        }
    };
    match res {
        Ok(f) => json!({ "argv": argv, "fingerprint": f }),
        Err(e) => json!({ "argv": argv, "error": e }),
    }
}

fn extract_run(v: &Value) -> Value {
    let i = &v["input"];
    let outs: Vec<(u32, &str)> = i["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| {
            (
                o["streamIndex"].as_u64().unwrap() as u32,
                o["raw"].as_str().unwrap(),
            )
        })
        .collect();
    let vtt: Vec<String> = strs(&v["tools"]["ass"])
        .iter()
        .map(|a| sub::ass_to_vtt(a))
        .collect();
    json!({ "argv": sub::extract_args(i["path"].as_str().unwrap(), &outs), "vtt": vtt })
}

fn burn_in_argv_run(v: &Value) -> Value {
    let i = &v["input"];
    let (id, idx) = (
        i["mediaFileId"].as_u64().unwrap() as u32,
        i["streamIndex"].as_u64().unwrap() as u32,
    );
    let out = format!("/tmp/fliks-burn-in/{}", sub::burn_in_file_name(id, idx));
    json!({ "argv": sub::burn_in_extract_args(i["videoPath"].as_str().unwrap(), idx, &out) })
}

fn run(kind: &str, v: &Value) -> Value {
    match kind {
        "scan" => scan_run(v),
        "adts-walker" => adts_run(v),
        "fingerprint" => fingerprint_run(v),
        "extract" => extract_run(v),
        "ass-to-vtt" => json!({ "vtt": sub::ass_to_vtt(v["input"]["ass"].as_str().unwrap()) }),
        "burn-in-argv" => burn_in_argv_run(v),
        "burn-in-filter" => {
            let s: sub::BurnInSource = serde_json::from_value(v["input"].clone()).unwrap();
            json!({ "filter": sub::burn_in_filter(&s) })
        }
        k => panic!("unknown kind {k}"),
    }
}

/// Replays a fixture directory; returns (cases, mismatches).
fn replay(dir: &str) -> (usize, Vec<String>) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/jobs")
        .join(dir);
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
        let kind = root["kind"].as_str().unwrap();
        let mut check = |idx: Option<usize>, case: &Value, name: &str| {
            n += 1;
            let got = run(kind, case);
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

fn replay_ok(dir: &str, at_least: usize) {
    let (n, bad) = replay(dir);
    assert!(n >= at_least, "{dir}: only {n} cases");
    assert!(
        bad.is_empty(),
        "{} of {n} {dir} cases differ:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

#[test]
fn scan_matches_host() {
    replay_ok("scan", 80);
}

#[test]
fn fingerprint_matches_host() {
    replay_ok("fingerprint", 20);
}

#[test]
fn subtitles_match_host() {
    replay_ok("subtitles", 80);
}

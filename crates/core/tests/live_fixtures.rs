//! Golden live TV vectors (`fixtures/live/*.json`, `BLESS=1` rewrites `expected`) through
//! `core::live`; see `docs/spec/live.md`.
mod common;

use common::{Fixture, blessing, diff};
use fliks_engine_core::api::{HwAccel, LiveCaps, LiveMode, LiveProbe};
use fliks_engine_core::live::*;
use serde_json::{Value, json};
use std::path::PathBuf;

fn caps(v: &Value) -> LiveCaps {
    serde_json::from_value(v.clone()).unwrap()
}

/// The host's `livetv_fast_zap` string as the setting it sends.
fn fast_zap(v: &Value) -> Option<bool> {
    match v.as_str() {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    }
}

fn hw(v: &Value) -> HwAccel {
    serde_json::from_value(v.clone()).unwrap()
}

fn mode(m: LiveMode) -> Value {
    serde_json::to_value(m).unwrap()
}

fn s(v: &Value) -> Option<&str> {
    v.as_str()
}

/// The TS harness: `recover()`, then a `drop` (the running upstream failing) recovers in place.
fn failover(n: usize, script: &[Value]) -> Value {
    let (mut f, mut attempts, mut k) = (Failover::new(n), vec![], 0);
    let mut recover = |f: &mut Failover, k: &mut usize| loop {
        let Some(u) = f.pick() else { return false };
        attempts.push(u);
        let ok = script.get(*k).and_then(Value::as_str) == Some("ok");
        *k += 1;
        if ok {
            return true;
        }
        f.failed(u);
    };
    let mut ok = recover(&mut f, &mut k);
    while ok && script.get(k).and_then(Value::as_str) == Some("drop") {
        k += 1;
        let cur = f.index % n;
        f.failed(cur);
        ok = recover(&mut f, &mut k);
    }
    json!({ "attempts": attempts, "ended": !ok, "failoverIndex": f.index, "restarts": f.restarts,
        "failures": f.failures })
}

fn run(func: &str, i: &Value) -> Value {
    match func {
        "parseProbeOutput" => {
            let p = parse_probe_output(i["stdout"].as_str().unwrap());
            json!({ "video": p.video_codec, "audio": p.audio_codec, "container": p.container })
        }
        "isManifestContainer" => json!(is_manifest_container(s(&i["container"]))),
        "isManifestPayload" => {
            let b = base64(i["base64"].as_str().unwrap());
            json!(is_manifest_payload(&b))
        }
        "isManifestContentType" => json!(is_manifest_content_type(s(&i["contentType"]).unwrap())),
        "snapBitrate" => json!(snap_bitrate(i["bps"].as_f64().unwrap())),
        "decideMode" => mode(decide_mode(
            &caps(&i["caps"]),
            fast_zap(&i["fastZap"]),
            hw(&i["hwAccel"]),
        )),
        "buildSessionKey" => {
            let m = serde_json::from_value(i["mode"].clone()).unwrap();
            json!(session_key(
                s(&i["channelId"]).unwrap(),
                m,
                &caps(&i["caps"])
            ))
        }
        "openPlan" => {
            let probe = LiveProbe {
                video_codec: s(&i["probe"]["probedVideoCodec"]).map(String::from),
                audio_codec: None,
                container: s(&i["probe"]["probedContainer"]).map(String::from),
            };
            let (m, c) = plan(
                &caps(&i["caps"]),
                fast_zap(&i["fastZap"]),
                hw(&i["hwAccel"]),
                Some(&probe),
            );
            json!({
                "mode": mode(m),
                "key": session_key(s(&i["channelId"]).unwrap(), m, &c),
                "effectiveMode": mode(effective_mode(m, probe.video_codec.as_deref())),
                "maxBitrateBps": c.max_bitrate_bps,
            })
        }
        "forcesTranscode" => json!(forces_transcode(
            s(&i["probedVideoCodec"]),
            s(&i["activeVideoCodec"])
        )),
        "failover" => failover(
            i["upstreams"].as_u64().unwrap() as usize,
            i["script"].as_array().unwrap(),
        ),
        "withToken" => json!(with_token(s(&i["raw"]).unwrap(), s(&i["token"]))),
        "isSegmentName" => json!(is_segment_name(s(&i["name"]).unwrap())),
        f => panic!("unknown fn {f}"),
    }
}

fn base64(t: &str) -> Vec<u8> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        _ => 63,
    };
    let bits: Vec<u8> = t.bytes().filter(|c| *c != b'=').map(val).collect();
    bits.chunks(4)
        .flat_map(|q| {
            let n = q
                .iter()
                .enumerate()
                .fold(0u32, |a, (i, v)| a | (*v as u32) << (18 - 6 * i));
            n.to_be_bytes()[1..q.len()].to_vec()
        })
        .collect()
}

#[test]
fn live_vectors_match_the_host() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/live");
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "fixtures/live is empty");
    let (mut bad, mut n) = (vec![], 0);
    for f in &files {
        let mut fx = Fixture::load(f);
        let root = fx.root.clone();
        let func = root["fn"].as_str().unwrap();
        for (i, c) in root["cases"].as_array().unwrap().iter().enumerate() {
            n += 1;
            let got = run(func, &c["input"]);
            let mut d = vec![];
            diff("", &got, &c["expected"], &mut d);
            if d.is_empty() {
                continue;
            }
            if blessing() {
                fx.bless(Some(i), got);
            } else {
                bad.push(format!("{func}[{i}] {}: {}", c["input"], d.join("; ")));
            }
        }
        fx.save();
    }
    for b in bad.iter().take(20) {
        eprintln!("FAIL {b}");
    }
    assert!(bad.is_empty(), "live: {} of {n} fail", bad.len());
}

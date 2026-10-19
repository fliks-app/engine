//! `fixtures/mp4/assembler/*.json`: real ffmpeg remux runs (GOP files, growing audio files) and
//! the served files the host's `RemuxSegmentAssembler` built from them, byte for byte.
use fliks_engine::remux::Assembler;
use fliks_engine_core::mp4::RemuxAssemblyPlan;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn unb64(s: &str) -> Vec<u8> {
    let (mut out, mut acc, mut bits) = (vec![], 0u32, 0);
    for c in s.bytes().filter(|c| *c != b'=') {
        acc = acc << 6 | B64.iter().position(|x| *x == c).expect("base64") as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

fn files(dir: &Path, rel: &str, out: &mut BTreeMap<String, Vec<u8>>) {
    for e in std::fs::read_dir(dir.join(rel)).unwrap().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let r = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        if e.file_type().unwrap().is_dir() {
            files(dir, &r, out);
        } else {
            out.insert(r, std::fs::read(e.path()).unwrap());
        }
    }
}

#[test]
fn assembler_reproduces_the_host() {
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/mp4/assembler");
    let mut n = 0;
    for e in std::fs::read_dir(fixtures).unwrap().flatten() {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(e.path()).unwrap()).unwrap();
        let name = v["name"].as_str().unwrap();
        let dir = std::env::temp_dir().join(format!("fliks-asm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let gop_dir = dir.join("gop-run");
        std::fs::create_dir_all(&gop_dir).unwrap();
        for (f, b) in v["input"].as_object().unwrap() {
            std::fs::write(gop_dir.join(f), unb64(b.as_str().unwrap())).unwrap();
        }
        let plan: RemuxAssemblyPlan = serde_json::from_value(v["plan"].clone()).unwrap();
        let renditions = plan.audio_renditions as usize;
        let mut a = Assembler::new(plan, dir.clone(), gop_dir.clone());
        a.finish(true);
        assert!(!gop_dir.exists(), "{name}: GOP dir left");
        let mut got = BTreeMap::new();
        files(&dir, "", &mut got);
        let want: BTreeMap<String, Vec<u8>> = v["expected"]["files"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, b)| (k.clone(), unb64(b.as_str().unwrap())))
            .collect();
        assert_eq!(
            got.keys().collect::<Vec<_>>(),
            want.keys().collect::<Vec<_>>(),
            "{name}: files"
        );
        for (k, w) in &want {
            assert!(
                got[k] == *w,
                "{name}: {k} differs ({} vs {} bytes)",
                got[k].len(),
                w.len()
            );
        }
        assert_eq!(
            a.view().progress(None).frontier.map(u64::from),
            v["expected"]["videoFrontier"].as_u64(),
            "{name}: video frontier"
        );
        for i in 1..=renditions {
            assert_eq!(
                a.view().progress(Some(i)).frontier.map(u64::from),
                v["expected"]["audioFrontiers"][i - 1].as_u64(),
                "{name}: audio {i} frontier"
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
        n += 1;
    }
    assert!(n >= 5, "fixtures/mp4/assembler: {n} replayed");
}

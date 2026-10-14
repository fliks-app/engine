//! Text-level vectors of the host (`fixtures/probe/_*.json`).
use super::build::format_fps;
use super::lang::{language_from_title, subtitle_flags_from_title};
use super::moviehash::{CHUNK, moviehash};
use serde_json::Value;

fn fixture(name: &str) -> Vec<Value> {
    let p = format!("{}/../../fixtures/probe/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str::<Vec<Value>>(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[test]
fn titles_infer_like_the_host() {
    let mut bad = vec![];
    let rows = fixture("_titles.json");
    for r in &rows {
        let title = r["title"].as_str().unwrap();
        let got = (
            language_from_title(Some(title)),
            subtitle_flags_from_title(Some(title)),
        );
        let want = (
            r["language"].as_str(),
            (
                r["forced"].as_bool().unwrap(),
                r["hearingImpaired"].as_bool().unwrap(),
            ),
        );
        if got != want {
            bad.push(format!("{title:?}: got {got:?}, host {want:?}"));
        }
    }
    assert!(rows.len() > 500);
    assert!(
        bad.is_empty(),
        "{} of {}\n{}",
        bad.len(),
        rows.len(),
        bad[..bad.len().min(20)].join("\n")
    );
}

#[test]
fn frame_rates_format_like_the_host() {
    let rows = fixture("_fps.json");
    let bad: Vec<_> = rows
        .iter()
        .filter(|r| format_fps(r["fps"].as_f64().unwrap()) != r["text"].as_str().unwrap())
        .map(|r| {
            format!(
                "{}: got {}, host {}",
                r["fps"],
                format_fps(r["fps"].as_f64().unwrap()),
                r["text"]
            )
        })
        .collect();
    assert!(rows.len() > 300);
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// The synthetic file of the host harness: a byte per absolute offset.
fn byte_at(o: u64) -> u8 {
    ((o % 251) + ((o / 251) % 241) * 3 + 17) as u8
}

#[test]
fn moviehash_follows_the_specification() {
    for r in fixture("_moviehash.json") {
        let size = r["size"].as_u64().unwrap();
        let head: Vec<u8> = (0..CHUNK as u64).map(byte_at).collect();
        let tail: Vec<u8> = (size.saturating_sub(CHUNK as u64)..size)
            .take(CHUNK)
            .map(byte_at)
            .collect();
        let got = moviehash(size, &head, &tail);
        assert_eq!(got.as_deref(), r["reference"].as_str(), "size {size}");
        // The host overflows the size's low word once bit 31 is set: its high word is one short.
        let host = r["host"].as_str();
        if let (Some(g), Some(h)) = (&got, host) {
            let (g, h) = (
                u64::from_str_radix(g, 16).unwrap(),
                u64::from_str_radix(h, 16).unwrap(),
            );
            let signed = size % (1 << 32) >= 1 << 31;
            let want = if signed { g.wrapping_sub(1 << 32) } else { g };
            assert_eq!(h, want, "host hash of size {size}");
        } else {
            assert_eq!(got.is_some(), host.is_some(), "size {size}");
        }
    }
}

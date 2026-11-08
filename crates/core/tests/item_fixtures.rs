//! Golden item decisions (`fixtures/item/*.json`, `BLESS=1` rewrites `expected`).
mod common;

use common::{Fixture, blessing, diff};
use fliks_engine_core::api::{
    DecideRequest, Decision, HostCaps, OpenSessionRequest, StreamingConfig,
};
use fliks_engine_core::decide::decide;
use fliks_engine_core::session::plan;
use std::path::PathBuf;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/item")
}

#[test]
fn decide_reproduces_expected() {
    let mut names: Vec<_> = std::fs::read_dir(dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    names.sort();
    assert!(!names.is_empty(), "fixtures/item is empty");
    let mut failed = vec![];
    for path in &names {
        let mut fx = Fixture::load(path);
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let f = &fx.root;
        let req: DecideRequest = serde_json::from_value(f["input"].clone())
            .unwrap_or_else(|e| panic!("{name}: input: {e}"));
        let caps: HostCaps = serde_json::from_value(f["hostCaps"].clone()).unwrap();
        let config: StreamingConfig = serde_json::from_value(f["config"].clone()).unwrap();
        let got = serde_json::to_value::<Decision>(decide(&req, &caps, &config)).unwrap();
        let mut d = vec![];
        diff("", &got, &f["expected"], &mut d);
        if d.is_empty() {
            continue;
        }
        if blessing() {
            fx.bless(None, got);
            fx.save();
        } else {
            failed.push(format!("{name}\n  {}", d.join("\n  ")));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

/// The audio track a session plays changes `-map`, so it forks the cache directory.
#[test]
fn the_picked_audio_track_forks_the_cache_hash() {
    let f = Fixture::load(&dir().join("aac-parts-second-audio-track.json"));
    let hash = |pick: i32| {
        let mut input = f.root["input"].clone();
        input["selection"]["audioStreamIndex"] = pick.into();
        let req: OpenSessionRequest =
            serde_json::from_value(serde_json::json!({ "decide": input })).unwrap();
        let caps: HostCaps = serde_json::from_value(f.root["hostCaps"].clone()).unwrap();
        let config: StreamingConfig = serde_json::from_value(f.root["config"].clone()).unwrap();
        let d = decide(&req.decide, &caps, &config);
        plan(&req, &d, &config, &caps).profile_hash
    };
    assert_ne!(hash(0), hash(1));
    assert_eq!(hash(1), hash(1));
}

/// A key change is never played as it is nor copied: it is encoded through the `rubberband`
/// filter, in a cache directory of its own.
#[test]
fn a_key_change_forces_a_transcode_and_forks_the_cache_hash() {
    let f = Fixture::load(&dir().join("mp3-parts-direct-play.json"));
    let caps: HostCaps = serde_json::from_value(f.root["hostCaps"].clone()).unwrap();
    let config: StreamingConfig = serde_json::from_value(f.root["config"].clone()).unwrap();
    let of = |pitch: Option<f64>| {
        let mut input = f.root["input"].clone();
        if let Some(p) = pitch {
            input["selection"]["pitchSemitones"] = p.into();
        }
        let req: OpenSessionRequest =
            serde_json::from_value(serde_json::json!({ "decide": input })).unwrap();
        let d = decide(&req.decide, &caps, &config);
        let p = plan(&req, &d, &config, &caps);
        (d, p)
    };
    let (plain, plain_plan) = of(None);
    assert_eq!(
        plain.response.play_method,
        fliks_engine_core::api::PlayMethod::DirectPlay
    );
    for (pitch, same) in [(Some(0.0), true), (Some(2.0), false), (Some(-3.0), false)] {
        let (d, p) = of(pitch);
        let shifted = pitch != Some(0.0);
        assert_eq!(
            d.response.play_method == fliks_engine_core::api::PlayMethod::Transcode,
            shifted
        );
        assert_eq!(
            d.response
                .transcode_reasons
                .iter()
                .any(|r| r.flag == "PitchShifted"),
            shifted
        );
        assert_eq!(p.profile_hash == plain_plan.profile_hash, same, "{pitch:?}");
        assert_eq!(
            p.item.as_ref().map(|i| i.pitch_semitones),
            shifted
                .then(|| pitch.unwrap())
                .or(p.item.as_ref().map(|_| 0.0))
        );
    }
    assert_ne!(of(Some(2.0)).1.profile_hash, of(Some(3.0)).1.profile_hash);
    // Out of range values are clamped, so a host cannot fill the cache with variants.
    assert_eq!(of(Some(40.0)).1.profile_hash, of(Some(12.0)).1.profile_hash);
}

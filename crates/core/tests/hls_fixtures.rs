//! Golden playlists (`fixtures/hls/*.json`, one call each; `BLESS=1` rewrites `expected`).
mod common;

use common::{Fixture, blessing, diff};
use fliks_engine_core::api::{AudioPlan, VideoStreamInfo};
use fliks_engine_core::decide::dolby_vision::{dv_standalone_codecs, dv_supplemental_codecs};
use fliks_engine_core::hls::parse_source_fps;
use fliks_engine_core::hls::{self, MasterOptions, Rendition, SegUrl};
use serde_json::{Value, json};
use std::path::PathBuf;

fn f(v: &Value) -> f64 {
    v.as_f64()
        .unwrap_or_else(|| panic!("number expected, got {v}"))
}

fn opt(v: &Value) -> Option<f64> {
    v.as_f64()
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

fn seg(a: &Value) -> SegUrl<'_> {
    SegUrl {
        prefix: s(&a["seg"]["prefix"]),
        suffix: s(&a["seg"]["suffix"]),
    }
}

fn prefix(a: &Value) -> String {
    format!("/api/stream/{}", a["mediaFileId"])
}

/// The host's `?token=..&sid=..` suffix.
fn token(a: &Value) -> String {
    let parts: Vec<String> = [("token", &a["token"]), ("sid", &a["sid"])]
        .into_iter()
        .filter(|(_, v)| !s(v).is_empty())
        .map(|(k, v)| format!("{k}={}", s(v)))
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

/// The route's duration: a positive `?duration=` hint, else the resolved one.
fn duration(a: &Value) -> f64 {
    let hint = a["durationHint"]
        .as_str()
        .and_then(|h| h.parse::<f64>().ok())
        .unwrap_or(0.0);
    if hint != 0.0 && !hint.is_nan() {
        hint
    } else {
        opt(&a["resolvedDuration"]).unwrap_or(0.0)
    }
}

fn route(a: &Value, f_: &str) -> String {
    let live = &a["live"];
    let fps = opt(&live["sourceFps"]).or_else(|| parse_source_fps(a["frameRate"].as_str()));
    let tok = token(a);
    let durations: Option<Vec<f64>> = live["remuxGrid"]["durations"]
        .as_array()
        .map(|d| d.iter().map(f).collect());
    let r = Rendition {
        token: &tok,
        use_ts: live["useTs"].as_bool().unwrap_or(false),
        remux_durations: durations.as_deref(),
        duration: duration(a),
        source_fps: fps,
        segment_duration: opt(&a["segmentDuration"]).unwrap_or(0.0),
    };
    let p = prefix(a);
    match f_ {
        "variantRoute" => hls::variant_playlist(
            &p,
            s(&a["quality"]),
            live["useExtXMedia"].as_bool().unwrap_or(false),
            &r,
        ),
        "audioRoute" => {
            let remux = live["kind"] == "remux";
            hls::audio_playlist(&p, a["audioIndex"].as_u64().unwrap() as u32, remux, &r)
        }
        "iframeRoute" => hls::iframe_index(&p, &tok, r.duration, fps, r.segment_duration),
        _ => {
            let path = match s(&a["kind"]) {
                "embedded" => format!("subtitles/embedded/{}", a["key"]),
                _ => format!("subtitles/{}", a["key"]),
            };
            hls::subtitle_playlist(
                opt(&a["durationSeconds"]).unwrap_or(0.0),
                &format!("{p}/{path}{tok}"),
            )
        }
    }
}

fn run(fn_: &str, a: &Value) -> Value {
    match fn_ {
        "masterPlaylist" => {
            let mut o = a.clone();
            o["prefix"] = json!(prefix(a));
            let opts: MasterOptions = serde_json::from_value(o).expect("master options");
            json!(hls::master_playlist(&opts))
        }
        "remuxBandwidthBps" => {
            let plans: Vec<AudioPlan> =
                serde_json::from_value(a["audioPlans"].clone()).expect("plans");
            json!(hls::remux_bandwidth_bps(
                opt(&a["sourceVideoBitrateBps"]),
                opt(&a["formatBitRate"]),
                &plans
            ))
        }
        "pinnedRungName" => json!(hls::pinned_rung_name(
            s(&a["onlyQuality"]),
            a["sourceWidth"].as_u64().unwrap() as u32,
            a["sourceHeight"].as_u64().unwrap() as u32,
            a["hdrLadder"].as_bool().unwrap_or(false),
        )),
        "vodPlaylist" => json!(hls::vod_playlist(
            f(&a["duration"]),
            seg(a),
            a["init"].as_str(),
            f(&a["segDuration"]),
            f(&a["frameSeconds"]),
        )),
        "iframePlaylist" => json!(hls::iframe_playlist(
            f(&a["duration"]),
            seg(a),
            f(&a["segDuration"]),
            f(&a["frameSeconds"]),
        )),
        "variableVodPlaylist" => {
            let d: Vec<f64> = a["durations"].as_array().unwrap().iter().map(f).collect();
            json!(hls::variable_vod_playlist(&d, seg(a), a["init"].as_str()))
        }
        "copySourceCodecString" => {
            let mut v = json!({ "streamIndex": 0, "codec": "" });
            for (k, x) in a["video"].as_object().unwrap() {
                v[k] = x.clone();
            }
            let v: VideoStreamInfo = serde_json::from_value(v).expect("video");
            json!(hls::copy_source_codec_string(&v))
        }
        "dvCodecs" => {
            let out: Vec<Value> = a["cases"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| {
                    let mut v = json!({ "streamIndex": 0, "codec": "" });
                    for (k, x) in c.as_object().unwrap() {
                        v[k] = x.clone();
                    }
                    let v: VideoStreamInfo = serde_json::from_value(v).expect("video");
                    json!([
                        dv_supplemental_codecs(Some(&v)),
                        dv_standalone_codecs(Some(&v))
                    ])
                })
                .collect();
            json!(out)
        }
        r if r.ends_with("Route") => json!(route(a, r)),
        other => panic!("unknown fn {other}"),
    }
}

/// Mismatch text, `None` when the port reproduces `expected`.
fn check(v: &Value) -> Option<String> {
    let got = run(s(&v["fn"]), &v["args"]);
    let want = &v["expected"]["ok"];
    let mut d = vec![];
    diff("", &got, want, &mut d);
    (!d.is_empty()).then(|| match (got.as_str(), want.as_str()) {
        (Some(g), Some(w)) => {
            let at = g.lines().zip(w.lines()).position(|(x, y)| x != y);
            format!(
                "first differing line {at:?}:\n  got  {:?}\n  want {:?}",
                at.map(|i| g.lines().nth(i)),
                at.map(|i| w.lines().nth(i))
            )
        }
        _ => d.join("\n  "),
    })
}

fn report(what: &str, n: usize, failed: &[String]) {
    for f in failed.iter().take(10) {
        eprintln!("FAIL {f}");
    }
    assert!(n > 0, "{what}: nothing replayed");
    assert!(failed.is_empty(), "{what}: {} of {n} fail", failed.len());
}

#[test]
fn hls_fixtures_reproduce_the_host() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/hls");
    let (mut n, mut failed) = (0, vec![]);
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let mut fx = Fixture::load(&p);
        if let Some(m) = check(&fx.root) {
            if blessing() {
                let got = run(s(&fx.root["fn"]), &fx.root["args"]);
                fx.bless(None, json!({ "ok": got }));
                fx.save();
                n += 1;
                continue;
            }
            failed.push(format!("{}: {m}", p.file_name().unwrap().to_string_lossy()));
        }
        n += 1;
    }
    report("fixtures/hls", n, &failed);
}

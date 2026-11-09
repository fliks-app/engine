//! An item of three audio parts against real ffmpeg (`FFMPEG_PATH`, `FFPROBE_PATH`), played as
//! one HLS timeline. Ignored unless `ENGINE_E2E` is set when building:
//! `ENGINE_E2E=1 cargo test -p fliks-engine --test item_e2e`.
#![cfg(unix)]
mod runtime_common;
use fliks_engine::runtime::Runtime;
use fliks_engine_core::api::ResolvedPart;
use fliks_engine_core::mp4::{first_tfdt, parse_init_tracks};
use runtime_common::*;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

/// Seconds of the three parts: 3 + 2 + 2 segments of 6 s.
const DURATIONS: [u32; 3] = [14, 9, 8];
const TOTAL: f64 = 31.0;

fn sine(codec_args: &str, ext: &str) -> Vec<PathBuf> {
    DURATIONS
        .iter()
        .enumerate()
        .map(|(i, d)| {
            clip(
                &format!("item-{ext}-{i}.{ext}"),
                &format!(
                    "-f lavfi -i sine=frequency={}:sample_rate=44100:duration={d} {codec_args}",
                    [440, 660, 880][i]
                ),
            )
        })
        .collect()
}

struct Item {
    e: Engine,
    parts: Vec<PathBuf>,
}

async fn item(parts: Vec<PathBuf>) -> Item {
    let e = engine().await;
    e.stub.grant("alice-jwt", "alice");
    let resolved = parts
        .iter()
        .map(|p| ResolvedPart {
            path: p.to_string_lossy().into(),
            duration_s: None,
            start_s: None,
            end_s: None,
            format: None,
        })
        .collect();
    e.stub.items.lock().unwrap().insert(1, resolved);
    Item { e, parts }
}

impl Item {
    async fn open(&self, device: Value, start_at: f64) -> Value {
        let mut parts = vec![];
        for (i, p) in self.parts.iter().enumerate() {
            let info = self.e.probe(10 + i as u32, p).await;
            parts.push(json!({ "path": p, "info": info }));
        }
        let req = json!({
            "decide": {
                "media": {
                    "mediaFileId": 1, "path": self.parts[0], "ext": ".mp3",
                    "contentType": "audio/mpeg", "parts": parts,
                },
                "info": parts[0]["info"],
                "device": device,
                "selection": { "tokenParam": "?token=t" },
            },
            "ownerId": "alice",
            "startAtS": start_at,
        });
        let r = self.e.control("POST", "/v1/sessions", Some(req)).await;
        assert_eq!(r.status, 200, "{}", r.text());
        r.json()
    }

    async fn get(&self, sid: &str, name: &str) -> Res {
        let path = format!("/api/stream/1/item/{name}?sid={sid}");
        for _ in 0..5 {
            let r = self.e.get(&path, "alice-jwt", &[]).await;
            if r.status != 503 {
                return r;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        self.e.get(&path, "alice-jwt", &[]).await
    }
}

fn tfdt_seconds(init: &[u8], seg: &[u8]) -> f64 {
    let tracks = parse_init_tracks(init).unwrap();
    let (id, t) = tracks.iter().find(|(_, t)| !t.is_video).copied().unwrap();
    first_tfdt(seg, id).unwrap().unwrap() as f64 / f64::from(t.timescale)
}

fn extinf_sum(playlist: &str) -> f64 {
    playlist
        .lines()
        .filter_map(|l| l.strip_prefix("#EXTINF:"))
        .map(|l| l.trim_end_matches(',').parse::<f64>().unwrap())
        .sum()
}

fn seg_name(n: u32) -> String {
    format!("seg-{n:04}.m4s")
}

/// Segments `(global index, expected start)` of each part, one apiece plus the seek target.
const PROBES: [(u32, f64); 4] = [(0, 0.0), (3, 14.0), (4, 20.0), (5, 23.0)];

async fn play(parts: Vec<PathBuf>, device: Value, copied: bool) {
    let it = item(parts).await;
    let open = it.open(device, 16.0).await;
    let r = &open["decision"]["response"];
    assert_eq!(r["audioCopyStream"], copied, "{r}");
    let sid = open["sessionId"].as_str().unwrap();
    assert_eq!(
        open["playPath"],
        format!("/api/stream/1/item/index.m3u8?token=t&sid={sid}")
    );
    let list = it.get(sid, "index.m3u8").await;
    assert_eq!(list.status, 200, "{}", list.text());
    let text = list.text();
    assert_eq!(text.matches("#EXTINF").count(), 7, "{text}");
    assert!((extinf_sum(&text) - TOTAL).abs() < 0.01, "{text}");
    assert!(text.contains("#EXT-X-MAP:URI=\"/api/stream/1/item/init.mp4?"));
    // The position is in part 2: its init is the one the session serves.
    let init = it.get(sid, "init.mp4").await;
    assert_eq!(init.status, 200);
    for (n, at) in PROBES {
        let s = it.get(sid, &seg_name(n)).await;
        assert_eq!(s.status, 200, "seg {n}: {}", s.text());
        let t = tfdt_seconds(&init.body, &s.body);
        assert!((t - at).abs() < 0.1, "seg {n} starts at {t}, want {at}");
    }
    // Every segment in order decodes to the item's length.
    let mut whole = init.body.to_vec();
    for n in 0..7 {
        whole.extend(it.get(sid, &seg_name(n)).await.body);
    }
    let dir = temp_dir("item-whole");
    let file = dir.join("whole.mp4");
    std::fs::write(&file, whole).unwrap();
    let out = std::process::Command::new(ffmpeg())
        .args(["-v", "error", "-i"])
        .arg(&file)
        .args(["-f", "null", "-"])
        .output()
        .unwrap();
    assert!(
        out.status.success() && out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let probe =
        std::process::Command::new(std::env::var("FFPROBE_PATH").unwrap_or("ffprobe".into()))
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "csv=p=0",
            ])
            .arg(&file)
            .output()
            .unwrap();
    let total: f64 = String::from_utf8_lossy(&probe.stdout)
        .trim()
        .parse()
        .unwrap();
    assert!((total - TOTAL).abs() < 0.3, "decoded {total} s");
    // A segment past the playlist is a 404, and so is the end of the last part.
    assert_eq!(it.get(sid, &seg_name(7)).await.status, 404);
    let snap =
        it.e.control("GET", &format!("/v1/sessions/{sid}"), None)
            .await
            .json();
    assert_eq!(
        snap["decision"]["response"]["parts"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

fn aac_only() -> Value {
    json!({
        "directPlayProfiles": [{ "containers": ["mp4"], "videoCodecs": [], "audioCodecs": ["aac"] }],
        "supportsDirectPlay": false,
    })
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn three_mp3_parts_play_as_one_aac_timeline() {
    play(sine("-c:a libmp3lame -b:a 96k", "mp3"), aac_only(), false).await;
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn three_aac_parts_are_copied_into_one_timeline() {
    play(sine("-c:a aac -b:a 96k", "m4a"), aac_only(), true).await;
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn parts_are_served_by_byte_range() {
    let it = item(sine("-c:a libmp3lame -b:a 96k", "mp3")).await;
    let r =
        it.e.get(
            "/api/stream/1/parts/1",
            "alice-jwt",
            &[("range", "bytes=0-9")],
        )
        .await;
    assert_eq!(r.status, 206);
    assert_eq!(r.body.len(), 10);
    assert_eq!(r.header("content-type").as_deref(), Some("audio/mpeg"));
    assert_eq!(
        it.e.get("/api/stream/1/parts/3", "alice-jwt", &[])
            .await
            .status,
        404
    );
    let _: &Runtime = &it.e.rt;
}

/// Normalised correlation of `a` and `b` shifted by `lag` (`b[i + lag]` against `a[i]`).
fn correlation(a: &[f32], b: &[f32], lag: isize) -> f64 {
    let (mut ab, mut aa, mut bb) = (0.0f64, 0.0f64, 0.0f64);
    for (i, x) in a.iter().enumerate() {
        let Some(y) = b.get((i as isize + lag) as usize) else {
            continue;
        };
        let (x, y) = (f64::from(*x), f64::from(*y));
        ab += x * y;
        aa += x * x;
        bb += y * y;
    }
    ab / (aa * bb).sqrt().max(1e-12)
}

fn decode_f32(file: &std::path::Path) -> Vec<f32> {
    let out = std::process::Command::new(ffmpeg())
        .args(["-v", "error", "-i"])
        .arg(file)
        .args(["-f", "f32le", "-ac", "1", "-ar", "44100", "-"])
        .output()
        .unwrap();
    out.stdout
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect()
}

/// A part longer than one run budget used to be encoded as several runs, and the join at 180 s
/// was off by some samples. One run now covers it: every window of the decoded HLS lines up with
/// the source at the delay of the first one.
#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn a_long_part_is_sample_aligned_with_its_source_past_180_seconds() {
    let part = clip(
        "item-seam-noise-372.flac",
        "-f lavfi -i anoisesrc=d=372:c=pink:r=44100:a=0.3:seed=7 -c:a flac",
    );
    let it = item(vec![part.clone()]).await;
    let open = it.open(aac_only(), 0.0).await;
    let sid = open["sessionId"].as_str().unwrap();
    let mut whole = it.get(sid, "init.mp4").await.body.to_vec();
    for n in 0..62 {
        // A client reads at playback speed: never faster than the run encodes.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let s = it.get(sid, &seg_name(n)).await;
        assert_eq!(s.status, 200, "seg {n}: {}", s.text());
        whole.extend(s.body);
    }
    assert_eq!(
        std::fs::read_dir(it.e.root.join("cache/v2/ualice/1"))
            .unwrap()
            .count(),
        1
    );
    let dir = temp_dir("item-seam");
    let file = dir.join("whole.mp4");
    std::fs::write(&file, whole).unwrap();
    let (src, out) = (decode_f32(&part), decode_f32(&file));
    const WIN: usize = 2048;
    // The encoder's delay, from a window early in the stream.
    let at = |s: f64| (s * 44100.0) as usize;
    let early = &src[at(2.0)..at(2.0) + WIN];
    let delay = (0..4096isize)
        .max_by(|a, b| {
            let (ca, cb) = (
                correlation(early, &out[at(2.0)..], *a),
                correlation(early, &out[at(2.0)..], *b),
            );
            ca.total_cmp(&cb)
        })
        .unwrap();
    let mut checked = 0;
    for t in (3..370).step_by(7).chain(170..190) {
        let w = &src[at(f64::from(t))..at(f64::from(t)) + WIN];
        let o = &out[at(f64::from(t))..];
        let best = (delay - 3..=delay + 3)
            .max_by(|a, b| correlation(w, o, *a).total_cmp(&correlation(w, o, *b)))
            .unwrap();
        assert_eq!(
            best,
            delay,
            "at {t} s the output is off by {}",
            best - delay
        );
        assert!(
            correlation(w, o, delay) > 0.8,
            "at {t} s the output is not the source"
        );
        checked += 1;
    }
    assert!(checked > 70);
}

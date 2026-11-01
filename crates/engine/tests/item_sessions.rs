//! Item sessions with a fake ffmpeg that writes MPEG-TS segments at once: which run a request
//! starts, with which argv, and what the session reports.
#![cfg(unix)]
mod runtime_common;
use fliks_engine_core::api::ResolvedPart;
use runtime_common::*;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

struct Item {
    e: Engine,
    sid: String,
}

fn info(seconds: f64) -> Value {
    json!({
        "video": [], "audio": [{ "streamIndex": 0, "codec": "mp3", "language": "und", "channels": 2, "sampleRate": 44100 }],
        "subtitles": [], "durationSeconds": seconds, "formatName": "mp3",
    })
}

/// An item of parts lasting `seconds`, played as MPEG-TS HLS by a client that only takes AAC.
async fn open(seconds: &[f64]) -> Item {
    let e = engine_with(
        host(
            fliks_engine_core::api::HwAccel::None,
            "/dev/dri/renderD128",
            "signal",
        ),
        &fake("item"),
        json!({}),
    )
    .await;
    e.stub.grant("alice-jwt", "alice");
    let paths: Vec<String> = (0..seconds.len())
        .map(|i| format!("/books/p{i}.mp3"))
        .collect();
    e.stub.items.lock().unwrap().insert(
        1,
        paths
            .iter()
            .map(|p| ResolvedPart {
                path: p.clone(),
                duration_s: None,
                start_s: None,
                end_s: None,
                format: None,
            })
            .collect(),
    );
    let parts: Vec<Value> = paths
        .iter()
        .zip(seconds)
        .map(|(p, d)| json!({ "path": p, "info": info(*d) }))
        .collect();
    let req = json!({
        "decide": {
            "media": { "mediaFileId": 1, "path": paths[0], "ext": ".mp3", "contentType": "audio/mpeg", "parts": parts },
            "info": info(seconds[0]),
            "device": {
                "directPlayProfiles": [{ "containers": ["mp4"], "videoCodecs": [], "audioCodecs": ["aac"] }],
                "useTs": true,
            },
            "selection": {},
        },
        "ownerId": "alice",
    });
    let r = e.control("POST", "/v1/sessions", Some(req)).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let sid = r.json()["sessionId"].as_str().unwrap().to_string();
    Item { e, sid }
}

impl Item {
    async fn get(&self, name: &str) -> Res {
        let path = format!("/api/stream/1/item/{name}?sid={}", self.sid);
        for _ in 0..5 {
            let r = self.e.get(&path, "alice-jwt", &[]).await;
            if r.status != 503 {
                return r;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        self.e.get(&path, "alice-jwt", &[]).await
    }

    /// The argv lines logged by the runs of part `part`.
    fn runs(&self, part: usize) -> Vec<Vec<String>> {
        fn find(dir: &Path, name: &str, out: &mut Vec<PathBuf>) {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    find(&p, name, out);
                } else if p
                    .parent()
                    .and_then(|d| d.file_name())
                    .is_some_and(|d| d == name)
                {
                    out.push(p);
                }
            }
        }
        let mut logs = vec![];
        find(&self.e.root, &format!("p{part}"), &mut logs);
        logs.iter()
            .filter(|p| p.file_name().is_some_and(|n| n == "argv.log"))
            .flat_map(|p| {
                std::fs::read_to_string(p)
                    .unwrap()
                    .lines()
                    .map(String::from)
                    .collect::<Vec<_>>()
            })
            .map(|l| serde_json::from_str(&l).unwrap())
            .collect()
    }
}

fn arg<'a>(argv: &'a [String], flag: &str) -> &'a str {
    &argv[argv.iter().position(|a| a == flag).unwrap() + 1]
}

#[tokio::test]
async fn the_playlist_covers_every_part_and_a_segment_runs_its_own_part() {
    let it = open(&[14.0, 9.0, 8.0]).await;
    let list = it.get("index.m3u8").await.text();
    assert_eq!(list.matches("#EXTINF").count(), 7, "{list}");
    assert!(!list.contains("EXT-X-MAP"), "{list}");
    // Global segment 4 is the second one of part 1 (6 s into its 9 s).
    let s = it.get("seg-0004.ts").await;
    assert_eq!((s.status, s.text().as_str()), (200, "segment 1"));
    let runs = it.runs(1);
    assert_eq!(runs.len(), 1);
    let argv = &runs[0];
    assert_eq!(arg(argv, "-i"), "/books/p1.mp3");
    assert_eq!((arg(argv, "-ss"), arg(argv, "-start_number")), ("6", "1"));
    // MPEG-TS carries the item's clock: 1 s headroom + part offset 14 + 6 into the part.
    assert_eq!(arg(argv, "-output_ts_offset"), "21");
    assert!(it.runs(2).is_empty());
    // The playlist warmed the run of the playhead's part, which this request reuses.
    let s = it.get("seg-0000.ts").await;
    assert_eq!(s.status, 200);
    let first = it.runs(0);
    assert_eq!(first.len(), 1);
    assert!(!first[0].contains(&"-ss".to_string()));
    assert_eq!(arg(&first[0], "-output_ts_offset"), "1");
    // The first part's run does not disturb the second's files.
    assert_eq!(it.get("seg-0004.ts").await.status, 200);
    assert_eq!(it.runs(1).len(), 1);
}

#[tokio::test]
async fn a_run_stops_after_its_budget_and_the_next_request_starts_another() {
    let it = open(&[400.0]).await;
    assert_eq!(it.get("seg-0029.ts").await.status, 200);
    let runs = it.runs(0);
    assert_eq!(runs.len(), 1);
    // Requested at 29, the run began there and is cut after 30 segments.
    assert_eq!(arg(&runs[0], "-t"), "180");
    assert_eq!(it.get("seg-0059.ts").await.status, 200);
    let runs = it.runs(0);
    assert_eq!(runs.len(), 2);
    assert_eq!(
        (arg(&runs[1], "-ss"), arg(&runs[1], "-start_number")),
        ("354", "59")
    );
}

#[tokio::test]
async fn past_the_end_is_a_404_and_the_snapshot_follows_the_playhead() {
    let it = open(&[14.0, 9.0]).await;
    assert_eq!(it.get("seg-0005.ts").await.status, 404);
    assert_eq!(it.get("seg-0003.ts").await.status, 200);
    let r =
        it.e.control(
            "POST",
            &format!("/v1/sessions/{}/heartbeat", it.sid),
            Some(json!({ "positionS": 15.0 })),
        )
        .await;
    assert_eq!(r.status, 200);
    let snap =
        it.e.control("GET", &format!("/v1/sessions/{}", it.sid), None)
            .await
            .json();
    assert_eq!(snap["job"]["quality"], "p1");
    assert_eq!(snap["job"]["startSegment"], 0);
}

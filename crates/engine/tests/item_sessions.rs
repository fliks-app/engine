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

/// An engine whose resolver answers `paths` as item 1, for alice and bob.
async fn engine_of(paths: &[String]) -> Engine {
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
    e.stub.grant("bob-jwt", "bob");
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
    e
}

/// Opens item 1 for alice, as MPEG-TS HLS for a client that only takes AAC.
async fn open_parts(e: &Engine, paths: &[String], infos: &[Value], start_at: f64) -> Res {
    let parts: Vec<Value> = paths
        .iter()
        .zip(infos)
        .map(|(p, i)| json!({ "path": p, "info": i }))
        .collect();
    let req = json!({
        "decide": {
            "media": { "mediaFileId": 1, "path": paths[0], "ext": ".mp3", "contentType": "audio/mpeg", "parts": parts },
            "info": infos[0],
            "device": {
                "directPlayProfiles": [{ "containers": ["mp4"], "videoCodecs": [], "audioCodecs": ["aac"] }],
                "useTs": true,
            },
            "selection": {},
        },
        "ownerId": "alice",
        "startAtS": start_at,
    });
    e.control("POST", "/v1/sessions", Some(req)).await
}

/// Opens item 1 with a key change of `semitones`.
async fn open_pitched(e: &Engine, paths: &[String], semitones: f64) -> Res {
    let parts: Vec<Value> = paths
        .iter()
        .map(|p| json!({ "path": p, "info": info(30.0) }))
        .collect();
    let req = json!({
        "decide": {
            "media": { "mediaFileId": 1, "path": paths[0], "ext": ".mp3", "contentType": "audio/mpeg", "parts": parts },
            "info": info(30.0),
            "device": { "directPlayProfiles": [{ "containers": ["mp3"], "videoCodecs": [], "audioCodecs": ["mp3"] }], "useTs": true },
            "selection": { "pitchSemitones": semitones },
        },
        "ownerId": "alice",
    });
    e.control("POST", "/v1/sessions", Some(req)).await
}

/// An item of fake parts lasting `seconds`.
async fn open(seconds: &[f64], start_at: f64) -> Item {
    let paths: Vec<String> = (0..seconds.len())
        .map(|i| format!("/books/p{i}.mp3"))
        .collect();
    let e = engine_of(&paths).await;
    let infos: Vec<Value> = seconds.iter().map(|d| info(*d)).collect();
    let r = open_parts(&e, &paths, &infos, start_at).await;
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
    let it = open(&[14.0, 9.0, 8.0], 0.0).await;
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
    // The open warms the run of segment 29, so the request at 29 is the playhead's, not a seek.
    let it = open(&[400.0], 174.0).await;
    assert_eq!(it.get("seg-0029.ts").await.status, 200);
    let runs = it.runs(0);
    assert_eq!(runs.len(), 1);
    // The run began at 29 and is cut after 30 segments.
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
    let it = open(&[14.0, 9.0], 0.0).await;
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

#[tokio::test]
async fn a_tail_under_half_a_second_lists_no_segment_of_its_own() {
    let it = open(&[12.005], 0.0).await;
    let list = it.get("index.m3u8").await.text();
    assert_eq!(list.matches("#EXTINF").count(), 2, "{list}");
    assert!(list.contains("#EXTINF:6.005,"), "{list}");
    assert_eq!(it.get("seg-0001.ts").await.status, 200);
    assert_eq!(it.get("seg-0002.ts").await.status, 404);
}

#[tokio::test]
async fn an_hls_item_needs_every_part_to_have_a_duration() {
    let paths = vec!["/books/p0.mp3".to_string(), "/books/p1.mp3".to_string()];
    let e = engine_of(&paths).await;
    let mut no_duration = info(5.0);
    no_duration
        .as_object_mut()
        .unwrap()
        .remove("durationSeconds");
    let r = open_parts(&e, &paths, &[info(5.0), no_duration], 0.0).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.text().contains("parts[1]"), "{}", r.text());
    // A slice that ends before it starts has none either.
    let parts = json!([{ "path": paths[0], "info": info(5.0), "startS": 4.0, "endS": 2.0 }]);
    let req = json!({
        "decide": {
            "media": { "mediaFileId": 1, "path": paths[0], "ext": ".mp3", "contentType": "audio/mpeg", "parts": parts },
            "info": info(5.0),
            "device": { "directPlayProfiles": [], "useTs": true },
            "selection": {},
        },
    });
    assert_eq!(
        e.control("POST", "/v1/sessions", Some(req)).await.status,
        400
    );
}

#[tokio::test]
async fn parts_are_ranged_gated_by_owner_and_served_while_the_host_is_down() {
    let dir = temp_dir("parts");
    let paths: Vec<String> = ["a.mp3", "b.mp3"]
        .iter()
        .map(|n| {
            std::fs::write(dir.join(n), b"0123456789").unwrap();
            dir.join(n).to_string_lossy().into_owned()
        })
        .collect();
    let e = engine_of(&paths).await;
    let r = open_parts(&e, &paths, &[info(5.0), info(5.0)], 0.0).await;
    let sid = r.json()["sessionId"].as_str().unwrap().to_string();
    let part = |n: u32, cred: &'static str, q: &str, range: &'static str| {
        let (e, sid, q) = (&e, sid.clone(), q.to_string());
        async move {
            let path = format!("/api/stream/1/parts/{n}?sid={sid}{q}");
            let h: Vec<(&str, &str)> = if range.is_empty() {
                vec![]
            } else {
                vec![("range", range)]
            };
            e.get(&path, cred, &h).await
        }
    };
    let r = part(1, "alice-jwt", "", "bytes=2-4").await;
    assert_eq!((r.status, r.text().as_str()), (206, "234"));
    assert_eq!(r.header("content-type").as_deref(), Some("audio/mpeg"));
    assert_eq!(part(1, "alice-jwt", "", "bytes=99-").await.status, 416);
    assert_eq!(part(2, "alice-jwt", "", "").await.status, 404);
    // Another user's credential on alice's session.
    assert_eq!(part(0, "bob-jwt", "", "").await.status, 403);
    let r = part(0, "alice-jwt", "&download=1", "").await;
    assert_eq!(r.status, 200);
    assert!(
        r.header("content-disposition")
            .is_some_and(|d| d.contains("a.mp3")),
        "{:?}",
        r.headers
    );
    // An unseen credential while the host is down: the session's own parts answer.
    e.stub.grant("alice2-jwt", "alice");
    e.stub.set_down(true);
    let r = part(1, "alice2-jwt", "", "bytes=0-1").await;
    assert_eq!((r.status, r.text().as_str()), (206, "01"));
}

#[tokio::test]
async fn a_key_change_runs_the_rubberband_filter_on_the_ffmpeg_that_has_it() {
    let paths = vec!["/books/p0.mp3".to_string()];
    // The pitch ffmpeg is another binary: its run is the one that logs the argv.
    let e = engine_of(&paths).await;
    let r = open_pitched(&e, &paths, 2.0).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["decision"]["response"]["playMethod"], "Transcode");
    let sid = r.json()["sessionId"].as_str().unwrap().to_string();
    let path = format!("/api/stream/1/item/seg-0000.ts?sid={sid}");
    // No ffmpeg has the filter: the run is never started.
    e.rt.karaoke.set_tools(fliks_engine::karaoke::Tools {
        whisper: None,
        pitch_ffmpeg: None,
        whisper_backend: "cpu",
    });
    let r = e.get(&path, "alice-jwt", &[]).await;
    assert_ne!(r.status, 200, "{}", r.text());

    let e = engine_of(&paths).await;
    e.rt.karaoke.set_tools(fliks_engine::karaoke::Tools {
        whisper: None,
        pitch_ffmpeg: Some(fake("item")),
        whisper_backend: "cpu",
    });
    let r = open_pitched(&e, &paths, 2.0).await;
    let sid = r.json()["sessionId"].as_str().unwrap().to_string();
    let it = Item { e, sid };
    assert_eq!(it.get("seg-0000.ts").await.status, 200);
    let runs = it.runs(0);
    assert_eq!(runs.len(), 1);
    let filter = arg(&runs[0], "-filter:a");
    assert!(filter.contains("rubberband=pitch=1.122462"), "{filter}");
    assert!(!runs[0].contains(&"libfdk_aac".to_string()));
}

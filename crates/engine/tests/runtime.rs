//! Runtime without ffmpeg: resolver gating, sid rules, playlists, sessions, revocation, direct play.
mod runtime_common;
use runtime_common::*;
use serde_json::{Value, json};

fn info() -> Value {
    json!({
        "video": [{ "streamIndex": 0, "codec": "h264", "profile": "High", "level": 31, "width": 1280,
            "height": 720, "frameRate": "24/1", "avgFrameRate": "24/1", "pixelFormat": "yuv420p",
            "bitDepth": 8, "startTimeSeconds": 0.0, "firstFrameSeconds": 0.0 }],
        "audio": [{ "streamIndex": 1, "codec": "aac", "language": "eng", "channels": 2, "sampleRate": 48000 }],
        "subtitles": [{ "streamIndex": 2, "codec": "subrip", "language": "fra", "title": "French",
            "forced": false, "hearingImpaired": false, "isImageBased": false }],
        "formatName": "matroska,webm", "formatStartSeconds": 0.0, "durationSeconds": 10.0, "schemaVersion": 1
    })
}

async fn setup() -> (Engine, std::path::PathBuf) {
    let e = engine().await;
    let dir = temp_dir("media");
    let file = dir.join("movie.mkv");
    std::fs::write(
        &file,
        (0..10_000u32).map(|i| (i % 251) as u8).collect::<Vec<_>>(),
    )
    .unwrap();
    e.stub.file(7, &file);
    e.stub.grant("alice-jwt", "alice");
    e.stub.grant("bob-jwt", "bob");
    (e, file)
}

async fn open(e: &Engine, file: &std::path::Path, owner: &str) -> Value {
    let info = serde_json::from_value(info()).unwrap();
    e.open(7, file, &info, owner, json!({ "tags": { "conn": "c1" } }))
        .await
}

#[tokio::test]
async fn direct_play_serves_byte_ranges() {
    let (e, file) = setup().await;
    let bytes = std::fs::read(&file).unwrap();
    let full = e.get("/api/stream/7", "alice-jwt", &[]).await;
    assert_eq!((full.status, full.body.len()), (200, 10_000));
    assert_eq!(full.header("accept-ranges").as_deref(), Some("bytes"));
    assert_eq!(
        full.header("content-type").as_deref(),
        Some("video/x-matroska")
    );
    let r = e
        .get("/api/stream/7", "alice-jwt", &[("range", "bytes=100-199")])
        .await;
    assert_eq!(r.status, 206);
    assert_eq!(
        r.header("content-range").as_deref(),
        Some("bytes 100-199/10000")
    );
    assert_eq!(&r.body[..], &bytes[100..200]);
    let tail = e
        .get("/api/stream/7", "alice-jwt", &[("range", "bytes=-10")])
        .await;
    assert_eq!(&tail.body[..], &bytes[9990..]);
    let bad = e
        .get("/api/stream/7", "alice-jwt", &[("range", "bytes=20000-")])
        .await;
    assert_eq!(bad.status, 416);
    assert_eq!(
        bad.header("content-range").as_deref(),
        Some("bytes */10000")
    );
    let dl = e.get("/api/stream/7?download=1", "alice-jwt", &[]).await;
    assert!(
        dl.header("content-disposition")
            .unwrap()
            .starts_with("attachment; filename=\"movie.mkv\"")
    );
    let purposes: Vec<_> = e
        .stub
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|c| c.purpose)
        .collect();
    assert!(purposes.contains(&fliks_engine_core::api::ResolvePurpose::Download));
    let head = http(
        "HEAD",
        &format!("{}/api/stream/7?token=alice-jwt", e.base),
        &[],
        None,
    )
    .await;
    assert_eq!((head.status, head.body.len()), (200, 0));
    assert_eq!(
        full.header("access-control-allow-origin").as_deref(),
        Some("*")
    );
}

#[tokio::test]
async fn resolver_gates_caches_and_falls_back() {
    let (e, file) = setup().await;
    assert_eq!(e.get("/api/stream/7", "mallory", &[]).await.status, 403);
    assert_eq!(e.get("/api/stream/8", "alice-jwt", &[]).await.status, 403);
    let before = e.stub.calls();
    e.get("/api/stream/7", "alice-jwt", &[("range", "bytes=0-0")])
        .await;
    e.get("/api/stream/7", "alice-jwt", &[("range", "bytes=0-0")])
        .await;
    assert_eq!(
        e.stub.calls(),
        before + 1,
        "the second answer comes from the cache"
    );
    // The credential forms reach the host verbatim.
    let r = http(
        "GET",
        &format!("{}/api/stream/7", e.base),
        &[("authorization", "Bearer x")],
        None,
    )
    .await;
    assert_eq!(r.status, 403);
    assert_eq!(
        e.stub.calls.lock().unwrap().last().unwrap().credential,
        "Bearer x"
    );
    // The host's extractor order: the cookie wins over a bearer and a `?token=`.
    let r = http(
        "GET",
        &format!("{}/api/stream/7?token=alice-jwt", e.base),
        &[("authorization", "Bearer x"), ("cookie", "access=y")],
        None,
    )
    .await;
    assert_eq!(r.status, 403);
    assert_eq!(
        e.stub.calls.lock().unwrap().last().unwrap().credential,
        "access=y"
    );
    // Host down: a cached grant keeps serving, a new credential gets 503.
    e.stub.set_down(true);
    assert_eq!(e.get("/api/stream/7", "alice-jwt", &[]).await.status, 200);
    let down = e.get("/api/stream/7", "bob-jwt", &[]).await;
    assert_eq!(down.status, 503);
    assert_eq!(down.header("retry-after").as_deref(), Some("2"));
    // ...but an existing session of that file keeps playing.
    e.stub.set_down(false);
    let s = open(&e, &file, "bob").await;
    let sid = s["sessionId"].as_str().unwrap().to_string();
    e.stub.set_down(true);
    let m = e
        .get(
            &format!("/api/stream/7/master.m3u8?sid={sid}"),
            "bob-jwt2",
            &[],
        )
        .await;
    assert_eq!(m.status, 200, "{}", m.text());
}

#[tokio::test]
async fn sessions_playlists_and_sid_rules() {
    let (e, file) = setup().await;
    let mut events = e.rt.events.subscribe();
    let s = open(&e, &file, "alice").await;
    let sid = s["sessionId"].as_str().unwrap().to_string();
    assert_eq!(s["decision"]["response"]["playMethod"], "Transcode");
    assert_eq!(
        s["playPath"],
        format!("/api/stream/7/master.m3u8?token=t&sid={sid}")
    );
    assert!(s["profileHash"].as_str().unwrap().len() >= 16);
    assert!(matches!(
        events.recv().await.unwrap(),
        fliks_engine_core::api::EngineEvent::SessionOpened { .. }
    ));

    let list = e
        .control("GET", "/v1/sessions?ownerId=alice", None)
        .await
        .json();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["tags"]["conn"], "c1");
    assert_eq!(list[0]["kind"], "transcode");
    assert!(list[0].get("job").is_none());

    let m = e
        .get(
            &format!("/api/stream/7/master.m3u8?sid={sid}"),
            "alice-jwt",
            &[],
        )
        .await;
    assert_eq!(m.status, 200);
    assert_eq!(
        m.header("content-type").as_deref(),
        Some("application/vnd.apple.mpegurl")
    );
    assert_eq!(m.header("cache-control").as_deref(), Some("no-store"));
    let text = m.text();
    assert!(
        text.contains(&format!(
            "/api/stream/7/720p/index.m3u8?token=alice-jwt&sid={sid}"
        )),
        "{text}"
    );
    let v = e
        .get(
            &format!("/api/stream/7/720p/index.m3u8?sid={sid}"),
            "alice-jwt",
            &[],
        )
        .await;
    let v = v.text();
    assert!(
        v.contains(&format!(
            "#EXT-X-MAP:URI=\"/api/stream/7/720p/init.mp4?token=alice-jwt&sid={sid}\""
        )),
        "{v}"
    );
    assert_eq!(v.matches("#EXTINF").count(), 4);
    assert_eq!(
        e.get(
            &format!("/api/stream/7/999p/index.m3u8?sid={sid}"),
            "alice-jwt",
            &[]
        )
        .await
        .status,
        400
    );
    let sub = e
        .get(
            &format!("/api/stream/7/subtitles/embedded/2/index.m3u8?sid={sid}"),
            "alice-jwt",
            &[],
        )
        .await;
    assert!(sub.text().contains(&format!(
        "/api/stream/7/subtitles/embedded/2?token=alice-jwt&sid={sid}"
    )));

    // Unknown sid 410 with the typed body; another owner's sid 403; no session at all 410.
    let gone = e
        .get("/api/stream/7/master.m3u8?sid=nope", "alice-jwt", &[])
        .await;
    assert_eq!(gone.status, 410);
    assert_eq!(gone.json()["code"], "session_expired");
    assert_eq!(gone.json()["sid"], "nope");
    assert_eq!(
        e.get(
            &format!("/api/stream/7/master.m3u8?sid={sid}"),
            "bob-jwt",
            &[]
        )
        .await
        .status,
        403
    );
    assert_eq!(
        e.get("/api/stream/7/master.m3u8", "bob-jwt", &[])
            .await
            .status,
        410
    );
    // Without a sid the owner's latest session answers.
    assert_eq!(
        e.get("/api/stream/7/master.m3u8", "alice-jwt", &[])
            .await
            .status,
        200
    );

    let hb =
        |owner: &str, pos: f64| json!({ "ownerId": owner, "positionS": pos, "state": "playing" });
    let r = e
        .control(
            "POST",
            &format!("/v1/sessions/{sid}/heartbeat"),
            Some(hb("alice", 12.5)),
        )
        .await;
    assert_eq!(r.json()["sessionLost"], false);
    assert_eq!(
        e.control(
            "POST",
            &format!("/v1/sessions/{sid}/heartbeat"),
            Some(hb("bob", 1.0))
        )
        .await
        .status,
        403
    );
    let one = e
        .control("GET", &format!("/v1/sessions/{sid}"), None)
        .await
        .json();
    assert_eq!(
        (one["positionS"].as_f64(), one["state"].as_str()),
        (Some(12.5), Some("playing"))
    );
    let cmd = e
        .control(
            "POST",
            &format!("/v1/sessions/{sid}/command"),
            Some(json!({ "action": "pauseJob" })),
        )
        .await;
    assert_eq!(cmd.status, 409, "no job runs");

    assert_eq!(
        e.control("DELETE", &format!("/v1/sessions/{sid}"), None)
            .await
            .status,
        204
    );
    assert_eq!(
        e.control("DELETE", &format!("/v1/sessions/{sid}"), None)
            .await
            .status,
        204
    );
    let r = e
        .control(
            "POST",
            &format!("/v1/sessions/{sid}/heartbeat"),
            Some(hb("alice", 1.0)),
        )
        .await;
    assert_eq!(r.json()["sessionLost"], true);
    assert_eq!(
        e.control("GET", &format!("/v1/sessions/{sid}"), None)
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn revoke_ends_sessions_and_purges_the_resolver_cache() {
    let (e, file) = setup().await;
    let a = open(&e, &file, "alice").await;
    let b = open(&e, &file, "bob").await;
    let (sa, sb) = (
        a["sessionId"].as_str().unwrap(),
        b["sessionId"].as_str().unwrap(),
    );
    assert_eq!(
        e.get(
            &format!("/api/stream/7/master.m3u8?sid={sa}"),
            "alice-jwt",
            &[]
        )
        .await
        .status,
        200
    );
    assert_eq!(
        e.control("POST", "/v1/revoke", Some(json!({})))
            .await
            .status,
        400
    );
    let r = e
        .control("POST", "/v1/revoke", Some(json!({ "ownerId": "alice" })))
        .await;
    assert_eq!(r.json()["sessions"], 1);
    // The revoked owner's grant was dropped: the next request asks the host again.
    let before = e.stub.calls();
    let gone = e
        .get(
            &format!("/api/stream/7/master.m3u8?sid={sa}"),
            "alice-jwt",
            &[],
        )
        .await;
    assert_eq!(gone.status, 410);
    assert_eq!(e.stub.calls(), before + 1);
    assert_eq!(
        e.get(
            &format!("/api/stream/7/master.m3u8?sid={sb}"),
            "bob-jwt",
            &[]
        )
        .await
        .status,
        200
    );
    let r = e
        .control("POST", "/v1/revoke", Some(json!({ "mediaFileIds": [7] })))
        .await;
    assert_eq!(r.json()["sessions"], 1);
}

#[tokio::test]
async fn hello_lists_the_runtime_and_events_stream() {
    let (e, file) = setup().await;
    let hello = e.control("GET", "/v1/hello", None).await.json();
    for f in ["sessions", "serve", "events", "revoke"] {
        assert!(
            hello["features"].as_array().unwrap().iter().any(|x| x == f),
            "{f}"
        );
    }
    // SSE: open the stream, then cause an event.
    use http_body_util::BodyExt;
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build_http::<http_body_util::Empty<axum::body::Bytes>>();
    let req = axum::http::Request::get(format!("{}/v1/events", e.base))
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(http_body_util::Empty::new())
        .unwrap();
    let res = client.request(req).await.unwrap();
    assert_eq!(res.headers()["content-type"], "text/event-stream");
    let mut body = res.into_body();
    open(&e, &file, "alice").await;
    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let text = String::from_utf8_lossy(frame.data_ref().unwrap()).into_owned();
    assert!(
        text.starts_with("event: SessionOpened\ndata: {\"type\":\"SessionOpened\""),
        "{text}"
    );
    assert_eq!(
        e.control("GET", "/v1/cache", None).await.json()["usedBytes"],
        0.0
    );
}

#[tokio::test]
async fn the_gateway_serves_the_data_plane_and_forwards_the_rest() {
    use fliks_engine::gateway::{RouteTable, Upstream};
    let (e, _) = setup().await;
    let up = Upstream::parse("http://127.0.0.1:9").unwrap();
    let gw = listen(fliks_engine::gateway::router(
        RouteTable::fliks(),
        up,
        Some(fliks_engine::serve::router(e.rt.clone())),
    ))
    .await;
    let r = http(
        "GET",
        &format!("{gw}/api/stream/7?token=alice-jwt"),
        &[("range", "bytes=0-9")],
        None,
    )
    .await;
    assert_eq!(r.status, 206);
    // The control plane is never public: `/v1/*` is forwarded (the upstream is down here).
    let v1 = http(
        "GET",
        &format!("{gw}/v1/hello"),
        &[("authorization", &format!("Bearer {TOKEN}"))],
        None,
    )
    .await;
    assert_eq!(v1.status, 502);
}

#[tokio::test]
async fn invalidate_and_cache_delete_remove_entries() {
    let (e, file) = setup().await;
    let mk = |owner: &str, id: u32, hash: &str| {
        let d = e.rt.cache.rung_dir(Some(owner), id, hash, "720p");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("seg-0000.m4s"), [1u8; 100]).unwrap();
    };
    mk("alice", 7, "aaaa");
    mk("bob", 7, "bbbb");
    mk("alice", 8, "cccc");
    mk("alice", 9, "dddd");
    // A session makes its own entry live: never deleted.
    let s = open(&e, &file, "alice").await;
    mk("alice", 7, s["profileHash"].as_str().unwrap());
    let c = e.control("GET", "/v1/cache", None).await.json();
    assert_eq!(c["entries"].as_array().unwrap().len(), 5);
    assert_eq!(c["usedBytes"], 500.0);
    assert_eq!(
        c["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["live"] == true)
            .count(),
        1
    );
    let d = e
        .control("DELETE", "/v1/cache?mediaFileId=8", None)
        .await
        .json();
    assert_eq!(d["removed"], 1);
    let d = e
        .control("DELETE", "/v1/cache?ownerId=alice&mediaFileId=7", None)
        .await
        .json();
    assert_eq!(d["removed"], 1, "the live entry stays");
    let i = e
        .control("POST", "/v1/invalidate", Some(json!({ "mediaFileId": 7 })))
        .await
        .json();
    assert_eq!(
        (i["jobs"].as_u64(), i["cacheEntries"].as_u64()),
        (Some(0), Some(2))
    );
    let left = e.control("GET", "/v1/cache", None).await.json();
    assert_eq!(left["entries"].as_array().unwrap().len(), 1);
    assert_eq!(left["entries"][0]["mediaFileId"], 9);
    // Sessions survive an invalidation.
    assert_eq!(
        e.control("GET", "/v1/sessions", None)
            .await
            .json()
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn dolby_vision_remux_carries_its_codec_strings() {
    let (e, file) = setup().await;
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/decide");
    for (name, codecs, supplemental) in [
        (
            "safari-tvos-dv-p81-mp4-cpu-auto",
            None,
            Some("dvh1.08.06/db1p"),
        ),
        ("safari-tvos-dv-p5-mp4-cpu-auto", Some("dvh1.05.06"), None),
    ] {
        let fx: Value = serde_json::from_str(
            &std::fs::read_to_string(fixtures.join(format!("{name}.json"))).unwrap(),
        )
        .unwrap();
        let mut decide = fx["input"].clone();
        decide["media"]["mediaFileId"] = json!(7);
        decide["media"]["path"] = json!(file);
        let r = e
            .control(
                "POST",
                "/v1/sessions",
                Some(json!({ "decide": decide, "ownerId": "alice" })),
            )
            .await;
        assert_eq!(r.status, 200, "{}", r.text());
        let s = r.json();
        assert_eq!(
            s["decision"]["response"]["playMethod"], "DirectStream",
            "{name}"
        );
        assert_eq!(s["decision"]["response"]["dolbyVision"], true, "{name}");
        let sid = s["sessionId"].as_str().unwrap();
        let m = e
            .get(
                &format!("/api/stream/7/master.m3u8?sid={sid}&remux=1"),
                "alice-jwt",
                &[],
            )
            .await
            .text();
        let line = m
            .lines()
            .find(|l| l.contains("NAME=\"remux\""))
            .unwrap_or_default();
        match supplemental {
            Some(sup) => assert!(
                line.contains(&format!("SUPPLEMENTAL-CODECS=\"{sup}\"")) && line.contains("hvc1"),
                "{name}: {line}"
            ),
            None => assert!(!line.contains("SUPPLEMENTAL-CODECS"), "{name}: {line}"),
        }
        if let Some(c) = codecs {
            assert!(
                line.contains(&format!("CODECS=\"{c}")) && line.contains("VIDEO-RANGE=PQ"),
                "{name}: {line}"
            );
        }
    }
}

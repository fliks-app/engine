//! Live TV against the `fake-ffmpeg` test binary in `live` mode (no real ffmpeg): it stands in for
//! ffprobe and writes segments and a playlist whose behaviour the upstream URL picks (`fail`,
//! `dies`, `stall`, `hevc`, `noprobe`). Spec: `docs/spec/live.md`.
mod runtime_common;
use axum::{Router, body::Body, routing::get};
use fliks_engine_core::api::{EngineEvent, HwAccel};
use http_body_util::BodyExt;
use runtime_common::*;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

async fn live_engine(lifetime: Value, livetv: Value) -> Engine {
    let fake = fake("live");
    let e = engine_with_tools(
        host(HwAccel::None, "/dev/dri/renderD128", "signal"),
        &fake,
        &fake,
        &fake,
        lifetime,
    )
    .await;
    let mut config = e.control("GET", "/v1/config", None).await.json();
    config["revision"] = json!(2);
    config["livetv"] = livetv;
    assert_eq!(
        e.control("PUT", "/v1/config", Some(config)).await.status,
        200
    );
    e.stub.grant("alice-jwt", "alice");
    e.stub.grant("bob-jwt", "bob");
    e
}

fn upstream(url: &str, priority: i32) -> Value {
    json!({ "url": url, "priority": priority, "probedVideoCodec": "h264", "probedContainer": "mpegts" })
}

fn open_req(channel: &str, upstreams: Vec<Value>, caps: Value, owner: &str, token: &str) -> Value {
    json!({ "channelId": channel, "upstreams": upstreams, "caps": caps, "ownerId": owner,
        "capacityToken": token, "tags": { "device": "tv" } })
}

async fn open(e: &Engine, body: Value) -> Value {
    let r = e.control("POST", "/v1/live", Some(body)).await;
    assert_eq!(r.status, 200, "{}", r.text());
    r.json()
}

fn sid(v: &Value) -> String {
    v["sessionId"].as_str().unwrap().into()
}

fn live_dirs(e: &Engine) -> Vec<PathBuf> {
    std::fs::read_dir(e.root.join("live"))
        .map(|d| d.map(|e| e.unwrap().path()).collect())
        .unwrap_or_default()
}

/// The argv of every run the fake ffmpeg saw in the one live directory.
fn runs(e: &Engine) -> Vec<Vec<String>> {
    let dirs = live_dirs(e);
    assert_eq!(dirs.len(), 1, "{dirs:?}");
    std::fs::read_to_string(dirs[0].join("argv.log"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn attempt(e: &EngineEvent) -> Option<(u32, Option<&str>)> {
    match e {
        EngineEvent::LiveAttempt {
            upstream, error, ..
        } => Some((*upstream, error.as_deref())),
        _ => None,
    }
}

#[tokio::test]
async fn remux_window_is_served_and_shared() {
    let e = live_engine(json!({}), json!({ "channelIdleSeconds": 0 })).await;
    let mut rx = e.rt.events.subscribe();
    let a = open(
        &e,
        open_req(
            "1",
            vec![upstream("http://up/a", 0)],
            json!({}),
            "alice",
            "tok-1",
        ),
    )
    .await;
    let s = sid(&a);
    assert_eq!(a["mode"], "remux");
    assert_eq!(
        a["playPath"],
        format!("/api/livetv/sessions/{s}/index.m3u8")
    );
    assert_eq!(
        (a["upstream"].clone(), a["capacityToken"].clone()),
        (json!(0), json!("tok-1"))
    );
    assert_eq!(
        (a["dvrWindowSeconds"].clone(), a["segmentSeconds"].clone()),
        (json!(900.0), json!(2.0))
    );
    assert_eq!(a["qoePipeline"]["source"]["codec"], "h264");
    assert!(a["qoePipeline"]["encode"].is_null());

    let r = e
        .get(
            &format!("/api/livetv/sessions/{s}/index.m3u8"),
            "alice-jwt",
            &[],
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(
        r.header("content-type").as_deref(),
        Some("application/vnd.apple.mpegurl")
    );
    assert_eq!(r.header("cache-control").as_deref(), Some("no-store"));
    assert_eq!(
        r.header("access-control-allow-origin").as_deref(),
        Some("*")
    );
    assert!(
        r.text()
            .contains("#EXT-X-MAP:URI=\"init.mp4?token=alice-jwt\""),
        "{}",
        r.text()
    );
    assert!(
        r.text().contains("\nseg-00000.m4s?token=alice-jwt\n"),
        "{}",
        r.text()
    );
    let call = e.stub.calls.lock().unwrap().last().cloned().unwrap();
    assert_eq!(serde_json::to_value(&call).unwrap()["mediaId"], json!(s));
    assert_eq!(serde_json::to_value(&call).unwrap()["purpose"], "live");

    let seg = |name: &str| format!("/api/livetv/sessions/{s}/{name}");
    let r = e.get(&seg("init.mp4"), "alice-jwt", &[]).await;
    assert_eq!(
        (r.status, r.header("content-type").as_deref()),
        (200, Some("video/mp4"))
    );
    let r = e.get(&seg("seg-00000.m4s"), "alice-jwt", &[]).await;
    assert_eq!((r.status, r.text().as_str()), (200, "segment 0"));
    assert_eq!(
        r.header("cache-control").as_deref(),
        Some("public, max-age=0")
    );
    let r = e
        .get(
            &seg("seg-00000.m4s"),
            "alice-jwt",
            &[("range", "bytes=0-6")],
        )
        .await;
    assert_eq!((r.status, r.text().as_str()), (206, "segment"));
    let r = e.get(&seg("seg-0.m4s"), "alice-jwt", &[]).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["message"], "Invalid segment name: seg-0.m4s");
    let r = e.get(&seg("seg-99999.m4s"), "alice-jwt", &[]).await;
    assert_eq!(
        r.json(),
        json!({ "statusCode": 404, "message": "Live TV segment not found", "error": "Not Found" })
    );
    let r = e
        .get("/api/livetv/sessions/nope/index.m3u8", "alice-jwt", &[])
        .await;
    assert_eq!(
        r.json(),
        json!({ "statusCode": 404, "message": "Live TV session not found", "error": "Not Found" })
    );
    let calls = e.stub.calls();
    e.get("/api/livetv/sessions/nope/seg-00000.m4s", "alice-jwt", &[])
        .await;
    assert_eq!(e.stub.calls(), calls, "an unknown sid asks no resolver");
    // Another user holding the sid.
    let r = e
        .get(
            &format!("/api/livetv/sessions/{s}/index.m3u8"),
            "bob-jwt",
            &[],
        )
        .await;
    assert_eq!(r.status, 403);

    // A second viewer joins: no new upstream, the first open's hold.
    let b = open(
        &e,
        open_req(
            "1",
            vec![upstream("http://up/a", 0)],
            json!({}),
            "bob",
            "tok-2",
        ),
    )
    .await;
    assert_eq!(b["capacityToken"], "tok-1");
    assert_eq!(runs(&e).len(), 1);
    let list = e.control("GET", "/v1/live", None).await.json();
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert_eq!(list[0]["viewersOnUpstream"], 2);
    assert_eq!(list[0]["container"], "fmp4");
    assert_eq!(list[0]["tags"]["device"], "tv");
    let bob = e.control("GET", "/v1/live?ownerId=bob", None).await.json();
    assert_eq!(bob[0]["sessionId"], b["sessionId"]);
    let one = e.control("GET", &format!("/v1/live/{s}"), None).await;
    assert_eq!(
        (one.status, one.json()["ownerId"].clone()),
        (200, json!("alice"))
    );

    // Another container is another upstream.
    let ts = open(
        &e,
        open_req(
            "1",
            vec![upstream("http://up/a", 0)],
            json!({ "useTs": true }),
            "bob",
            "tok-3",
        ),
    )
    .await;
    assert_eq!(ts["capacityToken"], "tok-3");
    assert_eq!(
        e.control("DELETE", &format!("/v1/sessions/{}", sid(&ts)), None)
            .await
            .status,
        204
    );
    event(&mut rx, 30, |ev| matches!(ev, EngineEvent::LiveReleased { capacity_token, .. } if capacity_token == "tok-3")).await;

    assert_eq!(
        e.control("DELETE", &format!("/v1/sessions/{s}"), None)
            .await
            .status,
        204
    );
    let ended = event(&mut rx, 30, |ev| {
        matches!(ev, EngineEvent::SessionEnded { .. })
    })
    .await;
    assert_eq!(
        ended,
        EngineEvent::SessionEnded {
            session_id: s.clone(),
            reason: "stopped".into()
        }
    );
    assert_eq!(e.ffmpeg_pids().len(), 1, "bob still watches");
    let bs = sid(&b);
    assert_eq!(
        e.control("DELETE", &format!("/v1/sessions/{bs}"), None)
            .await
            .status,
        204
    );
    let rel = event(&mut rx, 30, |ev| {
        matches!(ev, EngineEvent::LiveReleased { .. })
    })
    .await;
    assert_eq!(
        rel,
        EngineEvent::LiveReleased {
            capacity_token: "tok-1".into(),
            reason: "idle".into()
        }
    );
    until("ffmpeg gone", 30, || e.ffmpeg_pids().is_empty()).await;
    assert!(live_dirs(&e).is_empty());
    assert_eq!(
        e.control("DELETE", &format!("/v1/sessions/{bs}"), None)
            .await
            .status,
        204
    );
    assert_eq!(
        e.get(
            &format!("/api/livetv/sessions/{bs}/index.m3u8"),
            "bob-jwt",
            &[]
        )
        .await
        .status,
        404
    );
}

#[tokio::test]
async fn failover_by_priority_at_start_and_on_a_running_exit() {
    let e = live_engine(json!({}), json!({})).await;
    let mut rx = e.rt.events.subscribe();
    let ups = vec![
        upstream("http://up/ok", 2),
        upstream("http://up/fail", 0),
        upstream("http://up/dies", 1),
    ];
    let a = open(&e, open_req("9", ups, json!({}), "alice", "tok")).await;
    assert_eq!(a["upstream"], 2);
    let held = |s: Value| {
        (
            s["mode"].clone(),
            s["capacityToken"].clone(),
            s["upstreamUrl"].clone(),
            s["upstreamPriority"].clone(),
        )
    };
    let before = e
        .control("GET", &format!("/v1/live/{}", sid(&a)), None)
        .await
        .json();
    assert_eq!(
        held(before),
        (
            json!("remux"),
            json!("tok"),
            json!("http://up/dies"),
            json!(1)
        )
    );
    // `dies` holds its three segments until told: the snapshot above is of its run.
    std::fs::write(live_dirs(&e)[0].join("die"), b"").unwrap();
    let e1 = event(&mut rx, 30, |ev| attempt(ev).is_some()).await;
    assert_eq!(
        attempt(&e1),
        Some((1, Some("no segment produced within the startup window")))
    );
    assert!(
        matches!(&e1, EngineEvent::LiveAttempt { upstream_url: u, upstream_priority: 0, .. } if u == "http://up/fail")
    );
    assert_eq!(
        attempt(&event(&mut rx, 30, |ev| attempt(ev).is_some()).await),
        Some((2, None))
    );
    // `dies` exits after three segments: the next upstream picks up at once.
    let failed = event(&mut rx, 60, |ev| attempt(ev).is_some()).await;
    let (up, err) = attempt(&failed).unwrap();
    assert_eq!(up, 2);
    assert!(
        err.unwrap()
            .starts_with("exited (code=1): fake ffmpeg: http://up/dies: end of file"),
        "{err:?}"
    );
    assert_eq!(
        attempt(&event(&mut rx, 60, |ev| attempt(ev).is_some()).await),
        Some((0, None))
    );
    // Old segments count as the first one (the TS's check): the run may not have started yet.
    until("the third run", 30, || runs(&e).len() == 3).await;
    let runs = runs(&e);
    let last = runs.last().unwrap();
    assert!(last.contains(&"http://up/ok".to_string()));
    assert!(
        last.iter()
            .any(|a| a.ends_with("+append_list+discont_start")),
        "{last:?}"
    );
    let snap = e
        .control("GET", &format!("/v1/live/{}", sid(&a)), None)
        .await
        .json();
    assert_eq!(
        (snap["upstream"].clone(), snap["restarts"].clone()),
        (json!(0), json!(2))
    );
    assert_eq!(
        held(snap),
        (
            json!("remux"),
            json!("tok"),
            json!("http://up/ok"),
            json!(2)
        )
    );
    // A run counts as started when its first segment exists: the playlist follows it.
    let e = &e;
    let uri = format!("/api/livetv/sessions/{}/index.m3u8", sid(&a));
    until_async("numbering continues", 30, || async {
        e.get(&uri, "alice-jwt", &[])
            .await
            .text()
            .contains("seg-00003.m4s")
    })
    .await;
}

#[tokio::test]
async fn every_upstream_failing_answers_503_and_releases() {
    let e = live_engine(json!({}), json!({})).await;
    let mut rx = e.rt.events.subscribe();
    let r = e
        .control(
            "POST",
            "/v1/live",
            Some(open_req(
                "2",
                vec![upstream("http://up/fail", 0)],
                json!({}),
                "alice",
                "t",
            )),
        )
        .await;
    assert_eq!(
        (r.status, r.json()),
        (503, json!({ "code": "live_unavailable" }))
    );
    for _ in 0..2 {
        assert!(
            attempt(&event(&mut rx, 30, |ev| attempt(ev).is_some()).await)
                .unwrap()
                .1
                .is_some()
        );
    }
    let rel = event(&mut rx, 30, |ev| {
        matches!(ev, EngineEvent::LiveReleased { .. })
    })
    .await;
    assert_eq!(
        rel,
        EngineEvent::LiveReleased {
            capacity_token: "t".into(),
            reason: "failed".into()
        }
    );
    assert!(
        e.control("GET", "/v1/live", None)
            .await
            .json()
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(live_dirs(&e).is_empty());
}

#[tokio::test]
async fn capacity_gates_only_a_new_upstream() {
    let e = live_engine(json!({}), json!({})).await;
    let mut full = open_req(
        "3",
        vec![upstream("http://up/a", 0)],
        json!({}),
        "alice",
        "t1",
    );
    full["atCapacity"] = json!(true);
    let r = e.control("POST", "/v1/live", Some(full.clone())).await;
    assert_eq!(
        (r.status, r.json()),
        (409, json!({ "code": "live_at_capacity" }))
    );
    assert!(live_dirs(&e).is_empty());
    open(
        &e,
        open_req(
            "3",
            vec![upstream("http://up/a", 0)],
            json!({}),
            "alice",
            "t2",
        ),
    )
    .await;
    full["ownerId"] = json!("bob");
    let joined = open(&e, full).await;
    assert_eq!(joined["capacityToken"], "t2");
}

#[tokio::test]
async fn a_vetoed_upstream_is_never_used() {
    let e = live_engine(json!({}), json!({})).await;
    let veto = |url: &str, priority| {
        let mut u = upstream(url, priority);
        u["atCapacity"] = json!(true);
        u
    };
    // The primary is vetoed: the next upstream feeds the session.
    let r = open(
        &e,
        open_req(
            "8",
            vec![veto("http://up/a", 0), upstream("http://up/b", 1)],
            json!({}),
            "alice",
            "t1",
        ),
    )
    .await;
    assert_eq!(r["upstream"], 1);
    // Every upstream vetoed: 409 for a new session, a running one is still joined.
    let all = open_req(
        "9",
        vec![veto("http://up/a", 0), veto("http://up/b", 1)],
        json!({}),
        "alice",
        "t2",
    );
    let r = e.control("POST", "/v1/live", Some(all.clone())).await;
    assert_eq!(
        (r.status, r.json()),
        (409, json!({ "code": "live_at_capacity" }))
    );
    open(
        &e,
        open_req(
            "9",
            vec![upstream("http://up/a", 0)],
            json!({}),
            "alice",
            "t3",
        ),
    )
    .await;
    assert_eq!(open(&e, all).await["capacityToken"], "t3");
    // A failing upstream never fails over onto a vetoed one.
    let mut rx = e.rt.events.subscribe();
    let r = e
        .control(
            "POST",
            "/v1/live",
            Some(open_req(
                "10",
                vec![upstream("http://up/fail", 0), veto("http://up/b", 1)],
                json!({}),
                "alice",
                "t4",
            )),
        )
        .await;
    assert_eq!(r.status, 503);
    let mut failed0 = false;
    while let Ok(ev) = rx.try_recv() {
        if let Some((up, err)) = attempt(&ev) {
            assert_ne!(up, 1, "the vetoed upstream was attempted");
            failed0 |= up == 0 && err.is_some();
        }
    }
    assert!(failed0, "no failed attempt on upstream 0 was seen");
}

fn vetoed(url: &str, priority: i32) -> Value {
    let mut u = upstream(url, priority);
    u["atCapacity"] = json!(true);
    u
}

#[tokio::test]
async fn a_vetoed_primary_still_plans_direct_and_joins_a_running_session() {
    let e = live_engine(json!({}), json!({})).await;
    let (up, conns) = provider().await;
    let live = format!("{up}/live.ts");
    let direct = json!({ "directPlay": true });
    let a = open(
        &e,
        open_req(
            "11",
            vec![vetoed("http://127.0.0.1:1/x", 0), upstream(&live, 1)],
            direct.clone(),
            "alice",
            "t1",
        ),
    )
    .await;
    assert_eq!(
        (a["mode"].clone(), a["upstream"].clone()),
        (json!("direct"), json!(1))
    );
    // The vetoed primary's source feeds the running session: a new open joins it, no new hold.
    let b = open(
        &e,
        open_req(
            "11",
            vec![vetoed(&live, 0), upstream("http://127.0.0.1:1/x", 1)],
            direct,
            "bob",
            "t2",
        ),
    )
    .await;
    assert_eq!(b["capacityToken"], "t1");
    assert_eq!(conns.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn probe_picks_a_transcode_and_ts_segments() {
    let e = live_engine(json!({}), json!({})).await;
    let mut rx = e.rt.events.subscribe();
    let up =
        json!({ "url": "http://up/hevc", "headers": { "Referer": "http://site/", "X-Key": "k" } });
    let caps = json!({ "maxBitrateBps": 2500000, "useTs": true });
    let a = open(&e, open_req("4", vec![up], caps, "alice", "t")).await;
    assert_eq!(a["mode"], "transcode");
    assert_eq!(
        a["probe"],
        json!({ "videoCodec": "hevc", "audioCodec": "aac", "container": "mpegts" })
    );
    assert_eq!(a["qoePipeline"]["output"]["bitrate"], 3000000);
    assert_eq!(a["qoePipeline"]["encode"]["name"], "libx264");
    let ok = event(&mut rx, 30, |ev| attempt(ev).is_some()).await;
    assert!(
        matches!(ok, EngineEvent::LiveAttempt { error: None, probe: Some(ref p), .. } if p.video_codec.as_deref() == Some("hevc"))
    );
    let argv = &runs(&e)[0];
    let after = |f: &str| argv[argv.iter().position(|a| a == f).unwrap() + 1].clone();
    assert_eq!(
        (after("-c:v"), after("-b:v")),
        ("libx264".into(), "3000000".into())
    );
    assert_eq!(after("-headers"), "Referer: http://site/\r\nX-Key: k\r\n");
    assert_eq!(after("-user_agent"), "VLC/3.0.20 LibVLC/3.0.20");
    let r = e
        .get(
            &format!("/api/livetv/sessions/{}/seg-00000.ts", sid(&a)),
            "alice-jwt",
            &[],
        )
        .await;
    assert_eq!(
        (r.status, r.header("content-type").as_deref()),
        (200, Some("video/mp2t"))
    );
    let snap = e.control("GET", "/v1/live", None).await.json();
    assert_eq!(
        (snap[0]["container"].clone(), snap[0]["videoCodec"].clone()),
        (json!("mpegts"), json!("hevc"))
    );
}

#[tokio::test]
async fn a_stalled_upstream_fails_over_within_the_ts_window() {
    let e = live_engine(json!({}), json!({})).await;
    let mut rx = e.rt.events.subscribe();
    let ups = vec![upstream("http://up/stall", 0), upstream("http://up/ok", 1)];
    open(&e, open_req("5", ups, json!({}), "alice", "t")).await;
    assert_eq!(
        attempt(&event(&mut rx, 30, |ev| attempt(ev).is_some()).await),
        Some((0, None))
    );
    let t = std::time::Instant::now();
    let stalled = event(&mut rx, 60, |ev| attempt(ev).is_some()).await;
    assert_eq!(
        attempt(&stalled),
        Some((0, Some("no new segment for 15 s")))
    );
    assert!(t.elapsed() >= Duration::from_secs(14), "{:?}", t.elapsed());
    assert_eq!(
        attempt(&event(&mut rx, 60, |ev| attempt(ev).is_some()).await),
        Some((1, None))
    );
    until("the stalled run killed", 30, || e.ffmpeg_pids().len() == 1).await;
}

#[tokio::test]
async fn access_denial_and_revocation_end_viewers() {
    let e = live_engine(json!({}), json!({ "channelIdleSeconds": 0 })).await;
    let mut rx = e.rt.events.subscribe();
    let a = sid(&open(
        &e,
        open_req(
            "6",
            vec![upstream("http://up/a", 0)],
            json!({}),
            "alice",
            "t",
        ),
    )
    .await);
    open(
        &e,
        open_req(
            "6",
            vec![upstream("http://up/a", 0)],
            json!({}),
            "bob",
            "t2",
        ),
    )
    .await;
    let c = sid(&open(
        &e,
        open_req(
            "6",
            vec![upstream("http://up/a", 0)],
            json!({}),
            "alice",
            "t3",
        ),
    )
    .await);
    // Host down: a known viewer keeps watching.
    e.stub.set_down(true);
    assert_eq!(
        e.get(
            &format!("/api/livetv/sessions/{a}/index.m3u8"),
            "alice-jwt",
            &[]
        )
        .await
        .status,
        200
    );
    e.stub.set_down(false);
    e.stub.live_denied.lock().unwrap().push(a.clone());
    assert_eq!(
        e.get(
            &format!("/api/livetv/sessions/{a}/index.m3u8"),
            "alice-jwt",
            &[]
        )
        .await
        .status,
        403
    );
    let ended = event(&mut rx, 30, |ev| {
        matches!(ev, EngineEvent::SessionEnded { .. })
    })
    .await;
    assert_eq!(
        ended,
        EngineEvent::SessionEnded {
            session_id: a.clone(),
            reason: "revoked".into()
        }
    );
    assert_eq!(
        e.get(
            &format!("/api/livetv/sessions/{a}/index.m3u8"),
            "alice-jwt",
            &[]
        )
        .await
        .status,
        404
    );
    // A denial that is not for the viewer's owner (unknown credential, another user) only refuses.
    let url = format!("/api/livetv/sessions/{c}/index.m3u8");
    assert_eq!(e.get(&url, "garbage-jwt", &[]).await.status, 403);
    e.stub.live_denied.lock().unwrap().push(c.clone());
    assert_eq!(e.get(&url, "bob-jwt", &[]).await.status, 403);
    e.stub.live_denied.lock().unwrap().retain(|s| *s != c);
    assert_eq!(e.get(&url, "alice-jwt", &[]).await.status, 200);
    let r = e
        .control("POST", "/v1/revoke", Some(json!({ "ownerId": "alice" })))
        .await;
    assert_eq!(r.json()["sessions"], 1);
    let ended = event(&mut rx, 30, |ev| {
        matches!(ev, EngineEvent::SessionEnded { .. })
    })
    .await;
    assert_eq!(
        ended,
        EngineEvent::SessionEnded {
            session_id: c,
            reason: "revoked".into()
        }
    );
}

#[tokio::test]
async fn a_cached_owner_denial_ends_the_viewer_once() {
    let e = live_engine(json!({}), json!({})).await;
    let mut rx = e.rt.events.subscribe();
    *e.stub.live_ttl.lock().unwrap() = Some(60.0);
    let a = sid(&open(
        &e,
        open_req(
            "12",
            vec![upstream("http://up/a", 0)],
            json!({}),
            "alice",
            "t",
        ),
    )
    .await);
    let url = format!("/api/livetv/sessions/{a}/index.m3u8");
    e.stub.live_denied.lock().unwrap().push(a.clone());
    assert_eq!(e.get(&url, "alice-jwt", &[]).await.status, 403);
    let ended = event(&mut rx, 30, |ev| {
        matches!(ev, EngineEvent::SessionEnded { .. })
    })
    .await;
    assert_eq!(
        ended,
        EngineEvent::SessionEnded {
            session_id: a,
            reason: "revoked".into()
        }
    );
    // The viewer is gone: later requests are refused without ending anything again.
    assert_eq!(e.get(&url, "alice-jwt", &[]).await.status, 404);
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok())
            .any(|ev| matches!(ev, EngineEvent::SessionEnded { .. }))
    );
}

#[tokio::test]
async fn a_refused_request_does_not_keep_the_viewer() {
    let e = live_engine(
        json!({ "liveSessionTtlMs": 600 }),
        json!({ "channelIdleSeconds": 0 }),
    )
    .await;
    let mut rx = e.rt.events.subscribe();
    let a = sid(&open(
        &e,
        open_req(
            "13",
            vec![upstream("http://up/a", 0)],
            json!({}),
            "alice",
            "t",
        ),
    )
    .await);
    let url = format!("/api/livetv/sessions/{a}/index.m3u8");
    // Polled only with a refused credential: it still expires.
    let poll = async {
        loop {
            assert_eq!(e.get(&url, "garbage-jwt", &[]).await.status, 403);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    let ended = tokio::select! {
        ev = event(&mut rx, 30, |ev| matches!(ev, EngineEvent::SessionEnded { .. })) => ev,
        _ = poll => unreachable!(),
    };
    assert_eq!(
        ended,
        EngineEvent::SessionEnded {
            session_id: a,
            reason: "expired".into()
        }
    );
}

#[tokio::test]
async fn an_expired_viewer_is_dropped_then_the_idle_upstream_closes() {
    let e = live_engine(
        json!({ "liveSessionTtlMs": 600 }),
        json!({ "channelIdleSeconds": 0 }),
    )
    .await;
    let mut rx = e.rt.events.subscribe();
    let b = sid(&open(
        &e,
        open_req("6", vec![upstream("http://up/a", 0)], json!({}), "bob", "t"),
    )
    .await);
    // bob never fetches: expired after the TTL, then the idle upstream closes.
    let ended = event(&mut rx, 30, |ev| {
        matches!(ev, EngineEvent::SessionEnded { .. })
    })
    .await;
    assert_eq!(
        ended,
        EngineEvent::SessionEnded {
            session_id: b,
            reason: "expired".into()
        }
    );
    let rel = event(&mut rx, 30, |ev| {
        matches!(ev, EngineEvent::LiveReleased { .. })
    })
    .await;
    assert_eq!(
        rel,
        EngineEvent::LiveReleased {
            capacity_token: "t".into(),
            reason: "idle".into()
        }
    );
    until("ffmpeg gone", 30, || e.ffmpeg_pids().is_empty()).await;
}

#[tokio::test]
async fn bad_requests_are_400() {
    let e = live_engine(json!({}), json!({})).await;
    let mut bad = vec![
        open_req("7", vec![], json!({}), "alice", "t"),
        open_req(
            "7",
            vec![upstream("file:///etc/passwd", 0)],
            json!({}),
            "alice",
            "t",
        ),
        open_req(
            "7",
            vec![upstream("concat:http://a|http://b", 0)],
            json!({}),
            "alice",
            "t",
        ),
        open_req(
            "7",
            vec![json!({ "url": "http://a/", "headers": { "X": "a\r\nB: c" } })],
            json!({}),
            "alice",
            "t",
        ),
        open_req(
            "7",
            vec![upstream("http://a/", 0)],
            json!({}),
            "bad owner",
            "t",
        ),
    ];
    bad[0]["upstreams"] = json!([]);
    for b in bad {
        let r = e.control("POST", "/v1/live", Some(b.clone())).await;
        assert_eq!(r.status, 400, "{b}");
    }
    assert!(
        e.control("GET", "/v1/hello", None).await.json()["features"]
            .as_array()
            .unwrap()
            .contains(&json!("live"))
    );
}

/// An upstream server: `/live.ts` streams forever, `/sniff.ts` is a playlist sent as MPEG-TS,
/// `/list.m3u8` says it is a playlist.
async fn provider() -> (String, Arc<AtomicUsize>) {
    let conns = Arc::new(AtomicUsize::new(0));
    let c = conns.clone();
    let app = Router::new()
        .route(
            "/live.ts",
            get(move || {
                c.fetch_add(1, Ordering::SeqCst);
                let s = futures_util::stream::unfold(0u32, |n| async move {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Some((Ok::<_, std::io::Error>(format!("TS{n:04}")), n + 1))
                });
                async move { ([("content-type", "video/mp2t")], Body::from_stream(s)) }
            }),
        )
        .route(
            "/sniff.ts",
            get(|| async {
                (
                    [("content-type", "video/mp2t")],
                    "#EXTM3U\n#EXTINF:2,\na.ts\n",
                )
            }),
        )
        .route(
            "/list.m3u8",
            get(|| async {
                (
                    [("content-type", "application/vnd.apple.mpegurl")],
                    "#EXTM3U\n",
                )
            }),
        );
    (listen(app).await, conns)
}

/// The first `n` bytes of a streamed GET.
async fn read_some(url: &str, n: usize) -> (u16, Option<String>, Vec<u8>) {
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build_http::<http_body_util::Empty<axum::body::Bytes>>();
    let res = client.get(url.parse().unwrap()).await.unwrap();
    let ct = res
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string());
    let status = res.status().as_u16();
    let mut body = res.into_body();
    let mut out = vec![];
    while out.len() < n {
        match tokio::time::timeout(Duration::from_secs(5), body.frame()).await {
            Ok(Some(Ok(f))) => out.extend(f.into_data().unwrap_or_default()),
            _ => break,
        }
    }
    (status, ct, out)
}

#[tokio::test]
async fn direct_tees_one_upstream_and_packages_a_manifest() {
    let e = live_engine(json!({}), json!({})).await;
    let (up, conns) = provider().await;
    let direct = json!({ "directPlay": true });
    let a = open(
        &e,
        open_req(
            "8",
            vec![upstream(&format!("{up}/live.ts"), 0)],
            direct.clone(),
            "alice",
            "t",
        ),
    )
    .await;
    let b = open(
        &e,
        open_req(
            "8",
            vec![upstream(&format!("{up}/live.ts"), 0)],
            direct.clone(),
            "bob",
            "t2",
        ),
    )
    .await;
    assert_eq!(
        (a["mode"].clone(), a["dvrWindowSeconds"].clone()),
        (json!("direct"), json!(0.0))
    );
    assert_eq!(
        a["playPath"],
        format!("/api/livetv/sessions/{}/direct.ts", sid(&a))
    );
    assert_eq!(b["capacityToken"], "t");
    let (sa, sb) = (sid(&a), sid(&b));
    let ua = format!(
        "{}/api/livetv/sessions/{sa}/direct.ts?token=alice-jwt",
        e.base
    );
    let ub = format!(
        "{}/api/livetv/sessions/{sb}/direct.ts?token=bob-jwt",
        e.base
    );
    let (ra, rb) = tokio::join!(read_some(&ua, 24), read_some(&ub, 24));
    for (status, ct, body) in [ra, rb] {
        assert_eq!((status, ct.as_deref()), (200, Some("video/mp2t")));
        assert!(body.len() >= 24 && body.starts_with(b"TS"), "{body:?}");
    }
    assert_eq!(
        conns.load(Ordering::SeqCst),
        1,
        "one upstream for both viewers"
    );
    let r = e
        .get(
            &format!("/api/livetv/sessions/{sa}/index.m3u8"),
            "alice-jwt",
            &[],
        )
        .await;
    assert_eq!(
        (r.status, r.json()["message"].clone()),
        (404, json!("Playlist not ready yet"))
    );
    let r = e
        .get(
            &format!("/api/livetv/sessions/{sa}/seg-00000.ts"),
            "alice-jwt",
            &[],
        )
        .await;
    assert_eq!(
        (r.status, r.json()["message"].clone()),
        (404, json!("Live TV segment not found"))
    );
    assert!(e.ffmpeg_pids().is_empty(), "direct runs no ffmpeg");

    // A playlist behind a stream's content type, or saying so, is packaged instead.
    for (ch, path) in [("10", "sniff.ts"), ("11", "list.m3u8")] {
        let url = format!("{up}/{path}");
        let m = open(
            &e,
            open_req(ch, vec![upstream(&url, 0)], direct.clone(), "alice", ch),
        )
        .await;
        assert_eq!(m["mode"], "remux", "{path}");
        assert!(m["playPath"].as_str().unwrap().ends_with("/index.m3u8"));
    }
    // A probe that names a playlist never dials the upstream directly.
    let before = conns.load(Ordering::SeqCst);
    let mut probed = upstream(&format!("{up}/live.ts"), 0);
    probed["probedContainer"] = json!("hls");
    let m = open(&e, open_req("13", vec![probed], direct, "alice", "t13")).await;
    assert_eq!(m["mode"], "remux");
    assert_eq!(conns.load(Ordering::SeqCst), before);
}

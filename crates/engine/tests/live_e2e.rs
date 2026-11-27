//! Live TV against real ffmpeg and two upstreams streaming an H.264/AAC MPEG-TS at real time.
//! Ignored unless `ENGINE_E2E` is set when building (see `runtime_e2e.rs` for the tools); the GPU
//! test needs `--ignored` too, on a host with an Intel GPU on the default render node.
#![cfg(unix)]
mod runtime_common;
use axum::{Router, body::Body, routing::get};
use fliks_engine_core::api::{EngineEvent, HwAccel};
use fliks_engine_core::hw::DEFAULT_RENDER_NODE;
use runtime_common::*;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

const SECONDS: u32 = 120;

fn source() -> PathBuf {
    clip(
        "live-src-120s.ts",
        &format!(
            "-f lavfi -i testsrc2=size=640x360:rate=24:duration={SECONDS} \
             -f lavfi -i sine=frequency=440:sample_rate=48000:duration={SECONDS} \
             -c:v libx264 -preset veryfast -pix_fmt yuv420p -g 48 -c:a aac -ac 2 -shortest"
        ),
    )
}

/// `source` with its fields interlaced (top first) and flagged so.
fn interlaced_source() -> PathBuf {
    clip(
        "live-src-120s-tff.ts",
        &format!(
            "-f lavfi -i testsrc2=size=1280x720:rate=50:duration={SECONDS} \
             -f lavfi -i sine=frequency=440:sample_rate=48000:duration={SECONDS} \
             -vf interlace=scan=tff,setfield=tff -c:v libx264 -preset veryfast -pix_fmt yuv420p \
             -flags +ildct+ilme -x264opts tff=1 -g 50 -c:a aac -ac 2 -shortest"
        ),
    )
}

/// `source` in 10-bit H.264, which no Intel decoder reads.
fn ten_bit_source() -> PathBuf {
    clip(
        "live-src-120s-10bit.ts",
        &format!(
            "-f lavfi -i testsrc2=size=640x360:rate=24:duration={SECONDS} \
             -f lavfi -i sine=frequency=440:sample_rate=48000:duration={SECONDS} \
             -c:v libx264 -preset veryfast -pix_fmt yuv420p10le -g 48 -c:a aac -ac 2 -shortest"
        ),
    )
}

/// An upstream sending the clip at its own bitrate; `kill` resets its streams and closes it.
struct Provider {
    url: String,
    killed: Arc<AtomicBool>,
    server: tokio::task::JoinHandle<()>,
}

impl Provider {
    async fn start() -> Self {
        Self::serve(source()).await
    }

    async fn serve(clip: PathBuf) -> Self {
        let bytes = axum::body::Bytes::from(std::fs::read(clip).unwrap());
        let killed = Arc::new(AtomicBool::new(false));
        let k = killed.clone();
        let app = Router::new().route(
            "/live.ts",
            get(move || {
                let (bytes, k) = (bytes.clone(), k.clone());
                let chunk = 188 * 64;
                let pace =
                    Duration::from_secs_f64(chunk as f64 / (bytes.len() as f64 / SECONDS as f64));
                let s = futures_util::stream::unfold(0usize, move |at| {
                    let (bytes, k) = (bytes.clone(), k.clone());
                    async move {
                        tokio::time::sleep(pace).await;
                        if k.load(Ordering::SeqCst) {
                            return Some((Err(std::io::Error::other("provider gone")), usize::MAX));
                        }
                        (at < bytes.len()).then(|| {
                            let end = (at + chunk).min(bytes.len());
                            (Ok(bytes.slice(at..end)), end)
                        })
                    }
                });
                async move { ([("content-type", "video/mp2t")], Body::from_stream(s)) }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/live.ts", l.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let _ = axum::serve(l, app).await;
        });
        Self {
            url,
            killed,
            server,
        }
    }

    fn kill(&self) {
        self.killed.store(true, Ordering::SeqCst);
        self.server.abort();
    }
}

async fn live_engine() -> Engine {
    live_engine_on(host(HwAccel::None, "/dev/dri/renderD128", "signal")).await
}

async fn live_engine_on(hw: Arc<fliks_engine::hw::Hw>) -> Engine {
    let e = engine_with(hw, &ffmpeg(), json!({})).await;
    let mut config = e.control("GET", "/v1/config", None).await.json();
    config["revision"] = json!(2);
    config["livetv"] = json!({ "channelIdleSeconds": 0 });
    assert_eq!(
        e.control("PUT", "/v1/config", Some(config)).await.status,
        200
    );
    e.stub.grant("alice-jwt", "alice");
    e
}

async fn open(e: &Engine, upstreams: Value, caps: Value) -> Value {
    let body = json!({ "channelId": "e2e", "upstreams": upstreams, "caps": caps,
        "ownerId": "alice", "capacityToken": "tok" });
    let r = e.control("POST", "/v1/live", Some(body)).await;
    assert_eq!(r.status, 200, "{}", r.text());
    r.json()
}

async fn attempt(rx: &mut broadcast::Receiver<EngineEvent>, secs: u64) -> (u32, Option<String>) {
    tokio::time::timeout(Duration::from_secs(secs), async {
        loop {
            if let EngineEvent::LiveAttempt {
                upstream, error, ..
            } = rx.recv().await.unwrap()
            {
                return (upstream, error);
            }
        }
    })
    .await
    .expect("LiveAttempt")
}

/// The segment URIs of the live playlist, token included.
async fn segments(e: &Engine, sid: &str) -> Vec<String> {
    let r = e
        .get(
            &format!("/api/livetv/sessions/{sid}/index.m3u8"),
            "alice-jwt",
            &[],
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    r.text()
        .lines()
        .filter(|l| l.starts_with("seg-"))
        .map(String::from)
        .collect()
}

fn number(uri: &str) -> u32 {
    uri[4..uri.find('.').unwrap()].parse().unwrap()
}

async fn fetch(e: &Engine, sid: &str, uri: &str) -> Vec<u8> {
    let (name, _) = uri.split_once('?').unwrap();
    let r = e
        .get(
            &format!("/api/livetv/sessions/{sid}/{name}"),
            "alice-jwt",
            &[],
        )
        .await;
    assert_eq!(r.status, 200, "{uri}");
    assert!(!r.body.is_empty(), "{uri}");
    r.body.to_vec()
}

/// ffprobe's csv answer to `args` about `bytes`.
fn ffprobe(bytes: &[u8], args: &[&str]) -> String {
    let f_dir = temp_dir("live-probe");
    let f = f_dir.join("seg.bin");
    std::fs::write(&f, bytes).unwrap();
    let out = std::process::Command::new(fliks_engine::tool_path("FFPROBE_PATH", "ffprobe"))
        .args(["-v", "error", "-of", "csv=p=0"])
        .args(args)
        .arg(&f)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// ffprobe's `codec_name` of each stream of `bytes` (MPEG-TS lists its program's streams again).
fn codecs(bytes: &[u8]) -> Vec<String> {
    let mut names: Vec<String> = vec![];
    for l in ffprobe(bytes, &["-show_entries", "stream=codec_name"]).lines() {
        if !l.is_empty() && !names.iter().any(|n| n == l) {
            names.push(l.into());
        }
    }
    names
}

/// The video packets of `bytes` (an init segment and a media one).
fn video_packets(bytes: &[u8]) -> u32 {
    let count = ["-select_streams", "v:0", "-count_packets"];
    let entry = ["-show_entries", "stream=nb_read_packets"];
    let out = ffprobe(bytes, &[count.as_slice(), entry.as_slice()].concat());
    out.trim().parse().unwrap_or(0)
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn remux_fails_over_to_the_next_upstream_and_releases() {
    let e = live_engine().await;
    let (one, two) = (Provider::start().await, Provider::start().await);
    let mut rx = e.rt.events.subscribe();
    let a = open(
        &e,
        json!([{ "url": one.url, "priority": 0 }, { "url": two.url, "priority": 1 }]),
        json!({}),
    )
    .await;
    let sid = a["sessionId"].as_str().unwrap().to_string();
    assert_eq!(a["mode"], "remux");
    assert_eq!(
        a["probe"],
        json!({ "videoCodec": "h264", "audioCodec": "aac", "container": "mpegts", "fieldOrder": "progressive" })
    );
    assert_eq!(attempt(&mut rx, 1).await, (0, None));

    until_async(&e, &sid, 20, |s| s.len() >= 2).await;
    let list = segments(&e, &sid).await;
    let init = fetch(&e, &sid, "init.mp4?").await;
    let mut media = init.clone();
    media.extend(fetch(&e, &sid, &list[0]).await);
    assert_eq!(codecs(&media), ["h264", "aac"]);

    let last_before = number(list.last().unwrap());
    let t = Instant::now();
    one.kill();
    let (up, err) = attempt(&mut rx, 30).await;
    assert_eq!(up, 0);
    let err = err.expect("the failure reason");
    assert_eq!(attempt(&mut rx, 25).await, (1, None));
    let took = t.elapsed();
    // The TS's window: an exit fails over at once, a stall within 15 to 20 s, then up to 20 s
    // for the next upstream's first segment.
    assert!(took < Duration::from_secs(40), "{took:?}");
    until_async(&e, &sid, 30, |s| {
        s.last().is_some_and(|l| number(l) > last_before + 1)
    })
    .await;
    let after = segments(&e, &sid).await;
    let mut media = init;
    media.extend(fetch(&e, &sid, after.last().unwrap()).await);
    assert_eq!(codecs(&media), ["h264", "aac"]);

    assert_eq!(
        e.control("DELETE", &format!("/v1/sessions/{sid}"), None)
            .await
            .status,
        204
    );
    let released = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let EngineEvent::LiveReleased {
                capacity_token,
                reason,
            } = rx.recv().await.unwrap()
            {
                return (capacity_token, reason);
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(released, ("tok".into(), "idle".into()));
    until("ffmpeg gone", 5, || e.ffmpeg_pids().is_empty()).await;
    assert!(!e.root.join("live").read_dir().unwrap().any(|_| true));
    two.kill();
    eprintln!("failover took {took:?} after: {err}");
}

#[tokio::test]
#[cfg_attr(not(engine_e2e), ignore = "real ffmpeg: build with ENGINE_E2E=1")]
async fn transcode_into_ts_segments_without_direct_play() {
    let e = live_engine().await;
    let up = Provider::start().await;
    let a = open(
        &e,
        json!([{ "url": up.url, "probedVideoCodec": "h264", "probedAudioCodec": "aac" }]),
        json!({ "directPlay": false, "maxBitrateBps": 1000000, "useTs": true }),
    )
    .await;
    let sid = a["sessionId"].as_str().unwrap().to_string();
    assert_eq!(a["mode"], "transcode");
    assert_eq!(a["qoePipeline"]["output"]["bitrate"], 1500000);
    until_async(&e, &sid, 20, |s| s.len() >= 2).await;
    let list = segments(&e, &sid).await;
    assert!(list[0].contains(".ts?token=alice-jwt"), "{list:?}");
    let seg = fetch(&e, &sid, &list[1]).await;
    assert_eq!(codecs(&seg), ["h264", "aac"]);
    assert_eq!(
        e.control("DELETE", &format!("/v1/sessions/{sid}"), None)
            .await
            .status,
        204
    );
    until("ffmpeg gone", 10, || e.ffmpeg_pids().is_empty()).await;
    up.kill();
}

#[tokio::test]
#[ignore = "an Intel GPU on the default render node and real ffmpeg: ENGINE_E2E=1 cargo test -p \
    fliks-engine --test live_e2e -- --ignored"]
async fn a_gpu_encodes_frames_the_cpu_decoded() {
    // The boot probe left the backend no H.264 decoder: the CPU decodes (and deinterlaces a known
    // interlaced source, as without a GPU deinterlacer), the frames are uploaded to the encoder.
    let disabled = ["h264_vaapi_decode", "h264_qsv_decode"];
    for hw in [HwAccel::Vaapi, HwAccel::Qsv] {
        for (clip, field) in [(source(), "progressive"), (interlaced_source(), "tt")] {
            let host = host_without(hw, DEFAULT_RENDER_NODE, "signal", &disabled);
            let e = live_engine_on(host).await;
            let up = Provider::serve(clip).await;
            let a = open(
                &e,
                json!([{ "url": up.url, "probedVideoCodec": "h264", "probedAudioCodec": "aac",
                    "probedFieldOrder": field }]),
                json!({ "directPlay": false, "maxBitrateBps": 1000000, "useTs": true }),
            )
            .await;
            let sid = a["sessionId"].as_str().unwrap().to_string();
            assert_eq!(a["mode"], "transcode");
            let procs = e.ffmpeg_procs();
            let cmd = &procs.first().expect("the transcode").2;
            assert!(
                cmd.contains("format=nv12,hwupload") && !cmd.contains("-hwaccel"),
                "{cmd}"
            );
            until_async(&e, &sid, 20, |s| s.len() >= 2).await;
            let list = segments(&e, &sid).await;
            let seg = fetch(&e, &sid, &list[1]).await;
            assert_eq!(codecs(&seg), ["h264", "aac"], "{hw:?} {field}");
            let path = format!("/v1/sessions/{sid}");
            assert_eq!(e.control("DELETE", &path, None).await.status, 204);
            until("ffmpeg gone", 10, || e.ffmpeg_pids().is_empty()).await;
            up.kill();
        }
    }
}

#[tokio::test]
#[ignore = "an Intel GPU on the default render node and real ffmpeg: ENGINE_E2E=1 cargo test -p \
    fliks-engine --test live_e2e -- --ignored"]
async fn a_stream_the_gpu_decoder_refuses_is_redone_on_the_cpu() {
    // Named 8-bit H.264 by the host, 10-bit on the wire: the GPU decode run never gives a frame.
    for hw in [HwAccel::Vaapi, HwAccel::Qsv] {
        let e = live_engine_on(host(hw, DEFAULT_RENDER_NODE, "signal")).await;
        let up = Provider::serve(ten_bit_source()).await;
        let a = open(
            &e,
            json!([{ "url": up.url, "probedVideoCodec": "h264", "probedAudioCodec": "aac" }]),
            json!({ "directPlay": false, "maxBitrateBps": 1000000 }),
        )
        .await;
        let sid = a["sessionId"].as_str().unwrap().to_string();
        let procs = e.ffmpeg_procs();
        let cmd = &procs.first().expect("the transcode").2;
        assert!(
            cmd.contains("format=nv12,hwupload") && !cmd.contains("-hwaccel"),
            "{hw:?} {cmd}"
        );
        until_async(&e, &sid, 20, |s| s.len() >= 2).await;
        // The refused run left no segment behind: each one carries video.
        let init = fetch(&e, &sid, "init.mp4?").await;
        for uri in segments(&e, &sid).await {
            let seg = [init.clone(), fetch(&e, &sid, &uri).await].concat();
            assert!(video_packets(&seg) > 0, "{hw:?} {uri}");
        }
        let path = format!("/v1/sessions/{sid}");
        assert_eq!(e.control("DELETE", &path, None).await.status, 204);
        until("ffmpeg gone", 10, || e.ffmpeg_pids().is_empty()).await;
        up.kill();
    }
}

/// Polls the playlist until `ok` holds on its segment list.
async fn until_async(e: &Engine, sid: &str, secs: u64, ok: impl Fn(&[String]) -> bool) {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(secs) {
        if ok(&segments(e, sid).await) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("playlist: {:?}", segments(e, sid).await);
}

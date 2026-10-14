//! Harness of the runtime tests: a stub host resolver, an engine on a real listener, an HTTP
//! client and test media.
#![allow(dead_code)]
use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    routing::post,
};
use fliks_engine::hw::{Hw, Snapshot};
use fliks_engine::runtime::Runtime;
use fliks_engine_core::api::{
    HwAccel, MediaInfo, Platform, ResolveMediaId, ResolvePurpose, ResolveRequest, ResolveResponse,
};
use fliks_engine_core::hw::Detected;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub const HOST_TOKEN: &str = "host-secret";
pub const TOKEN: &str = "engine-secret";

/// The host side of `POST /engine/resolve`: credential -> owner, media id -> path.
#[derive(Default)]
pub struct Stub {
    pub owners: Mutex<HashMap<String, String>>,
    pub paths: Mutex<HashMap<u32, String>>,
    pub subtitles: Mutex<HashMap<u32, (String, String)>>,
    pub sprites: Mutex<HashMap<u32, String>>,
    pub calls: Mutex<Vec<ResolveRequest>>,
    pub down: AtomicBool,
}

impl Stub {
    pub fn grant(&self, credential: &str, owner: &str) {
        self.owners
            .lock()
            .unwrap()
            .insert(credential.into(), owner.into());
    }

    pub fn file(&self, id: u32, path: &Path) {
        self.paths
            .lock()
            .unwrap()
            .insert(id, path.to_string_lossy().into());
    }

    pub fn calls(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    pub fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }
}

async fn resolve(
    State(stub): State<Arc<Stub>>,
    headers: HeaderMap,
    Json(r): Json<ResolveRequest>,
) -> Result<Json<ResolveResponse>, StatusCode> {
    if headers.get("authorization").and_then(|v| v.to_str().ok())
        != Some(&format!("Bearer {HOST_TOKEN}"))
    {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if stub.down.load(Ordering::SeqCst) {
        return Err(StatusCode::BAD_GATEWAY);
    }
    stub.calls.lock().unwrap().push(r.clone());
    let deny = ResolveResponse {
        allow: false,
        path: None,
        owner_id: None,
        format: None,
        ttl_s: Some(60.0),
    };
    let ResolveMediaId::File(id) = r.media_id else {
        return Ok(Json(deny));
    };
    let Some(owner) = stub.owners.lock().unwrap().get(&r.credential).cloned() else {
        return Ok(Json(deny));
    };
    let (path, format) = match (r.purpose, r.subtitle_id) {
        (ResolvePurpose::Subtitle, Some(sub)) => match stub.subtitles.lock().unwrap().get(&sub) {
            Some((p, f)) => (Some(p.clone()), Some(f.clone())),
            None => (None, None),
        },
        (ResolvePurpose::Subtitle, None) => (
            stub.paths.lock().unwrap().get(&id).cloned(),
            Some("embedded".into()),
        ),
        (ResolvePurpose::Sprite, _) => (stub.sprites.lock().unwrap().get(&id).cloned(), None),
        _ => (stub.paths.lock().unwrap().get(&id).cloned(), None),
    };
    Ok(Json(match path {
        Some(p) => ResolveResponse {
            allow: true,
            path: Some(p),
            owner_id: Some(owner),
            format,
            ttl_s: Some(60.0),
        },
        None => deny,
    }))
}

pub async fn listen(app: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await });
    format!("http://{addr}")
}

pub fn host(hw: HwAccel, render_node: &str, pause: &str) -> Arc<Hw> {
    Hw::fixed(Snapshot {
        detected: Detected {
            hw_accel: hw,
            platform: Platform::Linux,
            libfdk_aac: false,
            vaapi_writes_hdr_metadata: false,
            probes: vec![],
            encoders: vec![],
            decoders: vec![],
            disabled_encoders: vec![],
            disabled_decoders: vec![],
        },
        gpus: vec![],
        pause_capability: pause.into(),
        render_node: render_node.into(),
    })
}

pub struct Engine {
    pub base: String,
    pub rt: Arc<Runtime>,
    pub stub: Arc<Stub>,
    pub root: PathBuf,
}

pub fn ffmpeg() -> String {
    std::env::var("FFMPEG_PATH").unwrap_or_else(|_| "ffmpeg".into())
}

pub fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "fliks-rt-{tag}-{}-{}",
        std::process::id(),
        fliks_engine::random_hex(4)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// An engine on its own cache root, configured with the stub as host and `lifetime`.
pub async fn engine_with(hw: Arc<Hw>, ffmpeg: &str, lifetime: Value) -> Engine {
    let stub = Arc::new(Stub::default());
    let host_url = listen(
        Router::new()
            .route("/engine/resolve", post(resolve))
            .with_state(stub.clone()),
    )
    .await;
    let root = temp_dir("cache");
    let rt = Runtime::new(
        hw,
        fliks_engine::cache::Cache { root: root.clone() },
        ffmpeg.into(),
    );
    let base = listen(fliks_engine::router(TOKEN.into(), rt.clone())).await;
    let e = Engine {
        base,
        rt,
        stub,
        root,
    };
    let r = e
        .control(
            "PUT",
            "/v1/config",
            Some(json!({
                "revision": 1, "hostUrl": host_url, "hostToken": HOST_TOKEN, "lifetime": lifetime,
            })),
        )
        .await;
    assert_eq!(r.status, 200);
    e
}

pub async fn engine() -> Engine {
    engine_with(
        host(HwAccel::None, "/dev/dri/renderD128", "signal"),
        &ffmpeg(),
        json!({}),
    )
    .await
}

pub struct Res {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Res {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn header(&self, n: &str) -> Option<String> {
        self.headers
            .get(n)
            .and_then(|v| v.to_str().ok())
            .map(String::from)
    }
}

pub async fn http(method: &str, url: &str, headers: &[(&str, &str)], body: Option<Value>) -> Res {
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let mut req = Request::builder().method(method).uri(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let body = body.map_or_else(Bytes::new, |b| Bytes::from(b.to_string()));
    if !body.is_empty() {
        req = req.header("content-type", "application/json");
    }
    let res = client
        .request(req.body(Full::new(body)).unwrap())
        .await
        .unwrap();
    let status = res.status().as_u16();
    let headers = res.headers().clone();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    Res {
        status,
        headers,
        body,
    }
}

impl Engine {
    pub async fn control(&self, method: &str, path: &str, body: Option<Value>) -> Res {
        http(
            method,
            &format!("{}{path}", self.base),
            &[("authorization", &format!("Bearer {TOKEN}"))],
            body,
        )
        .await
    }

    /// A data-plane GET carrying `?token=<credential>` (plus `query`).
    pub async fn get(&self, path: &str, credential: &str, headers: &[(&str, &str)]) -> Res {
        let sep = if path.contains('?') { '&' } else { '?' };
        http(
            "GET",
            &format!("{}{path}{sep}token={credential}", self.base),
            headers,
            None,
        )
        .await
    }

    pub async fn probe(&self, id: u32, path: &Path) -> MediaInfo {
        let r = self
            .control("POST", "/v1/probe", Some(json!({
                "media": { "mediaFileId": id, "path": path }, "crop": false, "mkvTags": false, "moviehash": false
            })))
            .await;
        assert_eq!(r.status, 200, "{}", r.text());
        let v = r.json();
        assert_eq!(v["ok"], true, "probe failed: {v}");
        serde_json::from_value(v["info"].clone()).unwrap()
    }

    /// `POST /v1/sessions`; `device` merges into a client that cannot direct play or copy.
    pub async fn open(
        &self,
        id: u32,
        path: &Path,
        info: &MediaInfo,
        owner: &str,
        extra: Value,
    ) -> Value {
        let mut device =
            json!({ "directPlayProfiles": [], "rejectCopy": true, "supportsAbr": true });
        let mut req = json!({
            "decide": {
                "media": { "mediaFileId": id, "path": path, "ext": ".mkv", "contentType": "video/x-matroska" },
                "info": info,
                "device": {},
                "selection": { "tokenParam": "?token=t" },
            },
            "ownerId": owner,
        });
        if let Some(d) = extra.get("device").and_then(Value::as_object) {
            for (k, v) in d {
                device[k] = v.clone();
            }
        }
        req["decide"]["device"] = device;
        if let Some(sel) = extra.get("selection").and_then(Value::as_object) {
            for (k, v) in sel {
                req["decide"]["selection"][k] = v.clone();
            }
        }
        if let Some(v) = extra.get("scan") {
            req["decide"]["scan"] = v.clone();
        }
        for k in ["startAtS", "pinned", "tags"] {
            if let Some(v) = extra.get(k) {
                req[k] = v.clone();
            }
        }
        let r = self.control("POST", "/v1/sessions", Some(req)).await;
        assert_eq!(r.status, 200, "{}", r.text());
        r.json()
    }

    /// `ffmpeg` processes whose argv names this engine's cache root.
    pub fn ffmpeg_pids(&self) -> Vec<u32> {
        let root = self.root.to_string_lossy().into_owned();
        let Ok(rd) = std::fs::read_dir("/proc") else {
            return vec![];
        };
        rd.flatten()
            .filter_map(|e| e.file_name().to_string_lossy().parse::<u32>().ok())
            .filter(|pid| {
                std::fs::read(format!("/proc/{pid}/cmdline"))
                    .map(|c| String::from_utf8_lossy(&c).contains(&root))
                    .unwrap_or(false)
            })
            .filter(|pid| {
                // A zombie waiting for its reaper is gone already.
                std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .is_ok_and(|s| !s.split(' ').nth(2).is_some_and(|st| st == "Z"))
            })
            .collect()
    }
}

/// Generates a clip with ffmpeg's lavfi sources, once per name; `args` split on whitespace.
pub fn clip(name: &str, args: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("fliks-engine-e2e-media");
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join(name);
    if out.exists() {
        return out;
    }
    let tmp = dir.join(format!("tmp-{}-{name}", fliks_engine::random_hex(4)));
    let st = std::process::Command::new(ffmpeg())
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args.split_whitespace())
        .arg(&tmp)
        .status()
        .expect("ffmpeg");
    assert!(st.success(), "clip {name}");
    std::fs::rename(&tmp, &out).unwrap();
    out
}

const X264: &str = "-c:v libx264 -preset veryfast -pix_fmt yuv420p -g 48";

/// H.264 SDR 1280x720 24 fps + AAC stereo.
pub fn h264_clip(seconds: u32) -> PathBuf {
    let s = seconds;
    clip(
        &format!("h264-{s}s.mkv"),
        &format!(
            "-f lavfi -i testsrc2=size=1280x720:rate=24:duration={s} \
             -f lavfi -i sine=frequency=440:sample_rate=48000:duration={s} {X264} -c:a aac -ac 2 -shortest"
        ),
    )
}

/// 10 s HEVC Main10 HDR10 1280x720 + AAC.
pub fn hevc_hdr_clip() -> PathBuf {
    clip(
        "hevc-hdr10-v2.mkv",
        "-f lavfi -i testsrc2=size=1280x720:rate=24:duration=10 \
         -f lavfi -i sine=frequency=330:sample_rate=48000:duration=10 \
         -vf setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc,format=yuv420p10le \
         -c:v libx265 -preset ultrafast \
         -x265-params log-level=error:hdr10=1:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1):max-cll=1000,400 \
         -c:a aac -ac 2 -shortest",
    )
}

/// 10 s H.264 with two audio tracks (eng, fra).
pub fn multi_audio_clip() -> PathBuf {
    clip(
        "multi-audio.mkv",
        &format!(
            "-f lavfi -i testsrc2=size=1280x720:rate=24:duration=10 \
             -f lavfi -i sine=frequency=440:sample_rate=48000:duration=10 \
             -f lavfi -i sine=frequency=880:sample_rate=48000:duration=10 \
             -map 0:v -map 1:a -map 2:a {X264} -c:a aac -ac 2 \
             -metadata:s:a:0 language=eng -metadata:s:a:1 language=fra -shortest"
        ),
    )
}

/// The host's `SourceScan` of a file's video packets: keyframes (decode order) and end; a packet
/// without a dts (Matroska's first ones) decodes `has_b_frames` frames before its pts.
pub fn scan(file: &Path) -> Value {
    let ffprobe = std::env::var("FFPROBE_PATH").unwrap_or_else(|_| "ffprobe".into());
    let run = |args: &[&str]| {
        let out = std::process::Command::new(&ffprobe)
            .args(["-v", "error", "-select_streams", "v:0"])
            .args(args)
            .arg(file)
            .output()
            .expect("ffprobe");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let stream = run(&[
        "-show_entries",
        "stream=has_b_frames,avg_frame_rate",
        "-of",
        "csv=p=0",
    ]);
    let (mut reorder, mut fps) = (0.0, 24.0);
    for f in stream.trim().split(',') {
        match f.split_once('/') {
            Some((n, d)) => {
                fps = n.parse::<f64>().unwrap_or(24.0) / d.parse::<f64>().unwrap_or(1.0)
            }
            None => reorder = f.parse().unwrap_or(0.0),
        }
    }
    let lead = reorder / fps;
    let (mut keyframes, mut end) = (vec![], 0f64);
    let packets = run(&[
        "-show_entries",
        "packet=pts_time,dts_time,duration_time,flags",
        "-of",
        "csv=p=0",
    ]);
    for l in packets.lines() {
        let f: Vec<&str> = l.split(',').collect();
        let Some(pts) = f.first().and_then(|p| p.parse::<f64>().ok()) else {
            continue;
        };
        let dts = f
            .get(1)
            .and_then(|d| d.parse::<f64>().ok())
            .unwrap_or(pts - lead);
        end = end.max(pts + f.get(2).and_then(|d| d.parse::<f64>().ok()).unwrap_or(0.0));
        if f.get(3).is_some_and(|k| k.starts_with('K')) {
            keyframes.push(serde_json::json!({ "pts": pts, "dts": dts }));
        }
    }
    serde_json::json!({ "keyframes": keyframes, "end": end, "audioConfigChanges": {} })
}

pub fn e2e() -> bool {
    std::env::var_os("ENGINE_E2E").is_some()
}

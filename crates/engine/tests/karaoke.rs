//! The model manager and the `Separate`, `Align` and `Melody` jobs: a local file server for the
//! downloads, and a fake network backend (splitting by frequency, a zero-crossing pitch) on the
//! real ffmpeg for the jobs. Tests that need ffmpeg skip without it.
mod runtime_common;
use axum::{
    Router,
    http::{StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use fliks_engine::karaoke::models::{Entry, FileEntry, Manager};
use fliks_engine::karaoke::{Backend, BackendInfo, Loaded, Net, Tensor, Tools, Warm};
use fliks_engine_core::api::{EngineEvent, HwAccel, ModelKind};
use runtime_common::*;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const PLAIN: &[u8] = b"model bytes";

fn sha(b: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, b)
        .as_ref()
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

/// A file server: `/ok` answers the bytes, `/moved` redirects to them, `/gone` is a 404.
async fn server() -> String {
    listen(
        Router::new()
            .route("/ok", get(|| async { PLAIN }))
            .route(
                "/moved",
                get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/ok")]).into_response() }),
            )
            .route("/gone", get(|| async { StatusCode::NOT_FOUND })),
    )
    .await
}

fn entry(id: &'static str, kind: ModelKind, default: bool, files: Vec<FileEntry>) -> Entry {
    Entry {
        id,
        kind,
        name: "test model",
        version: "7",
        licence: "MIT",
        licence_text: "MIT text".into(),
        clear: true,
        default,
        files,
    }
}

fn file(name: &'static str, url: String, bytes: &[u8]) -> FileEntry {
    FileEntry {
        name,
        url,
        sha256: Box::leak(sha(bytes).into_boxed_str()),
        size: bytes.len() as u64,
    }
}

async fn engine() -> Engine {
    let hw = host(HwAccel::None, "/dev/dri/renderD128", "signal");
    engine_with_tools(hw, &ffmpeg(), "ffprobe", "fpcalc", json!({})).await
}

/// Installs a model of the real manifest as sparse files of the right sizes.
fn install(e: &Engine, id: &str) {
    let models = e.rt.karaoke.models();
    let m = models.get(id).expect("manifest entry");
    let dir = models.dir().join(id);
    std::fs::create_dir_all(&dir).unwrap();
    for f in m.files {
        let file = std::fs::File::create(dir.join(&f.name)).unwrap();
        file.set_len(f.size_bytes as u64).unwrap();
    }
}

async fn settle(e: &Engine, id: &str, want: &str) -> Value {
    let mut last = Value::Null;
    for _ in 0..100 {
        let list = e.control("GET", "/v1/models", None).await.json();
        last = list["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap()
            .clone();
        if last["state"] == want {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("model {id} never became {want}: {last}");
}

#[tokio::test]
async fn a_model_is_downloaded_verified_listed_and_deleted() {
    let e = engine().await;
    let base = server().await;
    let dir = e.root.join("models-under-test");
    let tampered = FileEntry {
        sha256: Box::leak(sha(b"something else").into_boxed_str()),
        ..file("bad.bin", format!("{base}/ok"), PLAIN)
    };
    e.rt.karaoke.replace(
        Manager::new(
            dir.clone(),
            vec![
                entry(
                    "good",
                    ModelKind::Melody,
                    true,
                    vec![
                        file("a.bin", format!("{base}/ok"), PLAIN),
                        file("b.bin", format!("{base}/moved"), PLAIN),
                    ],
                ),
                entry("tampered", ModelKind::Alignment, true, vec![tampered]),
                entry(
                    "missing",
                    ModelKind::Separation,
                    true,
                    vec![file("c.bin", format!("{base}/gone"), PLAIN)],
                ),
            ],
            e.rt.events.clone(),
        ),
        Arc::new(NoOnnx),
    );
    let mut rx = e.rt.events.subscribe();
    let list = e.control("GET", "/v1/models", None).await.json();
    assert_eq!(list["modelsDir"], dir.to_string_lossy().as_ref());
    assert_eq!(list["runtime"]["onnx"], false);
    assert!(
        list["models"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["state"] == "absent")
    );

    let r = e.control("POST", "/v1/models/good/download", None).await;
    assert_eq!(r.status, 202, "{}", r.text());
    let m = settle(&e, "good", "installed").await;
    assert_eq!(m["downloadedBytes"], (PLAIN.len() * 2) as f64);
    assert_eq!(std::fs::read(dir.join("good/b.bin")).unwrap(), PLAIN);
    assert!(!dir.join("good/b.bin.part").exists());
    assert_eq!(
        e.control("POST", "/v1/models/good/download", None)
            .await
            .status,
        200,
        "installed already"
    );
    let ev = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let EngineEvent::ModelChanged { model } = rx.recv().await.unwrap()
                && model.id == "good"
                && model.state == fliks_engine_core::api::ModelState::Installed
            {
                return model;
            }
        }
    })
    .await
    .expect("a ModelChanged event");
    assert_eq!(ev.version, "7");

    for (id, why) in [("tampered", "sha256"), ("missing", "404")] {
        assert_eq!(
            e.control("POST", &format!("/v1/models/{id}/download"), None)
                .await
                .status,
            202
        );
        let m = settle(&e, id, "failed").await;
        assert!(m["error"].as_str().unwrap().contains(why), "{m}");
        let left: Vec<_> = std::fs::read_dir(dir.join(id)).unwrap().flatten().collect();
        assert!(left.is_empty(), "no partial file is kept: {left:?}");
    }

    assert_eq!(
        e.control("DELETE", "/v1/models/good", None).await.status,
        204
    );
    settle(&e, "good", "absent").await;
    assert!(!dir.join("good").exists());
    assert_eq!(
        e.control("DELETE", "/v1/models/nope", None).await.status,
        404
    );
    assert_eq!(
        e.control("POST", "/v1/models/nope/download", None)
            .await
            .status,
        404
    );
}

struct NoOnnx;

impl Backend for NoOnnx {
    fn runtime(&self) -> BackendInfo {
        BackendInfo {
            onnx: false,
            error: Some("no runtime here".into()),
            providers: vec![],
        }
    }

    fn load(&self, _: &Path, _: Option<&[String]>, _: &[Warm]) -> Result<Loaded, String> {
        Err("no runtime here".into())
    }
}

fn audio_info() -> Value {
    json!({ "video": [], "audio": [{ "streamIndex": 0, "codec": "flac", "language": "und",
        "channels": 2, "sampleRate": 44100 }], "subtitles": [], "durationSeconds": 3.0,
        "formatName": "flac" })
}

async fn run_kind(e: &Engine, file: &Path, kind: Value) -> Value {
    let mut rx = e.rt.events.subscribe();
    let r = e
        .control(
            "POST",
            "/v1/jobs",
            Some(json!({ "kind": kind, "media": { "mediaFileId": 5, "path": file } })),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let id = r.json()["jobId"].as_str().unwrap().to_string();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(120), rx.recv())
            .await
            .expect("BgJobDone")
            .unwrap();
        if let EngineEvent::BgJobDone { job_id, .. } = &ev
            && *job_id == id
        {
            return serde_json::to_value(&ev).unwrap();
        }
    }
}

#[tokio::test]
async fn jobs_name_the_missing_model_or_runtime_in_a_typed_error() {
    let e = engine().await;
    e.rt.karaoke
        .replace(e.rt.karaoke.models(), Arc::new(NoOnnx));
    let media = e.root.join("song.flac");
    std::fs::write(&media, b"x").unwrap();
    let out = e.root.join("v.opus");
    let kinds = [
        json!({ "type": "Separate", "info": audio_info(), "vocals": out }),
        json!({ "type": "Align", "lines": [{ "text": "la la" }] }),
        json!({ "type": "Melody", "spans": [{ "startMs": 0, "endMs": 500 }] }),
    ];
    for kind in &kinds {
        let v = run_kind(&e, &media, kind.clone()).await;
        assert_eq!(v["state"], "failed", "{v}");
        let err = v["error"].as_str().unwrap();
        assert!(err.starts_with("model_missing: "), "{err}");
        assert!(err.contains("/v1/models/"), "{err}");
    }
    let bad = run_kind(
        &e,
        &media,
        json!({ "type": "Separate", "info": audio_info(), "vocals": out, "model": "nope" }),
    )
    .await;
    assert!(
        bad["error"]
            .as_str()
            .unwrap()
            .contains("unknown model nope"),
        "{bad}"
    );
    let wrong = run_kind(
        &e,
        &media,
        json!({ "type": "Melody", "spans": [], "model": "spleeter-2stems" }),
    )
    .await;
    assert!(
        wrong["error"]
            .as_str()
            .unwrap()
            .starts_with("model_missing: "),
        "{wrong}"
    );

    // Installed models, but nothing to run them with (or no recogniser).
    for id in ["spleeter-2stems", "swiftf0", "whisper-small-q5_1"] {
        install(&e, id);
    }
    e.rt.karaoke.set_tools(Tools {
        whisper: None,
        pitch_ffmpeg: None,
        whisper_backend: "cpu",
    });
    for kind in &kinds {
        let v = run_kind(&e, &media, kind.clone()).await;
        let err = v["error"].as_str().unwrap();
        assert!(err.starts_with("unsupported: "), "{err}");
    }
    assert!(!out.exists(), "nothing written");
    // The request itself is refused when it cannot be a job.
    let r = e
        .control(
            "POST",
            "/v1/jobs",
            Some(json!({
            "kind": { "type": "Separate", "info": audio_info() },
            "media": { "mediaFileId": 5, "path": media } })),
        )
        .await;
    assert_eq!(r.status, 400, "{}", r.text());
}

/// Keeps the bins of the spectrogram on one side of bin 30 (about 1.3 kHz).
struct SplitNet {
    keep_high: bool,
}

impl Net for SplitNet {
    fn run(&mut self, mut inputs: Vec<(&'static str, Tensor)>) -> Result<Vec<Tensor>, String> {
        let t = inputs.remove(0).1;
        let data = t
            .data
            .iter()
            .enumerate()
            .map(|(i, v)| {
                if (i % 1024 >= 30) == self.keep_high {
                    *v
                } else {
                    0.0
                }
            })
            .collect();
        Ok(vec![Tensor {
            shape: t.shape,
            data,
        }])
    }
}

/// The pitch of each 256-sample hop from the zero crossings of the 1024 samples around it.
struct CrossingNet;

impl Net for CrossingNet {
    fn run(&mut self, mut inputs: Vec<(&'static str, Tensor)>) -> Result<Vec<Tensor>, String> {
        let audio = inputs.remove(0).1.data;
        let frames = audio.len() / 256;
        let (mut hz, mut conf) = (vec![], vec![]);
        for f in 0..frames {
            let (a, b) = (
                (f * 256).saturating_sub(384),
                (f * 256 + 640).min(audio.len()),
            );
            let w = &audio[a..b];
            let cross = w.windows(2).filter(|p| p[0] <= 0.0 && p[1] > 0.0).count();
            let peak = w.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            hz.push(cross as f32 * 16000.0 / w.len() as f32);
            conf.push(if peak > 0.02 { 0.9 } else { 0.1 });
        }
        let shape = vec![1, frames as i64];
        Ok(vec![
            Tensor {
                shape: shape.clone(),
                data: hz,
            },
            Tensor { shape, data: conf },
        ])
    }
}

struct Fake;

impl Backend for Fake {
    fn runtime(&self) -> BackendInfo {
        BackendInfo {
            onnx: true,
            error: None,
            providers: vec![],
        }
    }

    fn load(&self, file: &Path, allow: Option<&[String]>, _: &[Warm]) -> Result<Loaded, String> {
        let name = file.file_name().unwrap().to_string_lossy();
        let net: Box<dyn Net> = match &*name {
            "vocals.onnx" => Box::new(SplitNet { keep_high: true }),
            "accompaniment.onnx" => Box::new(SplitNet { keep_high: false }),
            _ => Box::new(CrossingNet),
        };
        Ok(Loaded {
            net,
            provider: allow
                .and_then(|a| a.first().cloned())
                .unwrap_or_else(|| "cpu".into()),
        })
    }
}

fn tool_ok(bin: &str, args: &[&str]) -> bool {
    std::process::Command::new(bin)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// An engine on the real ffmpeg with the fake backend and the real manifest installed.
async fn real() -> Option<Engine> {
    if !tool_ok(&ffmpeg(), &["-version"]) {
        eprintln!("ffmpeg not available: skipped");
        return None;
    }
    let e = engine().await;
    e.rt.karaoke.replace(e.rt.karaoke.models(), Arc::new(Fake));
    for id in ["spleeter-2stems", "swiftf0", "whisper-small-q5_1"] {
        install(&e, id);
    }
    Some(e)
}

fn make(e: &Engine, out: &Path, graph: &str) -> bool {
    tool_ok(
        &e.ffmpeg,
        &[
            "-v",
            "error",
            "-y",
            "-filter_complex",
            graph,
            "-map",
            "[o]",
            &out.to_string_lossy(),
        ],
    )
}

/// Decoded mono samples of an audio file at `rate`.
fn decode(e: &Engine, path: &Path, rate: u32) -> Vec<f32> {
    let out = std::process::Command::new(&e.ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-f", "f32le", "-ac", "1", "-ar", &rate.to_string(), "-"])
        .output()
        .unwrap();
    out.stdout
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect()
}

/// Amplitude of `freq` in `x`.
fn goertzel(x: &[f32], rate: u32, freq: f64) -> f64 {
    let (mut re, mut im) = (0.0f64, 0.0f64);
    for (i, v) in x.iter().enumerate() {
        let a = 2.0 * std::f64::consts::PI * freq * i as f64 / f64::from(rate);
        re += f64::from(*v) * a.cos();
        im += f64::from(*v) * a.sin();
    }
    2.0 * re.hypot(im) / x.len() as f64
}

fn dir(e: &Engine, name: &str) -> PathBuf {
    let d = e.root.join(name);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[tokio::test]
async fn separate_gives_the_session_the_providers_the_request_allows() {
    let Some(e) = real().await else { return };
    let d = dir(&e, "sep-providers");
    let song = d.join("song.flac");
    if !make(
        &e,
        &song,
        "sine=frequency=440:sample_rate=44100:duration=1,aformat=channel_layouts=stereo[o]",
    ) {
        eprintln!("ffmpeg cannot make the tone: skipped");
        return;
    }
    let kind = |out: &str, providers: Value| {
        json!({ "type": "Separate", "info": audio_info(), "vocals": d.join(out),
            "providers": providers })
    };
    let r = run_kind(&e, &song, kind("a.opus", json!(["directml"]))).await;
    assert_eq!(r["result"]["separate"]["provider"], "directml", "{r}");
    let r = run_kind(&e, &song, kind("b.opus", json!([]))).await;
    assert_eq!(r["result"]["separate"]["provider"], "cpu", "{r}");
}

#[tokio::test]
async fn separate_writes_the_stems_the_guide_and_nothing_else() {
    let Some(e) = real().await else { return };
    let d = dir(&e, "sep");
    let song = d.join("song.flac");
    // 172.27 Hz is bin 4 and 2756.25 Hz bin 64: one each side of the fake network's cut.
    let graph = "sine=frequency=172.265625:sample_rate=44100:duration=3[a];\
                 sine=frequency=2756.25:sample_rate=44100:duration=3[b];\
                 [a][b]amix=inputs=2:normalize=0,aformat=channel_layouts=stereo[o]";
    if !make(&e, &song, graph) {
        eprintln!("ffmpeg cannot make the tones: skipped");
        return;
    }
    let (v, i, g) = (d.join("v.opus"), d.join("i.opus"), d.join("g.opus"));
    let kind = json!({ "type": "Separate", "info": audio_info(), "vocals": v,
        "instrumental": i, "guide": g, "guideVocalsGain": 0.25, "bitrateKbps": 128 });
    let r = run_kind(&e, &song, kind.clone()).await;
    assert_eq!(r["state"], "done", "{r}");
    let m = &r["result"]["separate"];
    assert_eq!(
        (
            m["model"].as_str(),
            m["modelVersion"].as_str(),
            m["provider"].as_str()
        ),
        (Some("spleeter-2stems"), Some("1"), Some("cpu"))
    );
    assert!(
        (m["durationSeconds"].as_f64().unwrap() - 3.0).abs() < 0.05,
        "{m}"
    );
    for k in ["vocals", "instrumental", "guide"] {
        assert!(m[k]["bytes"].as_f64().unwrap() > 1000.0, "{k}: {m}");
    }
    let amp = |p: &Path, hz: f64| {
        let x = decode(&e, p, 48000);
        assert!(
            (x.len() as f64 / 48000.0 - 3.0).abs() < 0.1,
            "{} samples",
            x.len()
        );
        goertzel(&x[24000..120000], 48000, hz)
    };
    let (vh, vl) = (amp(&v, 2756.25), amp(&v, 172.265625));
    let (ih, il) = (amp(&i, 2756.25), amp(&i, 172.265625));
    let (gh, gl) = (amp(&g, 2756.25), amp(&g, 172.265625));
    // `sine` runs at amplitude 0.125.
    assert!(vh > 0.1 && vl < 0.005, "vocals {vh} {vl}");
    assert!(il > 0.1 && ih < 0.005, "instrumental {il} {ih}");
    assert!(gl > 0.1 && (gh - 0.031).abs() < 0.01, "guide {gl} {gh}");
    let ls: Vec<_> = std::fs::read_dir(&d)
        .unwrap()
        .flatten()
        .map(|f| f.file_name())
        .collect();
    assert_eq!(ls.len(), 4, "no temporary or scratch file is left: {ls:?}");
    assert!(
        std::fs::read_dir(e.rt.cache.merge_dir("x").parent().unwrap())
            .is_ok_and(|mut r| r.next().is_none()),
        "scratch removed"
    );

    // A name that is taken fails the job before anything is written or replaced.
    let kept = std::fs::read(&v).unwrap();
    let clash = json!({ "type": "Separate", "info": audio_info(), "vocals": v, "guide": d.join("g2.opus") });
    let r = run_kind(&e, &song, clash).await;
    assert_eq!(r["state"], "failed", "{r}");
    assert!(
        r["error"].as_str().unwrap().contains("output exists"),
        "{r}"
    );
    assert_eq!(std::fs::read(&v).unwrap(), kept);
    assert!(!d.join("g2.opus").exists());
    let ls: Vec<_> = std::fs::read_dir(&d).unwrap().flatten().collect();
    assert_eq!(ls.len(), 4);
}

#[tokio::test]
async fn separate_cancelled_leaves_no_file() {
    let Some(e) = real().await else { return };
    let d = dir(&e, "cancel");
    let song = d.join("long.flac");
    let tone =
        "sine=frequency=440:sample_rate=44100:duration=1500,aformat=channel_layouts=stereo[o]";
    if !make(&e, &song, tone) {
        eprintln!("ffmpeg cannot make the tone: skipped");
        return;
    }
    let out = d.join("v.opus");
    let r = e
        .control(
            "POST",
            "/v1/jobs",
            Some(
                json!({ "kind": { "type": "Separate", "info": audio_info(), "vocals": out },
                "media": { "mediaFileId": 5, "path": song } }),
            ),
        )
        .await;
    let id = r.json()["jobId"].as_str().unwrap().to_string();
    let tmp = |d: &Path| {
        std::fs::read_dir(d)
            .unwrap()
            .flatten()
            .any(|f| f.file_name().to_string_lossy().ends_with(".tmp"))
    };
    until_async("claimed", 30, || async { tmp(&d) }).await;
    assert_eq!(
        e.control("DELETE", &format!("/v1/jobs/{id}"), None)
            .await
            .status,
        204
    );
    let j = e
        .control("GET", &format!("/v1/jobs/{id}"), None)
        .await
        .json();
    assert_eq!(j["state"], "cancelled", "{j}");
    until_async("temporary file removed", 30, || async { !tmp(&d) }).await;
    assert!(!out.exists());
}

#[tokio::test]
async fn melody_finds_the_notes_of_each_span() {
    let Some(e) = real().await else { return };
    let d = dir(&e, "mel");
    let stem = d.join("vocals.flac");
    // 220 Hz for a second, half a second of silence, 330 Hz for a second.
    let graph = "sine=frequency=220:sample_rate=16000:duration=1,volume=0.5[a];\
                 anullsrc=r=16000:cl=mono:d=0.5[s];\
                 sine=frequency=330:sample_rate=16000:duration=1,volume=0.5[b];\
                 [a][s][b]concat=n=3:v=0:a=1[o]";
    if !make(&e, &stem, graph) {
        eprintln!("ffmpeg cannot make the tones: skipped");
        return;
    }
    let kind = json!({ "type": "Melody", "spans": [
        { "startMs": 0, "endMs": 1000 }, { "startMs": 1050, "endMs": 1450 },
        { "startMs": 1500, "endMs": 2500 }] });
    let r = run_kind(&e, &stem, kind).await;
    assert_eq!(r["state"], "done", "{r}");
    let m = &r["result"]["melody"];
    assert_eq!(
        (m["model"].as_str(), m["modelVersion"].as_str()),
        (Some("swiftf0"), Some("1"))
    );
    let spans = m["spans"].as_array().unwrap();
    assert_eq!(spans.len(), 3);
    let first = &spans[0][0];
    // The zero-crossing fake is within a few Hz: a semitone is plenty.
    assert!((first["midi"].as_f64().unwrap() - 57.0).abs() < 1.0, "{m}");
    assert!(
        first["startMs"].as_u64().unwrap() < 100 && first["endMs"].as_u64().unwrap() > 900,
        "{m}"
    );
    assert!(
        spans[1].as_array().unwrap().is_empty(),
        "silence has no note: {m}"
    );
    assert!(
        (spans[2][0]["midi"].as_f64().unwrap() - 64.0).abs() < 1.0,
        "{m}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn align_times_the_lyrics_from_what_the_recogniser_heard() {
    use std::os::unix::fs::PermissionsExt;
    let Some(e) = real().await else { return };
    let d = dir(&e, "align");
    let stem = d.join("vocals.flac");
    if !make(
        &e,
        &stem,
        "sine=frequency=220:sample_rate=16000:duration=4[o]",
    ) {
        eprintln!("ffmpeg cannot make the tone: skipped");
        return;
    }
    // A recogniser that logs its argv and hears three words, one of them misread.
    let script = d.join("whisper-cli");
    let args = d.join("args.txt");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {args}\nwhile [ $# -gt 0 ]; do [ \"$1\" = -of ] && of=\"$2\"; shift; done\n\
             cat > \"$of.json\" <<'J'\n{{\"transcription\":[\
             {{\"offsets\":{{\"from\":500,\"to\":900}},\"text\":\" Hello\"}},\
             {{\"offsets\":{{\"from\":900,\"to\":1500}},\"text\":\" word\"}},\
             {{\"offsets\":{{\"from\":2500,\"to\":3000}},\"text\":\" again\"}}]}}\nJ\n",
            args = args.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    e.rt.karaoke.set_tools(Tools {
        whisper: Some(script.to_string_lossy().into_owned()),
        pitch_ffmpeg: None,
        whisper_backend: "cpu",
    });
    let kind = json!({ "type": "Align", "language": "en", "lines": [
        { "text": "Hello, world" }, { "text": "again (x2)" }] });
    let r = run_kind(&e, &stem, kind).await;
    assert_eq!(r["state"], "done", "{r}");
    let a = &r["result"]["align"];
    assert_eq!(
        (a["model"].as_str(), a["modelVersion"].as_str()),
        (Some("whisper-small-q5_1"), Some("1"))
    );
    let lines = a["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 3, "the x2 line comes twice: {a}");
    assert_eq!(
        (lines[0]["startMs"].as_u64(), lines[0]["endMs"].as_u64()),
        (Some(500), Some(1500))
    );
    let w = &lines[0]["words"];
    assert_eq!(
        (
            w[0]["text"].as_str(),
            w[0]["byteStart"].as_u64(),
            w[0]["byteEnd"].as_u64()
        ),
        (Some("Hello,"), Some(0), Some(6))
    );
    assert_eq!(w[1]["text"], "world");
    assert_eq!(
        (
            lines[1]["line"].as_u64(),
            lines[1]["repeat"].as_u64(),
            lines[2]["repeat"].as_u64()
        ),
        (Some(1), Some(0), Some(1))
    );
    assert_eq!(lines[1]["words"][0]["startMs"], 2500);
    assert!(a["matched"].as_f64().unwrap() > 0.7, "{a}");
    assert_eq!(a["backend"], "cpu");
    assert!(
        std::fs::read_to_string(&args)
            .unwrap()
            .lines()
            .any(|l| l == "-ng"),
        "a CPU backend passes -ng"
    );
    let given = std::fs::read_to_string(&args).unwrap();
    for want in ["-ml", "1", "-l", "en", "--prompt", "Hello, world again"] {
        assert!(given.lines().any(|l| l == want), "{want} in {given}");
    }
    assert!(!d.join("words.json").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn align_repeats_a_failed_gpu_run_on_the_cpu() {
    use std::os::unix::fs::PermissionsExt;
    let Some(e) = real().await else { return };
    let d = dir(&e, "align-gpu");
    let stem = d.join("vocals.flac");
    if !make(
        &e,
        &stem,
        "sine=frequency=220:sample_rate=16000:duration=2[o]",
    ) {
        eprintln!("ffmpeg cannot make the tone: skipped");
        return;
    }
    // A recogniser whose GPU run dies and whose `-ng` run hears one word.
    let script = d.join("whisper-cli");
    std::fs::write(
        &script,
        "#!/bin/sh\ngpu=1\nwhile [ $# -gt 0 ]; do [ \"$1\" = -of ] && of=\"$2\"; [ \"$1\" = -ng ] && gpu=0; shift; done\n\
         [ $gpu = 1 ] && { echo device lost >&2; exit 1; }\n\
         echo '{\"transcription\":[{\"offsets\":{\"from\":100,\"to\":500},\"text\":\" Hello\"}]}' > \"$of.json\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    e.rt.karaoke.set_tools(Tools {
        whisper: Some(script.to_string_lossy().into_owned()),
        pitch_ffmpeg: None,
        whisper_backend: "vulkan",
    });
    let kind = json!({ "type": "Align", "language": "en", "lines": [{ "text": "Hello" }] });
    let r = run_kind(&e, &stem, kind).await;
    assert_eq!(r["state"], "done", "{r}");
    assert_eq!(r["result"]["align"]["backend"], "cpu");
}

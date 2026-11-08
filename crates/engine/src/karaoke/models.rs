//! The model manager: a built-in manifest of models, downloaded on the host's request into the
//! models directory, hash-checked, never shipped. Spec: `docs/spec/karaoke.md` section 2.
use crate::events::Events;
use crate::lock::Locked;
use bytes::Bytes;
use fliks_engine_core::api::{
    EngineEvent, ModelFile, ModelInfo, ModelKind, ModelLicence, ModelState, job_errors,
};
use http_body_util::{BodyExt, Empty};
use hyper::{Request, StatusCode, Uri};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;

const MAX_REDIRECTS: usize = 5;
const PROGRESS_EVERY: Duration = Duration::from_secs(1);
/// A download that moves no byte for this long is given up.
const STALL: Duration = Duration::from_secs(60);

/// One model of the manifest.
pub struct Entry {
    pub id: &'static str,
    pub kind: ModelKind,
    pub name: &'static str,
    pub version: &'static str,
    pub licence: &'static str,
    pub licence_text: String,
    pub clear: bool,
    pub default: bool,
    pub files: Vec<FileEntry>,
}

pub struct FileEntry {
    pub name: &'static str,
    pub url: String,
    pub sha256: &'static str,
    pub size: u64,
}

const MIT: &str = "Permission is hereby granted, free of charge, to any person obtaining a copy of \
this software and associated documentation files (the \"Software\"), to deal in the Software \
without restriction, including without limitation the rights to use, copy, modify, merge, \
publish, distribute, sublicense, and/or sell copies of the Software, and to permit persons to \
whom the Software is furnished to do so, subject to the following conditions: The above \
copyright notice and this permission notice shall be included in all copies or substantial \
portions of the Software. THE SOFTWARE IS PROVIDED \"AS IS\", WITHOUT WARRANTY OF ANY KIND, \
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR \
A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE \
LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR \
OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS \
IN THE SOFTWARE.";

const SPLEETER: &str = "MIT License. Copyright (c) 2019-present, Deezer SA. The ONNX export is \
published by the sherpa-onnx project.";
const SWIFTF0: &str = "MIT License. Copyright (c) 2025-2026 Lars Nieradzik.";
const WHISPER: &str = "MIT License. Copyright (c) 2022 OpenAI. The ggml conversion is \
published by the whisper.cpp project.";

const SPLEETER_REV: &str = "7001ba316a615cacddb3f9ef3ec416661a277e26";
const SWIFTF0_REV: &str = "d1ba77fe5331310b778ebe126b6cb3656d41d24f";
const WHISPER_REV: &str = "5359861c739e955e79d9a303bcbc70fb988958b1";

fn whisper_url(file: &str) -> String {
    format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/{WHISPER_REV}/{file}")
}

/// The models this engine knows. Pinned to a revision of their repository: a moved file fails
/// its hash instead of changing what runs.
pub fn manifest() -> Vec<Entry> {
    let spleeter = |file: &'static str| {
        format!(
            "https://huggingface.co/csukuangfj/sherpa-onnx-spleeter-2stems/resolve/{SPLEETER_REV}/{file}"
        )
    };
    vec![
        Entry {
            id: "spleeter-2stems",
            kind: ModelKind::Separation,
            name: "Spleeter 2 stems (vocals, accompaniment)",
            version: "1",
            licence: "MIT",
            licence_text: with_mit(SPLEETER),
            clear: true,
            default: true,
            files: vec![
                FileEntry {
                    name: "vocals.onnx",
                    url: spleeter("vocals.onnx"),
                    sha256: "bdc16ab6bf6117ddd4842c19e80e40e2be188fc555295064d424616b0224ac97",
                    size: 39_318_336,
                },
                FileEntry {
                    name: "accompaniment.onnx",
                    url: spleeter("accompaniment.onnx"),
                    sha256: "671ace17acd3720674a2bc14de32ac6292453dec20d9eb0ba4255d4ad8e3d8c0",
                    size: 39_318_343,
                },
            ],
        },
        Entry {
            id: "swiftf0",
            kind: ModelKind::Melody,
            name: "SwiftF0 (voice pitch)",
            version: "1",
            licence: "MIT",
            licence_text: with_mit(SWIFTF0),
            clear: true,
            default: true,
            files: vec![FileEntry {
                name: "model.onnx",
                url: format!(
                    "https://raw.githubusercontent.com/lars76/swift-f0/{SWIFTF0_REV}/swift_f0/model.onnx"
                ),
                sha256: "6385e8c2ebc3872e82c9ff5946228de44cd3be77a750ea53698b7dbfe94b0a22",
                size: 135_090,
            }],
        },
        Entry {
            id: "whisper-small-q5_1",
            kind: ModelKind::Alignment,
            name: "Whisper small, 5-bit (190 MB)",
            version: "1",
            licence: "MIT",
            licence_text: with_mit(WHISPER),
            clear: true,
            default: true,
            files: vec![FileEntry {
                name: "model.bin",
                url: whisper_url("ggml-small-q5_1.bin"),
                sha256: "ae85e4a935d7a567bd102fe55afc16bb595bdb618e11b2fc7591bc08120411bb",
                size: 190_085_487,
            }],
        },
    ]
}

/// Manifest ids that were dropped: their directory is removed at start.
const RETIRED: &[&str] = &["whisper-base-q5_1"];

/// Deletes what a retired model left in the models directory.
pub fn remove_retired(dir: &Path) {
    for id in RETIRED {
        let _ = std::fs::remove_dir_all(dir.join(id));
    }
}

/// The copyright line of a licence note followed by the MIT terms.
fn with_mit(note: &str) -> String {
    format!("{note} {MIT}")
}

struct Download {
    /// Tells a task's own entry from a later one of the same model.
    generation: u64,
    bytes: u64,
    error: Option<String>,
    cancel: watch::Sender<bool>,
    running: bool,
}

struct Inner {
    dir: PathBuf,
    entries: Vec<Entry>,
    downloads: Mutex<HashMap<String, Download>>,
    generations: AtomicU64,
    events: Events,
}

#[derive(Clone)]
pub struct Manager(Arc<Inner>);

/// A model picked for a job, with where its files are.
pub struct Picked {
    pub id: &'static str,
    pub version: &'static str,
    pub dir: PathBuf,
}

impl Picked {
    pub fn file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Manager {
    pub fn new(dir: PathBuf, entries: Vec<Entry>, events: Events) -> Self {
        Self(Arc::new(Inner {
            dir,
            entries,
            downloads: Mutex::new(HashMap::new()),
            generations: AtomicU64::new(0),
            events,
        }))
    }

    pub fn dir(&self) -> &Path {
        &self.0.dir
    }

    fn entry(&self, id: &str) -> Option<&Entry> {
        self.0.entries.iter().find(|e| e.id == id)
    }

    fn model_dir(&self, e: &Entry) -> PathBuf {
        self.0.dir.join(e.id)
    }

    /// Every file is on disk with its size (the hash was checked before it was kept).
    fn installed(&self, e: &Entry) -> bool {
        let dir = self.model_dir(e);
        e.files
            .iter()
            .all(|f| std::fs::metadata(dir.join(f.name)).is_ok_and(|m| m.len() == f.size))
    }

    fn info(&self, e: &Entry) -> ModelInfo {
        let total: u64 = e.files.iter().map(|f| f.size).sum();
        let (state, bytes, error) = {
            let dl = self.0.downloads.locked();
            match dl.get(e.id) {
                Some(d) if d.running => (ModelState::Downloading, d.bytes, None),
                _ if self.installed(e) => (ModelState::Installed, total, None),
                Some(Download {
                    error: Some(err), ..
                }) => (ModelState::Failed, 0, Some(err.clone())),
                _ => (ModelState::Absent, 0, None),
            }
        };
        ModelInfo {
            id: e.id.into(),
            kind: e.kind,
            name: e.name.into(),
            version: e.version.into(),
            licence: ModelLicence {
                name: e.licence.into(),
                text: e.licence_text.clone(),
                clear: e.clear,
            },
            default: e.default,
            files: e
                .files
                .iter()
                .map(|f| ModelFile {
                    name: f.name.into(),
                    url: f.url.clone(),
                    sha256: f.sha256.into(),
                    size_bytes: f.size as f64,
                })
                .collect(),
            state,
            downloaded_bytes: bytes as f64,
            error,
        }
    }

    pub fn list(&self) -> Vec<ModelInfo> {
        self.0.entries.iter().map(|e| self.info(e)).collect()
    }

    pub fn get(&self, id: &str) -> Option<ModelInfo> {
        self.entry(id).map(|e| self.info(e))
    }

    fn emit(&self, id: &str) {
        if let Some(m) = self.get(id) {
            self.0
                .events
                .emit(EngineEvent::ModelChanged { model: Box::new(m) });
        }
    }

    /// The model a job runs: `id`, else the default of `kind`; it must be installed.
    pub fn pick(&self, id: Option<&str>, kind: ModelKind) -> Result<Picked, String> {
        let e = match id {
            Some(id) => self.entry(id).ok_or_else(|| {
                job_errors::message(job_errors::MODEL_MISSING, format!("unknown model {id}"))
            })?,
            None => self
                .0
                .entries
                .iter()
                .find(|e| e.kind == kind && e.default)
                .ok_or_else(|| {
                    job_errors::message(job_errors::MODEL_MISSING, "no default model")
                })?,
        };
        if e.kind != kind {
            return Err(job_errors::message(
                job_errors::MODEL_MISSING,
                format!("{} is not a {kind:?} model", e.id).to_lowercase(),
            ));
        }
        if !self.installed(e) {
            return Err(job_errors::message(
                job_errors::MODEL_MISSING,
                format!(
                    "{} is not installed (POST /v1/models/{}/download)",
                    e.id, e.id
                ),
            ));
        }
        Ok(Picked {
            id: e.id,
            version: e.version,
            dir: self.model_dir(e),
        })
    }

    /// Starts the download of `id` unless it is installed or already running; `None` for an
    /// unknown id. The hash of every file is checked before the file is kept.
    pub fn start(&self, id: &str) -> Option<ModelInfo> {
        let e = self.entry(id)?;
        if !self.installed(e) {
            let (tx, rx) = watch::channel(false);
            let generation = self.0.generations.fetch_add(1, Ordering::Relaxed);
            {
                let mut dl = self.0.downloads.locked();
                if dl.get(id).is_some_and(|d| d.running) {
                    drop(dl);
                    return self.get(id);
                }
                dl.insert(
                    id.into(),
                    Download {
                        generation,
                        bytes: 0,
                        error: None,
                        cancel: tx,
                        running: true,
                    },
                );
            }
            let me = self.clone();
            let id = e.id;
            tokio::spawn(async move {
                let result = me.fetch(id, generation, rx).await;
                {
                    let mut dl = me.0.downloads.locked();
                    if let Some(d) = dl.get_mut(id).filter(|d| d.generation == generation) {
                        d.running = false;
                        d.error = result.err();
                    }
                }
                me.emit(id);
            });
            self.emit(id);
        }
        self.get(id)
    }

    /// Removes the model, stopping its download; false for an unknown id.
    pub async fn delete(&self, id: &str) -> bool {
        let Some(e) = self.entry(id) else {
            return false;
        };
        if let Some(d) = self.0.downloads.locked().remove(id) {
            d.cancel.send_replace(true);
        }
        let dir = self.model_dir(e);
        let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(dir)).await;
        self.emit(id);
        true
    }

    async fn fetch(
        &self,
        id: &'static str,
        generation: u64,
        cancel: watch::Receiver<bool>,
    ) -> Result<(), String> {
        let e = self.entry(id).ok_or("unknown model")?;
        let dir = self.model_dir(e);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| format!("create {}: {e}", dir.display()))?;
        let client = client();
        let mut done_before = 0u64;
        for f in &e.files {
            let dest = dir.join(f.name);
            if std::fs::metadata(&dest).is_ok_and(|m| m.len() == f.size) {
                done_before += f.size;
                self.set_bytes(id, generation, done_before);
                continue;
            }
            let part = dir.join(format!("{}.part", f.name));
            let r = self
                .fetch_file(
                    &client,
                    f,
                    &part,
                    done_before,
                    id,
                    generation,
                    cancel.clone(),
                )
                .await;
            if let Err(err) = r {
                if self.is_current(id, generation) {
                    let _ = tokio::fs::remove_file(&part).await;
                }
                return Err(err);
            }
            tokio::fs::rename(&part, &dest)
                .await
                .map_err(|e| format!("keep {}: {e}", f.name))?;
            done_before += f.size;
        }
        Ok(())
    }

    fn set_bytes(&self, id: &str, generation: u64, bytes: u64) {
        let mut dl = self.0.downloads.locked();
        if let Some(d) = dl.get_mut(id).filter(|d| d.generation == generation) {
            d.bytes = bytes;
        }
    }

    /// False once the model was deleted (and maybe restarted) since this task began.
    fn is_current(&self, id: &str, generation: u64) -> bool {
        let dl = self.0.downloads.locked();
        dl.get(id).is_some_and(|d| d.generation == generation)
    }

    #[allow(clippy::too_many_arguments)]
    async fn fetch_file(
        &self,
        client: &Client<HttpsConnector<HttpConnector>, Empty<Bytes>>,
        f: &FileEntry,
        part: &Path,
        done_before: u64,
        id: &'static str,
        generation: u64,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<(), String> {
        let mut body = open(client, &f.url).await?;
        let mut file = tokio::fs::File::create(part)
            .await
            .map_err(|e| format!("create {}: {e}", part.display()))?;
        let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
        let (mut got, mut last) = (0u64, Instant::now() - PROGRESS_EVERY);
        loop {
            let frame = tokio::select! {
                _ = cancel.wait_for(|c| *c) => return Err("cancelled".into()),
                r = tokio::time::timeout(STALL, body.frame()) => r
                    .map_err(|_| format!("{} stalled", f.name))?,
            };
            let Some(frame) = frame else { break };
            let frame = frame.map_err(|e| format!("download {}: {e}", f.name))?;
            let Some(chunk) = frame.data_ref() else {
                continue;
            };
            got += chunk.len() as u64;
            if got > f.size {
                return Err(format!("{} is larger than its manifest size", f.name));
            }
            hash.update(chunk);
            file.write_all(chunk)
                .await
                .map_err(|e| format!("write {}: {e}", part.display()))?;
            self.set_bytes(id, generation, done_before + got);
            if last.elapsed() >= PROGRESS_EVERY {
                last = Instant::now();
                self.emit(id);
            }
        }
        if got != f.size {
            return Err(format!("{} is {got} bytes, expected {}", f.name, f.size));
        }
        let digest = hex(hash.finish().as_ref());
        if digest != f.sha256 {
            return Err(format!("{} does not match its sha256 ({digest})", f.name));
        }
        file.sync_all()
            .await
            .map_err(|e| format!("write {}: {e}", part.display()))
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn client() -> Client<HttpsConnector<HttpConnector>, Empty<Bytes>> {
    let tls = crate::tls_builder().https_or_http().enable_http1().build();
    Client::builder(TokioExecutor::new()).build(tls)
}

/// The body of `url` after its redirects (the same scheme rules for every hop).
async fn open(
    client: &Client<HttpsConnector<HttpConnector>, Empty<Bytes>>,
    url: &str,
) -> Result<hyper::body::Incoming, String> {
    let mut url = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        let uri: Uri = url.parse().map_err(|e| format!("bad url {url}: {e}"))?;
        let req = Request::get(uri.clone())
            .header(
                "user-agent",
                concat!("fliks-engine/", env!("CARGO_PKG_VERSION")),
            )
            .body(Empty::new())
            .map_err(|e| e.to_string())?;
        let res = tokio::time::timeout(STALL, client.request(req))
            .await
            .map_err(|_| format!("{url} did not answer"))?
            .map_err(|e| format!("{url}: {e}"))?;
        let status = res.status();
        if status.is_redirection() {
            let to = res
                .headers()
                .get(hyper::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| format!("{url}: redirect without a location"))?;
            let next = absolute(&uri, to)?;
            if uri.scheme_str() == Some("https") && next.starts_with("http://") {
                return Err(format!("{url}: redirect to http refused"));
            }
            url = next;
            continue;
        }
        if status != StatusCode::OK {
            return Err(format!("{url}: HTTP {status}"));
        }
        return Ok(res.into_body());
    }
    Err("too many redirects".into())
}

/// `location` resolved against the request it answers.
fn absolute(base: &Uri, location: &str) -> Result<String, String> {
    if location.starts_with("http://") || location.starts_with("https://") {
        return Ok(location.into());
    }
    let (scheme, auth) = base
        .scheme_str()
        .zip(base.authority())
        .ok_or("redirect from a relative url")?;
    if location.starts_with('/') {
        return Ok(format!("{scheme}://{auth}{location}"));
    }
    let dir = base.path().rsplit_once('/').map_or("", |(d, _)| d);
    Ok(format!("{scheme}://{auth}{dir}/{location}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_is_consistent() {
        let m = manifest();
        for kind in [
            ModelKind::Separation,
            ModelKind::Alignment,
            ModelKind::Melody,
        ] {
            assert_eq!(
                m.iter().filter(|e| e.kind == kind && e.default).count(),
                1,
                "{kind:?} needs exactly one default"
            );
        }
        for e in &m {
            assert!(!e.files.is_empty() && e.licence_text.contains(e.licence));
            for f in &e.files {
                assert!(f.url.starts_with("https://") && f.size > 0);
                assert_eq!(f.sha256.len(), 64);
                assert!(f.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
            }
        }
        let ids: std::collections::HashSet<_> = m.iter().map(|e| e.id).collect();
        assert_eq!(ids.len(), m.len());
    }

    #[test]
    fn a_retired_model_is_removed_and_not_listed() {
        let d = std::env::temp_dir().join(format!("retired-{}", std::process::id()));
        let old = d.join(RETIRED[0]);
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("model.bin"), b"x").unwrap();
        remove_retired(&d);
        assert!(!old.exists());
        assert!(manifest().iter().all(|e| !RETIRED.contains(&e.id)));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_redirect_resolves_against_its_request() {
        let base: Uri = "https://a.test/x/y.bin?q=1".parse().unwrap();
        assert_eq!(absolute(&base, "/z").unwrap(), "https://a.test/z");
        assert_eq!(absolute(&base, "z.bin").unwrap(), "https://a.test/x/z.bin");
        assert_eq!(
            absolute(&base, "http://b.test/c").unwrap(),
            "http://b.test/c"
        );
    }

    /// A server that accepts and never answers, with its accepted-connection count.
    async fn silent_server() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/m.bin", l.local_addr().unwrap());
        let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = n.clone();
        tokio::spawn(async move {
            let mut held = vec![];
            while let Ok((s, _)) = l.accept().await {
                c.fetch_add(1, Ordering::SeqCst);
                held.push(s);
            }
        });
        (url, n)
    }

    async fn reached(n: &std::sync::atomic::AtomicUsize, want: usize) {
        for _ in 0..500 {
            if n.load(Ordering::SeqCst) >= want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn test_manager(url: String) -> Manager {
        let entry = Entry {
            id: "t",
            kind: ModelKind::Melody,
            name: "t",
            version: "1",
            licence: "MIT",
            licence_text: "MIT".into(),
            clear: true,
            default: true,
            files: vec![FileEntry {
                name: "m.bin",
                url,
                sha256: "",
                size: 4,
            }],
        };
        let dir = std::env::temp_dir().join(format!("fliks-models-{}", std::process::id()));
        Manager::new(dir, vec![entry], Events::default())
    }

    #[tokio::test]
    async fn two_quick_starts_download_once() {
        let (url, n) = silent_server().await;
        let m = test_manager(url);
        let (a, b) = (m.clone(), m.clone());
        let _ = tokio::join!(
            tokio::spawn(async move { a.start("t") }),
            tokio::spawn(async move { b.start("t") })
        );
        reached(&n, 1).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(n.load(Ordering::SeqCst), 1);
        m.delete("t").await;
    }

    #[tokio::test]
    async fn a_cancelled_task_leaves_the_restarted_entry_alone() {
        let (url, n) = silent_server().await;
        let m = test_manager(url);
        m.start("t");
        reached(&n, 1).await;
        m.delete("t").await;
        m.start("t");
        reached(&n, 2).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(n.load(Ordering::SeqCst), 2);
        let info = m.get("t").unwrap();
        assert_eq!(info.state, ModelState::Downloading);
        assert!(info.error.is_none());
        m.delete("t").await;
    }
}

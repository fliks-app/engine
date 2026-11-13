//! The model manager: a built-in manifest of models, downloaded on the host's request into the
//! models directory, hash-checked, never shipped. Spec: `docs/spec/karaoke.md` section 2.
use crate::events::Events;
use crate::lock::Locked;
use bytes::Bytes;
use fliks_engine_core::api::{
    EngineEvent, ModelEngine, ModelFile, ModelInfo, ModelKind, ModelLicence, ModelOrphan,
    ModelState, job_errors,
};
use http_body_util::{BodyExt, Empty};
use hyper::{Request, StatusCode, Uri};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
    /// The model's card or repository.
    pub homepage: &'static str,
    /// Two-letter codes of the languages an alignment model is for; empty: every language.
    pub languages: &'static [&'static str],
    /// An alignment model run as a CTC network (else the speech recogniser's).
    pub ctc: bool,
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

const APACHE: &str = "Licensed under the Apache License, Version 2.0 \
(https://www.apache.org/licenses/LICENSE-2.0): the weights are used, converted to ONNX in half \
precision, and not otherwise modified, on an \"AS IS\" basis, without warranties or conditions of \
any kind.";
const WAV2VEC2_FR: &str = "Apache-2.0. wav2vec2 XLSR-53 (Meta AI), fine-tuned on French by Jonatas \
Grosman (jonatasgrosman/wav2vec2-large-xlsr-53-french).";
const WAV2VEC2_EN: &str = "Apache-2.0. wav2vec2 Large LV-60 self-trained, 960 h (Meta AI, \
facebook/wav2vec2-large-960h-lv60-self).";
const OMNIASR: &str = "Apache-2.0. Omnilingual ASR CTC 300M (Meta AI, facebook/omniASR-CTC-300M), \
ONNX conversion published by OpenVoiceOS (OpenVoiceOS/omnilingual-asr-ctc-300m-onnx).";

const SPLEETER_REV: &str = "7001ba316a615cacddb3f9ef3ec416661a277e26";
const SWIFTF0_REV: &str = "d1ba77fe5331310b778ebe126b6cb3656d41d24f";
const WHISPER_REV: &str = "5359861c739e955e79d9a303bcbc70fb988958b1";

fn whisper_url(file: &str) -> String {
    format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/{WHISPER_REV}/{file}")
}

/// A CTC model's file: the release asset `<id>.<name>` of the engine's repository.
fn ctc_file(id: &str, name: &'static str, sha256: &'static str, size: u64) -> FileEntry {
    FileEntry {
        name,
        url: format!(
            "https://github.com/fliks-app/engine/releases/download/models-ctc-v1/{id}.{name}"
        ),
        sha256,
        size,
    }
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
            homepage: "https://github.com/deezer/spleeter",
            languages: &[],
            ctc: false,
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
            homepage: "https://github.com/lars76/swift-f0",
            languages: &[],
            ctc: false,
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
            homepage: "https://huggingface.co/ggerganov/whisper.cpp",
            languages: &[],
            ctc: false,
            files: vec![FileEntry {
                name: "model.bin",
                url: whisper_url("ggml-small-q5_1.bin"),
                sha256: "ae85e4a935d7a567bd102fe55afc16bb595bdb618e11b2fc7591bc08120411bb",
                size: 190_085_487,
            }],
        },
        Entry {
            id: "wav2vec2-ctc-fr",
            kind: ModelKind::Alignment,
            name: "wav2vec2 French, CTC (634 MB)",
            version: "1",
            licence: "Apache-2.0",
            licence_text: format!("{WAV2VEC2_FR} {APACHE}"),
            clear: true,
            default: false,
            homepage: "https://huggingface.co/jonatasgrosman/wav2vec2-large-xlsr-53-french",
            languages: &["fr"],
            ctc: true,
            files: vec![
                ctc_file(
                    "wav2vec2-ctc-fr",
                    "model.onnx",
                    "b868c38eb5e124ffb6ddbcd8482f47239cb4066db97b67ceb47a9c80c2bd62db",
                    634_197_385,
                ),
                ctc_file(
                    "wav2vec2-ctc-fr",
                    "vocab.json",
                    "061a54098e7993f516da7edb41d0404411ff9cb786c7bbcf2f69b1926a6bf060",
                    1026,
                ),
            ],
        },
        Entry {
            id: "wav2vec2-ctc-en",
            kind: ModelKind::Alignment,
            name: "wav2vec2 English, CTC (634 MB)",
            version: "1",
            licence: "Apache-2.0",
            licence_text: format!("{WAV2VEC2_EN} {APACHE}"),
            clear: true,
            default: false,
            homepage: "https://huggingface.co/facebook/wav2vec2-large-960h-lv60-self",
            languages: &["en"],
            ctc: true,
            files: vec![
                ctc_file(
                    "wav2vec2-ctc-en",
                    "model.onnx",
                    "cc20dc5db7ed39b8f3bbbb34bbb5a9b3a3e1468701ed414562901c134275cdfd",
                    634_142_034,
                ),
                ctc_file(
                    "wav2vec2-ctc-en",
                    "vocab.json",
                    "11c0856d5dbcfd9f3cf8a8d138a782df7e6d1e7f97e4315bc1e4b1609aeaa09e",
                    643,
                ),
            ],
        },
        Entry {
            id: "omniasr-ctc-300m",
            kind: ModelKind::Alignment,
            name: "Omnilingual ASR 300M, CTC (653 MB)",
            version: "1",
            licence: "Apache-2.0",
            licence_text: format!("{OMNIASR} {APACHE}"),
            clear: true,
            default: false,
            homepage: "https://huggingface.co/facebook/omniASR-CTC-300M",
            languages: &[],
            ctc: true,
            files: vec![
                ctc_file(
                    "omniasr-ctc-300m",
                    "model.onnx",
                    "1c92b3c113e9a3574b812e039433db2bf588d31ca90769179d2a4acf1b9fddd0",
                    652_837_719,
                ),
                ctc_file(
                    "omniasr-ctc-300m",
                    "vocab.json",
                    "333eb690077bb195de573f85afbfb0b6ec296c520c8b416ec9fd7edaa0dee104",
                    173_163,
                ),
            ],
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

/// Written into a model's directory once its files are verified.
const RECORD: &str = ".installed.json";

/// What an install left on disk: the manifest version it came from and each file's hash.
#[derive(Serialize, Deserialize)]
struct Record {
    version: String,
    files: Vec<RecordFile>,
}

#[derive(Serialize, Deserialize)]
struct RecordFile {
    name: String,
    sha256: String,
    size: u64,
}

struct Download {
    /// Tells a task's own entry from a later one of the same model.
    generation: u64,
    bytes: u64,
    rate: Option<f64>,
    error: Option<String>,
    cancel: watch::Sender<bool>,
    running: bool,
    /// A verification, not a download: its error says the files on disk are bad.
    verify: bool,
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
    /// The version on disk, which an outdated model still runs.
    pub version: String,
    pub ctc: bool,
    pub dir: PathBuf,
}

impl Picked {
    pub fn file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

struct Status {
    state: ModelState,
    outdated: bool,
    bytes: u64,
    rate: Option<f64>,
    error: Option<String>,
}

/// Paces a task's progress events to one a second and measures its rate.
struct Ticker {
    at: Instant,
    bytes: u64,
}

impl Ticker {
    fn start(bytes: u64) -> Self {
        Self {
            at: Instant::now(),
            bytes,
        }
    }

    /// The bytes per second since the last tick, once a second has passed.
    fn due(&mut self, bytes: u64) -> Option<f64> {
        let s = self.at.elapsed();
        if s < PROGRESS_EVERY {
            return None;
        }
        let rate = (bytes - self.bytes) as f64 / s.as_secs_f64();
        *self = Self::start(bytes);
        Some(rate)
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

    /// Where a download is built before it replaces the installed copy.
    fn staging_dir(&self, e: &Entry) -> PathBuf {
        self.0.dir.join(format!(".{}.new", e.id))
    }

    fn record(&self, e: &Entry) -> Option<Record> {
        let raw = std::fs::read(self.model_dir(e).join(RECORD)).ok()?;
        serde_json::from_slice(&raw).ok()
    }

    /// The files an install holds: the recorded ones, else the manifest's.
    fn expected(e: &Entry, rec: Option<&Record>) -> Vec<RecordFile> {
        match rec {
            Some(r) => r
                .files
                .iter()
                .map(|f| RecordFile {
                    name: f.name.clone(),
                    sha256: f.sha256.clone(),
                    size: f.size,
                })
                .collect(),
            None => e
                .files
                .iter()
                .map(|f| RecordFile {
                    name: f.name.into(),
                    sha256: f.sha256.into(),
                    size: f.size,
                })
                .collect(),
        }
    }

    /// Every expected file is on disk with its size (the hash was checked before it was kept).
    fn present(&self, e: &Entry, rec: Option<&Record>) -> bool {
        let dir = self.model_dir(e);
        Self::expected(e, rec)
            .iter()
            .all(|f| std::fs::metadata(dir.join(&f.name)).is_ok_and(|m| m.len() == f.size))
    }

    fn installed(&self, e: &Entry) -> bool {
        self.present(e, self.record(e).as_ref())
    }

    fn status(&self, e: &Entry, rec: Option<&Record>) -> Status {
        let total: u64 = e.files.iter().map(|f| f.size).sum();
        let installed = self.present(e, rec);
        let dl = self.0.downloads.locked();
        let d = dl.get(e.id);
        let (state, bytes, rate) = match d {
            Some(d) if d.running => (ModelState::Downloading, d.bytes, d.rate),
            Some(Download {
                verify: true,
                error: Some(_),
                ..
            }) => (ModelState::Failed, 0, None),
            _ if installed => (ModelState::Installed, total, None),
            Some(Download { error: Some(_), .. }) => (ModelState::Failed, 0, None),
            _ => (ModelState::Absent, 0, None),
        };
        Status {
            state,
            outdated: installed && rec.is_some_and(|r| r.version != e.version),
            bytes,
            rate,
            error: d.and_then(|d| d.error.clone()),
        }
    }

    fn info(&self, e: &Entry) -> ModelInfo {
        let rec = self.record(e);
        let st = self.status(e, rec.as_ref());
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
            languages: e.languages.iter().map(|l| (*l).into()).collect(),
            engine: (e.kind == ModelKind::Alignment).then_some(if e.ctc {
                ModelEngine::Ctc
            } else {
                ModelEngine::Whisper
            }),
            homepage: e.homepage.into(),
            recommended: if e.kind == ModelKind::Alignment {
                e.ctc
            } else {
                e.default
            },
            size_bytes: e.files.iter().map(|f| f.size).sum::<u64>() as f64,
            disk_bytes: dir_size(&self.model_dir(e)) as f64,
            installed_version: rec.map(|r| r.version),
            outdated: st.outdated,
            state: st.state,
            downloaded_bytes: st.bytes as f64,
            bytes_per_second: st.rate,
            error: st.error,
        }
    }

    pub fn list(&self) -> Vec<ModelInfo> {
        self.0.entries.iter().map(|e| self.info(e)).collect()
    }

    pub fn get(&self, id: &str) -> Option<ModelInfo> {
        self.entry(id).map(|e| self.info(e))
    }

    /// Free space of the filesystem that holds (or will hold) the models directory.
    pub fn free_bytes(&self) -> Option<u64> {
        let near = self.0.dir.ancestors().find(|p| p.exists())?;
        crate::jobs::disk::free_bytes(near)
    }

    /// What the models directory takes on disk, caches included.
    pub fn used_bytes(&self) -> u64 {
        dir_size(&self.0.dir)
    }

    /// Directories no manifest entry names; hidden ones are caches and staging.
    pub fn orphans(&self) -> Vec<ModelOrphan> {
        let Ok(rd) = std::fs::read_dir(&self.0.dir) else {
            return vec![];
        };
        let mut v: Vec<_> = rd
            .flatten()
            .filter(|d| d.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|d| {
                let name = d.file_name().into_string().ok()?;
                (!name.starts_with('.') && self.entry(&name).is_none()).then(|| ModelOrphan {
                    size_bytes: dir_size(&d.path()) as f64,
                    name,
                })
            })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Removes an orphan directory; false when `name` is not one.
    pub async fn delete_orphan(&self, name: &str) -> bool {
        if !self.orphans().iter().any(|o| o.name == name) {
            return false;
        }
        let dir = self.0.dir.join(name);
        let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(dir)).await;
        true
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
        let rec = self.record(e);
        if !self.present(e, rec.as_ref()) {
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
            version: rec.map_or_else(|| e.version.into(), |r| r.version),
            ctc: e.ctc,
            dir: self.model_dir(e),
        })
    }

    /// The model of an `Align`: `id`, else the first installed CTC model for `language` (the
    /// specific entries come first in the manifest), else the default recogniser.
    pub fn pick_aligner(&self, id: Option<&str>, language: Option<&str>) -> Result<Picked, String> {
        let ctc = self.0.entries.iter().find(|e| {
            e.kind == ModelKind::Alignment
                && e.ctc
                && (e.languages.is_empty() || language.is_some_and(|l| e.languages.contains(&l)))
                && self.installed(e)
        });
        match (id, ctc) {
            (None, Some(e)) => self.pick(Some(e.id), ModelKind::Alignment),
            _ => self.pick(id, ModelKind::Alignment),
        }
    }

    /// Runs `run` as the task of `id` unless one runs; its result becomes the entry's error.
    fn spawn<F, Fut>(&self, id: &'static str, verify: bool, run: F)
    where
        F: FnOnce(Manager, u64, watch::Receiver<bool>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
    {
        let (tx, rx) = watch::channel(false);
        let generation = self.0.generations.fetch_add(1, Ordering::Relaxed);
        {
            let mut dl = self.0.downloads.locked();
            if dl.get(id).is_some_and(|d| d.running) {
                return;
            }
            dl.insert(
                id.into(),
                Download {
                    generation,
                    bytes: 0,
                    rate: None,
                    error: None,
                    cancel: tx,
                    running: true,
                    verify,
                },
            );
        }
        let me = self.clone();
        tokio::spawn(async move {
            let result = run(me.clone(), generation, rx).await;
            {
                let mut dl = me.0.downloads.locked();
                if let Some(d) = dl.get_mut(id).filter(|d| d.generation == generation) {
                    d.running = false;
                    d.rate = None;
                    d.error = result.err();
                }
            }
            me.emit(id);
        });
        self.emit(id);
    }

    /// Starts the download of `id` unless it is installed and current, or already running;
    /// `None` for an unknown id. An outdated or damaged model is fetched beside the installed
    /// copy, which stays in use until every file is verified. The hash of every file is checked
    /// before the file is kept.
    pub fn start(&self, id: &str) -> Option<ModelInfo> {
        let e = self.entry(id)?;
        let rec = self.record(e);
        let st = self.status(e, rec.as_ref());
        if st.state != ModelState::Installed || st.outdated {
            let id = e.id;
            self.spawn(id, false, move |me, generation, rx| async move {
                me.fetch(id, generation, rx).await
            });
        }
        self.get(id)
    }

    /// Re-hashes the files of an installed model; a mismatch leaves it `failed`. `None` for an
    /// unknown id; the model comes back `downloading` while the check runs, else nothing started.
    pub fn verify(&self, id: &str) -> Option<ModelInfo> {
        let e = self.entry(id)?;
        if self.installed(e) {
            let id = e.id;
            self.spawn(id, true, move |me, generation, rx| async move {
                me.check(id, generation, rx).await
            });
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
        let (dir, stage) = (self.model_dir(e), self.staging_dir(e));
        let _ = tokio::task::spawn_blocking(move || {
            let _ = std::fs::remove_dir_all(stage);
            std::fs::remove_dir_all(dir)
        })
        .await;
        self.emit(id);
        true
    }

    async fn check(
        &self,
        id: &'static str,
        generation: u64,
        cancel: watch::Receiver<bool>,
    ) -> Result<(), String> {
        let e = self.entry(id).ok_or("unknown model")?;
        let dir = self.model_dir(e);
        let mut done = 0;
        for f in Self::expected(e, self.record(e).as_ref()) {
            let digest = self
                .hash_file(&dir.join(&f.name), id, generation, done, cancel.clone())
                .await?;
            if digest != f.sha256 {
                return Err(format!("{} does not match its sha256 ({digest})", f.name));
            }
            done += f.size;
        }
        Ok(())
    }

    async fn fetch(
        &self,
        id: &'static str,
        generation: u64,
        cancel: watch::Receiver<bool>,
    ) -> Result<(), String> {
        let e = self.entry(id).ok_or("unknown model")?;
        let stage = self.staging_dir(e);
        tokio::fs::create_dir_all(&stage)
            .await
            .map_err(|e| format!("create {}: {e}", stage.display()))?;
        let client = client();
        let mut done_before = 0u64;
        for f in &e.files {
            let dest = stage.join(f.name);
            if std::fs::metadata(&dest).is_ok_and(|m| m.len() == f.size)
                && self
                    .hash_file(&dest, id, generation, done_before, cancel.clone())
                    .await
                    .is_ok_and(|d| d == f.sha256)
            {
                done_before += f.size;
                self.set_bytes(id, generation, done_before);
                continue;
            }
            let part = stage.join(format!("{}.part", f.name));
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
        let record = Record {
            version: e.version.into(),
            files: Self::expected(e, None),
        };
        tokio::fs::write(
            stage.join(RECORD),
            serde_json::to_vec(&record).map_err(|e| e.to_string())?,
        )
        .await
        .map_err(|e| format!("write {RECORD}: {e}"))?;
        self.swap(e, id, generation).await
    }

    /// Puts the staged copy in place of the installed one.
    async fn swap(&self, e: &Entry, id: &str, generation: u64) -> Result<(), String> {
        if !self.is_current(id, generation) {
            return Err("cancelled".into());
        }
        let (dir, stage) = (self.model_dir(e), self.staging_dir(e));
        let old = self.0.dir.join(format!(".{}.old", e.id));
        let _ = tokio::fs::remove_dir_all(&old).await;
        let had = tokio::fs::rename(&dir, &old).await.is_ok();
        if let Err(err) = tokio::fs::rename(&stage, &dir).await {
            if had {
                let _ = tokio::fs::rename(&old, &dir).await;
            }
            return Err(format!("install {}: {err}", e.id));
        }
        let _ = tokio::fs::remove_dir_all(&old).await;
        Ok(())
    }

    fn set_bytes(&self, id: &str, generation: u64, bytes: u64) {
        let mut dl = self.0.downloads.locked();
        if let Some(d) = dl.get_mut(id).filter(|d| d.generation == generation) {
            d.bytes = bytes;
        }
    }

    fn set_rate(&self, id: &str, generation: u64, rate: f64) {
        let mut dl = self.0.downloads.locked();
        if let Some(d) = dl.get_mut(id).filter(|d| d.generation == generation) {
            d.rate = Some(rate);
        }
    }

    /// False once the model was deleted (and maybe restarted) since this task began.
    fn is_current(&self, id: &str, generation: u64) -> bool {
        let dl = self.0.downloads.locked();
        dl.get(id).is_some_and(|d| d.generation == generation)
    }

    /// The sha256 of a file, reported as progress after `done_before` bytes of the task.
    async fn hash_file(
        &self,
        path: &Path,
        id: &str,
        generation: u64,
        done_before: u64,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<String, String> {
        let mut file = tokio::fs::File::open(path)
            .await
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
        let mut buf = vec![0u8; 1 << 20];
        let (mut got, mut tick) = (0u64, Ticker::start(done_before));
        loop {
            let n = tokio::select! {
                _ = cancel.wait_for(|c| *c) => return Err("cancelled".into()),
                r = file.read(&mut buf) => r.map_err(|e| format!("read {}: {e}", path.display()))?,
            };
            if n == 0 {
                return Ok(hex(hash.finish().as_ref()));
            }
            hash.update(&buf[..n]);
            got += n as u64;
            self.set_bytes(id, generation, done_before + got);
            if let Some(rate) = tick.due(done_before + got) {
                self.set_rate(id, generation, rate);
                self.emit(id);
            }
        }
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
        let (mut got, mut tick) = (0u64, Ticker::start(done_before));
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
            if let Some(rate) = tick.due(done_before + got) {
                self.set_rate(id, generation, rate);
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

/// Bytes under `p`, symlinks not followed.
fn dir_size(p: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(p) else {
        return 0;
    };
    rd.flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_size(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
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
        for e in m.iter().filter(|e| e.ctc) {
            let names: Vec<_> = e.files.iter().map(|f| f.name).collect();
            assert_eq!(names, ["model.onnx", "vocab.json"], "{}", e.id);
            assert!(e.kind == ModelKind::Alignment && !e.default);
            assert!(e.homepage.starts_with("https://"));
            assert!(e.languages.iter().all(|l| l.len() == 2));
            let url = format!("/models-ctc-v1/{}.model.onnx", e.id);
            assert!(e.files[0].url.ends_with(&url));
        }
        let ids: std::collections::HashSet<_> = m.iter().map(|e| e.id).collect();
        assert_eq!(ids.len(), m.len());
    }

    fn dir_with(ids: &[&str]) -> (PathBuf, Manager) {
        let d = std::env::temp_dir().join(format!("aligners-{}-{}", std::process::id(), ids.len()));
        let m = Manager::new(d.clone(), manifest(), Events::default());
        for id in ids {
            let e = m.entry(id).unwrap();
            std::fs::create_dir_all(m.model_dir(e)).unwrap();
            for f in &e.files {
                let file = std::fs::File::create(m.model_dir(e).join(f.name)).unwrap();
                file.set_len(f.size).unwrap();
            }
        }
        (d, m)
    }

    #[test]
    fn an_aligner_is_the_installed_ctc_model_of_the_language_else_the_recogniser() {
        let (d, m) = dir_with(&["wav2vec2-ctc-fr", "omniasr-ctc-300m", "whisper-small-q5_1"]);
        let id = |model, lang| m.pick_aligner(model, lang).map(|p| p.id);
        assert_eq!(id(None, Some("fr")), Ok("wav2vec2-ctc-fr"));
        assert_eq!(id(None, Some("de")), Ok("omniasr-ctc-300m"));
        assert_eq!(id(None, None), Ok("omniasr-ctc-300m"));
        assert_eq!(
            id(Some("whisper-small-q5_1"), Some("fr")),
            Ok("whisper-small-q5_1")
        );
        assert!(m.pick_aligner(None, Some("fr")).unwrap().ctc);
        // The English model is not installed: Omnilingual serves it.
        assert_eq!(id(None, Some("en")), Ok("omniasr-ctc-300m"));
        assert!(
            id(Some("wav2vec2-ctc-en"), Some("en"))
                .unwrap_err()
                .contains("not installed")
        );
        std::fs::remove_dir_all(&d).unwrap();
        let (d, m) = dir_with(&["whisper-small-q5_1"]);
        let p = m.pick_aligner(None, Some("fr")).unwrap();
        assert_eq!((p.id, p.ctc), ("whisper-small-q5_1", false));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_ticker_reports_the_rate_once_a_second() {
        let mut t = Ticker::start(0);
        assert_eq!(t.due(5), None);
        t.at = Instant::now() - Duration::from_secs(2);
        let rate = t.due(2000).unwrap();
        assert!((900.0..=1000.0).contains(&rate), "{rate}");
        assert_eq!(t.due(2001), None);
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
            homepage: "",
            languages: &[],
            ctc: false,
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

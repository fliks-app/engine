//! The CTC `Align`: the vocal stem at 16 kHz through a CTC network, the known lyrics forced
//! onto its per-frame log-probabilities. Spec: `docs/spec/karaoke.md` section 4.
use super::models::Picked;
use super::run::{StopOnDrop, pcm_cmd, read_pcm, unsupported};
use super::{Backend, Loaded, Net, Tensor, Warm};
use crate::jobs::Ctx;
use fliks_engine_core::api::{AlignLine, AlignMeta, AlignedLine, JobErrorClass, LrcFit};
use fliks_engine_core::jobs::ctc::{
    AlignError, Case, Emissions, FRAME_MS, Vocab, Voicing, align, fits, lyric_ids,
};
use fliks_engine_core::jobs::fbank::{STACKED, input_features};
use fliks_engine_core::jobs::melody::SAMPLE_RATE;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

const TOO_LARGE: &str = "the lyrics and the audio are too long for one CTC pass";
const NO_PATH: &str = "the lyrics cannot be laid on the audio in order";

/// A call takes at most this much audio (Omnilingual refuses 40 s), with context on each side,
/// unless the export says otherwise.
const CHUNK_MS: u32 = 30_000;
const CONTEXT_MS: u32 = 1000;
/// Share of voiced frames that makes an all-blank chunk suspect, unless the export says otherwise.
const RETRY_VOICED: f32 = 0.2;
/// The shortest input the network takes, 200 ms.
const MIN_SAMPLES: usize = 3200;
const SAMPLES_PER_MS: usize = SAMPLE_RATE as usize / 1000;

/// What the network takes, named so by `tools/ctc-export`.
#[derive(Deserialize, Debug, Clone, Copy, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Input {
    /// `input_values [1, n]`: the audio.
    #[default]
    Audio,
    /// `input_features [1, T, 160]`: the SeamlessM4T fbank of the audio.
    Fbank,
}

impl Input {
    fn name(self) -> &'static str {
        match self {
            Input::Audio => "input_values",
            Input::Fbank => "input_features",
        }
    }

    /// The warm-up input: one second of silence, as the network takes it.
    fn silence(self, samples: usize) -> Tensor {
        match self {
            Input::Audio => Tensor {
                shape: vec![1, samples as i64],
                data: vec![0.0; samples],
            },
            Input::Fbank => {
                let frames = (samples / 320).max(1);
                Tensor {
                    shape: vec![1, frames as i64, STACKED as i64],
                    data: vec![0.0; frames * STACKED],
                }
            }
        }
    }
}

/// A chunk blank-argmax on `blank` of its frames while `voiced` of them are voiced is run again
/// as two halves.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Retry {
    blank: f32,
    voiced: f32,
}

/// How a chunk of the track is fed to the network and read back.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Window {
    input: Input,
    normalize: bool,
    chunk_ms: u32,
    context_ms: u32,
    onset_ms: i32,
    retry: Option<Retry>,
}

impl Default for Window {
    fn default() -> Self {
        Window {
            input: Input::Audio,
            normalize: false,
            chunk_ms: CHUNK_MS,
            context_ms: CONTEXT_MS,
            onset_ms: 0,
            retry: None,
        }
    }
}

/// `vocab.json` of an export.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VocabFile {
    sample_rate: u32,
    frame_ms: u32,
    do_normalize: bool,
    blank: u32,
    delimiter: u32,
    vocab: HashMap<String, String>,
    #[serde(default)]
    input: Input,
    chunk_ms: Option<u32>,
    context_ms: Option<u32>,
    #[serde(default)]
    onset_ms: i32,
    retry_blank: Option<f32>,
    retry_voiced: Option<f32>,
}

#[derive(Debug)]
struct Export {
    vocab: Vocab,
    win: Window,
    /// The network must have more classes than the largest id of its vocabulary.
    top_id: u32,
}

impl Export {
    fn check(&self, classes: usize) -> Result<(), String> {
        if classes <= self.top_id as usize {
            return Err(format!(
                "the vocabulary has token {} but the network {classes} classes",
                self.top_id
            ));
        }
        Ok(())
    }
}

fn read_export(path: &Path, case: Case) -> Result<Export, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let f: VocabFile = serde_json::from_slice(&bytes)
        .map_err(|e| format!("{} is not a CTC vocabulary: {e}", path.display()))?;
    if f.sample_rate != SAMPLE_RATE || f.frame_ms != FRAME_MS {
        return Err(format!(
            "{} wants {} Hz and {} ms frames, not {SAMPLE_RATE} Hz and {FRAME_MS} ms",
            path.display(),
            f.sample_rate,
            f.frame_ms
        ));
    }
    let (mut chars, mut top_id) = (vec![], f.blank.max(f.delimiter));
    for (id, token) in &f.vocab {
        let id: u32 = id
            .parse()
            .map_err(|_| format!("{}: token id {id}", path.display()))?;
        top_id = top_id.max(id);
        let mut it = token.chars();
        if let (Some(c), None) = (it.next(), it.next())
            && id != f.blank
            && id != f.delimiter
        {
            chars.push((c, id));
        }
    }
    let (chunk_ms, context_ms) = (
        f.chunk_ms.unwrap_or(CHUNK_MS),
        f.context_ms.unwrap_or(CONTEXT_MS),
    );
    if chunk_ms == 0 || chunk_ms % FRAME_MS != 0 || context_ms % FRAME_MS != 0 {
        return Err(format!(
            "{}: chunkMs and contextMs must be multiples of {FRAME_MS} ms",
            path.display()
        ));
    }
    Ok(Export {
        vocab: Vocab::new(chars, f.blank, f.delimiter, case),
        win: Window {
            input: f.input,
            normalize: f.do_normalize && f.input == Input::Audio,
            chunk_ms,
            context_ms,
            onset_ms: f.onset_ms,
            retry: f.retry_blank.map(|blank| Retry {
                blank,
                voiced: f.retry_voiced.unwrap_or(RETRY_VOICED),
            }),
        },
        top_id,
    })
}

/// Zero mean and unit variance, as the checkpoints' feature extractor does.
fn normalise(x: &mut [f32]) {
    let n = x.len().max(1) as f32;
    let mean = x.iter().sum::<f32>() / n;
    let var = x.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / n;
    let scale = (var + 1e-7).sqrt().recip();
    x.iter_mut().for_each(|v| *v = (*v - mean) * scale);
}

/// The network over the decoded track, one call per chunk of a requested span.
struct Spans<'a> {
    net: &'a mut dyn Net,
    pcm: &'a [f32],
    win: Window,
    silent: &'a [bool],
    stop: &'a AtomicBool,
    pulse: &'a dyn Fn(),
}

/// `(frames, classes)` of a CTC network's answer.
fn frames_and_classes(out: &[Tensor]) -> Result<(usize, usize), String> {
    let [logits] = out else {
        return Err("unexpected CTC network output".into());
    };
    let &[1, frames, classes] = &logits.shape[..] else {
        return Err(format!("unexpected CTC output shape {:?}", logits.shape));
    };
    let (frames, classes) = (frames as usize, classes as usize);
    if logits.data.len() != frames * classes {
        return Err("CTC network answered the wrong size".into());
    }
    Ok((frames, classes))
}

impl Spans<'_> {
    /// The emissions of `from_ms..to_ms` (frame 0 at `from_ms`, a multiple of `FRAME_MS`) for
    /// the classes `ids`; a chunk that is silent throughout is not run, it is all blank.
    fn emit(&mut self, from_ms: u32, to_ms: u32, ids: &[u32]) -> Result<Emissions, String> {
        let frames = (to_ms - from_ms).div_ceil(FRAME_MS) as usize;
        let mut log_probs = Vec::with_capacity(frames * (ids.len() + 1));
        for at in (from_ms..to_ms).step_by(self.win.chunk_ms as usize) {
            let to = (at + self.win.chunk_ms).min(to_ms);
            let (a, b) = ((at / FRAME_MS) as usize, to.div_ceil(FRAME_MS) as usize);
            if self.silent.get(a..b).is_some_and(|s| s.iter().all(|v| *v)) {
                for _ in a..b {
                    log_probs.push(0.0);
                    log_probs.extend(std::iter::repeat_n(-30.0, ids.len()));
                }
                continue;
            }
            log_probs.extend(self.chunk(at, to, ids)?.log_probs);
        }
        Ok(Emissions {
            ids: ids.to_vec(),
            log_probs,
        })
    }

    /// The logits of `from_ms..to_ms` (a call with the context around it), and the class count.
    fn call(&mut self, from_ms: u32, to_ms: u32) -> Result<(Vec<f32>, usize), String> {
        if self.stop.load(Relaxed) {
            return Err("stopped".into());
        }
        let ctx = self.win.context_ms;
        let lead_ms = from_ms.min(ctx);
        let end = self.pcm.len();
        let a = ((from_ms - lead_ms) as usize * SAMPLES_PER_MS).min(end);
        let b = ((to_ms + ctx) as usize * SAMPLES_PER_MS).clamp(a, end);
        let mut audio = self.pcm[a..b].to_vec();
        if self.win.normalize {
            normalise(&mut audio);
        }
        audio.resize(audio.len().max(MIN_SAMPLES), 0.0);
        let input = match self.win.input {
            Input::Audio => Tensor {
                shape: vec![1, audio.len() as i64],
                data: audio,
            },
            Input::Fbank => {
                let data = input_features(&audio);
                Tensor {
                    shape: vec![1, (data.len() / STACKED) as i64, STACKED as i64],
                    data,
                }
            }
        };
        let out = self.net.run(vec![(self.win.input.name(), input)])?;
        (self.pulse)();
        let (frames, classes) = frames_and_classes(&out)?;
        let skip = ((lead_ms / FRAME_MS) as usize).min(frames);
        let want = (to_ms - from_ms).div_ceil(FRAME_MS) as usize;
        let take = want.min(frames - skip);
        let mut rows = out[0].data[skip * classes..(skip + take) * classes].to_vec();
        // An fbank call answers fewer frames than its audio spans: the grid stays at 20 ms.
        if self.win.input == Input::Fbank && take < want {
            if frames == 0 {
                return Err("CTC network answered no frame".into());
            }
            let last = out[0].data[(skip + take).max(1) * classes - classes..][..classes].to_vec();
            for _ in take..want {
                rows.extend_from_slice(&last);
            }
        }
        Ok((rows, classes))
    }

    /// One call on `from_ms..to_ms`; a chunk the network answers blank over voiced audio is
    /// kept as two calls on its halves when those find fewer blank frames.
    fn chunk(&mut self, from_ms: u32, to_ms: u32, ids: &[u32]) -> Result<Emissions, String> {
        let (mut rows, classes) = self.call(from_ms, to_ms)?;
        if let Some(retry) = self.win.retry {
            let blank = ids[0] as usize;
            let blanks = |r: &[f32]| {
                r.chunks_exact(classes)
                    .filter(|f| {
                        f.iter()
                            .enumerate()
                            .max_by(|a, b| a.1.total_cmp(b.1))
                            .unwrap()
                            .0
                            == blank
                    })
                    .count()
            };
            let n = rows.len() / classes;
            let (a, b) = (
                (from_ms / FRAME_MS) as usize,
                (from_ms / FRAME_MS) as usize + n,
            );
            let voiced = self
                .silent
                .get(a..b)
                .map_or(0, |s| s.iter().filter(|v| !**v).count());
            let mid = from_ms + (to_ms - from_ms) / 2 / FRAME_MS * FRAME_MS;
            let first = blanks(&rows);
            if n > 1
                && mid > from_ms
                && first as f32 >= retry.blank * n as f32
                && voiced as f32 >= retry.voiced * n as f32
            {
                let (mut halves, _) = self.call(from_ms, mid)?;
                halves.extend(self.call(mid, to_ms)?.0);
                if blanks(&halves) < first {
                    rows = halves;
                }
            }
        }
        Ok(Emissions::from_logits(&rows, classes, ids))
    }
}

/// The two-letter code of a three-letter one the manifest's languages use.
fn two_letter(code: &str) -> &str {
    match code {
        "eng" => "en",
        "fra" => "fr",
        "deu" => "de",
        "spa" => "es",
        "ita" => "it",
        "nld" => "nl",
        "por" => "pt",
        c => c,
    }
}

/// The singing's language: the request's, else the one the lyrics are written in when it is
/// reliable.
pub fn language_of(requested: Option<&str>, lines: &[AlignLine]) -> Option<String> {
    let text = lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    requested
        .or_else(|| {
            whatlang::detect(&text)
                .filter(whatlang::Info::is_reliable)
                .map(|i| i.lang().code())
        })
        .map(|c| two_letter(c).to_owned())
}

/// The lyrics placed by `picked`. `Ok(Err(_))`: the CTC path failed, the recogniser can stand in;
/// `Err(_)`: the job ends (cancel, stall, undecodable source).
pub async fn run(
    ctx: &Ctx,
    picked: &Picked,
    case: Case,
    lines: &[AlignLine],
    language: Option<&str>,
    providers: Option<&[String]>,
) -> Result<Result<AlignMeta, String>, String> {
    let backend = ctx.rt.karaoke.backend();
    let runtime = backend.runtime();
    if !runtime.onnx {
        return Ok(Err(unsupported(runtime.error.unwrap_or_default())));
    }
    let export = match read_export(&picked.file("vocab.json"), case) {
        Ok(export) => Arc::new(export),
        Err(e) => return Ok(Err(e)),
    };
    let file = picked.file("model.onnx");
    let allow = providers.unwrap_or_default().to_vec();
    let loaded = {
        let (backend, file, export) = (backend.clone(), file.clone(), export.clone());
        super::run::low(move || load(&*backend, &file, &allow, &export))
            .await
            .map_err(|e| e.to_string())?
    };
    let Loaded { net, provider, .. } = match loaded {
        Ok(loaded) => loaded,
        Err(e) => return Ok(Err(e)),
    };
    ctx.pulse();
    let cmd = pcm_cmd(ctx);
    let (c, lines, language) = (
        ctx.clone(),
        lines.to_vec(),
        language.unwrap_or_default().to_owned(),
    );
    let (provider, placed) = ctx
        .pipeline(ctx.run_timeout_ms(), vec![cmd], |p| async move {
            let pcm = read_pcm(&c, &p[0]).await?;
            if pcm.is_empty() {
                return Err(JobErrorClass::Corrupt.message("no audio decoded"));
            }
            let stop = Arc::new(AtomicBool::new(false));
            let _guard = StopOnDrop(stop.clone());
            super::run::low(move || {
                let _busy = c.model_busy();
                let duration_ms = u32::try_from(pcm.len() / SAMPLES_PER_MS).unwrap_or(u32::MAX);
                if fits(&lines, &export.vocab, &language, duration_ms).is_err() {
                    return (provider, Err(TOO_LARGE.to_string()));
                }
                let voice = Voicing::of(&pcm);
                let (provider, em) = place_or_retry(
                    net,
                    provider,
                    &stop,
                    || load(&*backend, &file, &[], &export),
                    |net| {
                        let spans = Spans {
                            net,
                            pcm: &pcm,
                            win: export.win,
                            silent: &voice.silent,
                            stop: &stop,
                            pulse: &|| c.pulse(),
                        };
                        emissions(spans, duration_ms, &lines, &language, &export)
                    },
                );
                let placed = em.and_then(|em| {
                    align(&lines, &export.vocab, &language, &voice, &em).map_err(|e| match e {
                        AlignError::TooLarge => TOO_LARGE.to_string(),
                        AlignError::NoPath => NO_PATH.to_string(),
                    })
                });
                let placed = placed.map(|mut a| {
                    shift(
                        &mut a.lines,
                        &mut a.lrc_fit,
                        export.win.onset_ms,
                        duration_ms,
                    );
                    a
                });
                (provider, placed)
            })
            .await
            .map_err(|e| e.to_string())
        })
        .await??;
    Ok(placed.map(|a| AlignMeta {
        model: picked.id.into(),
        model_version: picked.version.clone(),
        backend: format!("ctc-{provider}"),
        lines: a.lines,
        matched: a.matched,
        fallback: None,
        lrc_fit: a.lrc_fit,
    }))
}

/// Every time moved by `onset_ms` (a network that fires before or after the sound), inside the audio.
fn shift(lines: &mut [AlignedLine], fit: &mut Option<LrcFit>, onset_ms: i32, end_ms: u32) {
    if let Some(f) = fit {
        f.offset_ms += f64::from(onset_ms);
    }
    let at =
        |t: &mut u32| *t = (i64::from(*t) + i64::from(onset_ms)).clamp(0, end_ms.into()) as u32;
    for l in lines {
        at(&mut l.start_ms);
        at(&mut l.end_ms);
        for w in &mut l.words {
            at(&mut w.start_ms);
            at(&mut w.end_ms);
        }
    }
}

/// `go` on `net`; a hardware provider's failed run, a stop aside, is repeated once on the CPU.
fn place_or_retry<T>(
    mut net: Box<dyn Net>,
    provider: String,
    stop: &AtomicBool,
    cpu: impl FnOnce() -> Result<Loaded, String>,
    go: impl Fn(&mut dyn Net) -> Result<T, String>,
) -> (String, Result<T, String>) {
    match go(&mut *net) {
        Err(e) if provider != "cpu" && !stop.load(Relaxed) => {
            tracing::warn!(provider, "CTC run failed, repeating on the CPU: {e}");
            match cpu() {
                Ok(mut cpu) => (cpu.provider, go(&mut *cpu.net)),
                Err(e) => (provider, Err(e)),
            }
        }
        placed => (provider, placed),
    }
}

/// The network on the first provider of `allow` that runs it (`&[]`: the CPU), its class count
/// checked against the export's vocabulary on the warm-up answer (the CPU's own run of silence).
fn load(
    backend: &dyn Backend,
    file: &Path,
    allow: &[String],
    export: &Export,
) -> Result<Loaded, String> {
    let input = export.win.input;
    let warm: &[Warm] = &[(
        input.name(),
        match input {
            Input::Audio => &[1, SAMPLE_RATE as i64],
            Input::Fbank => &[1, 100, STACKED as i64],
        },
    )];
    let mut loaded = backend.load(file, Some(allow), warm)?;
    let answer = if loaded.warmed.is_empty() {
        loaded
            .net
            .run(vec![(input.name(), input.silence(MIN_SAMPLES))])?
    } else {
        std::mem::take(&mut loaded.warmed)
    };
    export.check(frames_and_classes(&answer)?.1)?;
    Ok(loaded)
}

/// The emissions of the whole track for the characters of `lines`; blocking.
fn emissions(
    mut spans: Spans,
    duration_ms: u32,
    lines: &[AlignLine],
    language: &str,
    export: &Export,
) -> Result<Emissions, String> {
    spans.emit(0, duration_ms, &lyric_ids(lines, &export.vocab, language))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fliks_engine_core::api::AlignedWord;

    fn line(text: &str) -> AlignLine {
        AlignLine {
            text: text.into(),
            start_ms: None,
            end_ms: None,
        }
    }

    #[test]
    fn the_language_is_the_requests_else_the_lyrics_when_reliable() {
        let fr = [
            line("Non, rien de rien, non, je ne regrette rien"),
            line("Ni le bien qu'on m'a fait, ni le mal"),
            line("Tout ça m'est bien égal, c'est payé, balayé, oublié"),
        ];
        assert_eq!(language_of(Some("de"), &fr).as_deref(), Some("de"));
        assert_eq!(language_of(Some("fra"), &fr).as_deref(), Some("fr"));
        assert_eq!(language_of(None, &fr).as_deref(), Some("fr"));
        assert_eq!(language_of(None, &[line("la la la")]), None);
    }

    #[test]
    fn a_vocabulary_keeps_single_characters_and_the_ids_of_its_classes() {
        let d = std::env::temp_dir().join(format!("fliks-ctc-vocab-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let file = d.join("vocab.json");
        let json = |frame_ms: u32| {
            format!(
                r#"{{"sampleRate":16000,"frameMs":{frame_ms},"doNormalize":true,"blank":0,
                "delimiter":2,"vocab":{{"0":"<pad>","1":"<unk>","2":"|","3":"a","7":"b"}}}}"#
            )
        };
        std::fs::write(&file, json(20)).unwrap();
        let e = read_export(&file, Case::Lower).unwrap();
        assert_eq!(
            e.win,
            Window {
                normalize: true,
                ..Window::default()
            }
        );
        assert_eq!(
            (e.win.chunk_ms, e.win.context_ms, e.win.onset_ms),
            (30_000, 1000, 0)
        );
        assert_eq!(e.top_id, 7);
        std::fs::write(&file, json(40)).unwrap();
        assert!(
            read_export(&file, Case::Lower)
                .unwrap_err()
                .contains("40 ms")
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn an_fbank_export_names_its_input_chunks_onset_and_retry() {
        let d = std::env::temp_dir().join(format!("fliks-ctc-fbank-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let file = d.join("vocab.json");
        std::fs::write(
            &file,
            r#"{"sampleRate":16000,"frameMs":20,"doNormalize":true,"blank":0,"delimiter":1,
            "vocab":{"0":"<blank>","1":"|","3":"a"},"input":"fbank","chunkMs":15000,
            "contextMs":2000,"onsetMs":-60,"retryBlank":0.99}"#,
        )
        .unwrap();
        let w = read_export(&file, Case::Lower).unwrap().win;
        assert_eq!(w.input, Input::Fbank);
        assert!(
            !w.normalize,
            "the fbank is normalised per band, not the audio"
        );
        assert_eq!((w.chunk_ms, w.context_ms, w.onset_ms), (15_000, 2000, -60));
        assert_eq!(
            w.retry,
            Some(Retry {
                blank: 0.99,
                voiced: RETRY_VOICED
            })
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn an_onset_moves_every_time_and_keeps_them_inside_the_audio_in_order() {
        let word = |a, b| AlignedWord {
            text: "x".into(),
            byte_start: 0,
            byte_end: 1,
            start_ms: a,
            end_ms: b,
            guessed: false,
        };
        let line = |words: Vec<AlignedWord>| AlignedLine {
            line: 0,
            repeat: 0,
            start_ms: words[0].start_ms,
            end_ms: words[words.len() - 1].end_ms,
            matched: 1.0,
            words,
        };
        let mut lines = [line(vec![word(30, 500), word(900, 1990)])];
        let mut fit = Some(LrcFit {
            offset_ms: 1000.0,
            scale: 1.0,
            unreliable: false,
        });
        shift(&mut lines, &mut fit, -60, 2000);
        assert_eq!(fit.map(|f| f.offset_ms), Some(940.0));
        let t: Vec<_> = lines[0]
            .words
            .iter()
            .map(|w| (w.start_ms, w.end_ms))
            .collect();
        assert_eq!(t, [(0, 440), (840, 1930)]);
        assert_eq!((lines[0].start_ms, lines[0].end_ms), (0, 1930));
        shift(&mut lines, &mut fit, 200, 2000);
        assert_eq!(fit.map(|f| f.offset_ms), Some(1140.0));
        shift(&mut lines, &mut None, 0, 2000);
        assert_eq!(lines[0].words[1].end_ms, 2000);
    }

    #[test]
    fn a_normalised_window_has_zero_mean_and_unit_variance() {
        let mut x: Vec<f32> = (0..1000)
            .map(|i| 3.0 + (i as f32 * 0.1).sin() * 0.5)
            .collect();
        normalise(&mut x);
        let mean = x.iter().sum::<f32>() / 1000.0;
        let var = x.iter().map(|v| v * v).sum::<f32>() / 1000.0;
        assert!(
            mean.abs() < 1e-4 && (var - 1.0).abs() < 1e-3,
            "{mean} {var}"
        );
    }

    /// Answers a frame per 320 samples, a one-hot of the class `k % classes` where `k` is the
    /// 20 ms index of the frame's first sample in the input `i / 16 000` of a test.
    struct Echo {
        calls: Vec<usize>,
        classes: usize,
    }

    impl Net for Echo {
        fn run(&mut self, mut inputs: Vec<(&'static str, Tensor)>) -> Result<Vec<Tensor>, String> {
            let x = inputs.remove(0).1;
            let frames = (x.data.len() - 400) / 320 + 1;
            self.calls.push(x.data.len());
            let mut data = vec![0.0; frames * self.classes];
            for f in 0..frames {
                let k = (x.data[f * 320] * 50.0).round() as usize;
                data[f * self.classes + k % self.classes] = 20.0;
            }
            Ok(vec![Tensor {
                shape: vec![1, frames as i64, self.classes as i64],
                data,
            }])
        }
    }

    fn spans<'a>(
        net: &'a mut dyn Net,
        pcm: &'a [f32],
        normalize: bool,
        stop: &'a AtomicBool,
    ) -> Spans<'a> {
        spans_with(net, pcm, normalize, &[], stop)
    }

    fn spans_with<'a>(
        net: &'a mut dyn Net,
        pcm: &'a [f32],
        normalize: bool,
        silent: &'a [bool],
        stop: &'a AtomicBool,
    ) -> Spans<'a> {
        Spans {
            net,
            pcm,
            win: Window {
                normalize,
                ..Window::default()
            },
            silent,
            stop,
            pulse: &|| (),
        }
    }

    #[test]
    fn a_long_span_goes_through_in_chunks_with_context_and_keeps_every_frame_once() {
        let pcm: Vec<f32> = (0..16_000 * 70).map(|i| i as f32 / 16_000.0).collect();
        let mut net = Echo {
            calls: vec![],
            classes: 64,
        };
        let stop = AtomicBool::new(false);
        let ids: Vec<u32> = (0..64).collect();
        let em = spans(&mut net, &pcm, false, &stop)
            .emit(5_000, 65_000, &ids)
            .unwrap();
        assert_eq!((em.frames(), em.classes()), (3000, 65));
        for t in 0..3000 {
            let row = &em.log_probs[t * 65..t * 65 + 64];
            let top = (0..64).max_by(|a, b| row[*a].total_cmp(&row[*b])).unwrap();
            assert_eq!(top, (250 + t) % 64, "frame {t}");
        }
        // Two calls of 30 s with 1 s of context on each side.
        assert_eq!(net.calls, [16_000 * 32; 2]);
    }

    #[test]
    fn a_chunk_silent_throughout_is_not_run() {
        let pcm = vec![0.1f32; 16_000 * 60];
        let mut net = Echo {
            calls: vec![],
            classes: 4,
        };
        let stop = AtomicBool::new(false);
        let mut silent = vec![true; 1500];
        silent.extend(vec![false; 1500]);
        let em = spans_with(&mut net, &pcm, false, &silent, &stop)
            .emit(0, 60_000, &[0, 3])
            .unwrap();
        assert_eq!((em.frames() >= 2999, net.calls.len()), (true, 1));
        assert_eq!(&em.log_probs[..3], &[0.0, -30.0, -30.0]);
    }

    #[test]
    fn a_span_ending_a_few_ms_into_a_chunk_does_not_run_past_the_answer() {
        let pcm = vec![0.1f32; 16_000 * 60 + 48];
        let mut net = Echo {
            calls: vec![],
            classes: 4,
        };
        let stop = AtomicBool::new(false);
        let em = spans(&mut net, &pcm, false, &stop)
            .emit(0, 60_003, &[0, 3])
            .unwrap();
        assert_eq!(em.frames(), 2999);
    }

    #[test]
    fn a_vocabulary_larger_than_the_network_is_an_error_not_a_silent_miss() {
        let ex = Export {
            vocab: Vocab::new([('a', 1)], 0, 2, Case::Lower),
            win: Window::default(),
            top_id: 9,
        };
        let e = ex.check(4).unwrap_err();
        assert!(e.contains("token 9") && e.contains("4 classes"), "{e}");
        assert!(ex.check(10).is_ok());
    }

    struct NoRun;

    impl Net for NoRun {
        fn run(&mut self, _: Vec<(&'static str, Tensor)>) -> Result<Vec<Tensor>, String> {
            panic!("the warm-up answer was enough");
        }
    }

    struct Warmed;

    impl Backend for Warmed {
        fn runtime(&self) -> super::super::BackendInfo {
            Default::default()
        }

        fn load(&self, _: &Path, _: Option<&[String]>, _: &[Warm]) -> Result<Loaded, String> {
            let t = || Tensor {
                shape: vec![1, 2, 4],
                data: vec![0.0; 8],
            };
            Ok(Loaded {
                net: Box::new(NoRun),
                provider: "cuda".into(),
                warmed: vec![t()],
            })
        }
    }

    #[test]
    fn the_class_count_comes_from_the_warm_up_run_alone() {
        let export = |top_id| Export {
            vocab: Vocab::new([('a', 1)], 0, 2, Case::Lower),
            win: Window::default(),
            top_id,
        };
        let ok = load(&Warmed, Path::new("m.onnx"), &["cuda".into()], &export(3)).unwrap();
        assert_eq!(ok.provider, "cuda");
        let err = load(&Warmed, Path::new("m.onnx"), &["cuda".into()], &export(4));
        assert!(err.err().unwrap().contains("4 classes"));
    }

    fn loaded() -> Result<Loaded, String> {
        Ok(Loaded {
            net: Box::new(NoRun),
            provider: "cpu".into(),
            warmed: vec![],
        })
    }

    #[test]
    fn a_failed_gpu_run_is_repeated_on_the_cpu_unless_it_was_a_stop() {
        let net = || -> Box<dyn Net> { loaded().unwrap().net };
        let fails = |_: &mut dyn Net| Err::<u8, _>("stopped".to_string());
        let stop = AtomicBool::new(false);
        let calls = std::cell::Cell::new(0);
        let go = |n: &mut dyn Net| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 { fails(n) } else { Ok(9u8) }
        };
        let (provider, r) = place_or_retry(net(), "cuda".into(), &stop, loaded, go);
        assert_eq!((provider.as_str(), r.ok()), ("cpu", Some(9)));

        stop.store(true, Relaxed);
        let cpu_built = std::cell::Cell::new(false);
        let (provider, r) = place_or_retry(
            net(),
            "cuda".into(),
            &stop,
            || {
                cpu_built.set(true);
                loaded()
            },
            fails,
        );
        assert!(r.is_err() && provider == "cuda");
        assert!(!cpu_built.get(), "a stop does not build a CPU session");
    }

    /// An fbank network that answers three frames fewer than its input spans.
    struct Short {
        shapes: Vec<Vec<i64>>,
    }

    impl Net for Short {
        fn run(&mut self, inputs: Vec<(&'static str, Tensor)>) -> Result<Vec<Tensor>, String> {
            let (name, x) = &inputs[0];
            assert_eq!(*name, "input_features");
            self.shapes.push(x.shape.clone());
            let frames = x.shape[1] as usize - 3;
            let mut data = vec![0.0; frames * 4];
            (0..frames).for_each(|f| data[f * 4 + f % 4] = 10.0);
            Ok(vec![Tensor {
                shape: vec![1, frames as i64, 4],
                data,
            }])
        }
    }

    #[test]
    fn an_fbank_call_that_answers_fewer_frames_is_padded_to_the_span() {
        let pcm = vec![0.1f32; 16_000 * 2];
        let mut net = Short { shapes: vec![] };
        let stop = AtomicBool::new(false);
        let win = Window {
            input: Input::Fbank,
            context_ms: 0,
            ..Window::default()
        };
        let mut s = spans_with(&mut net, &pcm, false, &[], &stop);
        s.win = win;
        let em = s.emit(0, 2000, &[0, 3]).unwrap();
        // 2 s: 198 fbank frames, 99 stacked, 96 answered, 100 wanted: the last one is repeated.
        let row = |f: usize| &em.log_probs[f * 3..f * 3 + 3];
        assert_eq!((em.frames(), net.shapes), (100, vec![vec![1, 99, 160]]));
        assert!(row(95) != row(94) && (96..100).all(|f| row(f) == row(95)));
    }

    #[test]
    fn a_window_under_the_shortest_input_is_padded() {
        let pcm = vec![0.1f32; 1600];
        let mut net = Echo {
            calls: vec![],
            classes: 4,
        };
        let stop = AtomicBool::new(false);
        let em = spans(&mut net, &pcm, true, &stop)
            .emit(0, 100, &[0])
            .unwrap();
        assert_eq!(em.frames(), 5);
        assert_eq!(net.calls, [MIN_SAMPLES]);
    }

    /// Answers a frame per 320 samples, all blank for an input over `blank_over` samples, else
    /// all class 1.
    struct Collapse {
        calls: Vec<usize>,
        blank_over: usize,
    }

    impl Net for Collapse {
        fn run(&mut self, mut inputs: Vec<(&'static str, Tensor)>) -> Result<Vec<Tensor>, String> {
            let n = inputs.remove(0).1.data.len();
            self.calls.push(n);
            let class = usize::from(n <= self.blank_over);
            let mut data = vec![0.0; n / 320 * 2];
            for f in 0..n / 320 {
                data[f * 2 + class] = 5.0;
            }
            Ok(vec![Tensor {
                shape: vec![1, (n / 320) as i64, 2],
                data,
            }])
        }
    }

    /// The share of frames whose best class is the blank (id 0) in emissions over `[0, 1]`.
    fn blank_share(em: &Emissions) -> f64 {
        let blank = em
            .log_probs
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|f| f[0] >= f[1])
            .count();
        blank as f64 / em.frames() as f64
    }

    #[test]
    fn a_chunk_blank_over_voiced_audio_is_rerun_as_halves_that_are_kept_when_less_blank() {
        let pcm = vec![0.1f32; 16_000 * 10];
        let stop = AtomicBool::new(false);
        let silent = vec![false; 500];
        let win = |retry| Window {
            chunk_ms: 10_000,
            context_ms: 0,
            retry,
            ..Window::default()
        };
        let run = |blank_over: usize, retry: Option<Retry>, silent: &[bool]| {
            let mut net = Collapse {
                calls: vec![],
                blank_over,
            };
            let mut s = spans_with(&mut net, &pcm, false, silent, &stop);
            s.win = win(retry);
            let em = s.emit(0, 10_000, &[0, 1]).unwrap();
            (blank_share(&em), net.calls)
        };
        let retry = Some(Retry {
            blank: 0.99,
            voiced: 0.2,
        });
        let whole = 16_000 * 10;
        assert_eq!(
            run(whole / 2, retry, &silent),
            (0.0, vec![whole, whole / 2, whole / 2])
        );
        // The halves are no better: the chunk's own answer stays.
        assert_eq!(
            run(0, retry, &silent),
            (1.0, vec![whole, whole / 2, whole / 2])
        );
        // No retry configured, or too little voiced audio.
        assert_eq!(run(whole / 2, None, &silent).1, vec![whole]);
        let mostly_silent: Vec<bool> = (0..500).map(|f| f >= 50).collect();
        assert_eq!(run(whole / 2, retry, &mostly_silent).1, vec![whole]);
    }

    /// A WAV of 16 kHz mono 16-bit samples as floats.
    #[cfg(feature = "karaoke")]
    fn read_wav(path: &Path) -> Vec<f32> {
        let b = std::fs::read(path).unwrap();
        let mut at = 12;
        while &b[at..at + 4] != b"data" {
            at += 8 + u32::from_le_bytes(b[at + 4..at + 8].try_into().unwrap()) as usize;
        }
        let n = u32::from_le_bytes(b[at + 4..at + 8].try_into().unwrap()) as usize;
        let (samples, _) = b[at + 8..at + 8 + n].as_chunks::<2>();
        samples
            .iter()
            .map(|c| i16::from_le_bytes(*c) as f32 / 32768.0)
            .collect()
    }

    /// The pipeline on the tracks of `FLIKS_CTC_TRUTH` (`<id>.json` and `<id>.wav`, also in its
    /// `library-eval/`) with `FLIKS_CTC_MODELS`; its `thresholds.json` keyed by id is the bar.
    #[cfg(feature = "karaoke")]
    #[test]
    #[ignore = "needs FLIKS_CTC_TRUTH, FLIKS_CTC_MODELS and an ONNX Runtime (ORT_DYLIB_PATH)"]
    fn real_tracks_meet_their_thresholds() {
        use std::time::Instant;
        let dir = std::path::PathBuf::from(std::env::var("FLIKS_CTC_TRUTH").unwrap());
        let models = std::path::PathBuf::from(std::env::var("FLIKS_CTC_MODELS").unwrap());
        let limits: serde_json::Value = std::fs::read(dir.join("thresholds.json"))
            .map(|b| serde_json::from_slice(&b).unwrap())
            .unwrap_or_default();
        let backend = crate::karaoke::onnx::default_backend(&models);
        let mut tracks: Vec<_> = [dir.clone(), dir.join("library-eval")]
            .iter()
            .filter_map(|d| std::fs::read_dir(d).ok())
            .flatten()
            .filter_map(|e| e.ok()?.path().file_stem().map(|s| s.to_owned()))
            .map(|id| id.to_string_lossy().into_owned())
            .filter(|id| !id.contains('.'))
            .collect();
        tracks.sort();
        tracks.dedup();
        let mut failures = vec![];
        for id in tracks {
            let Some(base) = [dir.clone(), dir.join("library-eval")]
                .into_iter()
                .find(|d| {
                    d.join(format!("{id}.wav")).exists() && d.join(format!("{id}.json")).exists()
                })
            else {
                continue;
            };
            let doc: serde_json::Value =
                serde_json::from_slice(&std::fs::read(base.join(format!("{id}.json"))).unwrap())
                    .unwrap();
            let rows: Vec<_> = doc["lines"].as_array().unwrap().iter().collect();
            let start = |i: usize| rows[i]["start"].as_u64().unwrap() as u32;
            let lines: Vec<AlignLine> = (0..rows.len())
                .filter(|i| !rows[*i]["text"].as_str().unwrap().trim().is_empty())
                .map(|i| AlignLine {
                    text: rows[i]["text"].as_str().unwrap().to_owned(),
                    start_ms: Some(start(i)),
                    end_ms: Some(rows.get(i + 1).map_or(start(i) + 4000, |_| start(i + 1))),
                })
                .collect();
            let lang = doc["lang"].as_str().unwrap();
            let (model, case) = match lang {
                "fr" => ("wav2vec2-ctc-fr", Case::Lower),
                "en" => ("wav2vec2-ctc-en", Case::Upper),
                _ => ("omniasr-ctc-300m", Case::Lower),
            };
            let export = read_export(&models.join(model).join("vocab.json"), case).unwrap();
            let file = models.join(model).join("model.onnx");
            let mut loaded = load(&*backend, &file, &[], &export).unwrap();
            let pcm = read_wav(&base.join(format!("{id}.wav")));
            let stop = AtomicBool::new(false);
            let voice = Voicing::of(&pcm);
            let spans = Spans {
                net: &mut *loaded.net,
                pcm: &pcm,
                win: export.win,
                silent: &voice.silent,
                stop: &stop,
                pulse: &|| (),
            };
            let t0 = Instant::now();
            let em = emissions(
                spans,
                (pcm.len() / SAMPLES_PER_MS) as u32,
                &lines,
                lang,
                &export,
            )
            .unwrap();
            let t1 = Instant::now();
            let a = align(&lines, &export.vocab, lang, &voice, &em).unwrap();
            let (out, matched) = (a.lines, a.matched);
            let (net, vit) = (t1 - t0, t1.elapsed());
            let words: Vec<_> = out.iter().flat_map(|l| &l.words).collect();
            let frames = |w: &&fliks_engine_core::api::AlignedWord| {
                (w.start_ms / FRAME_MS) as usize..(w.end_ms / FRAME_MS) as usize
            };
            let silent_in = |w: &&fliks_engine_core::api::AlignedWord| {
                frames(w)
                    .filter(|t| voice.silent.get(*t) == Some(&true))
                    .count()
            };
            let long = words
                .iter()
                .filter(|w| w.end_ms - w.start_ms > 3000 && silent_in(w) * 5 >= frames(w).count())
                .count();
            let quiet = words
                .iter()
                .filter(|w| voice.silent.get((w.start_ms / FRAME_MS) as usize) == Some(&true))
                .count() as f64
                / words.len().max(1) as f64;
            let audio = pcm.len() as f64 / SAMPLE_RATE as f64;
            let wall = (net + vit).as_secs_f64() / audio;
            eprintln!(
                "{id}: matched {matched:.3} long {long} silent starts {quiet:.3} network {:.3}x align {:.4}x",
                net.as_secs_f64() / audio,
                vit.as_secs_f64() / audio
            );
            let limit = &limits[id.as_str()];
            let want = |k: &str| limit[k].as_f64();
            for (what, ok) in [
                (
                    "minMatched",
                    want("minMatched").is_none_or(|m| matched >= m),
                ),
                (
                    "maxLongWords",
                    want("maxLongWords").is_none_or(|m| long as f64 <= m),
                ),
                (
                    "maxSilentStarts",
                    want("maxSilentStarts").is_none_or(|m| quiet <= m),
                ),
                ("maxWall", want("maxWall").is_none_or(|m| wall <= m)),
            ] {
                if !ok {
                    failures.push(format!("{id}: {what}"));
                }
            }
        }
        assert!(failures.is_empty(), "{failures:?}");
    }
}

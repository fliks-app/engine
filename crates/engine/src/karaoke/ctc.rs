//! The CTC `Align`: the vocal stem at 16 kHz through a CTC network, the known lyrics forced
//! onto its per-frame log-probabilities. Spec: `docs/spec/karaoke.md` section 4.
use super::models::Picked;
use super::run::{StopOnDrop, pcm_cmd, read_pcm, unsupported};
use super::{Backend, Loaded, Net, Tensor, Warm};
use crate::jobs::Ctx;
use fliks_engine_core::api::{AlignLine, AlignMeta, AlignedLine, JobErrorClass};
use fliks_engine_core::jobs::ctc::{AlignError, Emissions, FRAME_MS, Vocab, align};
use fliks_engine_core::jobs::melody::SAMPLE_RATE;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

/// The network's input, named so by `tools/ctc-export`.
const INPUT: &str = "input_values";
/// A call takes at most this much audio (Omnilingual refuses 40 s), with context on each side.
const CHUNK_MS: u32 = 30_000;
const CONTEXT_MS: u32 = 1000;
/// The shortest input the network takes, 200 ms.
const MIN_SAMPLES: usize = 3200;
const SAMPLES_PER_MS: usize = SAMPLE_RATE as usize / 1000;

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
}

#[derive(Debug)]
struct Export {
    vocab: Vocab,
    normalize: bool,
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

fn read_export(path: &Path) -> Result<Export, String> {
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
    Ok(Export {
        vocab: Vocab::new(chars, f.blank, f.delimiter),
        normalize: f.do_normalize,
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
    normalize: bool,
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
    /// the classes `ids`.
    fn emit(&mut self, from_ms: u32, to_ms: u32, ids: &[u32]) -> Result<Emissions, String> {
        let classes = ids.len() + 1;
        let frames = (to_ms - from_ms).div_ceil(FRAME_MS) as usize;
        let mut log_probs = Vec::with_capacity(frames * classes);
        for at in (from_ms..to_ms).step_by(CHUNK_MS as usize) {
            let em = self.chunk(at, (at + CHUNK_MS).min(to_ms), ids)?;
            log_probs.extend(em.log_probs);
        }
        Ok(Emissions { classes, log_probs })
    }

    /// One call on `from_ms..to_ms` with `CONTEXT_MS` of audio around it, the context dropped.
    fn chunk(&mut self, from_ms: u32, to_ms: u32, ids: &[u32]) -> Result<Emissions, String> {
        if self.stop.load(Relaxed) {
            return Err("stopped".into());
        }
        let lead_ms = from_ms.min(CONTEXT_MS);
        let end = self.pcm.len();
        let a = ((from_ms - lead_ms) as usize * SAMPLES_PER_MS).min(end);
        let b = ((to_ms + CONTEXT_MS) as usize * SAMPLES_PER_MS).clamp(a, end);
        let mut audio = self.pcm[a..b].to_vec();
        if self.normalize {
            normalise(&mut audio);
        }
        audio.resize(audio.len().max(MIN_SAMPLES), 0.0);
        let shape = vec![1, audio.len() as i64];
        let out = self.net.run(vec![(INPUT, Tensor { shape, data: audio })])?;
        (self.pulse)();
        let (frames, classes) = frames_and_classes(&out)?;
        let skip = ((lead_ms / FRAME_MS) as usize).min(frames);
        let take = ((to_ms - from_ms).div_ceil(FRAME_MS) as usize).min(frames - skip);
        let rows = &out[0].data[skip * classes..(skip + take) * classes];
        Ok(Emissions::from_logits(rows, classes, ids))
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
    lines: &[AlignLine],
    language: Option<&str>,
    providers: Option<&[String]>,
) -> Result<Result<AlignMeta, String>, String> {
    let backend = ctx.rt.karaoke.backend();
    let runtime = backend.runtime();
    if !runtime.onnx {
        return Ok(Err(unsupported(runtime.error.unwrap_or_default())));
    }
    let export = match read_export(&picked.file("vocab.json")) {
        Ok(export) => Arc::new(export),
        Err(e) => return Ok(Err(e)),
    };
    let file = picked.file("model.onnx");
    let allow = providers.unwrap_or_default().to_vec();
    let loaded = {
        let (backend, file, export) = (backend.clone(), file.clone(), export.clone());
        tokio::task::spawn_blocking(move || load(&*backend, &file, &allow, &export))
            .await
            .map_err(|e| e.to_string())?
    };
    let Loaded { mut net, provider } = match loaded {
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
            tokio::task::spawn_blocking(move || {
                let go = |net: &mut dyn Net| {
                    place(net, &pcm, &lines, &language, &export, &stop, &|| c.pulse())
                };
                match go(&mut *net) {
                    Err(AlignError::Emit(e)) if provider != "cpu" => {
                        tracing::warn!(provider, "CTC run failed, repeating on the CPU: {e}");
                        match load(&*backend, &file, &[], &export) {
                            Ok(mut cpu) => (cpu.provider, go(&mut *cpu.net)),
                            Err(e) => (provider, Err(AlignError::Emit(e))),
                        }
                    }
                    placed => (provider, placed),
                }
            })
            .await
            .map_err(|e| e.to_string())
        })
        .await??;
    Ok(placed
        .map_err(|e| match e {
            AlignError::Emit(e) => e,
            AlignError::TooLarge => "the lyrics and the audio are too long for one CTC pass".into(),
        })
        .map(|(lines, matched)| AlignMeta {
            model: picked.id.into(),
            model_version: picked.version.clone(),
            backend: format!("ctc-{provider}"),
            lines,
            matched,
            fallback: None,
        }))
}

/// The network on the first provider of `allow` that runs it (`&[]`: the CPU), checked against the
/// export's vocabulary on a short silence.
fn load(
    backend: &dyn Backend,
    file: &Path,
    allow: &[String],
    export: &Export,
) -> Result<Loaded, String> {
    let warm: &[Warm] = &[(INPUT, &[1, SAMPLE_RATE as i64])];
    let mut loaded = backend.load(file, Some(allow), warm)?;
    let shape = vec![1, MIN_SAMPLES as i64];
    let silence = Tensor {
        shape,
        data: vec![0.0; MIN_SAMPLES],
    };
    export.check(frames_and_classes(&loaded.net.run(vec![(INPUT, silence)])?)?.1)?;
    Ok(loaded)
}

/// `lines` on the audio `pcm`; blocking.
fn place(
    net: &mut dyn Net,
    pcm: &[f32],
    lines: &[AlignLine],
    language: &str,
    export: &Export,
    stop: &AtomicBool,
    pulse: &dyn Fn(),
) -> Result<(Vec<AlignedLine>, f64), AlignError<String>> {
    let duration_ms = u32::try_from(pcm.len() / SAMPLES_PER_MS).unwrap_or(u32::MAX);
    let mut spans = Spans {
        net,
        pcm,
        normalize: export.normalize,
        stop,
        pulse,
    };
    align(
        lines,
        &export.vocab,
        language,
        duration_ms,
        &mut |a, b, ids| spans.emit(a, b, ids),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let e = read_export(&file).unwrap();
        assert!(e.normalize);
        assert_eq!(e.top_id, 7);
        std::fs::write(&file, json(40)).unwrap();
        assert!(read_export(&file).unwrap_err().contains("40 ms"));
        std::fs::remove_dir_all(&d).unwrap();
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
        net: &'a mut Echo,
        pcm: &'a [f32],
        normalize: bool,
        stop: &'a AtomicBool,
    ) -> Spans<'a> {
        Spans {
            net,
            pcm,
            normalize,
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
        assert_eq!((em.frames(), em.classes), (3000, 65));
        for t in 0..3000 {
            let row = &em.log_probs[t * 65..t * 65 + 64];
            let top = (0..64).max_by(|a, b| row[*a].total_cmp(&row[*b])).unwrap();
            assert_eq!(top, (250 + t) % 64, "frame {t}");
        }
        // Two calls of 30 s with 1 s of context on each side.
        assert_eq!(net.calls, [16_000 * 32; 2]);
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
        assert_eq!(em.frames(), 3000);
    }

    #[test]
    fn a_vocabulary_larger_than_the_network_is_an_error_not_a_silent_miss() {
        let ex = Export {
            vocab: Vocab::new([('a', 1)], 0, 2),
            normalize: false,
            top_id: 9,
        };
        let e = ex.check(4).unwrap_err();
        assert!(e.contains("token 9") && e.contains("4 classes"), "{e}");
        assert!(ex.check(10).is_ok());
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
}

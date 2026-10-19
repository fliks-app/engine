//! alass alignment (`alass-core`) of a subtitle text on reference time spans, and the voice
//! activity (`webrtc-vad`) that turns audio into such spans.
use alass_core::{
    NoProgressHandler, TimePoint, TimeSpan, align, align_nosplit, overlap_scoring, standard_scoring,
};
use fliks_engine_core::api::SubtitleFormat;
use fliks_engine_core::jobs::cues::{cue_spans, retime_cues};
use webrtc_vad::{SampleRate, Vad, VadMode};

/// Smallest unit alass aligns on, in ms.
const INTERVAL: i64 = 60;
const SPLIT_PENALTY: f64 = 7.0;
const SPEED_OPTIMIZATION: f64 = 1.0;
/// Framerates a subtitle may have been timed for.
const FRAMERATES: [f64; 8] = [
    23.976023976024,
    24.0,
    25.0,
    29.97002997003,
    30.0,
    50.0,
    59.9400599400599,
    60.0,
];

/// Sample rate of the audio `Voice` takes.
pub const SAMPLE_RATE: u32 = 8000;
const CHUNK_MS: usize = 30;
const CHUNK_SAMPLES: usize = CHUNK_MS * SAMPLE_RATE as usize / 1000;

fn alass_spans(spans: &[(i64, i64)]) -> Vec<TimeSpan> {
    spans
        .iter()
        .map(|&(s, e)| {
            TimeSpan::new_safe(TimePoint::from(s / INTERVAL), TimePoint::from(e / INTERVAL))
        })
        .collect()
}

/// `text` with every cue moved onto `reference` (ms spans); `ref_fps` enables framerate correction.
pub fn sync_text(
    text: &str,
    format: SubtitleFormat,
    reference: &[(i64, i64)],
    ref_fps: Option<f64>,
) -> String {
    let cues = cue_spans(text, format);
    let reference = alass_spans(reference);
    let mut spans = alass_spans(&cues);
    let mut factor = 1.0;
    if let Some(fps) = ref_fps {
        let (_, ratio) = FRAMERATES
            .iter()
            .map(|&f| {
                let ratio = f / fps;
                let scaled: Vec<_> = spans.iter().map(|s| s.scaled(ratio)).collect();
                let (_, score) =
                    align_nosplit(&reference, &scaled, overlap_scoring, NoProgressHandler);
                (score, ratio)
            })
            .max_by_key(|(score, _)| (score * 1_000_000_000.0) as i64)
            .expect("FRAMERATES is not empty");
        spans = spans.iter().map(|s| s.scaled(ratio)).collect();
        factor = ratio;
    }
    let deltas: Vec<i64> = if spans.is_empty() {
        vec![]
    } else {
        let (deltas, _) = align(
            &reference,
            &spans,
            SPLIT_PENALTY,
            Some(SPEED_OPTIMIZATION),
            standard_scoring,
            NoProgressHandler,
        );
        deltas.iter().map(|d| d.as_i64() * INTERVAL).collect()
    };
    retime_cues(text, format, |i, (s, e)| {
        let moved = |t: i64| (t as f64 * factor) as i64 + deltas[i];
        (moved(s), moved(e))
    })
}

/// Voice activity of 8 kHz mono samples, in 30 ms chunks.
pub struct Voice {
    vad: Vad,
    pending: Vec<i16>,
    active: Vec<bool>,
}

impl Voice {
    pub fn new() -> Self {
        Self {
            vad: Vad::new_with_rate_and_mode(SampleRate::Rate8kHz, VadMode::LowBitrate),
            pending: vec![],
            active: vec![],
        }
    }

    pub fn push(&mut self, samples: &[i16]) -> Result<(), String> {
        self.pending.extend_from_slice(samples);
        let (chunks, rest) = self.pending.as_chunks::<CHUNK_SAMPLES>();
        let whole = self.pending.len() - rest.len();
        for chunk in chunks {
            let voice = self
                .vad
                .is_voice_segment(chunk)
                .map_err(|()| "webrtc-vad unable to process samples".to_string())?;
            self.active.push(voice);
        }
        self.pending.drain(..whole);
        Ok(())
    }

    /// The spans (ms) of the chunks with voice; the last partial chunk counts, zero-padded.
    pub fn finish(mut self) -> Result<Vec<(i64, i64)>, String> {
        if !self.pending.is_empty() {
            let pad = vec![0; CHUNK_SAMPLES - self.pending.len()];
            self.push(&pad)?;
        }
        let mut spans = vec![];
        let mut from = None;
        for (i, voice) in self.active.iter().chain(&[false]).enumerate() {
            match (voice, from) {
                (true, None) => from = Some(i),
                (false, Some(start)) => {
                    spans.push(((start * CHUNK_MS) as i64, (i * CHUNK_MS) as i64));
                    from = None;
                }
                _ => {}
            }
        }
        Ok(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SubtitleFormat::Srt;

    #[test]
    fn a_late_cue_moves_onto_the_reference() {
        let late = "1\n00:01:00,000 --> 00:01:06,000\nsync me\n";
        let out = sync_text(late, Srt, &[(30_000, 36_000)], None);
        assert_eq!(out, "1\n00:00:30,000 --> 00:00:36,000\nsync me\n");
    }

    #[test]
    fn silence_has_no_voice_and_a_text_without_cues_is_unchanged() {
        let mut v = Voice::new();
        v.push(&vec![0; 8000]).unwrap();
        assert!(v.finish().unwrap().is_empty());
        assert_eq!(
            sync_text("nothing", Srt, &[(0, 1000)], Some(24.0)),
            "nothing"
        );
    }
}

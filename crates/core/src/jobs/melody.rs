//! The pure half of `Melody`: a frame-wise pitch track cut into the notes of word spans. The
//! pitch network (SwiftF0) runs in the engine. Spec: `docs/spec/karaoke.md` section 5.
use crate::api::{JobKind, MelodyNote, MelodySpan};

/// Sample rate the pitch network takes.
pub const SAMPLE_RATE: u32 = 16_000;
/// Samples per output frame.
pub const HOP: usize = 256;
pub const FRAME_MS: f64 = 1000.0 * HOP as f64 / SAMPLE_RATE as f64;
/// Frames of one network call. Its receptive field reaches 11 frames back and 10 ahead, which
/// the windows overlap so each frame is what a single pass over the file would give.
pub const WINDOW_FRAMES: usize = 1875;
pub const LEFT_FRAMES: usize = 11;
pub const LOOKAHEAD_FRAMES: usize = 10;
/// Frames whose audio peaks below this are silent: the network answers digital silence at random.
pub const SILENCE_PEAK: f32 = 1e-3;
/// Lowest and highest pitch the network scores, in Hz.
pub const FMIN: f32 = 46.875;
pub const FMAX: f32 = 2093.75;
/// Confidence from which a frame is voiced.
const VOICED: f32 = 0.5;
/// A note is dropped under this many frames (48 ms).
const MIN_FRAMES: usize = 3;
/// Unvoiced frames a note bridges (80 ms): a consonant inside a word.
const MAX_GAP: usize = 5;
/// Semitones a frame may sit from its note's median before it starts a new note.
const SPLIT: f64 = 0.8;
const MAX_SPANS: usize = 100_000;

pub fn validate(kind: &JobKind) -> Result<(), String> {
    let JobKind::Melody { spans, .. } = kind else {
        return Ok(());
    };
    if spans.len() > MAX_SPANS {
        return Err(format!("Melody takes at most {MAX_SPANS} spans"));
    }
    if let Some(i) = spans.iter().position(|s| s.end_ms <= s.start_ms) {
        return Err(format!("span {i} ends before it starts"));
    }
    Ok(())
}

/// What the pitch network answers per frame (`HOP` samples each).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PitchTrack {
    pub hz: Vec<f32>,
    pub confidence: Vec<f32>,
}

/// Frames of a signal of `samples` samples.
pub fn frames(samples: usize) -> usize {
    (samples / HOP).max(1)
}

/// The network calls over `frames` frames: `(first frame, end frame, first frame of the
/// samples it is given)`. A call reads `LEFT_FRAMES` before and `LOOKAHEAD_FRAMES` after its
/// frames, and returns the ones in `first..end` out of what it computed from `given..`.
pub fn windows(frames: usize) -> Vec<(usize, usize, usize)> {
    (0..frames)
        .step_by(WINDOW_FRAMES)
        .map(|start| {
            (
                start,
                (start + WINDOW_FRAMES).min(frames),
                start.saturating_sub(LEFT_FRAMES),
            )
        })
        .collect()
}

/// The samples one call is given.
pub fn window_samples(samples: usize, end: usize, given: usize) -> (usize, usize) {
    let to = if end < frames(samples) {
        (end + LOOKAHEAD_FRAMES) * HOP
    } else {
        samples
    };
    (given * HOP, to.min(samples))
}

/// MIDI pitch of a frequency: 69 is A4.
pub fn midi(hz: f32) -> f64 {
    69.0 + 12.0 * (f64::from(hz) / 440.0).log2()
}

fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    }
}

struct Open {
    first: usize,
    last: usize,
    vals: Vec<f64>,
}

impl Open {
    fn new(i: usize, v: f64) -> Self {
        Self {
            first: i,
            last: i,
            vals: vec![v],
        }
    }
}

/// Notes of one span: voiced frames, smoothed, cut where the pitch moves a semitone for three
/// frames or the voice stops for more than 80 ms.
pub fn notes(track: &PitchTrack, span: MelodySpan) -> Vec<MelodyNote> {
    let n = track.hz.len().min(track.confidence.len());
    let first = ((f64::from(span.start_ms) / FRAME_MS).floor() as usize).min(n);
    let end = ((f64::from(span.end_ms) / FRAME_MS).ceil() as usize).min(n);
    let raw: Vec<Option<f64>> = (first..end)
        .map(|i| (track.confidence[i] >= VOICED && track.hz[i] > 0.0).then(|| midi(track.hz[i])))
        .collect();
    // A median over the voiced neighbours drops single-frame spikes.
    let smooth: Vec<Option<f64>> = (0..raw.len())
        .map(|i| {
            raw[i]?;
            let near: Vec<f64> = raw[i.saturating_sub(1)..(i + 2).min(raw.len())]
                .iter()
                .flatten()
                .copied()
                .collect();
            Some(median(&near))
        })
        .collect();
    let mut out = vec![];
    let mut close = |o: Open| {
        if o.vals.len() >= MIN_FRAMES {
            let start = ((first + o.first) as f64 * FRAME_MS).round() as u32;
            let stop = ((first + o.last + 1) as f64 * FRAME_MS).round() as u32;
            out.push(MelodyNote {
                start_ms: start.max(span.start_ms),
                end_ms: stop.min(span.end_ms),
                midi: (median(&o.vals) * 100.0).round() / 100.0,
            });
        }
    };
    let (mut cur, mut pending, mut gap): (Option<Open>, Vec<(usize, f64)>, usize) =
        (None, vec![], 0);
    for (i, v) in smooth.into_iter().enumerate() {
        let Some(v) = v else {
            gap += 1;
            if gap > MAX_GAP {
                pending.clear();
                if let Some(o) = cur.take() {
                    close(o);
                }
            }
            continue;
        };
        gap = 0;
        let Some(c) = cur.as_mut() else {
            cur = Some(Open::new(i, v));
            continue;
        };
        if (v - median(&c.vals)).abs() <= SPLIT {
            // Frames that strayed and came back were noise inside the note.
            for (j, pv) in pending.drain(..) {
                c.vals.push(pv);
                c.last = j;
            }
            c.vals.push(v);
            c.last = i;
        } else {
            pending.push((i, v));
            if pending.len() >= MIN_FRAMES {
                let mut next = Open::new(pending[0].0, pending[0].1);
                for (j, pv) in pending.drain(..).skip(1) {
                    next.vals.push(pv);
                    next.last = j;
                }
                if let Some(old) = cur.replace(next) {
                    close(old);
                }
            }
        }
    }
    if let Some(o) = cur {
        close(o);
    }
    out
}

pub fn spans_notes(track: &PitchTrack, spans: &[MelodySpan]) -> Vec<Vec<MelodyNote>> {
    spans.iter().map(|s| notes(track, *s)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(parts: &[(usize, f32)]) -> PitchTrack {
        let mut t = PitchTrack::default();
        for (n, hz) in parts {
            for _ in 0..*n {
                t.hz.push(*hz);
                t.confidence.push(if *hz > 0.0 { 0.9 } else { 0.1 });
            }
        }
        t
    }

    fn span(a: u32, b: u32) -> MelodySpan {
        MelodySpan {
            start_ms: a,
            end_ms: b,
        }
    }

    #[test]
    fn midi_follows_the_a4_scale() {
        assert!((midi(440.0) - 69.0).abs() < 1e-9);
        assert!((midi(220.0) - 57.0).abs() < 1e-9);
        assert!((midi(261.6256) - 60.0).abs() < 1e-3);
    }

    #[test]
    fn a_silence_splits_two_notes_and_each_takes_its_median() {
        let t = track(&[(30, 220.0), (10, 0.0), (30, 329.6276)]);
        let n = notes(&t, span(0, 1280));
        assert_eq!(n.len(), 2);
        assert_eq!((n[0].midi, n[1].midi), (57.0, 64.0));
        assert_eq!((n[0].start_ms, n[0].end_ms), (0, 480));
        assert_eq!((n[1].start_ms, n[1].end_ms), (640, 1120));
    }

    #[test]
    fn a_step_without_a_gap_is_two_notes_and_a_spike_is_none() {
        let mut t = track(&[(20, 220.0), (20, 261.6256)]);
        assert_eq!(notes(&t, span(0, 640)).len(), 2);
        t = track(&[(10, 220.0), (1, 440.0), (10, 220.0)]);
        let n = notes(&t, span(0, 400));
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].midi, 57.0);
    }

    #[test]
    fn a_short_gap_inside_a_word_is_bridged() {
        let t = track(&[(10, 220.0), (3, 0.0), (10, 220.0)]);
        assert_eq!(notes(&t, span(0, 500)).len(), 1);
    }

    #[test]
    fn a_span_sees_only_its_own_frames_and_an_unvoiced_one_has_no_notes() {
        let t = track(&[(30, 220.0), (10, 0.0), (30, 329.6276)]);
        let all = spans_notes(&t, &[span(640, 1120), span(480, 640), span(0, 250)]);
        assert_eq!(all[0].len(), 1);
        assert_eq!(all[0][0].midi, 64.0);
        assert!(all[1].is_empty());
        assert_eq!(all[2][0].end_ms, 250);
    }

    #[test]
    fn windows_overlap_so_each_call_sees_its_neighbours() {
        let w = windows(WINDOW_FRAMES * 2 + 5);
        assert_eq!(w.len(), 3);
        assert_eq!(
            w[1],
            (
                WINDOW_FRAMES,
                2 * WINDOW_FRAMES,
                WINDOW_FRAMES - LEFT_FRAMES
            )
        );
        let samples = (WINDOW_FRAMES * 2 + 5) * HOP;
        assert_eq!(
            window_samples(samples, w[0].1, w[0].2),
            (0, (WINDOW_FRAMES + LOOKAHEAD_FRAMES) * HOP)
        );
        assert_eq!(window_samples(samples, w[2].1, w[2].2).1, samples);
    }

    #[test]
    fn a_span_that_ends_before_it_starts_is_refused() {
        let k = JobKind::Melody {
            spans: vec![span(10, 10)],
            model: None,
        };
        assert!(validate(&k).is_err());
    }
}

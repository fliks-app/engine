//! What the notes of a melody say about the song: tuning, range, key and tempo.
//! Spec: `docs/spec/karaoke.md` section 5.
use crate::api::{Key, KeyEstimate, KeyMode, MelodyAnalysis, MelodyNote, TempoEstimate};
use std::f64::consts::TAU;

const MIN_NOTES: usize = 8;
const MIN_TOTAL_MS: f64 = 5000.0;
const MIN_PITCH_CLASSES: usize = 3;
/// A runner-up key within this correlation of the best is reported as its alternative.
const ALTERNATIVE: f64 = 0.05;
const MIN_ONSETS: usize = 16;
const GRID_MS: f64 = 10.0;
const BPM_RANGE: (f64, f64) = (60.0, 200.0);
/// A period scoring this share of the best one is taken when it is shorter.
const PERIOD_SHARE: f64 = 0.8;

/// Krumhansl-Kessler key profiles, tonic first.
const MAJOR: [f64; 12] = [
    6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88,
];
const MINOR: [f64; 12] = [
    6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17,
];

pub fn analyse(notes: &[MelodyNote]) -> Option<MelodyAnalysis> {
    let w = |n: &MelodyNote| f64::from(n.end_ms.saturating_sub(n.start_ms));
    let total: f64 = notes.iter().map(w).sum();
    if notes.len() < MIN_NOTES || total < MIN_TOTAL_MS {
        return None;
    }
    let (s, c) = notes.iter().fold((0.0, 0.0), |(s, c), n| {
        let a = TAU * n.midi.rem_euclid(1.0);
        (s + w(n) * a.sin(), c + w(n) * a.cos())
    });
    let tuning = s.atan2(c) / TAU;
    let mut pitches: Vec<(f64, f64)> = notes.iter().map(|n| (n.midi - tuning, w(n))).collect();
    pitches.sort_by(|a, b| a.0.total_cmp(&b.0));
    let percentile = |p: f64| {
        let mut acc = 0.0;
        let hit = pitches.iter().find(|(_, d)| {
            acc += d;
            acc >= p * total
        });
        round(hit.unwrap_or(&pitches[pitches.len() - 1]).0, 100.0)
    };
    Some(MelodyAnalysis {
        tuning_cents: round(tuning * 100.0, 10.0),
        low_midi: percentile(0.05),
        high_midi: percentile(0.95),
        key: key(&pitches),
        tempo: tempo(notes),
    })
}

fn round(v: f64, scale: f64) -> f64 {
    (v * scale).round() / scale
}

fn key(pitches: &[(f64, f64)]) -> Option<KeyEstimate> {
    let mut hist = [0.0; 12];
    for (m, d) in pitches {
        hist[(m.round() as i64).rem_euclid(12) as usize] += d;
    }
    if hist.iter().filter(|v| **v > 0.0).count() < MIN_PITCH_CLASSES {
        return None;
    }
    let mut keys: Vec<(f64, Key)> = Vec::with_capacity(24);
    for (mode, profile) in [(KeyMode::Major, &MAJOR), (KeyMode::Minor, &MINOR)] {
        for tonic in 0..12 {
            let rotated: Vec<f64> = (0..12).map(|pc| profile[(pc + 12 - tonic) % 12]).collect();
            keys.push((
                pearson(&hist, &rotated),
                Key {
                    tonic: tonic as u32,
                    mode,
                },
            ));
        }
    }
    if !keys[0].0.is_finite() {
        return None;
    }
    keys.sort_by(|a, b| b.0.total_cmp(&a.0));
    Some(KeyEstimate {
        key: keys[0].1,
        confidence: round(keys[0].0, 1000.0),
        alternative: (keys[0].0 - keys[1].0 <= ALTERNATIVE).then_some(keys[1].1),
    })
}

fn pearson(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len() as f64;
    let (ma, mb) = (a.iter().sum::<f64>() / n, b.iter().sum::<f64>() / n);
    let (mut ab, mut aa, mut bb) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        ab += (x - ma) * (y - mb);
        aa += (x - ma).powi(2);
        bb += (y - mb).powi(2);
    }
    ab / (aa * bb).sqrt()
}

/// Autocorrelation of the note onsets on a 10 ms grid over the lags of 60 to 200 BPM.
fn tempo(notes: &[MelodyNote]) -> Option<TempoEstimate> {
    if notes.len() < MIN_ONSETS {
        return None;
    }
    let first = notes.iter().map(|n| n.start_ms).min()?;
    let last = notes.iter().map(|n| n.start_ms).max()?;
    let mut env = vec![0.0f64; ((last - first) as f64 / GRID_MS).round() as usize + 1];
    // Each onset spreads over 20 ms each side: onsets on the 16 ms pitch frames jitter on the grid.
    for n in notes {
        let at = ((n.start_ms - first) as f64 / GRID_MS).round() as usize;
        for (i, w) in [(at.wrapping_sub(1), 0.5), (at, 1.0), (at + 1, 0.5)] {
            if let Some(v) = env.get_mut(i) {
                *v += w;
            }
        }
    }
    let ac = |lag: usize| -> f64 {
        env.iter()
            .zip(env.iter().skip(lag))
            .map(|(a, b)| a * b)
            .sum()
    };
    let lag_of = |bpm: f64| (60_000.0 / bpm / GRID_MS).round() as usize;
    let (lo, hi) = (lag_of(BPM_RANGE.1), lag_of(BPM_RANGE.0));
    // The shortest strong period: a multiple of the beat scores as high once onsets jitter.
    let scores: Vec<f64> = (lo..=hi).map(ac).collect();
    let top = scores.iter().copied().fold(f64::MIN, f64::max);
    let best = scores
        .iter()
        .enumerate()
        .find(|(i, v)| {
            **v >= PERIOD_SHARE * top
                && *i > 0
                && scores.get(i + 1).is_none_or(|n| *v >= n)
                && scores[i - 1] <= **v
        })
        .map_or((lo, top), |(i, v)| (lo + i, *v));
    let zero = ac(0);
    if best.1 <= 0.0 || zero <= 0.0 {
        return None;
    }
    let (a, b, c) = (ac(best.0 - 1), best.1, ac(best.0 + 1));
    let denom = a - 2.0 * b + c;
    let shift = if denom < 0.0 {
        0.5 * (a - c) / denom
    } else {
        0.0
    };
    let lag_ms = (best.0 as f64 + shift.clamp(-1.0, 1.0)) * GRID_MS;
    Some(TempoEstimate {
        bpm: round(60_000.0 / lag_ms, 10.0),
        confidence: round((best.1 / zero).min(1.0), 1000.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(start_ms: u32, dur: u32, midi: f64) -> MelodyNote {
        MelodyNote {
            start_ms,
            end_ms: start_ms + dur,
            midi,
        }
    }

    /// One note of 400 ms every 500 ms, cycling through `pitches`.
    fn melody(pitches: &[f64], n: usize, offset: f64) -> Vec<MelodyNote> {
        (0..n)
            .map(|i| note(i as u32 * 500, 400, pitches[i % pitches.len()] + offset))
            .collect()
    }

    const C_MAJOR: [f64; 8] = [60.0, 64.0, 67.0, 60.0, 62.0, 64.0, 65.0, 67.0];
    const A_MINOR: [f64; 9] = [57.0, 60.0, 64.0, 57.0, 59.0, 60.0, 62.0, 64.0, 68.0];

    #[test]
    fn tuning_is_recovered_and_wraps() {
        let a = analyse(&melody(&C_MAJOR, 32, 0.3)).unwrap();
        assert!((a.tuning_cents - 30.0).abs() < 0.5, "{}", a.tuning_cents);
        let a = analyse(&melody(&C_MAJOR, 32, 0.48)).unwrap();
        assert!((a.tuning_cents - 48.0).abs() < 0.5);
        let a = analyse(&melody(&C_MAJOR, 32, -0.48)).unwrap();
        assert!((a.tuning_cents + 48.0).abs() < 0.5);
        assert_eq!(a.key.unwrap().key.tonic, 0);
    }

    #[test]
    fn key_of_a_major_melody() {
        let k = analyse(&melody(&C_MAJOR, 32, 0.0)).unwrap().key.unwrap();
        let c = Key {
            tonic: 0,
            mode: KeyMode::Major,
        };
        let am = Key {
            tonic: 9,
            mode: KeyMode::Minor,
        };
        assert!(
            k.key == c || (k.key == am && k.alternative == Some(c)),
            "{k:?}"
        );
    }

    #[test]
    fn key_of_a_minor_melody() {
        let k = analyse(&melody(&A_MINOR, 36, 0.0)).unwrap().key.unwrap();
        assert_eq!(
            k.key,
            Key {
                tonic: 9,
                mode: KeyMode::Minor
            }
        );
    }

    #[test]
    fn range_percentiles() {
        let notes: Vec<_> = (0..40)
            .map(|i| note(i * 500, 400, 50.0 + f64::from(i)))
            .collect();
        let a = analyse(&notes).unwrap();
        assert!((a.low_midi - 51.0).abs() <= 1.0 && (a.high_midi - 88.0).abs() <= 1.0);
    }

    #[test]
    fn tempo_of_an_onset_train() {
        let t = analyse(&melody(&C_MAJOR, 32, 0.0)).unwrap().tempo.unwrap();
        assert!((t.bpm - 120.0).abs() <= 1.0, "{t:?}");
        assert!(t.confidence > 0.5);
    }

    #[test]
    fn onsets_on_the_pitch_frame_grid_stay_inside_the_envelope() {
        let notes: Vec<_> = (0..32)
            .map(|i| note(i * 496, 400, 60.0 + f64::from(i % 5)))
            .collect();
        let t = analyse(&notes).unwrap().tempo.unwrap();
        assert!((t.bpm - 121.0).abs() <= 1.5, "{t:?}");
    }

    #[test]
    fn too_few_notes_is_none() {
        assert!(analyse(&melody(&C_MAJOR, 7, 0.0)).is_none());
        let short: Vec<_> = (0..10).map(|i| note(i * 100, 100, 60.0)).collect();
        assert!(analyse(&short).is_none());
        assert!(analyse(&melody(&C_MAJOR, 14, 0.0)).unwrap().tempo.is_none());
    }
}

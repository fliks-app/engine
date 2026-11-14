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
/// Confidence from which a frame is voiced: a separated stem keeps bleed, so sung frames often score
/// 0.2 to 0.4; the register filter drops the instruments this lets in.
const VOICED: f32 = 0.25;
/// Semitones a frame may sit from the singer's median: further out is a harmonic or another instrument.
const REGISTER: f64 = 15.0;
/// Highest harmonic number an out-of-register frame is tried as.
const HARMONICS: u32 = 8;
/// Semitones from the voice nearby within which a frame divided by a harmonic number is that harmonic.
const HARMONIC_FIT: f64 = 0.5;
/// Frames on each side (4 s) of the sung frames whose median is the register.
const REGISTER_REACH: usize = 250;
/// Semitones a step between two sung runs may span before it costs: a sung leap.
const LEAP: f64 = 5.0;
/// A step of at least this many semitones, or a gap over `MAX_GAP`, cuts a run for the octave pass.
const RUN_BREAK: f64 = 9.0;
/// Cost per voiced frame of moving a run an octave (a run of 14 frames weighs a 12 st leap).
const SHIFT_COST: f64 = 1.0;
/// Cost per frame and per semitone a run sits further than `FREE_REGISTER` from the local register.
const PULL: f64 = 1.0;
const FREE_REGISTER: f64 = 3.0;
/// Time (ms) over which a step across a gap stops counting.
const GAP_DECAY_MS: f64 = 600.0;
/// Octave-like steps within `REGISTER_REACH` frames that make a leap cost double, up to `HOP_MAX` times:
/// a singer seldom leaps an octave often, a tracker flipping between two voices does.
const HOP_STEP: usize = 2;
/// Semitones from which a step between runs counts as an octave-like hop.
const HOP_SEMITONES: f64 = 10.0;
const HOP_MAX: f64 = 4.0;
/// Extra cost per semitone a state change's step sits off a whole octave beyond `OCTAVE_STEP`.
const OFF_OCTAVE: f64 = 6.0;
/// Semitones a run's entry or exit step may sit from a whole octave at no extra cost.
const OCTAVE_STEP: f64 = 0.5;
/// Frames on each side of the smoothing median: two irons out vibrato.
const SMOOTH: usize = 2;
/// A note is dropped under this many voiced frames (96 ms).
const MIN_FRAMES: usize = 6;
/// Unvoiced frames a note bridges (128 ms): a consonant inside a word.
const MAX_GAP: usize = 8;
/// Semitones a frame may sit from its note's median before it starts a new note.
const SPLIT: f64 = 1.0;
/// Frames off the note's median that start the next note (64 ms).
const STEP_FRAMES: usize = 4;
/// Two notes this close (semitones), with at most `MERGE_GAP` frames (160 ms) between them, are one.
const MERGE: f64 = 0.75;
const MERGE_GAP: usize = 10;
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

fn frame_range(span: MelodySpan, n: usize) -> (usize, usize) {
    let first = ((f64::from(span.start_ms) / FRAME_MS).floor() as usize).min(n);
    let end = ((f64::from(span.end_ms) / FRAME_MS).ceil() as usize).min(n);
    (first, end)
}

/// The median of the voiced values within `reach` frames of `i`.
fn near(v: &[Option<f64>], i: usize, reach: usize) -> Option<f64> {
    let w: Vec<f64> = v[i.saturating_sub(reach)..(i + reach + 1).min(v.len())]
        .iter()
        .flatten()
        .copied()
        .collect();
    (!w.is_empty()).then(|| median(&w))
}

/// A stretch of voiced frames with no big step or long gap inside, which moves an octave as one.
struct Run {
    first: usize,
    last: usize,
    frames: usize,
    median: f64,
    head: f64,
    tail: f64,
}

/// `raw` with each run moved by the octave (-1, 0 or 1) the cheapest path over the runs picks: a step
/// costs what it exceeds `LEAP` by, less across a gap, a move costs per frame, and a run is pulled
/// toward the register nearby.
fn fold_octaves(raw: &[Option<f64>]) -> Vec<Option<f64>> {
    let voiced: Vec<usize> = (0..raw.len()).filter(|&i| raw[i].is_some()).collect();
    let at = |f: usize| raw[f].unwrap_or(0.0);
    let mut runs: Vec<Run> = vec![];
    let mut p = 0;
    while p < voiced.len() {
        let mut q = p + 1;
        while q < voiced.len()
            && voiced[q] - voiced[q - 1] - 1 <= MAX_GAP
            && (at(voiced[q]) - at(voiced[q - 1])).abs() < RUN_BREAK
        {
            q += 1;
        }
        let vals: Vec<f64> = voiced[p..q].iter().map(|&f| at(f)).collect();
        runs.push(Run {
            first: voiced[p],
            last: voiced[q - 1],
            frames: q - p,
            median: median(&vals),
            head: vals[0],
            tail: vals[vals.len() - 1],
        });
        p = q;
    }
    if runs.len() < 2 {
        return raw.to_vec();
    }
    let octave_steps: Vec<usize> = runs
        .windows(2)
        .map(|w| usize::from((w[1].head - w[0].tail).abs() >= HOP_SEMITONES))
        .collect();
    let crowd: Vec<f64> = (0..runs.len())
        .map(|t| {
            let (lo, hi) = (
                runs[t].first.saturating_sub(REGISTER_REACH),
                runs[t].last + REGISTER_REACH,
            );
            let hops = (1..runs.len())
                .filter(|&u| octave_steps[u - 1] == 1 && runs[u].first >= lo && runs[u].first <= hi)
                .count();
            (hops as f64 / HOP_STEP as f64).clamp(1.0, HOP_MAX)
        })
        .collect();
    let pull: Vec<[f64; 3]> = runs
        .iter()
        .enumerate()
        .map(|(t, r)| {
            let mid = (r.first + r.last) / 2;
            let reg = near(raw, mid, REGISTER_REACH).unwrap_or(r.median);
            [-12.0, 0.0, 12.0].map(|s| {
                let excess = (r.median + s - reg).abs() - FREE_REGISTER;
                r.frames as f64
                    * (PULL * (crowd[t] - 1.0) * excess.max(0.0)
                        + if s == 0.0 { 0.0 } else { SHIFT_COST })
            })
        })
        .collect();
    let step = |t: usize, from: usize, to: usize| -> f64 {
        let (a, b) = (&runs[t - 1], &runs[t]);
        let (sa, sb) = ([-12.0, 0.0, 12.0][from], [-12.0, 0.0, 12.0][to]);
        let gap_ms = (b.first - a.last - 1) as f64 * FRAME_MS;
        let d = ((b.head + sb) - (a.tail + sa)).abs();
        let raw_step = (b.head - a.tail).abs();
        let off = (raw_step - 12.0 * (raw_step / 12.0).round()).abs();
        let moved = if from == to {
            0.0
        } else {
            OFF_OCTAVE * (off - OCTAVE_STEP).max(0.0)
        };
        (d - LEAP).max(0.0) * (-gap_ms / GAP_DECAY_MS).exp() * crowd[t] + moved
    };
    let mut cost = pull[0];
    let mut back: Vec<[usize; 3]> = vec![[0; 3]];
    for t in 1..runs.len() {
        let mut next = [0.0; 3];
        let mut from = [0; 3];
        for to in 0..3 {
            let (best, arg) = (0..3)
                .map(|f| (cost[f] + step(t, f, to), f))
                .min_by(|x, y| x.0.total_cmp(&y.0))
                .unwrap_or((0.0, 1));
            next[to] = best + pull[t][to];
            from[to] = arg;
        }
        cost = next;
        back.push(from);
    }
    let mut state = (0..3)
        .min_by(|&x, &y| cost[x].total_cmp(&cost[y]))
        .unwrap_or(1);
    let mut out = raw.to_vec();
    for t in (0..runs.len()).rev() {
        let shift = [-12.0, 0.0, 12.0][state];
        for v in out[runs[t].first..=runs[t].last].iter_mut().flatten() {
            *v += shift;
        }
        state = back[t][state];
    }
    out
}

/// The MIDI pitch of every frame, `None` when unvoiced: frames out of the singer's register (the
/// median of the spans' voiced frames nearby) dropped unless a harmonic of the voice, short octave
/// errors folded back, then smoothed.
fn cleaned(track: &PitchTrack, spans: &[MelodySpan]) -> Vec<Option<f64>> {
    let n = track.hz.len().min(track.confidence.len());
    let mut raw: Vec<Option<f64>> = (0..n)
        .map(|i| (track.confidence[i] >= VOICED && track.hz[i] > 0.0).then(|| midi(track.hz[i])))
        .collect();
    let mut sung: Vec<Option<f64>> = vec![None; n];
    for s in spans {
        let (a, b) = frame_range(*s, n);
        sung[a..b].copy_from_slice(&raw[a..b]);
    }
    let Some(global) = near(&sung, 0, n) else {
        return vec![None; n];
    };
    let mut out_of_register = vec![];
    for (i, v) in raw.iter_mut().enumerate() {
        let register = near(&sung, i, REGISTER_REACH).unwrap_or(global);
        if let Some(x) = v.filter(|x| (x - register).abs() > REGISTER) {
            out_of_register.push((i, x));
            *v = None;
        }
    }
    let in_register = raw.clone();
    for (i, x) in out_of_register {
        let Some(voice) = near(&in_register, i, MAX_GAP) else {
            continue;
        };
        raw[i] = (2..=HARMONICS)
            .map(|k| x - 12.0 * f64::from(k).log2())
            .find(|y| (y - voice).abs() <= HARMONIC_FIT);
    }
    let folded = fold_octaves(&raw);
    (0..n)
        .map(|i| {
            folded[i]?;
            near(&folded, i, SMOOTH)
        })
        .collect()
}

/// Notes of one span: cut where the pitch moves a semitone for four frames or the voice stops for
/// more than 128 ms; a note under 96 ms lengthens its neighbour or is dropped, then equal ones merge.
fn notes(clean: &[Option<f64>], span: MelodySpan) -> Vec<MelodyNote> {
    let (first, end) = frame_range(span, clean.len());
    let mut closed: Vec<Open> = vec![];
    let (mut cur, mut pending, mut gap): (Option<Open>, Vec<(usize, f64)>, usize) =
        (None, vec![], 0);
    for (i, v) in clean.iter().enumerate().take(end).skip(first) {
        let Some(v) = *v else {
            gap += 1;
            if gap > MAX_GAP {
                pending.clear();
                if let Some(o) = cur.take() {
                    closed.push(o);
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
            for (j, pv) in pending.drain(..) {
                c.vals.push(pv);
                c.last = j;
            }
            c.vals.push(v);
            c.last = i;
        } else {
            pending.push((i, v));
            if pending.len() >= STEP_FRAMES {
                let mut next = Open::new(pending[0].0, pending[0].1);
                for (j, pv) in pending.drain(..).skip(1) {
                    next.vals.push(pv);
                    next.last = j;
                }
                if let Some(old) = cur.replace(next) {
                    closed.push(old);
                }
            }
        }
    }
    closed.extend(cur);
    let long = |o: &Open| o.vals.len() >= MIN_FRAMES;
    for k in 0..closed.len() {
        if long(&closed[k]) {
            continue;
        }
        let (first, last) = (closed[k].first, closed[k].last);
        if let Some(p) = closed[..k]
            .iter()
            .rposition(long)
            .filter(|&p| first <= closed[p].last + MAX_GAP + 1)
        {
            closed[p].last = closed[p].last.max(last);
        } else if let Some(n) = closed[k + 1..]
            .iter()
            .position(long)
            .map(|n| n + k + 1)
            .filter(|&n| closed[n].first <= last + MAX_GAP + 1)
        {
            closed[n].first = closed[n].first.min(first);
        }
    }
    let kept = closed.into_iter().filter(long);
    let mut merged: Vec<Open> = vec![];
    for o in kept {
        match merged.last_mut() {
            Some(m)
                if o.first - m.last - 1 <= MERGE_GAP
                    && (median(&o.vals) - median(&m.vals)).abs() <= MERGE =>
            {
                m.last = o.last;
                m.vals.extend(o.vals);
            }
            _ => merged.push(o),
        }
    }
    merged
        .into_iter()
        .map(|o| MelodyNote {
            start_ms: ((o.first as f64 * FRAME_MS).round() as u32).max(span.start_ms),
            end_ms: (((o.last + 1) as f64 * FRAME_MS).round() as u32).min(span.end_ms),
            midi: (median(&o.vals) * 100.0).round() / 100.0,
        })
        .collect()
}

pub fn spans_notes(track: &PitchTrack, spans: &[MelodySpan]) -> Vec<Vec<MelodyNote>> {
    let clean = cleaned(track, spans);
    spans.iter().map(|s| notes(&clean, *s)).collect()
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

    fn one(t: &PitchTrack, s: MelodySpan) -> Vec<MelodyNote> {
        spans_notes(t, &[s]).remove(0)
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
        let n = one(&t, span(0, 1280));
        assert_eq!(n.len(), 2);
        assert_eq!((n[0].midi, n[1].midi), (57.0, 64.0));
        assert_eq!((n[0].start_ms, n[0].end_ms), (0, 480));
        assert_eq!((n[1].start_ms, n[1].end_ms), (640, 1120));
    }

    #[test]
    fn a_step_without_a_gap_is_two_notes_and_a_spike_is_none() {
        let mut t = track(&[(20, 220.0), (20, 261.6256)]);
        assert_eq!(one(&t, span(0, 640)).len(), 2);
        t = track(&[(10, 220.0), (1, 440.0), (10, 220.0)]);
        let n = one(&t, span(0, 400));
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].midi, 57.0);
    }

    #[test]
    fn a_burst_far_out_of_the_register_is_dropped() {
        let t = track(&[(40, 220.0), (20, 1396.913), (40, 220.0)]);
        let n = one(&t, span(0, 1600));
        assert_eq!(n.iter().map(|n| n.midi).collect::<Vec<_>>(), [57.0, 57.0]);
        assert_eq!((n[0].end_ms, n[1].start_ms), (640, 960));
    }

    #[test]
    fn an_octave_error_inside_a_note_is_folded_back() {
        let t = track(&[(20, 220.0), (8, 440.0), (20, 220.0)]);
        let n = one(&t, span(0, 768));
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].midi, 57.0);
    }

    #[test]
    fn a_sung_octave_leap_between_two_notes_is_kept() {
        let t = track(&[(40, 220.0), (15, 440.0), (40, 220.0)]);
        let n = one(&t, span(0, 1520));
        assert_eq!(
            n.iter().map(|n| n.midi).collect::<Vec<_>>(),
            [57.0, 69.0, 57.0]
        );
    }

    fn mids(parts: &[(usize, f32)]) -> Vec<f64> {
        let n = one(&track(parts), span(0, 100_000));
        n.iter().map(|n| n.midi).collect()
    }

    #[test]
    fn an_octave_run_split_by_a_gap_is_one_run() {
        let up = [(40, 220.0), (8, 440.0), (2, 0.0), (9, 440.0), (40, 220.0)];
        assert_eq!(mids(&up), [57.0, 69.0, 57.0]);
        let short = [(20, 220.0), (5, 440.0), (2, 0.0), (4, 440.0), (20, 220.0)];
        assert_eq!(mids(&short), [57.0]);
    }

    #[test]
    fn alternating_runs_an_octave_apart_come_out_as_one_register() {
        let hops: Vec<(usize, f32)> = (0..8)
            .map(|i| (20, if i % 2 == 0 { 220.0 } else { 440.0 }))
            .collect();
        let m = mids(&hops);
        assert!(m.iter().all(|&x| (x - m[0]).abs() < 1.0), "{m:?}");
    }

    #[test]
    fn a_short_run_entered_by_a_non_octave_step_is_a_sung_note() {
        assert_eq!(
            mids(&[(40, 220.0), (10, 415.3), (40, 220.0)]),
            [57.0, 68.0, 57.0]
        );
    }

    #[test]
    fn a_long_octave_run_is_kept_even_with_octave_edges() {
        assert_eq!(
            mids(&[(40, 220.0), (20, 440.0), (40, 220.0)]),
            [57.0, 69.0, 57.0]
        );
    }

    #[test]
    fn a_high_section_far_from_the_rest_of_the_track_is_kept() {
        let t = track(&[(800, 220.0), (400, 880.0)]);
        let n = one(&t, span(0, 19_200));
        assert!(n.iter().any(|n| n.midi == 81.0 && n.end_ms >= 19_000));
    }

    #[test]
    fn voiced_frames_count_for_the_minimum_not_the_span() {
        let t = track(&[(1, 220.0), (8, 0.0), (1, 220.0)]);
        assert!(one(&t, span(0, 200)).is_empty());
    }

    #[test]
    fn a_fragment_between_two_equal_notes_is_dropped_and_they_merge() {
        let t = track(&[(20, 220.0), (4, 329.6276), (20, 220.0)]);
        let n = one(&t, span(0, 704));
        assert_eq!(n.len(), 1);
        assert_eq!((n[0].midi, n[0].end_ms), (57.0, 704));
    }

    #[test]
    fn a_short_gap_inside_a_word_is_bridged() {
        let t = track(&[(10, 220.0), (3, 0.0), (10, 220.0)]);
        assert_eq!(one(&t, span(0, 500)).len(), 1);
    }

    #[test]
    fn a_short_note_lengthens_the_note_before_it() {
        let t = track(&[(40, 220.0), (5, 246.94), (8, 0.0), (20, 196.0)]);
        let n = one(&t, span(0, 1200));
        assert_eq!(n.iter().map(|n| n.midi).collect::<Vec<_>>(), [57.0, 55.0]);
        assert_eq!(n[0].end_ms, 720);
    }

    #[test]
    fn a_short_note_lengthens_the_note_after_it_when_none_precedes() {
        let t = track(&[(5, 246.94), (3, 0.0), (20, 196.0)]);
        let n = one(&t, span(0, 800));
        assert_eq!(n.iter().map(|n| n.midi).collect::<Vec<_>>(), [55.0]);
        assert_eq!(n[0].start_ms, 0);
    }

    #[test]
    fn a_fifth_harmonic_out_of_the_register_is_folded_to_the_voice() {
        assert_eq!(mids(&[(40, 220.0), (12, 1100.0), (40, 220.0)]), [57.0]);
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

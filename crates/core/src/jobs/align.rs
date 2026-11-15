//! The pure half of `Align`: known lyric words matched onto the words a speech recogniser heard
//! in the vocal stem, and timed from them. The recogniser runs in the engine. Spec:
//! `docs/spec/karaoke.md` section 4.
use crate::api::{AlignLine, AlignedLine, AlignedWord, JobKind, LrcFit};

const MAX_LINES: usize = 3000;
const MAX_WORDS: usize = 6000;
/// Milliseconds a heard word may lie outside its line's times and still match it.
const WINDOW_MARGIN_MS: u32 = 1500;
/// Time given to a word nothing was heard for, per character, and its floor.
const MS_PER_CHAR: u32 = 70;
const MIN_WORD_MS: u32 = 120;
const MAX_REPEAT: u32 = 9;
/// Lines the fit needs, matched at least this well by the untimed pass: fewer is noise.
const FIT_MIN_LINES: usize = 8;
const FIT_MATCHED: f64 = 0.8;
/// Line pairs closer than this give a slope dominated by the 20 ms frame rounding.
const FIT_MIN_SPAN_MS: f64 = 5000.0;
/// A fit beyond these bounds is a mismatched file, not a recording speed or an intro.
const FIT_MAX_SCALE_DEV: f64 = 0.15;
const FIT_MAX_OFFSET_MS: f64 = 30_000.0;
/// The times are flagged unreliable past these (a few percent is a different cut or speed).
const UNRELIABLE_SCALE_DEV: f64 = 0.03;
const UNRELIABLE_OFFSET_MS: f64 = 1000.0;
/// Another pass replaces the timed one only when it finds this much more.
const MIN_GAIN: f64 = 0.01;
/// The remapped pass beats the untimed one unless it finds this much less: it keeps the
/// protection the bands give repeated choruses.
const ANCHOR_MARGIN: f64 = 0.02;
/// A timed pass finding this share needs no other.
pub(super) const GOOD_ENOUGH: f64 = 0.99;

/// A word the recogniser heard.
#[derive(Debug, Clone, PartialEq)]
pub struct Heard {
    pub text: String,
    pub start_ms: u32,
    pub end_ms: u32,
}

pub fn validate(kind: &JobKind) -> Result<(), String> {
    let JobKind::Align {
        lines, language, ..
    } = kind
    else {
        return Ok(());
    };
    if let Some(l) = language
        && (!(2..=3).contains(&l.len()) || !l.bytes().all(|b| b.is_ascii_lowercase()))
    {
        return Err("language must be an ISO 639 code in lowercase".into());
    }
    if lines.len() > MAX_LINES {
        return Err(format!("Align takes at most {MAX_LINES} lines"));
    }
    if let Some(i) = lines
        .iter()
        .position(|l| l.start_ms.zip(l.end_ms).is_some_and(|(s, e)| e < s))
    {
        return Err(format!("line {i} ends before it starts"));
    }
    if words_of(lines).iter().map(|l| l.words.len()).sum::<usize>() > MAX_WORDS {
        return Err(format!("Align takes at most {MAX_WORDS} words"));
    }
    if lines.iter().all(|l| l.text.trim().is_empty()) {
        return Err("Align needs at least one word".into());
    }
    Ok(())
}

/// How a lyric word counts: sung as written, optional, or not sung at all.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Role {
    Word,
    /// Inside parentheses (one group per pair) or in a repeat of a line sung once (group 0): it may
    /// be skipped, and only counts when it is found.
    Optional(u32),
    /// Only a `[...]` section label.
    Label,
}

/// A line copy to place: the words of `text` (byte ranges into it) and their roles.
#[derive(Debug, Clone, PartialEq)]
pub struct Copy {
    pub line: u32,
    pub repeat: u32,
    /// Times the line is known to have, shared out when it repeats.
    pub window: Option<(u32, u32)>,
    pub words: Vec<(u32, u32)>,
    pub roles: Vec<Role>,
    pub text: String,
}

/// `x2`, `(x2)`, `[x2]`, `2x`, `(2x)`, with `x` or `×`: how many times a line is sung.
fn repeat_marker(token: &str) -> Option<u32> {
    let t = token
        .trim_matches(|c| matches!(c, '(' | ')' | '[' | ']'))
        .to_lowercase();
    let n = t
        .strip_prefix(['x', '×'])
        .or_else(|| t.strip_suffix(['x', '×']))?;
    n.parse::<u32>()
        .ok()
        .filter(|n| (2..=MAX_REPEAT).contains(n))
}

/// The copies to place, repeats expanded: a line marked `x2` comes twice.
pub fn words_of(lines: &[AlignLine]) -> Vec<Copy> {
    let mut out = vec![];
    for (i, l) in lines.iter().enumerate() {
        let mut words = vec![];
        let mut roles = vec![];
        let (mut times, mut at) = (1, 0);
        let (mut parens, mut brackets, mut group) = (0u32, 0u32, 0u32);
        for tok in l.text.split_inclusive(char::is_whitespace) {
            let word = tok.trim_end();
            let lead = tok.len() - tok.trim_start().len();
            let start = at + lead;
            at += tok.len();
            if word.trim().is_empty() {
                continue;
            }
            let (s, e) = (
                start as u32,
                (at - (tok.len() - tok.trim_end().len())) as u32,
            );
            let w = &l.text[s as usize..e as usize];
            if let Some(n) = repeat_marker(w) {
                times = n;
                continue;
            }
            words.push((s, e));
            let (mut grouped, mut bare) = (parens > 0 || w.starts_with('('), false);
            let labelled = brackets > 0 || w.starts_with('[');
            for c in w.chars() {
                match c {
                    '(' => {
                        if parens == 0 {
                            group += 1;
                        }
                        parens += 1;
                        grouped = true;
                    }
                    ')' => parens = parens.saturating_sub(1),
                    '[' => brackets += 1,
                    ']' => brackets = brackets.saturating_sub(1),
                    c if c.is_alphanumeric() && brackets == 0 => bare = true,
                    _ => {}
                }
            }
            roles.push(match (labelled && !bare, grouped) {
                (true, _) => Role::Label,
                (_, true) => Role::Optional(group),
                _ => Role::Word,
            });
        }
        for r in 0..times {
            let window = l.start_ms.zip(l.end_ms).map(|(s, e)| {
                let part = (e - s) / times;
                (
                    s + part * r,
                    if r + 1 == times {
                        e
                    } else {
                        s + part * (r + 1)
                    },
                )
            });
            let roles = if r == 0 {
                roles.clone()
            } else {
                roles
                    .iter()
                    .map(|k| {
                        if *k == Role::Label {
                            *k
                        } else {
                            Role::Optional(0)
                        }
                    })
                    .collect()
            };
            out.push(Copy {
                line: i as u32,
                repeat: r,
                window,
                words: words.clone(),
                roles,
                text: l.text.clone(),
            });
        }
    }
    out
}

fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn distance(a: &[char], b: &[char]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            cur.push(
                (prev[j] + usize::from(ca != cb))
                    .min(prev[j + 1] + 1)
                    .min(cur[j] + 1),
            );
        }
        prev = cur;
    }
    prev[b.len()]
}

/// How well a lyric word fits a heard one: 3 for the same, 1 for a close misreading, 0 for none.
fn similarity(a: &str, b: &str) -> i32 {
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    if a == b {
        return 3;
    }
    let (ca, cb): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let longest = ca.len().max(cb.len());
    i32::from(longest >= 4 && distance(&ca, &cb) <= longest / 4)
}

struct Slot {
    key: String,
    window: Option<(u32, u32)>,
}

/// Lyric words in order onto heard ones: the best order-preserving pairing by similarity, a
/// word only pairing with a heard one near its line's times when it has them.
fn pair(slots: &[Slot], heard: &[Heard], keys: &[String]) -> Vec<Option<usize>> {
    let (n, m) = (slots.len(), heard.len());
    let allowed = |i: usize, j: usize| match slots[i].window {
        Some((s, e)) => {
            heard[j].end_ms + WINDOW_MARGIN_MS >= s && heard[j].start_ms <= e + WINDOW_MARGIN_MS
        }
        None => true,
    };
    let mut score = vec![0i32; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in 1..=n {
        for j in 1..=m {
            let mut best = score[at(i - 1, j)].max(score[at(i, j - 1)]);
            if allowed(i - 1, j - 1) {
                let s = similarity(&slots[i - 1].key, &keys[j - 1]);
                if s > 0 {
                    best = best.max(score[at(i - 1, j - 1)] + s);
                }
            }
            score[at(i, j)] = best;
        }
    }
    let mut out = vec![None; n];
    let (mut i, mut j) = (n, m);
    while i > 0 && j > 0 {
        let here = score[at(i, j)];
        if here == score[at(i - 1, j)] {
            i -= 1;
        } else if here == score[at(i, j - 1)] {
            j -= 1;
        } else {
            out[i - 1] = Some(j - 1);
            i -= 1;
            j -= 1;
        }
    }
    out
}

/// The lines placed, the share of lyric words found and how the request's times map on the audio.
#[derive(Debug, Clone, PartialEq)]
pub struct Aligned {
    pub lines: Vec<AlignedLine>,
    pub matched: f64,
    pub lrc_fit: Option<LrcFit>,
}

/// The same lines without times.
pub(super) fn untimed(lines: &[AlignLine]) -> Vec<AlignLine> {
    lines
        .iter()
        .map(|l| AlignLine {
            text: l.text.clone(),
            start_ms: None,
            end_ms: None,
        })
        .collect()
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// Theil-Sen line `y = a + b x` through `(x, y)` pairs, `None` under `FIT_MIN_LINES` pairs.
fn theil_sen(pts: &[(f64, f64)]) -> Option<(f64, f64)> {
    if pts.len() < FIT_MIN_LINES {
        return None;
    }
    let mut slopes = vec![];
    for (i, a) in pts.iter().enumerate() {
        for b in &pts[i + 1..] {
            if (b.0 - a.0).abs() >= FIT_MIN_SPAN_MS {
                slopes.push((b.1 - a.1) / (b.0 - a.0));
            }
        }
    }
    if slopes.is_empty() {
        return None;
    }
    let b = median(slopes);
    Some((median(pts.iter().map(|p| p.1 - b * p.0).collect()), b))
}

/// One placement pass: the lines, the share of words found and, per word in flat order, whether
/// it was found.
pub(super) type Run = (Vec<AlignedLine>, f64, Vec<bool>);

/// The map from the request's line times to the audio, fitted on the first copy of every line of
/// `run` that has a found word and is found at `FIT_MATCHED`, at that word's start. `None`
/// without enough lines or when the fit leaves its bounds.
pub(super) fn fit_lrc(lines: &[AlignLine], run: &Run, duration_ms: u32) -> Option<LrcFit> {
    let (mut pts, mut at) = (vec![], 0);
    for (c, p) in words_of(lines).iter().zip(&run.0) {
        let first = (0..c.words.len()).find(|k| run.2[at + k] && c.roles[*k] == Role::Word);
        at += c.words.len();
        if let (0, true, Some(k), Some((s, _))) =
            (c.repeat, p.matched >= FIT_MATCHED, first, c.window)
        {
            pts.push((f64::from(s), f64::from(p.words[k].start_ms)));
        }
    }
    let (offset_ms, scale) = theil_sen(&pts)?;
    if (scale - 1.0).abs() > FIT_MAX_SCALE_DEV || offset_ms.abs() > FIT_MAX_OFFSET_MS {
        return None;
    }
    let last = lines.iter().filter_map(|l| l.start_ms).max().unwrap_or(0);
    Some(LrcFit {
        offset_ms,
        scale,
        unreliable: (scale - 1.0).abs() > UNRELIABLE_SCALE_DEV
            || offset_ms.abs() > UNRELIABLE_OFFSET_MS
            || last > duration_ms,
    })
}

/// The lines with their times mapped onto the audio.
pub(super) fn remap(lines: &[AlignLine], fit: &LrcFit) -> Vec<AlignLine> {
    let map = |t: u32| (fit.offset_ms + fit.scale * f64::from(t)).round().max(0.0) as u32;
    lines
        .iter()
        .map(|l| {
            let start_ms = l.start_ms.map(map);
            AlignLine {
                text: l.text.clone(),
                start_ms,
                end_ms: l.end_ms.map(map).zip(start_ms).map(|(e, s)| e.max(s)),
            }
        })
        .collect()
}

/// Which pass to keep.
#[derive(Debug, PartialEq)]
pub(super) enum Pass {
    Timed,
    Untimed,
    Remapped,
}

/// The timed pass unless another finds `MIN_GAIN` more; the remapped one over the untimed one
/// unless it finds `ANCHOR_MARGIN` less.
pub(super) fn prefer(timed: f64, untimed: Option<f64>, remapped: Option<f64>) -> Pass {
    let (pass, share) = match (untimed, remapped) {
        (Some(u), Some(r)) if r >= u - ANCHOR_MARGIN => (Pass::Remapped, r),
        (Some(u), _) => (Pass::Untimed, u),
        (None, Some(r)) => (Pass::Remapped, r),
        (None, None) => return Pass::Timed,
    };
    if share > timed + MIN_GAIN {
        pass
    } else {
        Pass::Timed
    }
}

/// The result of `timed`, the pass over the lines as given, or of a better pass: `again` runs the
/// same search on other lines (`None`: it failed). The track is `duration_ms` long.
///
/// When `timed` finds under `GOOD_ENOUGH`, the lines are searched untimed and with their times
/// mapped by a fit on that pass; the timed pass is kept unless another finds `MIN_GAIN` more. A
/// fit that is not unreliable never gives up the bands (they protect repeated choruses): only
/// the mapped pass may replace the timed one. The fit is reported whichever pass is kept.
pub(super) fn choose(
    lines: &[AlignLine],
    duration_ms: u32,
    timed: Run,
    again: impl Fn(&[AlignLine]) -> Option<Run>,
) -> Aligned {
    let done = |run: Run, lrc_fit| Aligned {
        lines: run.0,
        matched: run.1,
        lrc_fit,
    };
    if lines.iter().all(|l| l.start_ms.is_none()) {
        return done(timed, None);
    }
    if timed.1 >= GOOD_ENOUGH {
        let fit = fit_lrc(lines, &timed, duration_ms);
        return done(timed, fit);
    }
    let free = again(&untimed(lines));
    let fit = free.as_ref().and_then(|f| fit_lrc(lines, f, duration_ms));
    let remapped = fit.as_ref().and_then(|f| again(&remap(lines, f)));
    let allow_free = fit.is_none_or(|f| f.unreliable);
    let choice = prefer(
        timed.1,
        free.as_ref().map(|f| f.1).filter(|_| allow_free),
        remapped.as_ref().map(|r| r.1),
    );
    let run = match (choice, free, remapped) {
        (Pass::Remapped, _, Some(r)) => r,
        (Pass::Untimed, Some(f), _) => f,
        _ => timed,
    };
    done(run, fit)
}

/// The lines placed on the audio (`duration_ms` long), and the share of lyric words that were
/// heard; a request whose times do not fit the audio is placed by `choose`.
pub fn align_fitted(lines: &[AlignLine], heard: &[Heard], duration_ms: u32) -> Aligned {
    let lines = &ended(lines, duration_ms);
    choose(lines, duration_ms, run(lines, heard), |l| {
        Some(run(l, heard))
    })
}

/// The lines, a line with a start and no end ending where the next start is, else at `duration_ms`.
pub(super) fn ended(lines: &[AlignLine], duration_ms: u32) -> Vec<AlignLine> {
    let mut out = lines.to_vec();
    for (i, l) in out.iter_mut().enumerate() {
        let next = lines[i + 1..].iter().find_map(|n| n.start_ms);
        if let (Some(s), None) = (l.start_ms, l.end_ms) {
            l.end_ms = Some(next.unwrap_or(duration_ms).max(s));
        }
    }
    out
}

/// The lines placed on the audio, and the share of lyric words that were heard.
pub fn align(lines: &[AlignLine], heard: &[Heard]) -> (Vec<AlignedLine>, f64) {
    let (out, share, _) = run(lines, heard);
    (out, share)
}

fn run(lines: &[AlignLine], heard: &[Heard]) -> Run {
    let copies = words_of(lines);
    let heard: Vec<Heard> = heard
        .iter()
        .filter(|h| !norm(&h.text).is_empty())
        .cloned()
        .collect();
    let keys: Vec<String> = heard.iter().map(|h| norm(&h.text)).collect();
    let mut slots = vec![];
    for c in &copies {
        for ((s, e), role) in c.words.iter().zip(&c.roles) {
            let key = match role {
                Role::Label => String::new(),
                _ => norm(&c.text[*s as usize..*e as usize]),
            };
            slots.push(Slot {
                key,
                window: c.window,
            });
        }
    }
    let paired = pair(&slots, &heard, &keys);
    let mut times: Vec<Option<(u32, u32)>> = paired
        .iter()
        .map(|p| {
            p.map(|j| {
                (
                    heard[j].start_ms,
                    heard[j].end_ms.max(heard[j].start_ms + 1),
                )
            })
        })
        .collect();
    let found: Vec<bool> = paired.iter().map(Option::is_some).collect();
    let (matched, share) = shares(&copies, &found);
    (
        finish(&copies, &mut times, &matched, u32::MAX),
        share,
        found,
    )
}

/// Per copy, and over the track, the share of its words found: the optional ones count only when
/// found, the labels never. A copy with nothing to find is whole (1), a track with none is 0.
pub(super) fn shares(copies: &[Copy], found: &[bool]) -> (Vec<f64>, f64) {
    let (mut at, mut track) = (0, (0usize, 0usize));
    let per = copies
        .iter()
        .map(|c| {
            let (mut hit, mut counted) = (0, 0);
            for (k, role) in c.roles.iter().enumerate() {
                let f = found[at + k];
                counted += usize::from(f || *role == Role::Word);
                hit += usize::from(f);
            }
            at += c.words.len();
            track = (track.0 + hit, track.1 + counted);
            if counted == 0 {
                1.0
            } else {
                hit as f64 / counted as f64
            }
        })
        .collect();
    (
        per,
        if track.1 == 0 {
            0.0
        } else {
            track.0 as f64 / track.1 as f64
        },
    )
}

/// The lines of `copies` from the word times in `times` (flat word order): the words nothing
/// was placed for are timed first, see `fill`. `matched` is each copy's share found, `end_ms` the
/// track's length.
pub(super) fn finish(
    copies: &[Copy],
    times: &mut [Option<(u32, u32)>],
    matched: &[f64],
    end_ms: u32,
) -> Vec<AlignedLine> {
    fill(copies, times, end_ms);
    let mut out: Vec<AlignedLine> = vec![];
    let mut next = 0;
    let mut last_end = 0;
    for (c, matched) in copies.iter().zip(matched) {
        let words: Vec<AlignedWord> = c
            .words
            .iter()
            .enumerate()
            .map(|(wi, (s, e))| {
                let (start_ms, end_ms) = times[next + wi].unwrap_or((last_end, last_end));
                AlignedWord {
                    text: c.text[*s as usize..*e as usize].to_string(),
                    byte_start: *s,
                    byte_end: *e,
                    start_ms,
                    end_ms,
                }
            })
            .collect();
        next += c.words.len();
        let (start_ms, end_ms) = match (words.first(), words.last()) {
            (Some(f), Some(l)) => (f.start_ms, l.end_ms),
            _ => (last_end, last_end),
        };
        last_end = end_ms;
        out.push(AlignedLine {
            line: c.line,
            repeat: c.repeat,
            start_ms,
            end_ms,
            matched: *matched,
            words,
        });
    }
    out
}

/// Times the words nothing was placed for: shared out, by length, between the placed words
/// around them and inside their line's times, none past `end_ms`. The result never runs backwards.
fn fill(copies: &[Copy], times: &mut [Option<(u32, u32)>], end_ms: u32) {
    // Per word: its weight, its copy's times and whether it opens its copy.
    let flat: Vec<_> = copies
        .iter()
        .flat_map(|c| {
            c.words.iter().enumerate().map(move |(k, (s, e))| {
                let key = norm(&c.text[*s as usize..*e as usize]);
                (key.chars().count() as u32 + 1, c.window, k == 0)
            })
        })
        .collect();
    let n = times.len();
    let mut i = 0;
    while i < n {
        if times[i].is_some() {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < n && times[j].is_none() && !(flat[j].2 && flat[j].1.is_some()) {
            j += 1;
        }
        let weights: Vec<u32> = (i..j).map(|k| flat[k].0).collect();
        let total: u32 = weights.iter().sum();
        let mut left = i.checked_sub(1).and_then(|k| times[k]).map_or(0, |t| t.1);
        if let Some((s, _)) = flat[i].1 {
            left = left.max(s);
        }
        let next = times.get(j).copied().flatten();
        let after = times[j..].iter().flatten().next().copied();
        if let Some((s, _)) = after {
            left = left.min(s);
        }
        let own = (total * MS_PER_CHAR).max(MIN_WORD_MS * (j - i) as u32);
        let mut right = next.map_or(left + own, |t| t.0);
        // A timed line that ends the gap is not stretched past its own end to the next one.
        if let Some((_, e)) = flat[j - 1].1
            && (next.is_none() || flat[j].2)
        {
            right = (left + own).min(e.max(left + MIN_WORD_MS));
        }
        if let Some((s, _)) = after {
            right = right.min(s);
        }
        if right <= left {
            right = left + MIN_WORD_MS * (j - i) as u32;
        }
        let mut at = left;
        let mut spent = 0;
        for (w, k) in (i..j).enumerate() {
            spent += weights[w];
            let end =
                left + ((u64::from(right - left) * u64::from(spent)) / u64::from(total)) as u32;
            times[k] = Some((at.min(end_ms), end.max(at).min(end_ms)));
            at = end;
        }
        i = j;
    }
    // A word that outlasts its successor is trimmed, from the end so a collapse carries back.
    for k in (1..n).rev() {
        if let (Some(a), Some(b)) = (times[k - 1], times[k])
            && a.1 > b.0
        {
            times[k - 1] = Some((a.0.min(b.0), b.0));
        }
    }
}

/// The words `whisper-cli`'s JSON holds (`-ml 1`: one word per entry), in order.
pub fn heard_from_json(json: &str) -> Result<Vec<Heard>, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("whisper output: {e}"))?;
    let items = v
        .get("transcription")
        .and_then(|t| t.as_array())
        .ok_or("whisper output has no transcription")?;
    Ok(items
        .iter()
        .filter_map(|it| {
            let text = it.get("text")?.as_str()?.trim();
            let off = it.get("offsets")?;
            let (from, to) = (off.get("from")?.as_i64()?, off.get("to")?.as_i64()?);
            // Sound labels (`[MUSIC]`, `(applause)`) are not words.
            let label = (text.starts_with('[') && text.ends_with(']'))
                || (text.starts_with('(') && text.ends_with(')'));
            (!text.is_empty() && !label).then(|| Heard {
                text: text.to_string(),
                start_ms: from.max(0) as u32,
                end_ms: to.max(from).max(0) as u32,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lrc(start: u32) -> AlignLine {
        AlignLine {
            text: "x".into(),
            start_ms: Some(start),
            end_ms: Some(start + 1000),
        }
    }

    fn placed(lines: &[AlignLine], map: impl Fn(u32) -> u32) -> Run {
        let out: Vec<AlignedLine> = lines
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let s = map(l.start_ms.unwrap());
                AlignedLine {
                    line: i as u32,
                    repeat: 0,
                    start_ms: s,
                    end_ms: s + 500,
                    matched: 1.0,
                    words: vec![AlignedWord {
                        text: "x".into(),
                        byte_start: 0,
                        byte_end: 1,
                        start_ms: s,
                        end_ms: s + 500,
                    }],
                }
            })
            .collect();
        let n = out.len();
        (out, 1.0, vec![true; n])
    }

    #[test]
    fn theil_sen_ignores_outliers() {
        let mut pts: Vec<(f64, f64)> = (0..20)
            .map(|k| (k as f64 * 6000.0, 1349.0 + 0.931 * k as f64 * 6000.0))
            .collect();
        pts[3].1 += 40_000.0;
        pts[11].1 -= 25_000.0;
        let (a, b) = theil_sen(&pts).unwrap();
        assert!(
            (b - 0.931).abs() < 1e-6 && (a - 1349.0).abs() < 1.0,
            "{a} {b}"
        );
        assert_eq!(theil_sen(&pts[..FIT_MIN_LINES - 1]), None);
    }

    #[test]
    fn the_fit_flags_a_file_made_for_another_recording() {
        let lines: Vec<AlignLine> = (0..20).map(|k| lrc(k * 6000)).collect();
        let slow = placed(&lines, |t| 1349 + (0.931 * f64::from(t)) as u32);
        let fit = fit_lrc(&lines, &slow, 200_000).unwrap();
        assert!(fit.unreliable && (fit.scale - 0.931).abs() < 1e-3);
        let exact = placed(&lines, |t| t + 200);
        assert!(!fit_lrc(&lines, &exact, 200_000).unwrap().unreliable);
        assert!(fit_lrc(&lines, &exact, 100_000).unwrap().unreliable);
        let wild = placed(&lines, |t| t * 2);
        assert_eq!(fit_lrc(&lines, &wild, 400_000), None);
        assert_eq!(
            fit_lrc(&lines[..5], &placed(&lines[..5], |t| t + 200), 200_000),
            None
        );
    }

    #[test]
    fn the_recogniser_pairs_lines_whose_times_drifted_with_a_fit() {
        let word = |k: u32| {
            format!(
                "w{}{}",
                (b'a' + (k / 26) as u8) as char,
                (b'a' + (k % 26) as u8) as char
            )
        };
        let heard: Vec<Heard> = (0..30)
            .map(|k| Heard {
                text: word(k),
                start_ms: 1000 + 2000 * k,
                end_ms: 1500 + 2000 * k,
            })
            .collect();
        let lines: Vec<AlignLine> = (0..30)
            .map(|k| {
                let s = (f64::from(1000 + 2000 * k) / 0.9) as u32;
                AlignLine {
                    text: word(k),
                    start_ms: Some(s),
                    end_ms: Some(s + 1000),
                }
            })
            .collect();
        let (_, timed) = align(&lines, &heard);
        let got = align_fitted(&lines, &heard, 62_000);
        assert!(timed < 0.5 && got.matched > 0.95, "{timed} {}", got.matched);
        assert!(got.lrc_fit.unwrap().unreliable);
    }

    fn with_share(mut r: Run, share: f64) -> Run {
        r.1 = share;
        r
    }

    #[test]
    fn a_reliable_fit_keeps_the_bands_of_the_timed_lines() {
        let lines: Vec<AlignLine> = (0..20).map(|k| lrc(k * 6000)).collect();
        let again = |map: fn(u32) -> u32| {
            let lines = lines.clone();
            move |l: &[AlignLine]| {
                let free = l[0].start_ms.is_none();
                Some(with_share(
                    placed(&lines, map),
                    if free { 0.99 } else { 0.6 },
                ))
            }
        };
        let timed = || with_share(placed(&lines, |t| t), 0.5);
        let near = choose(&lines, 200_000, timed(), again(|t| t + 200));
        assert_eq!(
            (near.matched, near.lrc_fit.map(|f| f.unreliable)),
            (0.6, Some(false))
        );
        let far = choose(&lines, 200_000, timed(), again(|t| t * 9 / 10));
        assert_eq!(
            (far.matched, far.lrc_fit.map(|f| f.unreliable)),
            (0.99, Some(true))
        );
    }

    #[test]
    fn the_fit_reports_on_a_timed_pass_that_found_everything() {
        let lines: Vec<AlignLine> = (0..20).map(|k| lrc(k * 6000)).collect();
        let got = choose(&lines, 200_000, placed(&lines, |t| t + 1500), |_| None);
        assert!(got.lrc_fit.unwrap().unreliable);
        let none = choose(&untimed(&lines), 200_000, placed(&lines, |t| t), |_| None);
        assert_eq!(none.lrc_fit, None);
    }

    #[test]
    fn the_recogniser_flags_a_last_line_past_the_real_track_length() {
        let word = |k: u32| format!("w{}", (b'a' + k as u8) as char);
        let heard: Vec<Heard> = (0..25)
            .map(|k| Heard {
                text: word(k),
                start_ms: 1000 + 2000 * k,
                end_ms: 1500 + 2000 * k,
            })
            .collect();
        let mut lines: Vec<AlignLine> = (0..25)
            .map(|k| AlignLine {
                text: word(k),
                start_ms: Some(1300 + 2000 * k),
                end_ms: Some(2300 + 2000 * k),
            })
            .collect();
        lines.push(lrc(100_000));
        lines[25].text = "unheard".into();
        let flag = |ms| align_fitted(&lines, &heard, ms).lrc_fit.unwrap().unreliable;
        assert!(!flag(150_000));
        assert!(flag(60_000));
    }

    #[test]
    fn optional_and_unfound_words_do_not_feed_the_fit() {
        let lines: Vec<AlignLine> = (0..20)
            .map(|k| AlignLine {
                text: "(a) b".into(),
                start_ms: Some(k * 6000),
                end_ms: Some(k * 6000 + 1000),
            })
            .collect();
        let line = |k: u32, starts: [u32; 2]| AlignedLine {
            line: k,
            repeat: 0,
            start_ms: starts[0],
            end_ms: starts[1] + 100,
            matched: 1.0,
            words: ["(a)", "b"]
                .iter()
                .zip(starts)
                .map(|(t, s)| AlignedWord {
                    text: (*t).into(),
                    byte_start: 0,
                    byte_end: 1,
                    start_ms: s,
                    end_ms: s + 100,
                })
                .collect(),
        };
        let out: Vec<AlignedLine> = (0..20).map(|k| line(k, [0, k * 6000 + 200])).collect();
        let run: Run = (out.clone(), 1.0, [true, true].repeat(20));
        let fit = fit_lrc(&lines, &run, 200_000).unwrap();
        assert!((fit.offset_ms - 200.0).abs() < 1.0 && (fit.scale - 1.0).abs() < 1e-6);
        let only_optional: Run = (out, 1.0, [true, false].repeat(20));
        assert_eq!(fit_lrc(&lines, &only_optional, 200_000), None);
    }

    #[test]
    fn remapped_times_follow_the_fit() {
        let fit = LrcFit {
            offset_ms: 1000.0,
            scale: 0.5,
            unreliable: true,
        };
        let out = remap(&[lrc(4000)], &fit);
        assert_eq!((out[0].start_ms, out[0].end_ms), (Some(3000), Some(3500)));
    }

    #[test]
    fn a_pass_replaces_the_timed_one_only_for_a_clear_gain() {
        assert_eq!(prefer(0.9, Some(0.905), None), Pass::Timed);
        assert_eq!(prefer(0.25, Some(0.93), Some(0.915)), Pass::Remapped);
        assert_eq!(prefer(0.25, Some(0.93), Some(0.8)), Pass::Untimed);
        assert_eq!(prefer(0.25, Some(0.93), None), Pass::Untimed);
        assert_eq!(prefer(0.9, None, None), Pass::Timed);
    }

    fn line(text: &str) -> AlignLine {
        AlignLine {
            text: text.into(),
            start_ms: None,
            end_ms: None,
        }
    }

    fn timed(text: &str, s: u32, e: u32) -> AlignLine {
        AlignLine {
            text: text.into(),
            start_ms: Some(s),
            end_ms: Some(e),
        }
    }

    fn heard(words: &[(&str, u32, u32)]) -> Vec<Heard> {
        words
            .iter()
            .map(|(t, s, e)| Heard {
                text: (*t).into(),
                start_ms: *s,
                end_ms: *e,
            })
            .collect()
    }

    #[test]
    fn words_keep_their_byte_ranges_and_a_repeat_marker_expands_the_line() {
        let c = words_of(&[line("Héllo  wörld (x2)"), line("2x la la")]);
        assert_eq!(c.len(), 4);
        assert_eq!((c[0].line, c[0].repeat, c[1].repeat), (0, 0, 1));
        assert_eq!(c[0].words.len(), 2);
        let (s, e) = c[0].words[1];
        assert_eq!(&c[0].text[s as usize..e as usize], "wörld");
        assert_eq!((c[2].line, c[3].repeat, c[2].words.len()), (1, 1, 2));
        assert_eq!(words_of(&[line("take x"), line("x10")]).len(), 2);
    }

    #[test]
    fn words_in_parentheses_and_repeats_are_optional_and_labels_are_not_words() {
        let c = words_of(&[
            line("a (b c) d (e) [Chorus] [Verse 1] f(g"),
            line("x (y) 2x"),
        ]);
        use Role::*;
        assert_eq!(
            c[0].roles,
            vec![
                Word,
                Optional(1),
                Optional(1),
                Word,
                Optional(2),
                Label,
                Label,
                Label,
                Optional(3)
            ]
        );
        assert_eq!(c[1].roles, vec![Word, Optional(1)]);
        assert_eq!(c[2].roles, vec![Optional(0), Optional(0)]);
    }

    #[test]
    fn optional_words_count_only_when_heard() {
        let l = [line("one (two) three (four)")];
        let h = heard(&[
            ("one", 1000, 1200),
            ("two", 1300, 1500),
            ("three", 1600, 1900),
        ]);
        let (out, share) = align(&l, &h);
        assert_eq!((share, out[0].matched), (1.0, 1.0));
        let (out, share) = align(&l, &h[..1]);
        assert!((share - 0.5).abs() < 1e-9 && (out[0].matched - 0.5).abs() < 1e-9);
    }

    #[test]
    fn matched_words_take_the_heard_times_and_misreadings_still_match() {
        let l = [line("Hello, world"), line("sing along tonight")];
        let h = heard(&[
            ("Hello", 1000, 1400),
            ("word", 1400, 1900),
            ("sing", 3000, 3300),
            ("along", 3300, 3800),
            ("tonight", 3800, 4500),
        ]);
        let (out, share) = align(&l, &h);
        assert_eq!(share, 1.0);
        assert_eq!((out[0].start_ms, out[0].end_ms), (1000, 1900));
        assert_eq!(out[0].words[1].text, "world");
        assert_eq!(out[1].words[2].start_ms, 3800);
    }

    #[test]
    fn a_word_nothing_was_heard_for_is_placed_between_its_neighbours() {
        let l = [line("one two three")];
        let h = heard(&[("one", 1000, 1500), ("three", 2500, 3000)]);
        let (out, share) = align(&l, &h);
        assert!((share - 2.0 / 3.0).abs() < 1e-9);
        let w = &out[0].words;
        assert_eq!((w[1].start_ms, w[1].end_ms), (1500, 2500));
        assert_eq!(w[2].start_ms, 2500);
    }

    #[test]
    fn lines_with_times_keep_to_them_and_the_second_copy_of_a_chorus_is_its_own() {
        let l = [timed("la la", 0, 2000), timed("oh yeah (x2)", 5000, 9000)];
        let h = heard(&[
            ("la", 100, 500),
            ("la", 600, 1200),
            ("oh", 5200, 5500),
            ("yeah", 5500, 6000),
            ("oh", 7000, 7300),
            ("yeah", 7300, 8000),
        ]);
        let (out, share) = align(&l, &h);
        assert_eq!(out.len(), 3);
        assert_eq!(share, 1.0);
        assert_eq!((out[1].repeat, out[1].words[0].start_ms), (0, 5200));
        assert_eq!((out[2].repeat, out[2].words[0].start_ms), (1, 7000));
    }

    #[test]
    fn nothing_heard_still_orders_every_word_inside_the_line_times() {
        let l = [timed("alpha beta", 1000, 3000), timed("gamma", 3000, 4000)];
        let (out, share) = align(&l, &[]);
        assert_eq!(share, 0.0);
        let all: Vec<(u32, u32)> = out
            .iter()
            .flat_map(|l| l.words.iter().map(|w| (w.start_ms, w.end_ms)))
            .collect();
        assert!(all.windows(2).all(|p| p[0].1 <= p[1].0 && p[0].0 <= p[0].1));
        assert!(all[0].0 >= 1000 && all[1].1 <= 3000);
    }

    #[test]
    fn a_last_line_with_a_start_alone_keeps_to_it() {
        let l = [
            timed("one", 0, 1000),
            AlignLine {
                start_ms: Some(5000),
                ..line("two")
            },
        ];
        let out = align_fitted(&l, &heard(&[("one", 100, 400)]), 9000).lines;
        assert_eq!(out[1].start_ms, 5000);
    }

    #[test]
    fn unheard_lines_in_a_row_each_keep_to_their_own_times() {
        let l = [
            timed("one", 0, 1000),
            timed("two two", 5000, 8000),
            timed("three", 8000, 9000),
            timed("one", 20_000, 21_000),
        ];
        let h = heard(&[("one", 100, 400), ("one", 20_100, 20_400)]);
        let (out, _) = align(&l, &h);
        assert_eq!((out[1].start_ms, out[2].start_ms), (5000, 8000));
        assert!(out[1].end_ms <= 8000 && out[2].end_ms <= 9000);
    }

    #[test]
    fn whisper_json_gives_words_without_sound_labels() {
        let j = r#"{"transcription":[
            {"offsets":{"from":0,"to":400},"text":" [MUSIC]"},
            {"offsets":{"from":500,"to":900},"text":" Hello,"},
            {"offsets":{"from":900,"to":900},"text":" "},
            {"offsets":{"from":1000,"to":1500},"text":" world"}]}"#;
        let h = heard_from_json(j).unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(
            (h[0].text.as_str(), h[0].start_ms, h[1].end_ms),
            ("Hello,", 500, 1500)
        );
        assert!(heard_from_json("{}").is_err());
    }

    #[test]
    fn a_request_with_no_words_or_a_backwards_line_is_refused() {
        let kind = |lines| JobKind::Align {
            lines,
            language: None,
            model: None,
            providers: None,
        };
        assert!(validate(&kind(vec![line("  ")])).is_err());
        assert!(validate(&kind(vec![timed("a", 500, 100)])).is_err());
        assert!(validate(&kind(vec![line("a b")])).is_ok());
        let bad = JobKind::Align {
            lines: vec![line("a")],
            language: Some("--prompt".into()),
            model: None,
            providers: None,
        };
        assert!(validate(&bad).is_err());
    }
}

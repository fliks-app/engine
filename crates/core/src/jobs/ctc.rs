//! The pure half of the CTC `Align`: the lyrics forced onto a network's per-frame log-probabilities
//! (Viterbi), giving word times and scores. Spec: `docs/spec/karaoke.md` section 4.
use std::collections::HashMap;

use unicode_normalization::UnicodeNormalization;

use super::align::{Copy, finish, words_of};
use crate::api::{AlignLine, AlignedLine};

/// Duration of one emission frame.
pub const FRAME_MS: u32 = 20;
/// Per-frame log-probability of the star token: the price of audio the lyrics do not explain.
const STAR_LOGP: f32 = -2.3;
/// A word whose mean log-probability reaches this counts as found (about 22 %).
const MIN_SCORE: f32 = -1.5;
/// A window of a line starts this much before its line, ends this much after the next one starts.
const LEAD_MS: u32 = 1000;
const TAIL_MS: u32 = 500;
/// A window is at most its line's length plus this.
const EXTRA_MS: u32 = 3000;
/// Largest `frames x states` table (one byte each) a pass may fill.
const MAX_CELLS: usize = 100_000_000;

/// The characters a CTC network emits, and its two special classes.
#[derive(Debug, Clone)]
pub struct Vocab {
    ids: HashMap<char, u32>,
    blank: u32,
    delimiter: u32,
    case: Case,
}

/// The case a network's characters are in: the text is folded to it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Case {
    Lower,
    Upper,
}

/// Token standing for a character the vocabulary lacks: it scores the best non-blank class.
const WILDCARD: u32 = u32::MAX;

impl Vocab {
    /// `chars`: id of each single character the network emits (the delimiter and the blank
    /// excluded); the text is folded to `case`.
    pub fn new(
        chars: impl IntoIterator<Item = (char, u32)>,
        blank: u32,
        delimiter: u32,
        case: Case,
    ) -> Self {
        Vocab {
            ids: chars.into_iter().collect(),
            blank,
            delimiter,
            case,
        }
    }
}

/// Log-probabilities of a span of audio: per frame the columns of the asked ids, then the best
/// non-blank class (a `WILDCARD`'s score).
#[derive(Debug, Clone, PartialEq)]
pub struct Emissions {
    pub classes: usize,
    pub log_probs: Vec<f32>,
}

impl Emissions {
    pub fn frames(&self) -> usize {
        self.log_probs.len().checked_div(self.classes).unwrap_or(0)
    }

    /// The columns `ids` (the blank first) of the log-softmax of raw network rows of `total`
    /// classes, plus the wildcard column; every id is below `total`.
    pub fn from_logits(logits: &[f32], total: usize, ids: &[u32]) -> Self {
        let mut log_probs = Vec::with_capacity(logits.len() / total.max(1) * (ids.len() + 1));
        for row in logits.chunks_exact(total.max(1)) {
            let max = row.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            let lse = max + row.iter().map(|v| (v - max).exp()).sum::<f32>().ln();
            log_probs.extend(ids.iter().map(|id| row[*id as usize] - lse));
            let best = row
                .iter()
                .enumerate()
                .filter(|(k, _)| *k != ids[0] as usize)
                .fold(f32::NEG_INFINITY, |m, (_, v)| m.max(*v));
            log_probs.push(best - lse);
        }
        Emissions {
            classes: ids.len() + 1,
            log_probs,
        }
    }

    fn row(&self, t: usize) -> &[f32] {
        &self.log_probs[t * self.classes..(t + 1) * self.classes]
    }
}

const EN_UNITS: [&str; 20] = [
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
];
const EN_TENS: [&str; 10] = [
    "", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
];
const FR_UNITS: [&str; 17] = [
    "zéro", "un", "deux", "trois", "quatre", "cinq", "six", "sept", "huit", "neuf", "dix", "onze",
    "douze", "treize", "quatorze", "quinze", "seize",
];
const FR_TENS: [&str; 7] = [
    "",
    "",
    "vingt",
    "trente",
    "quarante",
    "cinquante",
    "soixante",
];

/// `rest` appended after `head` with a space, nothing when it is 0.
fn then(head: String, rest: u32, spell: fn(u32) -> String) -> String {
    if rest == 0 {
        head
    } else {
        format!("{head} {}", spell(rest))
    }
}

fn spell_en(n: u32) -> String {
    match n {
        0..=19 => EN_UNITS[n as usize].into(),
        20..=99 => then(EN_TENS[(n / 10) as usize].into(), n % 10, spell_en),
        100..=999 => then(format!("{} hundred", spell_en(n / 100)), n % 100, spell_en),
        _ => then(
            format!("{} thousand", spell_en(n / 1000)),
            n % 1000,
            spell_en,
        ),
    }
}

fn spell_fr(n: u32) -> String {
    match n {
        0..=16 => FR_UNITS[n as usize].into(),
        17..=19 => format!("dix {}", FR_UNITS[(n - 10) as usize]),
        21 | 31 | 41 | 51 | 61 => format!("{} et un", FR_TENS[(n / 10) as usize]),
        20..=69 => then(FR_TENS[(n / 10) as usize].into(), n % 10, spell_fr),
        71 => "soixante et onze".into(),
        70..=79 => format!("soixante {}", spell_fr(n - 60)),
        80..=99 => then("quatre vingt".into(), n - 80, spell_fr),
        100..=999 => {
            let head = if n < 200 {
                "cent".into()
            } else {
                format!("{} cent", spell_fr(n / 100))
            };
            then(head, n % 100, spell_fr)
        }
        _ => {
            let head = if n < 2000 {
                "mille".into()
            } else {
                format!("{} mille", spell_fr(n / 1000))
            };
            then(head, n % 1000, spell_fr)
        }
    }
}

/// A number written out in words for the languages that have a table, else nothing.
fn spell(n: u32, language: &str) -> String {
    match language {
        "fr" => spell_fr(n),
        "en" => spell_en(n),
        _ => String::new(),
    }
}

/// The token ids of one lyric word: characters outside the vocabulary become a `WILDCARD`,
/// punctuation is dropped, spaces, spelled numbers and a `-` the vocabulary lacks become delimiters.
pub fn tokens(word: &str, vocab: &Vocab, language: &str) -> Vec<u32> {
    let hyphen = vocab.ids.get(&'-').copied();
    let mut out: Vec<u32> = vec![];
    let mut gap = |out: &mut Vec<u32>| {
        if out.last().is_some_and(|t| *t != vocab.delimiter) {
            out.push(vocab.delimiter);
        }
    };
    let mut digits = String::new();
    let flush = |digits: &mut String, out: &mut Vec<u32>, gap: &mut dyn FnMut(&mut Vec<u32>)| {
        if digits.is_empty() {
            return;
        }
        let spelled = match digits.parse::<u32>() {
            Ok(n) if n < 10_000 => tokens(&spell(n, language), vocab, language),
            _ => vec![],
        };
        gap(out);
        if spelled.is_empty() {
            out.push(WILDCARD);
        } else {
            out.extend(spelled);
        }
        gap(out);
        digits.clear();
    };
    for c in word.nfc() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        flush(&mut digits, &mut out, &mut gap);
        let c = match c {
            '’' | '‘' | 'ʼ' | '`' | '´' | '′' => '\'',
            c => c,
        };
        if c == '-'
            && let Some(h) = hyphen
        {
            if out.last().is_some_and(|t| *t != vocab.delimiter && *t != h) {
                out.push(h);
            }
        } else if c == '-' || c.is_whitespace() {
            gap(&mut out);
        } else if c.is_alphabetic() || c == '\'' {
            let cased: Vec<char> = match vocab.case {
                Case::Upper => c.to_uppercase().collect(),
                Case::Lower => c.to_lowercase().collect(),
            };
            out.extend(
                cased
                    .iter()
                    .map(|k| vocab.ids.get(k).copied().unwrap_or(WILDCARD)),
            );
        } else if c.is_alphanumeric() {
            out.push(WILDCARD);
        }
    }
    flush(&mut digits, &mut out, &mut gap);
    while out
        .last()
        .is_some_and(|t| *t == vocab.delimiter || Some(*t) == hyphen)
    {
        out.pop();
    }
    out
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Blank,
    Star,
    Tok(u32),
}

/// A word placed on a span, in frames (ends exclusive): where its tokens start and end, where
/// its trailing blank ends, and the mean log-probability of its token frames.
#[derive(Clone, Copy)]
struct Placed {
    start: u32,
    tok_end: u32,
    end: u32,
    score: f32,
}

/// The states of a pass: a star before each line with a token and after the last one, each
/// token followed by a blank; `spans` the first and last state of each word, `ids` the classes
/// it scores (the blank first), `cols` the emission column of each state (the wildcard's last).
struct Layout {
    ids: Vec<u32>,
    cols: Vec<usize>,
    states: Vec<State>,
    spans: Vec<Vec<Option<(usize, usize)>>>,
}

fn layout(vocab: &Vocab, lines: &[&[Vec<u32>]]) -> Option<Layout> {
    let mut states = vec![];
    let mut spans: Vec<Vec<Option<(usize, usize)>>> = vec![];
    for words in lines {
        let mut mine = vec![None; words.len()];
        if words.iter().any(|w| !w.is_empty()) {
            states.extend([State::Star, State::Blank]);
            let mut first = true;
            for (i, w) in words.iter().enumerate().filter(|(_, w)| !w.is_empty()) {
                if !first {
                    states.extend([State::Tok(vocab.delimiter), State::Blank]);
                }
                first = false;
                let a = states.len();
                for t in w.iter() {
                    states.extend([State::Tok(*t), State::Blank]);
                }
                mine[i] = Some((a, states.len() - 2));
            }
        }
        spans.push(mine);
    }
    if states.is_empty() {
        return None;
    }
    states.push(State::Star);
    let mut ids = vec![vocab.blank];
    let mut cols: Vec<usize> = states
        .iter()
        .map(|s| match s {
            State::Star | State::Blank => 0,
            State::Tok(WILDCARD) => usize::MAX,
            State::Tok(t) => ids.iter().position(|i| i == t).unwrap_or_else(|| {
                ids.push(*t);
                ids.len() - 1
            }),
        })
        .collect();
    let wildcard = ids.len();
    cols.iter_mut()
        .filter(|c| **c == usize::MAX)
        .for_each(|c| *c = wildcard);
    Some(Layout {
        ids,
        cols,
        states,
        spans,
    })
}

/// The lines of `lay` forced onto `em`: per line, per word, where it sits; `None` for a word
/// with no token and for everything when no alignment fits.
fn viterbi(em: &Emissions, lay: &Layout) -> Option<Vec<Vec<Option<Placed>>>> {
    let Layout {
        ids,
        cols,
        states,
        spans,
    } = lay;
    let (n, frames) = (states.len(), em.frames());
    debug_assert_eq!(em.classes, ids.len() + 1);
    if frames == 0 {
        return None;
    }
    let skip: Vec<bool> = (0..n)
        .map(|s| {
            s >= 2
                && ((states[s - 1] == State::Blank
                    && !matches!((states[s], states[s - 2]), (State::Tok(a), State::Tok(b)) if a == b))
                    || (states[s - 1] == State::Star && states[s] == State::Blank))
        })
        .collect();
    let emit = |row: &[f32], s: usize| match states[s] {
        State::Star => STAR_LOGP,
        _ => row[cols[s]],
    };
    let mut back = vec![0u8; frames * n];
    let mut prev = vec![f32::NEG_INFINITY; n];
    let mut cur = vec![f32::NEG_INFINITY; n];
    for (s, p) in prev.iter_mut().enumerate().take(3.min(n)) {
        *p = emit(em.row(0), s);
    }
    for t in 1..frames {
        let row = em.row(t);
        for s in 0..n {
            let (mut best, mut from) = (prev[s], 0u8);
            if s >= 1 && prev[s - 1] > best {
                (best, from) = (prev[s - 1], 1);
            }
            if skip[s] && prev[s - 2] > best {
                (best, from) = (prev[s - 2], 2);
            }
            cur[s] = best + emit(row, s);
            back[t * n + s] = from;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let mut s = (n.saturating_sub(3)..n).max_by(|a, b| prev[*a].total_cmp(&prev[*b]))?;
    if prev[s] == f32::NEG_INFINITY {
        return None;
    }
    let mut path = vec![0usize; frames];
    for t in (0..frames).rev() {
        path[t] = s;
        s -= usize::from(back[t * n + s]);
    }
    // Per token or blank state: first and last frame, summed token log-probability, token frames.
    let mut seen: Vec<Option<(u32, u32, f32, u32)>> = vec![None; n];
    for (t, s) in path.iter().enumerate() {
        let (lp, count) = match states[*s] {
            State::Star => continue,
            State::Blank => (0.0, 0),
            State::Tok(_) => (emit(em.row(t), *s), 1),
        };
        let e = seen[*s].get_or_insert((t as u32, t as u32, 0.0, 0));
        *e = (e.0, t as u32, e.2 + lp, e.3 + count);
    }
    Some(
        spans
            .iter()
            .map(|line| {
                line.iter()
                    .map(|span| {
                        let (a, b) = (*span)?;
                        let (mut sum, mut count) = (0.0, 0);
                        for e in seen[a..=b].iter().flatten() {
                            sum += e.2;
                            count += e.3;
                        }
                        let (first, last) = (seen[a]?, seen[b]?);
                        let tail = seen[b + 1].map_or(last.1, |e| e.1);
                        Some(Placed {
                            start: first.0,
                            tok_end: last.1 + 1,
                            end: tail + 1,
                            score: sum / count.max(1) as f32,
                        })
                    })
                    .collect()
            })
            .collect(),
    )
}

/// The window of copy `i` of the lines, in ms: `floor` is where the previous line's last word
/// ended.
fn window_of(copies: &[Copy], i: usize, floor: u32, duration_ms: u32) -> Option<(u32, u32)> {
    let (s, e) = copies[i].window?;
    let next = copies[i + 1..].iter().find_map(|c| c.window).map(|w| w.0);
    let lo = s.saturating_sub(LEAD_MS).max(floor) / FRAME_MS * FRAME_MS;
    let hi = next.map_or(e + EXTRA_MS, |n| n + TAIL_MS);
    let hi = hi.min(lo + (e - s) + EXTRA_MS).min(duration_ms);
    (hi > lo).then_some((lo, hi))
}

/// Why an alignment gave nothing.
#[derive(Debug, PartialEq)]
pub enum AlignError<E> {
    /// The emissions callback failed.
    Emit(E),
    /// A pass would need more than `MAX_CELLS` cells: the caller falls back to another aligner.
    TooLarge,
}

/// The lines placed by CTC forced alignment, and the share of words found. `emit(from, to, ids)`
/// gives the emissions of a span (frame 0 at `from`, a multiple of `FRAME_MS`) for the classes `ids`.
pub fn align<E, F: FnMut(u32, u32, &[u32]) -> Result<Emissions, E>>(
    lines: &[AlignLine],
    vocab: &Vocab,
    language: &str,
    duration_ms: u32,
    emit: &mut F,
) -> Result<(Vec<AlignedLine>, f64), AlignError<E>> {
    let copies = words_of(lines);
    let toks: Vec<Vec<Vec<u32>>> = copies
        .iter()
        .map(|c| {
            c.words
                .iter()
                .map(|(s, e)| tokens(&c.text[*s as usize..*e as usize], vocab, language))
                .collect()
        })
        .collect();
    let offsets: Vec<usize> = copies
        .iter()
        .scan(0, |n, c| {
            *n += c.words.len();
            Some(*n - c.words.len())
        })
        .collect();
    let total: usize = copies.iter().map(|c| c.words.len()).sum();
    let mut times: Vec<Option<(u32, u32)>> = vec![None; total];
    let (mut found, mut floor, mut ci) = (0, 0, 0);
    while ci < copies.len() {
        let start = ci;
        let (lo, hi) = if copies[ci].window.is_some() {
            ci += 1;
            let Some(w) = window_of(&copies, start, floor, duration_ms) else {
                continue;
            };
            w
        } else {
            ci = (start..copies.len())
                .find(|k| copies[*k].window.is_some())
                .unwrap_or(copies.len());
            let end = copies
                .get(ci)
                .and_then(|c| c.window)
                .map_or(duration_ms, |w| w.0 + TAIL_MS);
            (floor / FRAME_MS * FRAME_MS, end.min(duration_ms))
        };
        let run: Vec<&[Vec<u32>]> = toks[start..ci].iter().map(|t| t.as_slice()).collect();
        let Some(lay) = layout(vocab, &run).filter(|_| hi > lo) else {
            continue;
        };
        if (hi - lo).div_ceil(FRAME_MS) as usize * lay.states.len() > MAX_CELLS {
            return Err(AlignError::TooLarge);
        }
        let em = emit(lo, hi, &lay.ids).map_err(AlignError::Emit)?;
        let Some(out) = viterbi(&em, &lay) else {
            continue;
        };
        for (k, placed) in out.iter().enumerate() {
            for (wi, p) in placed.iter().enumerate() {
                if let Some(p) = p {
                    times[offsets[start + k] + wi] =
                        Some((lo + p.start * FRAME_MS, lo + p.end * FRAME_MS));
                    found += usize::from(p.score >= MIN_SCORE);
                    floor = floor.max(lo + p.tok_end * FRAME_MS);
                }
            }
        }
    }
    let out = finish(&copies, &mut times);
    let share = if total == 0 {
        0.0
    } else {
        found as f64 / total as f64
    };
    Ok((out, share))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLANK: u32 = 0;
    const DELIM: u32 = 1;
    const CLASSES: usize = 7;

    fn vocab() -> Vocab {
        Vocab::new(
            [('a', 2), ('b', 3), ('c', 4), ('\'', 5), ('é', 6)],
            BLANK,
            DELIM,
            Case::Lower,
        )
    }

    /// Frames of blank-dominated probabilities of every class, with `marks` (frame, class,
    /// probability) laid on top.
    fn emissions(frames: usize, marks: &[(usize, u32, f32)]) -> Vec<f32> {
        let mut v = vec![];
        for t in 0..frames {
            let mark = marks.iter().rev().find(|m| m.0 == t);
            let (main, p) = mark.map_or((BLANK, 0.94), |m| (m.1, m.2));
            let rest = (1.0 - p) / (CLASSES - 1) as f32;
            v.extend((0..CLASSES as u32).map(|c| (if c == main { p } else { rest }).ln()));
        }
        v
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

    fn run(
        lines: &[AlignLine],
        em: &[f32],
        asked: &mut Vec<(u32, u32)>,
    ) -> (Vec<AlignedLine>, f64) {
        let frames = em.len() / CLASSES;
        align(
            lines,
            &vocab(),
            "xx",
            frames as u32 * FRAME_MS,
            &mut |s, e, ids| {
                asked.push((s, e));
                let (a, b) = (
                    (s / FRAME_MS) as usize,
                    (e.div_ceil(FRAME_MS) as usize).min(frames),
                );
                Ok::<_, ()>(Emissions::from_logits(
                    &em[a * CLASSES..b * CLASSES],
                    CLASSES,
                    ids,
                ))
            },
        )
        .unwrap()
    }

    fn forward(out: &[AlignedLine]) {
        let mut at = 0;
        for w in out.iter().flat_map(|l| &l.words) {
            assert!(
                w.start_ms >= at && w.end_ms >= w.start_ms,
                "{w:?} after {at}"
            );
            at = w.end_ms;
        }
    }

    #[test]
    fn text_becomes_tokens_per_the_vocabulary() {
        let v = vocab();
        assert_eq!(tokens("A’b", &v, "xx"), vec![2, 5, 3]);
        assert_eq!(
            tokens("e\u{301}!x1", &v, "xx"),
            vec![6, WILDCARD, DELIM, WILDCARD]
        );
        assert_eq!(tokens("a-b", &v, "xx"), vec![2, DELIM, 3]);
        assert!(tokens("???", &v, "xx").is_empty());
        let up = Vocab::new([('A', 2)], BLANK, DELIM, Case::Upper);
        assert_eq!(tokens("a", &up, "xx"), vec![2]);
    }

    #[test]
    fn numbers_are_spelled_for_fr_and_en_only() {
        assert_eq!(spell_fr(21), "vingt et un");
        assert_eq!(spell_fr(71), "soixante et onze");
        assert_eq!(spell_fr(1984), "mille neuf cent quatre vingt quatre");
        assert_eq!(spell_en(1984), "one thousand nine hundred eighty four");
        let v = Vocab::new(
            "abcdeinoqrstuvxzé".chars().zip(2..),
            BLANK,
            DELIM,
            Case::Lower,
        );
        assert!(!tokens("12", &v, "fr").is_empty());
        assert_eq!(tokens("12", &v, "de"), vec![WILDCARD]);
    }

    #[test]
    fn a_whole_track_pass_times_words_on_their_frames() {
        let em = emissions(
            40,
            &[(5, 2, 0.9), (10, 3, 0.9), (13, DELIM, 0.9), (15, 4, 0.9)],
        );
        let (out, share) = run(&[line("ab c")], &em, &mut vec![]);
        let w = &out[0].words;
        assert_eq!((w[0].start_ms, w[0].end_ms), (100, 260));
        assert_eq!((w[1].start_ms, w[1].end_ms), (300, 800));
        assert_eq!(share, 1.0);
        forward(&out);
    }

    #[test]
    fn the_star_absorbs_audio_between_lines_instead_of_stretching_them() {
        let mut marks = vec![(5, 2, 0.9), (60, 3, 0.9)];
        marks.extend((20..50).map(|t| (t, 4, 0.9)));
        let em = emissions(80, &marks);
        let (out, _) = run(&[line("a"), line("b")], &em, &mut vec![]);
        assert_eq!((out[0].start_ms, out[0].end_ms), (100, 400));
        assert_eq!((out[1].start_ms, out[1].end_ms), (1200, 1600));
    }

    #[test]
    fn an_unknown_word_is_interpolated_and_lowers_the_matched_share() {
        let em = emissions(60, &[(5, 2, 0.9), (30, 3, 0.9)]);
        let (out, share) = run(&[timed("a $$ b", 0, 1200)], &em, &mut vec![]);
        let w = &out[0].words;
        assert_eq!(w.len(), 3);
        assert!(
            w[1].end_ms > w[1].start_ms
                && w[1].start_ms >= w[0].end_ms
                && w[1].end_ms <= w[2].start_ms
        );
        assert!((share - 2.0 / 3.0).abs() < 1e-9);
        forward(&out);
    }

    #[test]
    fn windows_start_after_the_previous_word_and_never_run_over_a_long_gap() {
        let em = emissions(3500, &[(120, 2, 0.9), (150, 3, 0.9), (2500, 4, 0.9)]);
        let mut asked = vec![];
        let lines = [
            timed("a", 1500, 2500),
            timed("b", 2500, 3500),
            timed("c", 50_000, 51_000),
        ];
        let (out, _) = run(&lines, &em, &mut asked);
        assert_eq!(asked, vec![(500, 3000), (2420, 6420), (49_000, 53_000)]);
        assert_eq!(out[2].words[0].start_ms, 50_000);
        forward(&out);
    }

    #[test]
    fn a_tokenless_line_after_the_next_words_start_never_runs_backwards() {
        let em = emissions(700, &[(480, 2, 0.9)]);
        let lines = [
            timed("\u{266a} \u{266a} \u{266a}", 10_000, 10_500),
            timed("a", 10_500, 12_000),
        ];
        let (out, _) = run(&lines, &em, &mut vec![]);
        assert_eq!(out[1].words[0].start_ms, 9600);
        forward(&out);
    }

    #[test]
    fn untimed_lines_between_timed_ones_get_their_own_pass() {
        let em = emissions(200, &[(5, 2, 0.9), (60, 3, 0.9), (100, 4, 0.9)]);
        let mut asked = vec![];
        let lines = [timed("a", 0, 500), line("b"), timed("c", 2000, 2500)];
        let (out, _) = run(&lines, &em, &mut asked);
        assert_eq!(asked, vec![(0, 2500), (120, 2500), (1220, 4000)]);
        let starts: Vec<u32> = out.iter().map(|l| l.start_ms).collect();
        assert_eq!(starts, vec![100, 1200, 2000]);
        forward(&out);
    }

    #[test]
    fn the_text_is_folded_to_the_declared_case_even_when_the_vocabulary_holds_both() {
        let v = Vocab::new([('a', 2), ('A', 3)], BLANK, DELIM, Case::Lower);
        assert_eq!(tokens("aA", &v, "xx"), vec![2, 2]);
    }

    #[test]
    fn a_wildcard_word_stays_anchored_on_the_loudest_class() {
        let em = emissions(60, &[(5, 2, 0.9), (18, 4, 0.9), (30, 3, 0.9)]);
        let (out, share) = run(&[timed("a x b", 0, 1200)], &em, &mut vec![]);
        assert_eq!(share, 1.0);
        forward(&out);
    }

    #[test]
    fn a_hyphen_is_the_vocabulary_hyphen_when_it_has_one() {
        let v = Vocab::new([('a', 2), ('b', 3), ('-', 4)], BLANK, DELIM, Case::Lower);
        assert_eq!(tokens("a-b", &v, "xx"), vec![2, 4, 3]);
        assert_eq!(tokens("a-", &v, "xx"), vec![2]);
        assert!(tokens("-", &v, "xx").is_empty());
    }

    #[test]
    fn a_number_is_set_apart_from_the_letters_after_it() {
        let v = Vocab::new("twond".chars().zip(2..), BLANK, DELIM, Case::Lower);
        assert_eq!(tokens("2nd", &v, "en"), vec![2, 3, 4, DELIM, 5, 6]);
    }

    #[test]
    fn a_pass_over_the_cell_cap_is_an_error_before_any_audio_is_read() {
        let text = vec!["a"; 3000].join(" ");
        let mut asked = 0;
        let r = align(&[line(&text)], &vocab(), "xx", 200_000, &mut |_, _, _| {
            asked += 1;
            Ok::<_, ()>(Emissions::from_logits(
                &emissions(1, &[]),
                CLASSES,
                &[BLANK],
            ))
        });
        assert_eq!(r.unwrap_err(), AlignError::TooLarge);
        assert_eq!(asked, 0);
    }

    #[test]
    fn emissions_keep_the_asked_columns_and_the_best_non_blank_class() {
        let logits = [1.0, 4.0, 0.0, 2.0, 0.0, 9.0, 3.0, 1.0];
        let em = Emissions::from_logits(&logits, 4, &[3, 1]);
        assert_eq!((em.classes, em.frames()), (3, 2));
        let lse = |r: [f32; 4]| r.iter().map(|v| v.exp()).sum::<f32>().ln();
        let (a, b) = (lse([1.0, 4.0, 0.0, 2.0]), lse([0.0, 9.0, 3.0, 1.0]));
        let want = [2.0 - a, 4.0 - a, 4.0 - a, 1.0 - b, 9.0 - b, 9.0 - b];
        for (g, w) in em.log_probs.iter().zip(want) {
            assert!((g - w).abs() < 1e-5, "{g} {w}");
        }
    }
}

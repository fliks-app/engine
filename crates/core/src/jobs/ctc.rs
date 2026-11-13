//! The pure half of the CTC `Align`: the lyrics forced onto a network's per-frame log-probabilities
//! (Viterbi), giving word times and scores. Spec: `docs/spec/karaoke.md` section 4.
use std::collections::{BTreeSet, HashMap};
use std::ops::Range;

use unicode_normalization::UnicodeNormalization;

use super::align::{Copy, finish, words_of};
use super::melody::SAMPLE_RATE;
use crate::api::{AlignLine, AlignedLine};

/// Duration of one emission frame.
pub const FRAME_MS: u32 = 20;
/// Per-frame log-probability the star token scores at least, a blank's when higher: audio the
/// lyrics do not explain costs no more than silence.
const STAR_LOGP: f32 = -2.3;
/// A word whose mean log-probability reaches this counts as found (about 22 %).
const MIN_SCORE: f32 = -1.5;
/// The band of a timed line starts this much before it and ends this much after the next one starts.
const LEAD_MS: u32 = 2000;
const TAIL_MS: u32 = 1500;
/// A band is at most its line's length plus this.
const EXTRA_MS: u32 = 3000;
/// Most cells (two bits each) the back-pointer table may hold: 100 MB.
const MAX_CELLS: usize = 400_000_000;
/// A frame is silent when its level is this far below the track's 95th percentile and part of a
/// run of at least `MIN_SILENT_MS`.
const SILENT_DB: f32 = -35.0;
const MIN_SILENT_MS: u32 = 100;
/// Log-probability a word's state loses per silent frame: a word does not stretch over silence.
const SILENT_PEN: f32 = -3.0;
/// A word's end is held over voiced frames at most this long after its last token.
const HOLD_MS: u32 = 500;

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
    /// The asked ids, the blank first.
    pub ids: Vec<u32>,
    pub log_probs: Vec<f32>,
}

impl Emissions {
    pub fn classes(&self) -> usize {
        self.ids.len() + 1
    }

    pub fn frames(&self) -> usize {
        self.log_probs
            .len()
            .checked_div(self.classes())
            .unwrap_or(0)
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
            ids: ids.to_vec(),
            log_probs,
        }
    }

    fn row(&self, t: usize) -> &[f32] {
        &self.log_probs[t * self.classes()..(t + 1) * self.classes()]
    }
}

/// Per `FRAME_MS` frame of the 16 kHz track `pcm`: whether the vocal stem is silent there.
pub fn silent_frames(pcm: &[f32]) -> Vec<bool> {
    let per = (SAMPLE_RATE * FRAME_MS / 1000) as usize;
    let rms: Vec<f32> = pcm
        .chunks_exact(per)
        .map(|f| (f.iter().map(|v| v * v).sum::<f32>() / per as f32).sqrt())
        .collect();
    let mut sorted = rms.clone();
    sorted.sort_by(f32::total_cmp);
    let limit =
        sorted.get(sorted.len() * 95 / 100).copied().unwrap_or(0.0) * 10f32.powf(SILENT_DB / 20.0);
    let min = (MIN_SILENT_MS / FRAME_MS) as usize;
    let mut out = vec![false; rms.len()];
    let mut at = 0;
    while at < rms.len() {
        let run = rms[at..].iter().take_while(|v| **v < limit).count();
        if run >= min {
            out[at..at + run].fill(true);
        }
        at += run.max(1);
    }
    out
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

/// A word placed on the track, in frames (ends exclusive): where its tokens start and end, where
/// the word ends (its tokens' end held over voiced frames), and the mean log-probability of its
/// token frames.
#[derive(Clone, Copy)]
struct Placed {
    start: u32,
    tok_end: u32,
    end: u32,
    score: f32,
}

/// The states of the lyrics (a star before each line with a token and after the last, each token
/// then a blank); `spans` the word bounds, `bodies` each line's states after its star, `cols`
/// each state's emission column, `in_word` the states silence may not hold (a word's tokens and
/// the blanks between them).
struct Layout {
    cols: Vec<usize>,
    in_word: Vec<bool>,
    states: Vec<State>,
    spans: Vec<Vec<Option<(usize, usize)>>>,
    bodies: Vec<Range<usize>>,
}

/// The lyric lines as states, their columns looked up in `ids`.
fn layout(vocab: &Vocab, lines: &[&[Vec<u32>]], ids: &[u32]) -> Option<Layout> {
    let mut states = vec![];
    let mut spans: Vec<Vec<Option<(usize, usize)>>> = vec![];
    let mut bodies = vec![];
    for words in lines {
        let mut mine = vec![None; words.len()];
        let body = states.len() + 1;
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
        bodies.push(body.min(states.len())..states.len());
    }
    if states.is_empty() {
        return None;
    }
    states.push(State::Star);
    let mut in_word = vec![false; states.len()];
    for (a, b) in spans.iter().flatten().flatten() {
        in_word[*a..=*b].fill(true);
    }
    let cols = states
        .iter()
        .map(|s| match s {
            State::Star | State::Blank => 0,
            State::Tok(WILDCARD) => ids.len(),
            // The emissions hold every id `lyric_ids` lists.
            State::Tok(t) => ids.iter().position(|i| i == t).unwrap_or(ids.len()),
        })
        .collect();
    Some(Layout {
        cols,
        in_word,
        states,
        spans,
        bodies,
    })
}

/// Every id the lyrics' tokens use, the blank first: the classes to keep of the network's answer.
pub fn lyric_ids(lines: &[AlignLine], vocab: &Vocab, language: &str) -> Vec<u32> {
    let mut ids = vec![vocab.blank, vocab.delimiter];
    for c in words_of(lines) {
        for (s, e) in &c.words {
            for t in tokens(&c.text[*s as usize..*e as usize], vocab, language) {
                if t != WILDCARD && !ids.contains(&t) {
                    ids.push(t);
                }
            }
        }
    }
    ids
}

/// The frames each state may be on: a line's states its band, the star before it from the start of
/// the previous line's band to the end of its own (a star elsewhere cannot lead to a finished path).
fn state_bands(lay: &Layout, bands: &[(u32, u32)], frames: usize) -> Vec<(u32, u32)> {
    let end = frames as u32;
    let mut band = vec![(0, end); lay.states.len()];
    let mut from = 0;
    for (body, (lo, hi)) in lay.bodies.iter().zip(bands).filter(|(b, _)| !b.is_empty()) {
        band[body.start - 1] = (from, (*hi).min(end));
        band[body.clone()].fill(((*lo).min(end), (*hi).min(end)));
        from = (*lo).min(end);
    }
    if let Some(last) = band.last_mut() {
        *last = (from, end);
    }
    band
}

/// Per frame, the half-open range of states between the lowest and the highest one whose band
/// holds it.
fn state_ranges(band: &[(u32, u32)], frames: usize) -> Vec<(usize, usize)> {
    let held = || band.iter().enumerate().filter(|(_, b)| b.0 < b.1);
    let mut opens: Vec<(u32, usize)> = held().map(|(s, b)| (b.0, s)).collect();
    let mut closes: Vec<(u32, usize)> = held().map(|(s, b)| (b.1, s)).collect();
    opens.sort_unstable();
    closes.sort_unstable();
    let (mut open, mut close) = (0, 0);
    let mut live = BTreeSet::new();
    (0..frames as u32)
        .map(|t| {
            while open < opens.len() && opens[open].0 <= t {
                live.insert(opens[open].1);
                open += 1;
            }
            while close < closes.len() && closes[close].0 <= t {
                live.remove(&closes[close].1);
                close += 1;
            }
            match (live.first(), live.last()) {
                (Some(a), Some(b)) => (*a, *b + 1),
                _ => (0, 0),
            }
        })
        .collect()
}

/// Whether state `s` of `lay` may be passed to from two states before it.
fn skips(states: &[State]) -> Vec<bool> {
    (0..states.len())
        .map(|s| {
            s >= 2
                && ((states[s - 1] == State::Blank
                    && !matches!((states[s], states[s - 2]), (State::Tok(a), State::Tok(b)) if a == b))
                    || (states[s - 1] == State::Star && states[s] == State::Blank))
        })
        .collect()
}

/// The log-probability of `s` at frame `t` of `row`, without the silence penalty.
fn emit(lay: &Layout, band: &[(u32, u32)], row: &[f32], t: usize, s: usize) -> f32 {
    match lay.states[s] {
        _ if (t as u32) < band[s].0 || t as u32 >= band[s].1 => f32::NEG_INFINITY,
        State::Star => row[0].max(STAR_LOGP),
        _ => row[lay.cols[s]],
    }
}

/// The best state of every frame, `None` when no path fits the bands. Back pointers (two bits a
/// cell: stay, one state, two states) are kept only inside each frame's state range.
fn best_path(
    em: &Emissions,
    lay: &Layout,
    band: &[(u32, u32)],
    silent: &[bool],
) -> Result<Option<Vec<usize>>, TooLarge> {
    let (n, frames) = (lay.states.len(), em.frames());
    if frames == 0 {
        return Ok(None);
    }
    let skip = skips(&lay.states);
    let ranges = state_ranges(band, frames);
    let mut offsets = Vec::with_capacity(frames);
    let mut cells = 0usize;
    for (lo, hi) in &ranges {
        offsets.push(cells);
        cells += hi - lo;
        if cells > MAX_CELLS {
            return Err(TooLarge);
        }
    }
    let mut back = vec![0u8; cells.div_ceil(4)];
    let mut prev = vec![f32::NEG_INFINITY; n];
    let mut cur = vec![f32::NEG_INFINITY; n];
    for (s, p) in prev.iter_mut().enumerate().take(3.min(n)) {
        *p = emit(lay, band, em.row(0), 0, s);
    }
    for t in 1..frames {
        let (lo, hi) = ranges[t];
        let (stale_lo, stale_hi) = match t {
            1 => (0, 0),
            2 => (0, 3.min(n)),
            _ => ranges[t - 2],
        };
        cur[stale_lo..stale_hi].fill(f32::NEG_INFINITY);
        let row = em.row(t);
        let quiet = silent.get(t).copied().unwrap_or(false);
        for s in lo..hi {
            let (mut best, mut from) = (prev[s], 0u8);
            if s >= 1 && prev[s - 1] > best {
                (best, from) = (prev[s - 1], 1);
            }
            if skip[s] && prev[s - 2] > best {
                (best, from) = (prev[s - 2], 2);
            }
            let held = if quiet && lay.in_word[s] {
                SILENT_PEN
            } else {
                0.0
            };
            cur[s] = best + emit(lay, band, row, t, s) + held;
            let at = offsets[t] + s - lo;
            back[at / 4] |= from << (at % 4 * 2);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let Some(mut s) = (n.saturating_sub(3)..n).max_by(|a, b| prev[*a].total_cmp(&prev[*b])) else {
        return Ok(None);
    };
    if prev[s] == f32::NEG_INFINITY {
        return Ok(None);
    }
    let mut path = vec![0usize; frames];
    for t in (0..frames).rev() {
        path[t] = s;
        if t > 0 {
            let at = offsets[t] + s - ranges[t].0;
            s -= usize::from(back[at / 4] >> (at % 4 * 2) & 3);
        }
    }
    Ok(Some(path))
}

/// The lines of `lay` forced onto `em`: per line, per word, where it sits; `None` for a word
/// with no token and for everything when no alignment fits. `bands` (frames, per line) are the
/// only frames a line's tokens may use. `silent` flags the silent frames of the track.
fn viterbi(
    em: &Emissions,
    lay: &Layout,
    bands: &[(u32, u32)],
    silent: &[bool],
) -> Result<Option<Vec<Vec<Option<Placed>>>>, TooLarge> {
    let (n, frames) = (lay.states.len(), em.frames());
    let band = state_bands(lay, bands, frames);
    let Some(path) = best_path(em, lay, &band, silent)? else {
        return Ok(None);
    };
    // Per token or blank state: first and last frame, summed token log-probability, token frames.
    let mut seen: Vec<Option<(u32, u32, f32, u32)>> = vec![None; n];
    for (t, s) in path.iter().enumerate() {
        let (lp, count) = match lay.states[*s] {
            State::Star => continue,
            State::Blank => (0.0, 0),
            State::Tok(_) => (emit(lay, &band, em.row(t), t, *s), 1),
        };
        let e = seen[*s].get_or_insert((t as u32, t as u32, 0.0, 0));
        *e = (e.0, t as u32, e.2 + lp, e.3 + count);
    }
    let mut placed: Vec<Vec<Option<Placed>>> = lay
        .spans
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
                    Some(Placed {
                        start: first.0,
                        tok_end: last.1 + 1,
                        end: last.1 + 1,
                        score: sum / count.max(1) as f32,
                    })
                })
                .collect()
        })
        .collect();
    // A tokenless word after a placed one keeps the gap: `fill` times it there.
    let (mut limit, mut gap) = (frames as u32, false);
    for word in placed.iter_mut().flat_map(|l| l.iter_mut()).rev() {
        let Some(p) = word else {
            gap = true;
            continue;
        };
        let cap = if gap {
            p.tok_end
        } else {
            limit.min(p.tok_end + HOLD_MS / FRAME_MS)
        };
        p.end = (p.tok_end..cap)
            .find(|t| silent.get(*t as usize).copied().unwrap_or(false))
            .unwrap_or(cap);
        (limit, gap) = (p.start, false);
    }
    Ok(Some(placed))
}

/// The band of timed copy `i`, in ms: from `LEAD_MS` before its start to `TAIL_MS` after the next
/// timed copy starts.
fn band_of(copies: &[Copy], i: usize, duration_ms: u32) -> Option<(u32, u32)> {
    let (s, e) = copies[i].window?;
    let next = copies[i + 1..].iter().find_map(|c| c.window).map(|w| w.0);
    let lo = s.saturating_sub(LEAD_MS) / FRAME_MS * FRAME_MS;
    let hi = next.map_or(e + EXTRA_MS, |n| n + TAIL_MS);
    let hi = hi.min(lo + (e - s) + EXTRA_MS).min(duration_ms);
    (hi > lo).then_some((lo, hi))
}

/// The frames a copy's tokens need: one each, a delimiter between its words, a blank between two
/// equal tokens in a row.
fn frames_needed(words: &[Vec<u32>]) -> usize {
    let words = words.iter().filter(|w| !w.is_empty());
    let repeats = words
        .clone()
        .map(|w| w.windows(2).filter(|p| p[0] == p[1]).count())
        .sum::<usize>();
    let tokens = words.clone().map(Vec::len).sum::<usize>();
    (tokens + repeats + words.count()).saturating_sub(1)
}

/// The alignment would need more cells than `MAX_CELLS`.
#[derive(Debug, PartialEq)]
pub struct TooLarge;

/// The lines placed by CTC forced alignment, and the share of words found. `em` holds the
/// emissions of the whole track for `lyric_ids` of `lines`, `silent` flags its silent frames
/// (`silent_frames`). All the lines go through one Viterbi pass, so neighbours share the frames
/// they both reach: a line with times may only sit in its band, a line without may sit anywhere
/// up to 1.5 s after the next timed line starts.
pub fn align(
    lines: &[AlignLine],
    vocab: &Vocab,
    language: &str,
    silent: &[bool],
    em: &Emissions,
) -> Result<(Vec<AlignedLine>, f64), TooLarge> {
    let copies = words_of(lines);
    let mut toks: Vec<Vec<Vec<u32>>> = copies
        .iter()
        .map(|c| {
            c.words
                .iter()
                .map(|(s, e)| tokens(&c.text[*s as usize..*e as usize], vocab, language))
                .collect()
        })
        .collect();
    let duration_ms = em.frames() as u32 * FRAME_MS;
    let mut bands: Vec<(u32, u32)> = vec![(0, 0); copies.len()];
    let mut ci = 0;
    while ci < copies.len() {
        if copies[ci].window.is_some() {
            bands[ci] = band_of(&copies, ci, duration_ms).unwrap_or((duration_ms, duration_ms));
            ci += 1;
        } else {
            let next = (ci..copies.len()).find(|k| copies[*k].window.is_some());
            let hi = next
                .and_then(|k| copies[k].window)
                .map_or(duration_ms, |w| w.0 + TAIL_MS)
                .min(duration_ms);
            for band in &mut bands[ci..next.unwrap_or(copies.len())] {
                *band = (0, hi);
            }
            ci = next.unwrap_or(copies.len());
        }
    }
    // A band too short for its tokens reaches to the next one's end; still too short, the copy is timed
    // between its neighbours.
    for i in 0..copies.len() {
        let need = frames_needed(&toks[i]) as u32 * FRAME_MS;
        if copies[i].window.is_some() && bands[i].1 - bands[i].0 < need {
            let next = (i + 1..copies.len())
                .find(|k| copies[*k].window.is_some())
                .map_or(duration_ms, |k| bands[k].1);
            bands[i].1 = bands[i].1.max(next);
            if bands[i].1 - bands[i].0 < need {
                toks[i].iter_mut().for_each(Vec::clear);
            }
        }
    }
    let frames: Vec<(u32, u32)> = bands
        .iter()
        .map(|(lo, hi)| (lo / FRAME_MS, hi.div_ceil(FRAME_MS)))
        .collect();
    let run: Vec<&[Vec<u32>]> = toks.iter().map(Vec::as_slice).collect();
    let mut times: Vec<Option<(u32, u32)>> = vec![None; copies.iter().map(|c| c.words.len()).sum()];
    let (mut found, mut at) = (0, 0);
    if let Some(lay) = layout(vocab, &run, &em.ids)
        && let Some(placed) = viterbi(em, &lay, &frames, silent)?
    {
        for (copy, line) in copies.iter().zip(&placed) {
            for (wi, p) in line.iter().enumerate() {
                if let Some(p) = p {
                    times[at + wi] = Some((p.start * FRAME_MS, p.end * FRAME_MS));
                    found += usize::from(p.score >= MIN_SCORE);
                }
            }
            at += copy.words.len();
        }
    }
    let share = if times.is_empty() {
        0.0
    } else {
        found as f64 / times.len() as f64
    };
    Ok((finish(&copies, &mut times), share))
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

    fn run(lines: &[AlignLine], em: &[f32]) -> (Vec<AlignedLine>, f64) {
        run_silent(lines, em, &[])
    }

    fn run_silent(lines: &[AlignLine], em: &[f32], silent: &[bool]) -> (Vec<AlignedLine>, f64) {
        let ids = lyric_ids(lines, &vocab(), "xx");
        let em = Emissions::from_logits(em, CLASSES, &ids);
        align(lines, &vocab(), "xx", silent, &em).unwrap()
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
        let (out, share) = run(&[line("ab c")], &em);
        let w = &out[0].words;
        assert_eq!((w[0].start_ms, w[0].end_ms), (100, 300));
        assert_eq!((w[1].start_ms, w[1].end_ms), (300, 800));
        assert_eq!(share, 1.0);
        forward(&out);
    }

    #[test]
    fn the_star_absorbs_audio_between_lines_instead_of_stretching_them() {
        let mut marks = vec![(5, 2, 0.9), (60, 3, 0.9)];
        marks.extend((20..50).map(|t| (t, 4, 0.9)));
        let em = emissions(80, &marks);
        let (out, _) = run(&[line("a"), line("b")], &em);
        assert_eq!((out[0].start_ms, out[0].end_ms), (100, 620));
        assert_eq!((out[1].start_ms, out[1].end_ms), (1200, 1600));
    }

    /// Frames `a..b` flagged silent in a track of `frames`.
    fn quiet(frames: usize, a: usize, b: usize) -> Vec<bool> {
        (0..frames).map(|t| (a..b).contains(&t)).collect()
    }

    #[test]
    fn silence_flags_only_runs_of_a_hundred_ms_below_the_track_level() {
        let per = 320;
        let mut pcm = vec![0.5f32; per * 400];
        pcm[per * 100..per * 140].fill(0.0);
        pcm[per * 200..per * 203].fill(0.0);
        pcm[per * 300..per * 305].fill(0.0);
        let s = silent_frames(&pcm);
        assert_eq!(s.len(), 400);
        let flagged: Vec<usize> = (0..400).filter(|t| s[*t]).collect();
        assert_eq!(flagged, (100..140).chain(300..305).collect::<Vec<_>>());
    }

    #[test]
    fn a_word_does_not_stretch_over_silence_to_reach_a_spike() {
        let marks = [(5, 2, 0.9), (7, 3, 0.5), (400, 3, 0.99)];
        let em = emissions(800, &marks);
        let silent = quiet(800, 20, 700);
        let (free, _) = run(&[line("ab")], &em);
        assert!(free[0].words[0].end_ms > 8000);
        let (held, _) = run_silent(&[line("ab")], &em, &silent);
        assert_eq!(
            (held[0].words[0].start_ms, held[0].words[0].end_ms),
            (100, 400)
        );
    }

    #[test]
    fn a_voiced_tail_extends_the_word_end_and_a_silent_one_does_not() {
        let em = emissions(120, &[(5, 2, 0.9), (60, 3, 0.9)]);
        let (voiced, _) = run(&[line("a b")], &em);
        assert_eq!(voiced[0].words[0].end_ms, 620);
        let (quiet, _) = run_silent(&[line("a b")], &em, &quiet(120, 10, 50));
        assert_eq!(quiet[0].words[0].end_ms, 200);
        let (late, _) = run(
            &[line("a b")],
            &emissions(120, &[(5, 2, 0.9), (20, 3, 0.9)]),
        );
        assert_eq!(late[0].words[0].end_ms, 400);
    }

    #[test]
    fn an_unknown_word_is_interpolated_and_lowers_the_matched_share() {
        let em = emissions(60, &[(5, 2, 0.9), (30, 3, 0.9)]);
        let (out, share) = run(&[timed("a $$ b", 0, 1200)], &em);
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
    fn a_long_gap_between_timed_lines_leaves_each_on_its_own_band() {
        let em = emissions(3500, &[(120, 2, 0.9), (150, 3, 0.9), (2500, 4, 0.9)]);
        let lines = [
            timed("a", 1500, 2500),
            timed("b", 2500, 3500),
            timed("c", 50_000, 51_000),
        ];
        let (out, _) = run(&lines, &em);
        assert_eq!(out[2].words[0].start_ms, 50_000);
        forward(&out);
    }

    #[test]
    fn lines_times_that_run_late_still_place_every_word() {
        let mut marks = vec![];
        for (k, p) in [(0usize, 0.8), (1, 0.95), (2, 0.8)] {
            marks.extend([(50 + 100 * k, 2, p), (100 + 100 * k, 3, p)]);
        }
        let em = emissions(400, &marks);
        let late = |k: u32| timed("a b", 1700 + 2000 * k, 3700 + 2000 * k);
        let (out, share) = run(&[late(0), late(1), late(2)], &em);
        let starts: Vec<u32> = out
            .iter()
            .flat_map(|l| &l.words)
            .map(|w| w.start_ms)
            .collect();
        assert_eq!(starts, vec![1000, 2000, 3000, 4000, 5000, 6000]);
        assert_eq!(share, 1.0);
    }

    #[test]
    fn a_frame_inside_two_bands_goes_to_the_line_that_sings_it() {
        let em = emissions(
            300,
            &[(50, 2, 0.9), (150, 3, 0.9), (160, 4, 0.9), (200, 2, 0.9)],
        );
        let lines = [timed("a b", 1000, 3500), timed("c a", 3500, 5000)];
        let (out, share) = run(&lines, &em);
        let starts: Vec<u32> = out
            .iter()
            .flat_map(|l| &l.words)
            .map(|w| w.start_ms)
            .collect();
        assert_eq!(starts, vec![1000, 3000, 3200, 4000]);
        assert_eq!(share, 1.0);
    }

    #[test]
    fn a_tokenless_line_after_the_next_words_start_never_runs_backwards() {
        let em = emissions(700, &[(480, 2, 0.9)]);
        let lines = [
            timed("\u{266a} \u{266a} \u{266a}", 10_000, 10_500),
            timed("a", 10_500, 12_000),
        ];
        let (out, _) = run(&lines, &em);
        assert_eq!(out[1].words[0].start_ms, 9600);
        forward(&out);
    }

    #[test]
    fn untimed_lines_between_timed_ones_sit_between_their_neighbours() {
        let em = emissions(200, &[(5, 2, 0.9), (60, 3, 0.9), (100, 4, 0.9)]);
        let lines = [timed("a", 0, 500), line("b"), timed("c", 2000, 2500)];
        let (out, _) = run(&lines, &em);
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
        let (out, share) = run(&[timed("a x b", 0, 1200)], &em);
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
    fn an_absurd_table_is_an_error_before_it_is_allocated() {
        let text = vec!["a"; 3000].join(" ");
        let lines = [line(&text)];
        let ids = lyric_ids(&lines, &vocab(), "xx");
        let em = Emissions::from_logits(&emissions(35_000, &[]), CLASSES, &ids);
        let r = align(&lines, &vocab(), "xx", &[], &em);
        assert_eq!(r.unwrap_err(), TooLarge);
    }

    #[test]
    fn a_band_too_short_for_its_words_reaches_the_next_bands_end() {
        let text = vec!["a"; 100].join(" ");
        let mut marks: Vec<_> = (0..100).map(|k| (10 + 3 * k, 2, 0.9)).collect();
        marks.extend((0..99).map(|k| (11 + 3 * k, DELIM, 0.9)));
        marks.push((340, 3, 0.9));
        let lines = [timed(&text, 0, 100), timed("b", 100, 10_000)];
        let (out, share) = run(&lines, &emissions(700, &marks));
        assert_eq!(share, 1.0);
        assert_eq!(out[0].words[99].start_ms, 6140);
        assert_eq!(out[1].words[0].start_ms, 6800);
    }

    #[test]
    fn a_line_beyond_the_audio_is_timed_between_its_neighbours() {
        let em = emissions(40, &[(5, 2, 0.9)]);
        let (out, share) = run(&[timed("a", 0, 300), timed("b", 5000, 6000)], &em);
        assert_eq!(share, 0.5);
        assert_eq!(out[0].words[0].start_ms, 100);
        forward(&out);
    }

    /// The textbook table, every state at every frame: `best_path` must walk the same path.
    fn dense_path(
        em: &Emissions,
        lay: &Layout,
        band: &[(u32, u32)],
        silent: &[bool],
    ) -> Option<Vec<usize>> {
        let (n, frames) = (lay.states.len(), em.frames());
        let skip = skips(&lay.states);
        let mut prev = vec![f32::NEG_INFINITY; n];
        let mut back = vec![0usize; frames * n];
        for (s, p) in prev.iter_mut().enumerate().take(3.min(n)) {
            *p = emit(lay, band, em.row(0), 0, s);
        }
        for t in 1..frames {
            let mut cur = vec![f32::NEG_INFINITY; n];
            for s in 0..n {
                let (mut best, mut from) = (prev[s], 0);
                if s >= 1 && prev[s - 1] > best {
                    (best, from) = (prev[s - 1], 1);
                }
                if skip[s] && prev[s - 2] > best {
                    (best, from) = (prev[s - 2], 2);
                }
                let held = if silent.get(t) == Some(&true) && lay.in_word[s] {
                    SILENT_PEN
                } else {
                    0.0
                };
                cur[s] = best + emit(lay, band, em.row(t), t, s) + held;
                back[t * n + s] = from;
            }
            prev = cur;
        }
        let mut s = (n.saturating_sub(3)..n).max_by(|a, b| prev[*a].total_cmp(&prev[*b]))?;
        if prev[s] == f32::NEG_INFINITY {
            return None;
        }
        let mut path = vec![0; frames];
        for t in (0..frames).rev() {
            path[t] = s;
            s -= back[t * n + s];
        }
        Some(path)
    }

    struct Lcg(u64);

    impl Lcg {
        fn below(&mut self, n: u32) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32 % n
        }
    }

    #[test]
    fn the_banded_search_walks_the_dense_tables_path() {
        let (mut rng, mut found) = (Lcg(7), 0);
        for _ in 0..200 {
            let lines: Vec<AlignLine> = (0..1 + rng.below(4))
                .map(|_| {
                    let words: Vec<String> = (0..1 + rng.below(4))
                        .map(|_| {
                            (0..1 + rng.below(4))
                                .map(|_| ["a", "b", "c", "'", "é"][rng.below(5) as usize])
                                .collect()
                        })
                        .collect();
                    line(&words.join(" "))
                })
                .collect();
            let frames = 30 + rng.below(70) as usize;
            let ids = lyric_ids(&lines, &vocab(), "xx");
            let logits: Vec<f32> = (0..frames * CLASSES)
                .map(|_| rng.below(600) as f32 / 100.0 - 3.0)
                .collect();
            let em = Emissions::from_logits(&logits, CLASSES, &ids);
            let silent: Vec<bool> = (0..frames)
                .map(|t| (t / 7) % 3 == rng.below(3) as usize)
                .collect();
            let toks: Vec<Vec<Vec<u32>>> = lines
                .iter()
                .map(|l| {
                    l.text
                        .split(' ')
                        .map(|w| tokens(w, &vocab(), "xx"))
                        .collect()
                })
                .collect();
            let run: Vec<&[Vec<u32>]> = toks.iter().map(Vec::as_slice).collect();
            let lay = layout(&vocab(), &run, &ids).unwrap();
            let bands: Vec<(u32, u32)> = lines
                .iter()
                .map(|_| {
                    let lo = rng.below(frames as u32 / 2);
                    (lo, lo + frames as u32 / 2 + rng.below(frames as u32 / 2))
                })
                .collect();
            let band = state_bands(&lay, &bands, frames);
            let sparse = best_path(&em, &lay, &band, &silent).unwrap();
            found += usize::from(sparse.is_some());
            assert_eq!(sparse, dense_path(&em, &lay, &band, &silent));
        }
        assert!(found > 100, "{found} cases had a path");
    }

    #[test]
    fn ten_minutes_of_untimed_lyrics_are_placed() {
        let mut rng = Lcg(3);
        let text: Vec<String> = (0..800)
            .map(|_| {
                (0..2 + rng.below(8))
                    .map(|_| ["a", "b", "c"][rng.below(3) as usize])
                    .collect()
            })
            .collect();
        let lines = [line(&text.join(" "))];
        let ids = lyric_ids(&lines, &vocab(), "xx");
        let em = Emissions::from_logits(&emissions(30_000, &[]), CLASSES, &ids);
        let (out, _) = align(&lines, &vocab(), "xx", &[], &em).unwrap();
        assert_eq!(out[0].words.len(), 800);
        forward(&out);
    }

    #[test]
    fn emissions_keep_the_asked_columns_and_the_best_non_blank_class() {
        let logits = [1.0, 4.0, 0.0, 2.0, 0.0, 9.0, 3.0, 1.0];
        let em = Emissions::from_logits(&logits, 4, &[3, 1]);
        assert_eq!((em.classes(), em.frames()), (3, 2));
        let lse = |r: [f32; 4]| r.iter().map(|v| v.exp()).sum::<f32>().ln();
        let (a, b) = (lse([1.0, 4.0, 0.0, 2.0]), lse([0.0, 9.0, 3.0, 1.0]));
        let want = [2.0 - a, 4.0 - a, 4.0 - a, 1.0 - b, 9.0 - b, 9.0 - b];
        for (g, w) in em.log_probs.iter().zip(want) {
            assert!((g - w).abs() < 1e-5, "{g} {w}");
        }
    }
}

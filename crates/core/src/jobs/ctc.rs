//! The pure half of the CTC `Align`: the lyrics forced onto a network's per-frame log-probabilities
//! (Viterbi), giving word times and scores. Spec: `docs/spec/karaoke.md` section 4.
use std::collections::{BTreeSet, HashMap};
use std::ops::Range;

use unicode_normalization::UnicodeNormalization;

use super::align::{Copy, Role, finish, shares, words_of};
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
/// Marks a state no optional span is skipped to.
const NO_JUMP: usize = usize::MAX;
/// A frame is silent when its level is this far under the loud level around it, for at least
/// `MIN_SILENT_MS`. Over a stem's noise floor (bleed) the bar is lower, see `Voicing::of`.
const SILENT_DB: f32 = -35.0;
const MIN_SILENT_MS: u32 = 100;
/// Log-probability a word's states lose per silent frame: a word does not stretch over silence.
const SILENT_PEN: f32 = -3.0;
/// A word's end is held while the level stays within `HOLD_DROP_DB` of its tokens' own, for at
/// most `HOLD_MAX_MS`.
const HOLD_DROP_DB: f32 = 15.0;
const HOLD_MAX_MS: u32 = 4000;

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

/// What the vocal stem says of each `FRAME_MS` frame.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Voicing {
    pub silent: Vec<bool>,
    pub level: Vec<f32>,
}

impl Voicing {
    /// Of the 16 kHz track `pcm`. The loud level around a frame is the 95th percentile of the
    /// RMS within 10 s (never under 25 dB below the track's own, so a gap is not its own bar);
    /// a stem with a noise floor also counts as silent what is 20 dB under it and near the floor.
    pub fn of(pcm: &[f32]) -> Self {
        let per = (SAMPLE_RATE * FRAME_MS / 1000) as usize;
        let level: Vec<f32> = pcm
            .chunks_exact(per)
            .map(|f| (f.iter().map(|v| v * v).sum::<f32>() / per as f32).sqrt())
            .collect();
        let pct = |v: &[f32], p: usize| {
            let mut v = v.to_vec();
            v.sort_by(f32::total_cmp);
            v.get(v.len() * p / 100).copied().unwrap_or(0.0)
        };
        let db = |x: f32, d: f32| x * 10f32.powf(d / 20.0);
        let (loud, floor) = (pct(&level, 95), pct(&level, 10));
        let (block, reach) = (1000 / FRAME_MS as usize, 10_000 / FRAME_MS as usize);
        let mut silent = vec![false; level.len()];
        for from in (0..level.len()).step_by(block) {
            let near = &level[from.saturating_sub(reach)..(from + block + reach).min(level.len())];
            let here = pct(near, 95).max(db(loud, -25.0));
            let bar = db(here, SILENT_DB).max(db(floor, 6.0).min(db(here, -20.0)));
            for t in from..(from + block).min(level.len()) {
                silent[t] = level[t] < bar;
            }
        }
        let min = (MIN_SILENT_MS / FRAME_MS) as usize;
        let mut at = 0;
        while at < silent.len() {
            let run = silent[at..].iter().take_while(|v| **v).count();
            if run > 0 && run < min {
                silent[at..at + run].fill(false);
            }
            at += run.max(1);
        }
        Voicing { silent, level }
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

/// A word placed on the track, in frames (ends exclusive): its tokens' start and end, the word's
/// end (held over voiced frames) and the mean log-probability of its token frames.
#[derive(Clone, Copy)]
struct Placed {
    start: u32,
    tok_end: u32,
    end: u32,
    score: f32,
}

/// The lyrics as states: a star before each line and after the last, each token then a blank.
/// `in_word` marks the blanks between a word's tokens (silence costs there), `jump` the skips.
struct Layout {
    cols: Vec<usize>,
    in_word: Vec<bool>,
    jump: Vec<usize>,
    states: Vec<State>,
    spans: Vec<Vec<Option<(usize, usize)>>>,
    bodies: Vec<Range<usize>>,
}

/// The lines (tokens and roles of their words) as states, columns looked up in `ids`. A span of
/// optional words is jumped blank to blank, dropping the delimiter after it, else before it.
fn layout(vocab: &Vocab, lines: &[(&[Vec<u32>], &[Role])], ids: &[u32]) -> Option<Layout> {
    let mut states = vec![];
    let mut spans: Vec<Vec<Option<(usize, usize)>>> = vec![];
    let mut bodies = vec![];
    let mut jumps: Vec<(usize, usize)> = vec![];
    for (words, roles) in lines {
        let mut mine = vec![None; words.len()];
        let body = states.len() + 1;
        if words.iter().any(|w| !w.is_empty()) {
            states.extend([State::Star, State::Blank]);
            let mut seq: Vec<(usize, usize, Option<u32>)> = vec![];
            for (i, w) in words.iter().enumerate().filter(|(_, w)| !w.is_empty()) {
                if !seq.is_empty() {
                    states.extend([State::Tok(vocab.delimiter), State::Blank]);
                }
                let a = states.len();
                for t in w.iter() {
                    states.extend([State::Tok(*t), State::Blank]);
                }
                mine[i] = Some((a, states.len() - 2));
                let group = match roles[i] {
                    Role::Optional(g) => Some(g),
                    _ => None,
                };
                seq.push((a, states.len() - 2, group));
            }
            let mut k = 0;
            while k < seq.len() {
                let Some(g) = seq[k].2 else {
                    k += 1;
                    continue;
                };
                let m = (k..seq.len()).take_while(|j| seq[*j].2 == Some(g)).count() + k - 1;
                let (first, last) = (seq[k].0, seq[m].1);
                let jump = if m + 1 < seq.len() {
                    (first - 1, last + 3)
                } else if k > 0 && seq[k - 1].2.is_none() {
                    (first - 3, last + 1)
                } else {
                    (first - 1, last + 1)
                };
                jumps.push((jump.1, jump.0));
                k = m + 1;
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
        (a + 1..*b).step_by(2).for_each(|s| in_word[s] = true);
    }
    let mut jump = vec![NO_JUMP; states.len()];
    for (dst, src) in jumps {
        jump[dst] = src;
    }
    let cols = states
        .iter()
        .map(|s| match s {
            State::Star | State::Blank => 0,
            State::Tok(WILDCARD) => ids.len(),
            State::Tok(t) => {
                let col = ids.iter().position(|i| i == t);
                debug_assert!(col.is_some(), "token {t} is not among the emissions' ids");
                col.unwrap_or(ids.len())
            }
        })
        .collect();
    Some(Layout {
        cols,
        in_word,
        jump,
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

/// Whether state `s` may be reached from two states before it.
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

/// Per frame its state range and where its back pointers start, and the cells in all.
struct Table {
    ranges: Vec<(usize, usize)>,
    offsets: Vec<usize>,
    cells: usize,
}

fn table(band: &[(u32, u32)], frames: usize) -> Result<Table, TooLarge> {
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
    Ok(Table {
        ranges,
        offsets,
        cells,
    })
}

/// The best state of every frame, `None` when no path fits the bands. Back pointers (two bits a
/// cell: stay, one state, two states, a jump) are kept only inside each frame's state range.
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
    let Table {
        ranges,
        offsets,
        cells,
    } = table(band, frames)?;
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
            let j = lay.jump[s];
            if j != NO_JUMP && prev[j] > best {
                (best, from) = (prev[j], 3);
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
            s = match back[at / 4] >> (at % 4 * 2) & 3 {
                3 => lay.jump[s],
                back => s - usize::from(back),
            };
        }
    }
    Ok(Some(path))
}

/// The lines of `lay` forced onto `em`, per word where it sits (`None`: no token, or no path).
/// `bands` (frames, per line) are the only frames a line's tokens may use.
fn viterbi(
    em: &Emissions,
    lay: &Layout,
    bands: &[(u32, u32)],
    voice: &Voicing,
) -> Result<Option<Vec<Vec<Option<Placed>>>>, TooLarge> {
    let (n, frames) = (lay.states.len(), em.frames());
    let band = state_bands(lay, bands, frames);
    let Some(path) = best_path(em, lay, &band, &voice.silent)? else {
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
        let at = |t: usize| voice.level.get(t).copied();
        let own = (p.start..p.tok_end)
            .filter_map(|t| at(t as usize))
            .fold(0.0, f32::max);
        let bar = own * 10f32.powf(-HOLD_DROP_DB / 20.0);
        let cap = if gap {
            p.tok_end
        } else {
            limit.min(p.tok_end + HOLD_MAX_MS / FRAME_MS)
        };
        let ends = |t: u32| {
            voice.silent.get(t as usize) == Some(&true) || at(t as usize).is_some_and(|l| l < bar)
        };
        p.end = (p.tok_end..cap).find(|t| ends(*t)).unwrap_or(cap);
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

/// The frames a copy needs at least: one a token of a required word, a delimiter between words, a
/// blank between two equal tokens in a row, one for the star before it, two for the blanks a jump
/// over an optional span goes between.
fn frames_needed(words: &[Vec<u32>], roles: &[Role]) -> usize {
    let words = words.iter().zip(roles).filter(|(w, _)| !w.is_empty());
    let (mut required, mut spans, mut last) = (vec![], 0, None);
    for (w, r) in words.clone() {
        match r {
            Role::Word => required.push(w),
            Role::Optional(g) if last != Some(g) => spans += 1,
            _ => {}
        }
        last = match r {
            Role::Optional(g) => Some(g),
            _ => None,
        };
    }
    let repeats = required
        .iter()
        .map(|w| w.windows(2).filter(|p| p[0] == p[1]).count())
        .sum::<usize>();
    let tokens = required.iter().map(|w| w.len()).sum::<usize>();
    let star = usize::from(words.count() > 0);
    (tokens + repeats + required.len()).saturating_sub(1) + star + 2 * spans
}

/// `word` without its `[...]` parts (labels inside a word).
fn unlabelled(word: &str) -> String {
    let mut depth = 0u32;
    word.chars()
        .filter(|c| {
            match c {
                '[' => depth += 1,
                ']' if depth > 0 => depth -= 1,
                _ => return depth == 0,
            }
            false
        })
        .collect()
}

/// The alignment would need more cells than `MAX_CELLS`.
#[derive(Debug, PartialEq)]
pub struct TooLarge;

/// Why `align` placed nothing.
#[derive(Debug, PartialEq)]
pub enum AlignError {
    TooLarge,
    /// The lyrics cannot be laid on the audio in order.
    NoPath,
}

impl From<TooLarge> for AlignError {
    fn from(_: TooLarge) -> Self {
        Self::TooLarge
    }
}

/// What one Viterbi pass is given: the copies, their tokens and the frames each may sit on.
struct Plan {
    copies: Vec<Copy>,
    toks: Vec<Vec<Vec<u32>>>,
    frames: Vec<(u32, u32)>,
}

/// The copies of `lines` with their frames. A band too short reaches the next one's end; a copy
/// that cannot follow the ones before it (stale times) is untimed; one that fits nowhere has no
/// tokens, so the copies kept always have a path.
fn plan(lines: &[AlignLine], vocab: &Vocab, language: &str, duration_ms: u32) -> Plan {
    let mut copies = words_of(lines);
    let mut toks: Vec<Vec<Vec<u32>>> = copies
        .iter()
        .map(|c| {
            let words = c.words.iter().zip(&c.roles);
            words
                .map(|((s, e), role)| match role {
                    Role::Label => vec![],
                    _ => tokens(
                        &unlabelled(&c.text[*s as usize..*e as usize]),
                        vocab,
                        language,
                    ),
                })
                .collect()
        })
        .collect();
    let needs = |toks: &[Vec<Vec<u32>>], copies: &[Copy], i: usize| {
        frames_needed(&toks[i], &copies[i].roles) as u32 * FRAME_MS
    };
    let widened = |copies: &[Copy], need: u32, i: usize| {
        let (lo, hi) = band_of(copies, i, duration_ms).unwrap_or((duration_ms, duration_ms));
        let next = (i + 1..copies.len()).find(|k| copies[*k].window.is_some());
        let reach = next.map_or(duration_ms, |k| {
            band_of(copies, k, duration_ms).map_or(duration_ms, |b| b.1)
        });
        (lo, if hi - lo < need { hi.max(reach) } else { hi })
    };
    // A stale copy's neighbours are as stale as it is: only the track's end bounds it.
    let untimed_hi = |copies: &[Copy], stale: bool, i: usize| {
        let next = copies[i + 1..].iter().find_map(|c| c.window);
        match next {
            Some(w) if !stale => (w.0 + TAIL_MS).min(duration_ms),
            _ => duration_ms,
        }
    };
    // A band follows the next timed copy, so clearing a window can shrink an earlier band: walk
    // again until a walk clears none.
    let (full, mut stale) = (toks.clone(), vec![false; copies.len()]);
    loop {
        let (mut cursor, mut cleared) = (0, false);
        toks.clone_from(&full);
        for i in 0..copies.len() {
            let need = needs(&toks, &copies, i);
            if copies[i].window.is_some() {
                let (lo, hi) = widened(&copies, need, i);
                if cursor.max(lo) + need <= hi {
                    cursor = cursor.max(lo) + need;
                    continue;
                }
                (copies[i].window, stale[i], cleared) = (None, true, true);
            }
            if cursor + need <= untimed_hi(&copies, stale[i], i) {
                cursor += need;
            } else {
                toks[i].iter_mut().for_each(Vec::clear);
            }
        }
        if !cleared {
            break;
        }
    }
    let frames = (0..copies.len())
        .map(|i| {
            let need = needs(&toks, &copies, i);
            let (lo, hi) = match copies[i].window {
                Some(_) => widened(&copies, need, i),
                None => (0, untimed_hi(&copies, stale[i], i)),
            };
            (lo / FRAME_MS, hi.div_ceil(FRAME_MS))
        })
        .collect();
    Plan {
        copies,
        toks,
        frames,
    }
}

/// The states and bands of `plan`, for emissions of `ids`.
fn layout_of(plan: &Plan, vocab: &Vocab, ids: &[u32]) -> Option<Layout> {
    let run: Vec<(&[Vec<u32>], &[Role])> = plan
        .toks
        .iter()
        .zip(&plan.copies)
        .map(|(t, c)| (t.as_slice(), c.roles.as_slice()))
        .collect();
    layout(vocab, &run, ids)
}

/// Whether the alignment of `lines` on a track of `duration_ms` stays within `MAX_CELLS`: known
/// before the network runs.
pub fn fits(
    lines: &[AlignLine],
    vocab: &Vocab,
    language: &str,
    duration_ms: u32,
) -> Result<(), TooLarge> {
    let frames = duration_ms.div_ceil(FRAME_MS) as usize;
    let plan = plan(lines, vocab, language, frames as u32 * FRAME_MS);
    let ids = lyric_ids(lines, vocab, language);
    match layout_of(&plan, vocab, &ids) {
        Some(lay) => table(&state_bands(&lay, &plan.frames, frames), frames).map(|_| ()),
        None => Ok(()),
    }
}

/// The lines placed by CTC forced alignment in one Viterbi pass, and the share of words found:
/// `em` the track's emissions for `lyric_ids`, `voice` its levels.
pub fn align(
    lines: &[AlignLine],
    vocab: &Vocab,
    language: &str,
    voice: &Voicing,
    em: &Emissions,
) -> Result<(Vec<AlignedLine>, f64), AlignError> {
    let duration_ms = em.frames() as u32 * FRAME_MS;
    let plan = plan(lines, vocab, language, duration_ms);
    let total = plan.copies.iter().map(|c| c.words.len()).sum();
    let (mut times, mut found, mut at) = (vec![None; total], vec![false; total], 0);
    if let Some(lay) = layout_of(&plan, vocab, &em.ids) {
        let placed = viterbi(em, &lay, &plan.frames, voice)?.ok_or(AlignError::NoPath)?;
        for (copy, line) in plan.copies.iter().zip(placed) {
            for (wi, p) in line.iter().enumerate() {
                if let Some(p) = p {
                    times[at + wi] = Some((p.start * FRAME_MS, p.end * FRAME_MS));
                    found[at + wi] = p.score >= MIN_SCORE;
                }
            }
            at += copy.words.len();
        }
    }
    let (matched, share) = shares(&plan.copies, &found);
    Ok((
        finish(&plan.copies, &mut times, &matched, duration_ms),
        share,
    ))
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
        let voice = Voicing {
            silent: silent.to_vec(),
            level: vec![],
        };
        run_voiced(lines, em, &voice)
    }

    fn run_voiced(lines: &[AlignLine], em: &[f32], voice: &Voicing) -> (Vec<AlignedLine>, f64) {
        let ids = lyric_ids(lines, &vocab(), "xx");
        let em = Emissions::from_logits(em, CLASSES, &ids);
        align(lines, &vocab(), "xx", voice, &em).unwrap()
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
        assert_eq!((out[0].start_ms, out[0].end_ms), (100, 1200));
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
        let s = Voicing::of(&pcm).silent;
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
    fn a_word_is_held_while_the_level_stays_near_its_own() {
        let em = emissions(120, &[(5, 2, 0.9), (60, 3, 0.9)]);
        let level = |until: usize, tail: f32| -> Vec<f32> {
            (0..120)
                .map(|t| if t < until { 1.0 } else { tail })
                .collect()
        };
        let held = Voicing {
            silent: vec![],
            level: level(120, 1.0),
        };
        assert_eq!(
            run_voiced(&[line("a b")], &em, &held).0[0].words[0].end_ms,
            1200
        );
        let faded = Voicing {
            silent: vec![],
            level: level(12, 0.01),
        };
        assert_eq!(
            run_voiced(&[line("a b")], &em, &faded).0[0].words[0].end_ms,
            240
        );
        let quiet = Voicing {
            silent: quiet(120, 10, 50),
            level: level(120, 1.0),
        };
        assert_eq!(
            run_voiced(&[line("a b")], &em, &quiet).0[0].words[0].end_ms,
            200
        );
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
        let r = align(&lines, &vocab(), "xx", &Voicing::default(), &em);
        assert_eq!(r.unwrap_err(), AlignError::TooLarge);
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
                if lay.jump[s] != NO_JUMP && prev[lay.jump[s]] > best {
                    (best, from) = (prev[lay.jump[s]], 3);
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
            s = match back[t * n + s] {
                3 => lay.jump[s],
                from => s - from,
            };
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
        let (mut rng, mut found, mut jumps, mut taken) = (Lcg(7), 0, 0, 0);
        for _ in 0..200 {
            let lines: Vec<AlignLine> = (0..1 + rng.below(4))
                .map(|_| {
                    let mut words: Vec<String> = (0..1 + rng.below(5))
                        .map(|_| {
                            (0..1 + rng.below(4))
                                .map(|_| ["a", "b", "c", "'", "é"][rng.below(5) as usize])
                                .collect()
                        })
                        .collect();
                    let (from, len) = (rng.below(words.len() as u32) as usize, 1 + rng.below(2));
                    if rng.below(2) == 0 {
                        words[from].insert(0, '(');
                        let to = (from + len as usize).min(words.len()) - 1;
                        words[to].push(')');
                    }
                    if rng.below(4) == 0 {
                        words.push("x2".into());
                    }
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
            let copies = words_of(&lines);
            let toks: Vec<Vec<Vec<u32>>> = copies
                .iter()
                .map(|c| {
                    let text = |(s, e): &(u32, u32)| &c.text[*s as usize..*e as usize];
                    c.words
                        .iter()
                        .map(|w| tokens(text(w), &vocab(), "xx"))
                        .collect()
                })
                .collect();
            let run: Vec<(&[Vec<u32>], &[Role])> = toks
                .iter()
                .zip(&copies)
                .map(|(t, c)| (t.as_slice(), c.roles.as_slice()))
                .collect();
            let lay = layout(&vocab(), &run, &ids).unwrap();
            jumps += lay.jump.iter().filter(|j| **j != NO_JUMP).count();
            let bands: Vec<(u32, u32)> = copies
                .iter()
                .map(|_| {
                    let lo = rng.below(frames as u32 / 2);
                    (lo, lo + frames as u32 / 2 + rng.below(frames as u32 / 2))
                })
                .collect();
            let band = state_bands(&lay, &bands, frames);
            let sparse = best_path(&em, &lay, &band, &silent).unwrap();
            found += usize::from(sparse.is_some());
            let path = sparse.iter().flatten().copied().collect::<Vec<_>>();
            taken += path.windows(2).filter(|w| lay.jump[w[1]] == w[0]).count();
            assert_eq!(sparse, dense_path(&em, &lay, &band, &silent));
        }
        assert!(
            found > 100 && jumps > 50 && taken > 20,
            "{found} paths, {jumps} jumps, {taken} taken"
        );
    }

    fn starts(out: &[AlignedLine]) -> Vec<u32> {
        out.iter()
            .flat_map(|l| &l.words)
            .map(|w| w.start_ms)
            .collect()
    }

    #[test]
    fn a_parenthesised_word_nobody_sings_is_skipped_and_leaves_the_matched_share() {
        let marks = [(5, 2, 0.9), (30, 3, 0.9)];
        let (out, share) = run(&[line("a (c) b")], &emissions(60, &marks));
        assert_eq!(starts(&out)[0], 100);
        assert_eq!(starts(&out)[2], 600);
        assert_eq!(
            (out[0].words[0].end_ms, share, out[0].matched),
            (120, 1.0, 1.0)
        );
        forward(&out);
        let sung = [
            (5, 2, 0.9),
            (12, DELIM, 0.9),
            (18, 4, 0.9),
            (24, DELIM, 0.9),
            (30, 3, 0.9),
        ];
        let (out, share) = run(&[line("a (c) b")], &emissions(60, &sung));
        assert_eq!((starts(&out), share), (vec![100, 360, 600], 1.0));
    }

    #[test]
    fn a_translation_in_parentheses_after_the_sung_part_is_skipped() {
        let marks = [(25, 2, 0.9), (40, 3, 0.9), (150, 4, 0.9), (170, 2, 0.9)];
        let em = emissions(260, &marks);
        let tail = "c a b c a b c a b c a b";
        let lines = |tail: String| {
            [
                timed(&format!("a b {tail}"), 0, 3000),
                timed("c a", 3000, 4000),
            ]
        };
        let (out, share) = run(&lines(format!("({tail})")), &em);
        assert_eq!(starts(&out)[..2], [500, 800]);
        assert_eq!(starts(&out)[14..], [3000, 3400]);
        assert_eq!((share, out[0].matched, out[1].matched), (1.0, 1.0, 1.0));
        forward(&out);
        let (_, bare) = run(&lines(tail.into()), &em);
        assert!(bare < 0.3, "{bare}");
    }

    #[test]
    fn a_line_sung_once_loses_nothing_for_its_xn_copies() {
        let marks = [(5, 2, 0.9), (7, DELIM, 0.9), (10, 3, 0.9)];
        let (out, share) = run(&[line("a b x2")], &emissions(40, &marks));
        assert_eq!((out.len(), share), (2, 1.0));
        assert_eq!((out[0].matched, out[1].matched), (1.0, 1.0));
        forward(&out);
        let twice = [
            (5, 2, 0.9),
            (7, DELIM, 0.9),
            (10, 3, 0.9),
            (25, 2, 0.9),
            (27, DELIM, 0.9),
            (30, 3, 0.9),
        ];
        let (out, share) = run(&[line("a b x2")], &emissions(40, &twice));
        assert_eq!((starts(&out), share), (vec![100, 200, 500, 600], 1.0));
    }

    #[test]
    fn a_section_label_has_no_tokens_and_is_not_counted() {
        let (out, share) = run(&[line("[b] a")], &emissions(30, &[(5, 2, 0.9)]));
        assert_eq!((out[0].words.len(), share, out[0].matched), (2, 1.0, 1.0));
        assert_eq!(out[0].words[1].start_ms, 100);
        forward(&out);
    }

    #[test]
    fn each_line_reports_the_share_of_its_words_found() {
        let marks = [(5, 2, 0.9), (10, 3, 0.9), (60, 3, 0.9), (70, 2, 0.9)];
        let (out, share) = run(&[line("a b c"), line("b a")], &emissions(100, &marks));
        assert!((out[0].matched - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(out[1].matched, 1.0);
        assert!((share - 0.8).abs() < 1e-9);
    }

    #[test]
    fn adjacent_optional_spans_are_all_skipped_by_jumps() {
        let lines = [line("(a) (b) c"), line("c (a) (b)"), line("a (b) (c)")];
        let ids = lyric_ids(&lines, &vocab(), "xx");
        let copies = words_of(&lines);
        let toks: Vec<Vec<Vec<u32>>> = copies
            .iter()
            .map(|c| {
                c.words
                    .iter()
                    .map(|(s, e)| tokens(&c.text[*s as usize..*e as usize], &vocab(), "xx"))
                    .collect()
            })
            .collect();
        let run: Vec<(&[Vec<u32>], &[Role])> = toks
            .iter()
            .zip(&copies)
            .map(|(t, c)| (t.as_slice(), c.roles.as_slice()))
            .collect();
        let lay = layout(&vocab(), &run, &ids).unwrap();
        let marks = [(10, 4, 0.9), (30, 4, 0.9), (50, 2, 0.9)];
        let em = Emissions::from_logits(&emissions(80, &marks), CLASSES, &ids);
        let free = vec![(0, 80); 3];
        let band = state_bands(&lay, &free, 80);
        let path = best_path(&em, &lay, &band, &[]).unwrap().unwrap();
        let taken: Vec<_> = path.windows(2).filter(|w| lay.jump[w[1]] == w[0]).collect();
        assert!(taken.len() >= 4, "{} jumps", taken.len());
        let (out, share) = run_voiced(&lines, &emissions(80, &marks), &Voicing::default());
        assert_eq!(
            (starts(&out)[2], starts(&out)[3], starts(&out)[6]),
            (200, 600, 1000)
        );
        assert_eq!(share, 1.0);
    }

    #[test]
    fn a_block_with_stale_times_is_placed_by_the_audio() {
        let sung = [
            (2400, 2, 0.9),
            (2600, 3, 0.9),
            (2800, 4, 0.9),
            (3000, 2, 0.9),
        ];
        let lines = [
            timed("a", 48_000, 49_000),
            timed("b", 52_000, 53_000),
            timed("c", 1000, 2000),
            timed("a", 2000, 3000),
        ];
        let (out, share) = run(&lines, &emissions(3300, &sung));
        assert_eq!(
            (starts(&out), share),
            (vec![48_000, 52_000, 56_000, 60_000], 1.0)
        );
    }

    #[test]
    fn an_outro_that_does_not_fit_costs_only_its_overflow_copies() {
        let marks = [(55, 2, 0.9), (250, 3, 0.95), (480, 3, 0.6)];
        let em = emissions(1000, &marks);
        let mut lines = vec![timed("a", 1000, 2000), timed("b", 9000, 10_000)];
        lines.extend((0..20).map(|k| timed("c c c c c c", 18_000 + k * 50, 18_050 + k * 50)));
        let p = plan(&lines, &vocab(), "xx", 20_000);
        let dropped = p
            .toks
            .iter()
            .filter(|t| t.iter().all(Vec::is_empty))
            .count();
        assert_eq!(dropped, 4);
        let (out, _) = run(&lines, &em);
        assert!(
            (7000..=11_000).contains(&out[1].start_ms),
            "{}",
            out[1].start_ms
        );
        assert!(out.iter().all(|l| l.end_ms <= 20_000));
        forward(&out);
    }

    #[test]
    fn a_plan_always_has_a_path_and_no_time_passes_the_track() {
        let mut rng = Lcg(11);
        for _ in 0..400 {
            let frames = 40 + rng.below(500);
            let lines: Vec<AlignLine> = (0..1 + rng.below(12))
                .map(|_| {
                    let words: Vec<&str> = (0..1 + rng.below(6))
                        .map(|_| {
                            ["a", "ab", "abc", "bb", "c", "(ab)", "(a", "c)"][rng.below(8) as usize]
                        })
                        .collect();
                    let text = words.join(" ") + ["", "", " x2", " x3"][rng.below(4) as usize];
                    match rng.below(3) {
                        0 => line(&text),
                        _ => {
                            let s = rng.below(frames * FRAME_MS * 4 / 3);
                            timed(&text, s, s + rng.below(4000))
                        }
                    }
                })
                .collect();
            let em = emissions(frames as usize, &[]);
            let (out, _) = run(&lines, &em);
            assert!(
                out.iter().all(|l| l.end_ms <= frames * FRAME_MS),
                "{lines:?}"
            );
            forward(&out);
        }
    }

    #[test]
    fn a_label_inside_a_word_gives_no_tokens() {
        assert_eq!(unlabelled("[ab]c"), "c");
        assert_eq!(unlabelled("a[b]"), "a");
        let (out, share) = run(&[line("[ab]c")], &emissions(30, &[(5, 4, 0.9)]));
        assert_eq!((out[0].words.len(), share), (1, 1.0));
    }

    #[test]
    fn the_cells_are_known_before_the_audio_is_read() {
        let text = vec!["a"; 3000].join(" ");
        assert_eq!(fits(&[line(&text)], &vocab(), "xx", 700_000), Err(TooLarge));
        assert_eq!(fits(&[line(&text)], &vocab(), "xx", 100_000), Ok(()));
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
        let (out, _) = align(&lines, &vocab(), "xx", &Voicing::default(), &em).unwrap();
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

//! The pure half of `Align`: known lyric words matched onto the words a speech recogniser heard
//! in the vocal stem, and timed from them. The recogniser runs in the engine. Spec:
//! `docs/spec/karaoke.md` section 4.
use crate::api::{AlignLine, AlignedLine, AlignedWord, JobKind};

const MAX_LINES: usize = 3000;
const MAX_WORDS: usize = 6000;
/// Milliseconds a heard word may lie outside its line's times and still match it.
const WINDOW_MARGIN_MS: u32 = 1500;
/// Time given to a word nothing was heard for, per character, and its floor.
const MS_PER_CHAR: u32 = 70;
const MIN_WORD_MS: u32 = 120;
const MAX_REPEAT: u32 = 9;

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

/// The lines placed on the audio, and the share of lyric words that were heard.
pub fn align(lines: &[AlignLine], heard: &[Heard]) -> (Vec<AlignedLine>, f64) {
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
    (finish(&copies, &mut times, &matched, u32::MAX), share)
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
    let flat: Vec<(u32, Option<(u32, u32)>)> = copies
        .iter()
        .flat_map(|c| {
            c.words.iter().map(move |(s, e)| {
                let key = norm(&c.text[*s as usize..*e as usize]);
                (key.chars().count() as u32 + 1, c.window)
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
        let mut j = i;
        while j < n && times[j].is_none() {
            j += 1;
        }
        let weights: Vec<u32> = (i..j).map(|k| flat[k].0).collect();
        let total: u32 = weights.iter().sum();
        let mut left = i.checked_sub(1).and_then(|k| times[k]).map_or(0, |t| t.1);
        if let Some((s, _)) = flat[i].1 {
            left = left.max(s);
        }
        let next = times.get(j).copied().flatten();
        if let Some((s, _)) = next {
            left = left.min(s);
        }
        let own = (total * MS_PER_CHAR).max(MIN_WORD_MS * (j - i) as u32);
        let mut right = next.map_or(left + own, |t| t.0);
        if let Some((_, e)) = flat[j - 1].1
            && next.is_none()
        {
            right = right.max(left).min(e.max(left + MIN_WORD_MS));
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

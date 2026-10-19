//! Cue times of a subtitle text, read and rewritten in place: only the timestamps change, the
//! rest of the text is kept as is. Formats: SubRip, WebVTT, SSA/ASS and MicroDVD.
use crate::api::SubtitleFormat;

/// MicroDVD counts frames, not time.
const MICRODVD_FPS: f64 = 30.0;

/// `(start, end)` in ms of every cue, in file order.
pub fn cue_spans(text: &str, format: SubtitleFormat) -> Vec<(i64, i64)> {
    let mut out = vec![];
    rewrite(text, format, |_, span| {
        out.push((span.0.min(span.1), span.0.max(span.1)));
        span
    });
    out
}

/// `text` with the times of cue `i` replaced by `f(i, (start, end))`; negative times become 0.
pub fn retime_cues(
    text: &str,
    format: SubtitleFormat,
    mut f: impl FnMut(usize, (i64, i64)) -> (i64, i64),
) -> String {
    rewrite(text, format, |i, span| {
        let (s, e) = f(i, span);
        (s.max(0), e.max(0))
    })
}

fn rewrite(
    text: &str,
    format: SubtitleFormat,
    mut f: impl FnMut(usize, (i64, i64)) -> (i64, i64),
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cue = 0;
    // ASS: the column of Start and End among a Dialogue line's fields, once `[Events]` is read.
    let mut ass = Ass::default();
    for raw in text.split_inclusive('\n') {
        let body = raw.trim_end_matches(['\r', '\n']);
        let eol = &raw[body.len()..];
        let new = match format {
            SubtitleFormat::Srt | SubtitleFormat::Vtt => timing_line(body, format, &mut |s| {
                cue += 1;
                f(cue - 1, s)
            }),
            SubtitleFormat::Ass | SubtitleFormat::Ssa => ass.line(body, &mut |s| {
                cue += 1;
                f(cue - 1, s)
            }),
            SubtitleFormat::Sub => microdvd_line(body, &mut |s| {
                cue += 1;
                f(cue - 1, s)
            }),
        };
        out.push_str(new.as_deref().unwrap_or(body));
        out.push_str(eol);
    }
    out
}

/// `[[h:]m:]s[,.]f` in ms.
fn clock_ms(t: &str) -> Option<i64> {
    let (clock, frac) = match t.split_once([',', '.']) {
        Some((c, f)) => (c, f),
        None => (t, ""),
    };
    let mut secs = 0i64;
    for part in clock.split(':') {
        secs = secs * 60 + part.trim().parse::<i64>().ok()?;
    }
    let mut ms = 0;
    for (i, d) in frac.bytes().take(3).enumerate() {
        ms += i64::from(d.checked_sub(b'0').filter(|d| *d < 10)?) * 10i64.pow(2 - i as u32);
    }
    Some(secs * 1000 + ms)
}

fn clock_text(ms: i64, sep: char) -> String {
    let (h, m, s, f) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    format!("{h:02}:{m:02}:{s:02}{sep}{f:03}")
}

/// SubRip and WebVTT: `start --> end`, the end followed by cue settings in VTT.
fn timing_line(
    line: &str,
    format: SubtitleFormat,
    f: &mut dyn FnMut((i64, i64)) -> (i64, i64),
) -> Option<String> {
    let (left, right) = line.split_once("-->")?;
    let right_trim = right.trim_start();
    let end_len = right_trim
        .find(char::is_whitespace)
        .unwrap_or(right_trim.len());
    let (start, end) = (clock_ms(left.trim())?, clock_ms(&right_trim[..end_len])?);
    let (s, e) = f((start, end));
    let sep = if format == SubtitleFormat::Vtt {
        '.'
    } else {
        ','
    };
    Some(format!(
        "{} --> {}{}",
        clock_text(s, sep),
        clock_text(e, sep),
        &right_trim[end_len..]
    ))
}

#[derive(Default)]
struct Ass {
    events: bool,
    /// `(Start, End, field count)`.
    columns: Option<(usize, usize, usize)>,
}

impl Ass {
    fn line(&mut self, line: &str, f: &mut dyn FnMut((i64, i64)) -> (i64, i64)) -> Option<String> {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            self.events = trimmed.eq_ignore_ascii_case("[events]");
            return None;
        }
        if !self.events {
            return None;
        }
        let (key, rest) = trimmed.split_once(':')?;
        if key.eq_ignore_ascii_case("format") {
            let names: Vec<_> = rest
                .split(',')
                .map(|n| n.trim().to_ascii_lowercase())
                .collect();
            let at = |n: &str| names.iter().position(|x| x == n);
            self.columns = at("start").zip(at("end")).map(|(s, e)| (s, e, names.len()));
            return None;
        }
        if !key.eq_ignore_ascii_case("dialogue") {
            return None;
        }
        let (si, ei, n) = self.columns.unwrap_or((1, 2, 10));
        let head = line.len() - line.trim_start().len() + key.len() + 1;
        let mut fields: Vec<&str> = line[head..].splitn(n, ',').collect();
        let pad = &fields.first()?[..fields[0].len() - fields[0].trim_start().len()];
        let (start, end) = (ass_ms(fields.get(si)?)?, ass_ms(fields.get(ei)?)?);
        let (s, e) = f((start, end));
        let (s, e) = (ass_text(s), ass_text(e));
        fields[si] = &s;
        fields[ei] = &e;
        Some(format!(
            "{}{}{}",
            &line[..head],
            pad,
            fields.join(",").trim_start()
        ))
    }
}

/// `h:mm:ss.cc`.
fn ass_ms(t: &str) -> Option<i64> {
    let (clock, cs) = t.trim().split_once('.').unwrap_or((t.trim(), "0"));
    let ms = clock_ms(clock)?;
    let frac = cs.get(..cs.len().min(2))?.parse::<i64>().ok()?;
    Some(ms + frac * if cs.len() == 1 { 100 } else { 10 })
}

fn ass_text(ms: i64) -> String {
    let cs = (ms + 5) / 10;
    format!(
        "{}:{:02}:{:02}.{:02}",
        cs / 360_000,
        cs / 6000 % 60,
        cs / 100 % 60,
        cs % 100
    )
}

/// `{start}{end}text`, frames at 30 fps; the `{1}{1}fps` header is not a cue.
fn microdvd_line(line: &str, f: &mut dyn FnMut((i64, i64)) -> (i64, i64)) -> Option<String> {
    let rest = line.strip_prefix('{')?;
    let (a, rest) = rest.split_once('}')?;
    let rest = rest.strip_prefix('{')?;
    let (b, text) = rest.split_once('}')?;
    let (a, b) = (a.trim().parse::<i64>().ok()?, b.trim().parse::<i64>().ok()?);
    if (a, b) == (1, 1) && text.trim().parse::<f64>().is_ok() {
        return None;
    }
    let ms = |frames: i64| (frames as f64 * 1000.0 / MICRODVD_FPS).round() as i64;
    let frames = |ms: i64| (ms as f64 * MICRODVD_FPS / 1000.0).round() as i64;
    let (s, e) = f((ms(a), ms(b)));
    Some(format!("{{{}}}{{{}}}{text}", frames(s), frames(e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use SubtitleFormat::*;

    #[test]
    fn srt_cues_are_read_and_only_their_times_rewritten() {
        let srt = "1\r\n00:00:10,000 --> 00:00:12,500\r\nsync me\r\n\r\n2\r\n01:00:00,5 --> 01:00:01,000\r\nlater --> text\r\n";
        assert_eq!(
            cue_spans(srt, Srt),
            [(10_000, 12_500), (3_600_500, 3_601_000)]
        );
        let out = retime_cues(srt, Srt, |i, (s, e)| {
            (s - 11_000 * (1 - i as i64), e - 11_000)
        });
        assert_eq!(
            out,
            "1\r\n00:00:00,000 --> 00:00:01,500\r\nsync me\r\n\r\n2\r\n01:00:00,500 --> 00:59:50,000\r\nlater --> text\r\n"
        );
    }

    #[test]
    fn vtt_keeps_its_cue_settings_and_dot_separator() {
        let vtt = "WEBVTT\n\n00:01.000 --> 00:02.000 align:start\nhi\n";
        assert_eq!(cue_spans(vtt, Vtt), [(1000, 2000)]);
        assert_eq!(
            retime_cues(vtt, Vtt, |_, (s, e)| (s + 500, e + 500)),
            "WEBVTT\n\n00:00:01.500 --> 00:00:02.500 align:start\nhi\n"
        );
    }

    #[test]
    fn ass_reads_the_format_line_and_keeps_text_commas() {
        let ass = "[Script Info]\nTitle: x\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:10.50,0:00:12.00,Default,,0,0,0,,Hello, world\nComment: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,note\n";
        assert_eq!(cue_spans(ass, Ass), [(10_500, 12_000)]);
        assert_eq!(
            retime_cues(ass, Ass, |_, (s, e)| (s - 10_000, e - 10_000)),
            ass.replace("0:00:10.50,0:00:12.00", "0:00:00.50,0:00:02.00")
        );
    }

    #[test]
    fn microdvd_counts_frames_at_30_fps_and_skips_the_fps_header() {
        let sub = "{1}{1}25\n{30}{60}one\n";
        assert_eq!(cue_spans(sub, Sub), [(1000, 2000)]);
        assert_eq!(
            retime_cues(sub, Sub, |_, (s, e)| (s + 1000, e + 1000)),
            "{1}{1}25\n{60}{90}one\n"
        );
    }
}

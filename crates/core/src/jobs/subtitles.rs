//! Embedded text subtitle extraction and the burn-in `.ass`: argv, the ASS to WebVTT conversion
//! and the libass filter. [ts:modules/streaming/{subtitle-stream.service,subtitle-vtt.util,subtitle-burn-in.service}.ts]
use super::v;
use crate::api::{BurnInKind, MediaInfo};
use crate::jsnum::{is_space, js_string, number, to_fixed};
use std::collections::HashMap;

/// Cap on one extraction (a full container read).
pub const EXTRACT_TIMEOUT_MS: u64 = 15 * 60_000;
pub const BURN_IN_TIMEOUT_MS: u64 = 30_000;
/// The host rejects a stream index above this.
pub const MAX_STREAM_INDEX: u32 = 999;

/// ffmpeg writing each stream as ASS to its own file in one run: the container read dominates the cost.
/// `-analyzeduration 0 -probesize 200000` trusts the stored stream info.
pub fn extract_args(path: &str, outputs: &[(u32, &str)]) -> Vec<String> {
    let mut a = v(&[
        "-analyzeduration",
        "0",
        "-probesize",
        "200000",
        "-vn",
        "-an",
        "-i",
    ]);
    a.push(path.into());
    for (idx, raw) in outputs {
        a.extend(v(&["-map"]));
        a.push(format!("0:{idx}"));
        a.extend(v(&["-c:s", "ass", "-f", "ass", "-y"]));
        a.push((*raw).into());
    }
    a
}

/// Streams the extraction applies to: every non-bitmap track.
pub fn text_stream_indices(info: &MediaInfo) -> Vec<u32> {
    info.subtitles
        .iter()
        .filter(|s| !s.is_image_based)
        .map(|s| s.stream_index)
        .collect()
}

/// ffmpeg can exit 0 having written nothing; a track with no cues still has its ASS header.
pub fn check_extracted(len: u64, raw: &str) -> Result<(), String> {
    if len == 0 {
        return Err(format!("ffmpeg wrote an empty subtitle file: \"{raw}\""));
    }
    Ok(())
}

/// ffmpeg writing the embedded text track `stream_index` as ASS to `out` for a burn-in.
pub fn burn_in_extract_args(video_path: &str, stream_index: u32, out: &str) -> Vec<String> {
    let mut a = v(&["-y", "-i"]);
    a.push(video_path.into());
    a.push("-map".into());
    a.push(format!("0:{stream_index}"));
    a.extend(v(&["-c:s", "ass"]));
    a.push(out.into());
    a
}

pub fn burn_in_file_name(media_file_id: u32, stream_index: u32) -> String {
    format!("{media_file_id}-{stream_index}.ass")
}

/// What the burn-in filter reads: an extracted or sidecar file, or a stream of the video itself.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BurnInSource {
    #[serde(rename = "type")]
    pub kind: BurnInKind,
    pub video_path: String,
    #[serde(default)]
    pub subtitle_path: Option<String>,
    #[serde(default)]
    pub stream_index: Option<u32>,
    /// Source time cue 0 of a text file sits at (the container start).
    #[serde(default)]
    pub cue_offset_seconds: Option<f64>,
}

fn escape(p: &str) -> String {
    p.replace('\\', "\\\\")
        .replace(':', "\\:")
        .replace('\'', "'\\''")
}

/// Node's `path.extname` on a POSIX path, lowercased.
fn extname(p: &str) -> String {
    let base = p.rsplit('/').next().unwrap_or(p);
    match base.rfind('.') {
        Some(i) if i > 0 && base != ".." => base[i..].to_lowercase(),
        _ => String::new(),
    }
}

/// Seconds as a signed `setpts` term, microsecond precision.
fn pts_term(sign: f64, seconds: f64) -> String {
    let x = sign * seconds;
    let abs = number(&to_fixed(x.abs(), 6)).unwrap_or(f64::NAN);
    format!("{}{}/TB", if x < 0.0 { '-' } else { '+' }, js_string(abs))
}

/// `filter` rendered against cue time: under `-copyts` frames carry source PTS, which libass would read as cue time.
pub fn at_cue_time(filter: &str, cue_offset_seconds: f64) -> String {
    if cue_offset_seconds == 0.0 {
        return filter.into();
    }
    format!(
        "setpts=PTS{},{filter},setpts=PTS{}",
        pts_term(-1.0, cue_offset_seconds),
        pts_term(1.0, cue_offset_seconds)
    )
}

/// The `-vf` step of a text subtitle; `None` for an image one (overlaid, no filter).
pub fn burn_in_filter(s: &BurnInSource) -> Option<String> {
    if s.kind != BurnInKind::Text {
        return None;
    }
    if let Some(p) = &s.subtitle_path {
        let e = escape(p);
        let f = match extname(p).as_str() {
            ".ass" | ".ssa" => format!("ass='{e}'"),
            _ => format!("subtitles='{e}'"),
        };
        return Some(at_cue_time(&f, s.cue_offset_seconds.unwrap_or(0.0)));
    }
    s.stream_index
        .map(|i| format!("subtitles='{}':si={i}", escape(&s.video_path)))
}

/// SSA v4 `\a` / style numbering to the numpad layout `\an` uses.
fn ssa_to_numpad(n: i64) -> Option<i64> {
    Some(match n {
        1 => 1,
        2 => 2,
        3 => 3,
        5 => 7,
        6 => 8,
        7 => 9,
        9 => 4,
        10 => 5,
        11 => 6,
        _ => return None,
    })
}

/// Numpad alignment to WebVTT cue settings, as percentages every renderer maps the same way.
fn cue_settings(an: Option<i64>) -> String {
    let Some(an) = an.filter(|n| (1..=9).contains(n)) else {
        return String::new();
    };
    let mut s: Vec<&str> = vec![];
    if an >= 7 {
        s.push("line:5%");
    } else if an >= 4 {
        s.push("line:45%");
    }
    if an % 3 == 1 {
        s.extend(["position:10%", "align:left"]);
    } else if an % 3 == 0 {
        s.extend(["position:90%", "align:right"]);
    }
    if s.is_empty() {
        String::new()
    } else {
        format!(" {}", s.join(" "))
    }
}

/// `{...}` blocks of `text`: the regex `\{[^}]*\}`, as (start, end) byte ranges.
fn blocks(text: &str) -> Vec<(usize, usize)> {
    let mut out = vec![];
    let mut i = 0;
    while let Some(o) = text[i..].find('{') {
        let s = i + o;
        match text[s..].find('}') {
            Some(c) => {
                out.push((s, s + c + 1));
                i = s + c + 1;
            }
            None => break,
        }
    }
    out
}

fn digits(b: &[u8], from: usize) -> usize {
    b[from.min(b.len())..]
        .iter()
        .take_while(|c| c.is_ascii_digit())
        .count()
}

/// Last alignment override in a cue's blocks, as a numpad value: `\anN` or SSA `\aN`.
fn inline_alignment(text: &str) -> Option<i64> {
    let mut an = None;
    for (s, e) in blocks(text) {
        let b = &text.as_bytes()[s..e];
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'\\' && b.get(i + 1) == Some(&b'a') {
                if b.get(i + 2) == Some(&b'n') && digits(b, i + 3) > 0 {
                    an = Some(i64::from(b[i + 3] - b'0'));
                    i += 4;
                    continue;
                }
                let n = digits(b, i + 2);
                if n > 0 {
                    an = std::str::from_utf8(&b[i + 2..i + 2 + n])
                        .ok()
                        .and_then(|d| d.parse::<i64>().ok())
                        .and_then(ssa_to_numpad);
                    i += 2 + n;
                    continue;
                }
            }
            i += 1;
        }
    }
    an
}

/// Override block to the b/i/u tags it toggles; everything else is dropped.
fn overrides_to_tags(block: &str, open: &mut Vec<char>) -> String {
    let b = block.as_bytes();
    let (mut out, mut i) = (String::new(), 0);
    while i < b.len() {
        if b[i] == b'\\' && matches!(b.get(i + 1), Some(b'b' | b'i' | b'u')) {
            let n = digits(b, i + 2);
            if n > 0 {
                let tag = b[i + 1] as char;
                let on = &block[i + 2..i + 2 + n] != "0";
                if on && !open.contains(&tag) {
                    open.push(tag);
                    out.push_str(&format!("<{tag}>"));
                } else if !on && open.contains(&tag) {
                    open.retain(|t| *t != tag);
                    out.push_str(&format!("</{tag}>"));
                }
                i += 2 + n;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// `H:MM:SS.CC` to `HH:MM:SS.CC0`; empty when it does not read so.
fn ass_time(t: &str) -> String {
    let t = t.trim_matches(is_space);
    let all = |s: &str, n: Option<usize>| {
        !s.is_empty() && n.is_none_or(|n| s.len() == n) && s.bytes().all(|c| c.is_ascii_digit())
    };
    let p: Vec<&str> = t.split(':').collect();
    if let [h, m, rest] = p[..]
        && let [s, cs] = rest.split('.').collect::<Vec<_>>()[..]
        && all(h, None)
        && all(m, Some(2))
        && all(s, Some(2))
        && all(cs, Some(2))
    {
        return format!("{h:0>2}:{m}:{s}.{cs}0");
    }
    String::new()
}

/// JS `parseInt(s, 10)`; `None` is NaN.
fn parse_int(s: &str) -> Option<i64> {
    let t = s.trim_start_matches(is_space);
    let (neg, d) = t
        .strip_prefix('-')
        .map_or((false, t.strip_prefix('+').unwrap_or(t)), |d| (true, d));
    let n = d.bytes().take_while(u8::is_ascii_digit).count();
    let x = d[..n].parse::<f64>().ok()?;
    Some((if neg { -x } else { x }) as i64)
}

fn strip_tags(s: &str) -> String {
    let (mut out, mut rest) = (String::new(), s);
    while let Some(o) = rest.find('<') {
        match rest[o..].find('>') {
            Some(c) => {
                out.push_str(&rest[..o]);
                rest = &rest[o + c + 1..];
            }
            None => break,
        }
    }
    out + rest
}

fn convert_text(text: &str) -> String {
    let mut open: Vec<char> = vec![];
    let (mut body, mut last) = (String::new(), 0);
    for (s, e) in blocks(text) {
        body.push_str(&text[last..s]);
        body.push_str(&overrides_to_tags(&text[s + 1..e - 1], &mut open));
        last = e;
    }
    body.push_str(&text[last..]);
    let mut out = String::new();
    let mut it = body.chars().peekable();
    while let Some(c) = it.next() {
        match (c, it.peek()) {
            ('\\', Some('n' | 'N')) => {
                it.next();
                out.push('\n');
            }
            ('\\', Some('h')) => {
                it.next();
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    for t in open {
        out.push_str(&format!("</{t}>"));
    }
    out.trim_matches(is_space).to_string()
}

/// ASS to WebVTT. Reads field positions from the `Format:` lines, so any column order works.
pub fn ass_to_vtt(ass: &str) -> String {
    let mut style_align: HashMap<String, Option<i64>> = HashMap::new();
    let (mut section, mut style_format, mut event_format) = (String::new(), vec![], vec![]);
    let mut cues: Vec<String> = vec![];
    let col = |f: &[String], name: &str| f.iter().position(|x| x == name);

    for raw in ass.strip_prefix('\u{feff}').unwrap_or(ass).split('\n') {
        let line = raw.trim_matches(is_space);
        if line.starts_with('[') {
            section = line.to_lowercase();
            continue;
        }
        let Some(sep) = line.find(':') else { continue };
        let key = line[..sep].trim_matches(is_space).to_lowercase();
        let value = &line[sep + 1..];
        let f: Vec<&str> = value.split(',').collect();

        if key == "format" {
            let fields = f
                .iter()
                .map(|x| x.trim_matches(is_space).to_lowercase())
                .collect();
            if section.contains("styles") {
                style_format = fields;
            } else if section == "[events]" {
                event_format = fields;
            }
        } else if key == "style" && !style_format.is_empty() {
            let name = col(&style_format, "name")
                .and_then(|i| f.get(i))
                .map(|s| s.trim_matches(is_space))
                .filter(|s| !s.is_empty());
            let align = col(&style_format, "alignment")
                .and_then(|i| f.get(i))
                .and_then(|s| parse_int(s));
            if let (Some(name), Some(align)) = (name, align) {
                let legacy = section == "[v4 styles]";
                style_align.insert(
                    name.into(),
                    if legacy {
                        ssa_to_numpad(align)
                    } else {
                        Some(align)
                    },
                );
            }
        } else if key == "dialogue" && !event_format.is_empty() {
            let Some(text_idx) = col(&event_format, "text") else {
                continue;
            };
            let text = f.get(text_idx..).map_or(String::new(), |t| t.join(","));
            let time = |name: &str| {
                ass_time(
                    col(&event_format, name)
                        .and_then(|i| f.get(i))
                        .unwrap_or(&""),
                )
            };
            let (start, end) = (time("start"), time("end"));
            if start.is_empty() || end.is_empty() {
                continue;
            }
            let body = convert_text(&text);
            if strip_tags(&body).trim_matches(is_space).is_empty() {
                continue;
            }
            let style = col(&event_format, "style")
                .and_then(|i| f.get(i))
                .map(|s| {
                    let s = s.trim_matches(is_space);
                    s.strip_prefix('*').unwrap_or(s)
                })
                .unwrap_or("");
            let an = inline_alignment(&text).or_else(|| style_align.get(style).copied().flatten());
            cues.push(format!("{start} --> {end}{}\n{body}", cue_settings(an)));
        }
    }
    format!("WEBVTT\n\n{}\n", cues.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::SubtitleStreamInfo;

    #[test]
    fn zero_bytes_is_the_only_failed_extraction() {
        assert!(check_extracted(1, "r").is_ok());
        assert_eq!(
            check_extracted(0, "/t/r").unwrap_err(),
            "ffmpeg wrote an empty subtitle file: \"/t/r\""
        );
    }

    #[test]
    fn bitmap_tracks_are_not_extracted() {
        let s = |i, img| SubtitleStreamInfo {
            stream_index: i,
            codec: String::new(),
            language: String::new(),
            language_tag: None,
            title: None,
            forced: false,
            hearing_impaired: false,
            is_image_based: img,
        };
        let info = MediaInfo {
            subtitles: vec![s(2, false), s(3, true), s(4, false)],
            ..Default::default()
        };
        assert_eq!(text_stream_indices(&info), [2, 4]);
    }
}

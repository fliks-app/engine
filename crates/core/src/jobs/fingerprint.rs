//! Intro fingerprint extraction: the fpcalc/ffmpeg argv, the audio window and the fpcalc output.
//! Matching stays out. [ts:modules/markers/intro-detection.service.ts:815-940]
use crate::api::Fingerprint;
use crate::jsnum::{js_string, parse_float};
use crate::strs;

/// The host's `execFile` timeout of the head read and the kill timer of the window pipeline.
pub const TIMEOUT_MS: u64 = 180_000;
/// `introMaxLookupMinutes` default.
pub const DEFAULT_MAX_LOOKUP_MINUTES: f64 = 10.0;
pub const NO_FINGERPRINT: &str = "fpcalc returned no FINGERPRINT line";

/// Seconds of audio fingerprinted: the setting in minutes (NaN, 0 or unset gives the default).
pub fn lookup_seconds(setting_minutes: Option<f64>) -> f64 {
    setting_minutes
        .filter(|m| *m != 0.0 && !m.is_nan())
        .unwrap_or(DEFAULT_MAX_LOOKUP_MINUTES)
        * 60.0
}

/// Where an episode's outro is searched: the last `lookup` seconds, none without a known duration.
pub fn outro_window(duration: f64, lookup: f64) -> Option<(f64, f64)> {
    (duration != 0.0 && !duration.is_nan()).then(|| ((duration - lookup).max(0.0), lookup))
}

/// The head of the file (an intro), read by fpcalc itself.
pub fn head_args(path: &str, max_seconds: f64) -> Vec<String> {
    let mut a = strs(&["-raw", "-length"]);
    a.push(js_string(max_seconds));
    a.push(path.into());
    a
}

/// ffmpeg writing `[start, start + length]` of the audio as mono 22.05 kHz WAV on stdout, for
/// `window_fpcalc_args`. fpcalc has no start flag, so the seek is whole seconds.
pub fn window_ffmpeg_args(path: &str, start: f64, length: f64) -> Vec<String> {
    let mut a = strs(&["-nostdin", "-hide_banner", "-loglevel", "error", "-ss"]);
    a.push(js_string(start.floor().max(0.0)));
    a.push("-i".into());
    a.push(path.into());
    a.push("-t".into());
    a.push(js_string(length));
    a.extend(strs(&["-vn", "-ac", "1", "-ar", "22050", "-f", "wav", "-"]));
    a
}

pub fn window_fpcalc_args(length: f64) -> Vec<String> {
    let mut a = strs(&["-raw", "-length"]);
    a.push(js_string(length));
    a.push("-".into());
    a
}

/// The error of a window pipeline whose fpcalc exited non-zero.
pub fn window_failure(code: i32, fpcalc_stderr: &str, ffmpeg_stderr: &str) -> String {
    let (a, b) = (fpcalc_stderr.trim(), ffmpeg_stderr.trim());
    format!("fpcalc exited {code}: {}", if a.is_empty() { b } else { a })
}

/// The text after the first `key` followed by at least one char `ok` accepts, up to the first that does not.
fn capture<'a>(text: &'a str, key: &str, ok: impl Fn(char) -> bool) -> Option<&'a str> {
    text.match_indices(key).find_map(|(i, _)| {
        let rest = &text[i + key.len()..];
        let n = rest.find(|c| !ok(c)).unwrap_or(rest.len());
        (n > 0).then(|| &rest[..n])
    })
}

/// JS `parseInt(s, 10) | 0`.
fn int32(s: &str) -> i32 {
    let (neg, digits) = match s.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, s),
    };
    let n = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    if n == 0 {
        return 0;
    }
    let x = digits[..n].parse::<f64>().unwrap_or(f64::INFINITY);
    let x = if neg { -x } else { x };
    if !x.is_finite() {
        return 0;
    }
    let m = x.rem_euclid(4_294_967_296.0);
    (if m >= 2_147_483_648.0 {
        m - 4_294_967_296.0
    } else {
        m
    }) as i32
}

/// `fpcalc -raw` stdout as a fingerprint; `fallback_seconds` stands for a missing `DURATION=` line.
pub fn parse_fpcalc(
    stdout: &str,
    fallback_seconds: f64,
    base_offset_sec: f64,
) -> Result<Fingerprint, String> {
    let fp = capture(stdout, "FINGERPRINT=", |c| {
        c.is_ascii_digit() || c == ',' || c == '-'
    })
    .ok_or(NO_FINGERPRINT)?;
    let duration = capture(stdout, "DURATION=", |c| c.is_ascii_digit() || c == '.')
        .map_or(fallback_seconds, |d| {
            parse_float(d).unwrap_or(fallback_seconds)
        });
    Ok(Fingerprint {
        hashes: fp.split(',').map(int32).collect(),
        duration_seconds: duration,
        base_offset_sec,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_is_minutes_with_a_default() {
        assert_eq!(lookup_seconds(None), 600.0);
        assert_eq!(lookup_seconds(Some(0.0)), 600.0);
        assert_eq!(lookup_seconds(Some(f64::NAN)), 600.0);
        assert_eq!(lookup_seconds(Some(3.0)), 180.0);
    }

    #[test]
    fn outro_window_is_the_tail_and_needs_a_duration() {
        assert_eq!(outro_window(1500.0, 600.0), Some((900.0, 600.0)));
        assert_eq!(outro_window(300.0, 600.0), Some((0.0, 600.0)));
        assert_eq!(outro_window(0.0, 600.0), None);
    }
}

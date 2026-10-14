//! JavaScript number semantics the host's probe relies on.

/// `\s` of a JS regex.
pub fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// `\w` of a JS regex: `\b` is ASCII-only there.
pub fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Length of the decimal literal `[+-]?(d+[.d*]|.d+)([eE][+-]?d+)?` at the start of `s`.
fn decimal_prefix(s: &str) -> usize {
    let b = s.as_bytes();
    let mut i = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let digits = |from: usize| b[from..].iter().take_while(|c| c.is_ascii_digit()).count();
    let int = digits(i);
    i += int;
    let mut frac = 0;
    if b.get(i) == Some(&b'.') {
        frac = digits(i + 1);
        if int > 0 || frac > 0 {
            i += 1 + frac;
        }
    }
    if int == 0 && frac == 0 {
        return 0;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        let mut j = i + 1 + usize::from(matches!(b.get(i + 1), Some(b'+' | b'-')));
        let exp = digits(j);
        j += exp;
        if exp > 0 {
            i = j;
        }
    }
    i
}

/// `Number(s)`; `None` for NaN and the infinities (JSON would write them as `null`).
pub fn number(s: &str) -> Option<f64> {
    let t = s.trim_matches(|c: char| is_space(c) || c == '\u{2028}');
    if t.is_empty() {
        return Some(0.0);
    }
    if decimal_prefix(t) != t.len() {
        return None;
    }
    t.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// `Number.parseFloat(s)`: the longest numeric prefix.
pub fn parse_float(s: &str) -> Option<f64> {
    let t = s.trim_start_matches(is_space);
    t[..decimal_prefix(t)].parse::<f64>().ok()
}

/// `Math.round`: half up, toward +infinity.
pub fn round(x: f64) -> f64 {
    let f = x.floor();
    if x - f >= 0.5 { f + 1.0 } else { f }
}

/// `x.toFixed(3)` for x >= 0: exact decimal expansion, ties rounded up (Rust rounds them to even).
pub fn to_fixed3(x: f64) -> String {
    let long = format!("{x:.40}");
    let rest = long.split_once('.').map_or("", |(_, f)| &f[3..]);
    if rest.starts_with('5') && rest[1..].bytes().all(|b| b == b'0') {
        // A tie is exact in binary, so x * 1000 is exactly n + 0.5.
        let n = (x * 1000.0).floor() as u64 + 1;
        return format!("{}.{:03}", n / 1000, n % 1000);
    }
    format!("{x:.3}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(number("0.000000"), Some(0.0));
        assert_eq!(number(" 12 "), Some(12.0));
        assert_eq!(number("1e3"), Some(1000.0));
        assert_eq!(number(".5"), Some(0.5));
        assert_eq!(number("5."), Some(5.0));
        assert_eq!(number(""), Some(0.0));
        assert_eq!(number("N/A"), None);
        assert_eq!(number("12abc"), None);
        assert_eq!(number("inf"), None);
        assert_eq!(parse_float("12.5,K"), Some(12.5));
        assert_eq!(parse_float("N/A"), None);
        assert_eq!(parse_float("-0.5x"), Some(-0.5));
    }

    #[test]
    fn rounding() {
        assert_eq!(round(2.5), 3.0);
        assert_eq!(round(-2.5), -2.0);
        assert_eq!(round(0.499_999_999_999_999_94), 0.0);
        assert_eq!(to_fixed3(23.976_023_976), "23.976");
        assert_eq!(to_fixed3(0.0625), "0.063");
        assert_eq!(to_fixed3(24.0), "24.000");
    }
}

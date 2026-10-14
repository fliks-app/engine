//! JS number semantics the host's text and math depend on: parsing, rounding, printing.

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

/// JS `String(x)`: shortest round-trip digits, exponent form below 1e-6 and from 1e21.
pub fn js_string(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x == 0.0 {
        return "0".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if x < 0.0 {
        return format!("-{}", js_string(-x));
    }
    let e = format!("{x:e}");
    let (m, exp) = e.split_once('e').expect("exponent form");
    let digits: String = m.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    let n = exp.parse::<i32>().expect("exponent") + 1;
    if k <= n && n <= 21 {
        digits + &"0".repeat((n - k) as usize)
    } else if 0 < n && n <= 21 {
        let (a, b) = digits.split_at(n as usize);
        format!("{a}.{b}")
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let rest = if k == 1 {
            String::new()
        } else {
            format!(".{}", &digits[1..])
        };
        let sign = if n > 0 { '+' } else { '-' };
        format!("{}{rest}e{sign}{}", &digits[..1], (n - 1).abs())
    }
}

/// `x.toFixed(f)`: exact decimal expansion, ties away from zero (Rust rounds exact ties to even).
pub fn to_fixed(x: f64, f: usize) -> String {
    if !x.is_finite() || x.abs() >= 1e21 {
        return js_string(x);
    }
    let sign = if x < 0.0 { "-" } else { "" };
    let a = x.abs();
    // A double's exact decimal expansion needs at most 52 - exponent fractional digits.
    let p = if a >= 1e-9 { 90 } else { 1100 };
    let exact = format!("{a:.p$}");
    let frac = exact.split_once('.').map_or("", |(_, r)| r);
    let tie = frac[f..].starts_with('5') && frac[f + 1..].bytes().all(|b| b == b'0');
    let s = if tie {
        // Round the truncated value up by one unit in the last place.
        let mut d: Vec<u8> = exact[..exact.len() - (p - f)].bytes().collect();
        if f == 0 {
            d.pop();
        }
        let mut i = d.len();
        loop {
            if i == 0 {
                d.insert(0, b'1');
                break;
            }
            i -= 1;
            match d[i] {
                b'.' => continue,
                b'9' => d[i] = b'0',
                c => {
                    d[i] = c + 1;
                    break;
                }
            }
        }
        String::from_utf8(d).unwrap()
    } else {
        format!("{a:.f$}")
    };
    format!("{sign}{s}")
}

/// `Math.round`: halves toward +infinity.
pub fn round(x: f64) -> f64 {
    let f = x.floor();
    if x - f >= 0.5 { f + 1.0 } else { f }
}

/// `formatSeconds`: microsecond precision, then the shortest text.
pub fn seconds(x: f64) -> String {
    js_string(to_fixed(x, 6).parse::<f64>().unwrap_or(f64::NAN))
}

/// `Math.min`: NaN wins.
pub fn min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        a.min(b)
    }
}

/// `Math.max`: NaN wins.
pub fn max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        a.max(b)
    }
}

/// JS truthiness of a number.
pub fn truthy(x: f64) -> bool {
    x != 0.0 && !x.is_nan()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings() {
        for (x, s) in [
            (1.0, "1"),
            (-0.0, "0"),
            (0.1, "0.1"),
            (1e21, "1e+21"),
            (123456789012345680000.0, "123456789012345680000"),
            (1.5e-7, "1.5e-7"),
            (0.000001, "0.000001"),
            (2.5, "2.5"),
            (1e-7, "1e-7"),
            (3.003, "3.003"),
            (f64::NAN, "NaN"),
            (-3.25, "-3.25"),
            (-2.5, "-2.5"),
        ] {
            assert_eq!(js_string(x), s);
        }
        assert_eq!(to_fixed(-0.0001, 3), "-0.000");
        assert_eq!(to_fixed(-0.0, 3), "0.000");
        assert_eq!(to_fixed(1e21, 3), "1e+21");
        assert_eq!(to_fixed(2.0005, 3), "2.001");
    }

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
        assert_eq!(to_fixed(23.976_023_976, 3), "23.976");
        assert_eq!(to_fixed(0.0625, 3), "0.063");
        assert_eq!(to_fixed(24.0, 3), "24.000");
    }

    #[test]
    fn fixed_and_seconds() {
        assert_eq!(to_fixed(0.0078125, 6), "0.007813");
        assert_eq!(to_fixed(2.5, 0), "3");
        assert_eq!(to_fixed(9.5, 0), "10");
        assert_eq!(to_fixed(-0.0000001, 6), "-0.000000");
        assert_eq!(to_fixed(1.005, 2), "1.00");
        assert_eq!(seconds(-0.0000001), "0");
        assert_eq!(seconds(2.9999), "2.9999");
        assert_eq!(seconds(7.0 * 3.003), "21.021");
    }

    #[test]
    fn to_fixed_bounded_precision_matches_full_expansion() {
        let full = |x: f64, f: usize| {
            let e = format!("{:.1100}", x.abs());
            let frac = e.split_once('.').map_or("", |(_, r)| r);
            frac[f..].starts_with('5') && frac[f + 1..].bytes().all(|b| b == b'0')
        };
        let mut x = 1e-9_f64;
        while x < 1e12 {
            for v in [x, x * 1.000_000_7, x + 0.0005, x + 0.125] {
                for f in 0..=6 {
                    let tie = full(v, f);
                    let e = format!("{:.90}", v.abs());
                    let frac = e.split_once('.').map_or("", |(_, r)| r);
                    let bounded =
                        frac[f..].starts_with('5') && frac[f + 1..].bytes().all(|b| b == b'0');
                    assert_eq!(tie, bounded, "{v} {f}");
                }
            }
            x *= 1.37;
        }
    }
}

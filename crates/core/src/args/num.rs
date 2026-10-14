//! JavaScript number-to-text semantics: every number in an argv is printed the way the host prints it.

/// `String(x)`: shortest round-trip digits, exponent form below 1e-6 and from 1e21.
pub fn js(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x == 0.0 {
        return "0".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    let sign = if x < 0.0 { "-" } else { "" };
    let e = format!("{:e}", x.abs());
    let (mant, exp) = e.split_once('e').unwrap();
    let digits = mant.replace('.', "");
    let (k, n) = (digits.len() as i32, exp.parse::<i32>().unwrap() + 1);
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let es = if e >= 0 {
            format!("+{e}")
        } else {
            e.to_string()
        };
        match k {
            1 => format!("{digits}e{es}"),
            _ => format!("{}.{}e{es}", &digits[..1], &digits[1..]),
        }
    };
    format!("{sign}{body}")
}

/// `x.toFixed(f)`: exact decimal expansion, ties away from zero (Rust rounds exact ties to even).
pub fn to_fixed(x: f64, f: usize) -> String {
    if !x.is_finite() || x.abs() >= 1e21 {
        return js(x);
    }
    let sign = if x < 0.0 { "-" } else { "" };
    let a = x.abs();
    let exact = format!("{a:.1100}");
    let frac = exact.split_once('.').map_or("", |(_, r)| r);
    let tie = frac[f..].starts_with('5') && frac[f + 1..].bytes().all(|b| b == b'0');
    let s = if tie {
        // Round the truncated value up by one unit in the last place.
        let mut d: Vec<u8> = exact[..exact.len() - (1100 - f)].bytes().collect();
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
    js(to_fixed(x, 6).parse::<f64>().unwrap_or(f64::NAN))
}

/// `parseBitrateToBps`: `8M`, `500k`, `192k` or bare bps; anything else is 0.
pub fn parse_bitrate(s: &str) -> f64 {
    let t = s.trim();
    let (num, unit) = match t.char_indices().last() {
        Some((i, c)) if "kKmM".contains(c) => (t[..i].trim_end(), Some(c.to_ascii_lowercase())),
        _ => (t, None),
    };
    let (int, frac) = num.split_once('.').unwrap_or((num, "0"));
    let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    if !digits(int) || !digits(frac) {
        return 0.0;
    }
    let n: f64 = num.parse().unwrap_or(0.0);
    match unit {
        Some('m') => round(n * 1e6),
        Some('k') => round(n * 1e3),
        _ => round(n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_text() {
        assert_eq!(js(1e-7), "1e-7");
        assert_eq!(js(1.5e-7), "1.5e-7");
        assert_eq!(js(0.000001), "0.000001");
        assert_eq!(js(1e21), "1e+21");
        assert_eq!(js(123456789012345680000.0), "123456789012345680000");
        assert_eq!(js(-2.5), "-2.5");
        assert_eq!(js(-0.0), "0");
        assert_eq!(js(3.003), "3.003");
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
    fn bitrates() {
        assert_eq!(parse_bitrate("8M"), 8e6);
        assert_eq!(parse_bitrate(" 192k "), 192e3);
        assert_eq!(parse_bitrate("1.5 M"), 1.5e6);
        assert_eq!(parse_bitrate("8000000"), 8e6);
        assert_eq!(parse_bitrate("8MB"), 0.0);
        assert_eq!(parse_bitrate(".5M"), 0.0);
    }
}

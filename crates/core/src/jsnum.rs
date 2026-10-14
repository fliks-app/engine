//! JS number semantics the host's text and math depend on.
pub use crate::probe::js::round;

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

/// JS `x.toFixed(3)`, any sign.
pub fn to_fixed3(x: f64) -> String {
    if !x.is_finite() || x.abs() >= 1e21 {
        return js_string(x);
    }
    if x < 0.0 {
        return format!("-{}", crate::probe::js::to_fixed3(-x));
    }
    crate::probe::js::to_fixed3(x + 0.0)
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
            (f64::NAN, "NaN"),
            (-3.25, "-3.25"),
        ] {
            assert_eq!(js_string(x), s);
        }
        assert_eq!(to_fixed3(-0.0001), "-0.000");
        assert_eq!(to_fixed3(-0.0), "0.000");
        assert_eq!(to_fixed3(1e21), "1e+21");
        assert_eq!(to_fixed3(2.0005), "2.001");
    }
}

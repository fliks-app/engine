//! Shared by the decide fixture and fuzz tests.
use serde_json::Value;

/// JSON equality where `180` and `180.0` are the same number (JS has one number type).
pub fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
        }
        _ => a == b,
    }
}

/// Paths where `a` and `b` differ, as `path: got != expected`, compact.
pub fn diff(path: &str, a: &Value, b: &Value, out: &mut Vec<String>) {
    if same(a, b) {
        return;
    }
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for k in x.keys().chain(y.keys().filter(|k| !x.contains_key(*k))) {
                let (p, null) = (format!("{path}.{k}"), Value::Null);
                match (x.get(k), y.get(k)) {
                    (Some(p1), Some(p2)) => diff(&p, p1, p2, out),
                    (Some(p1), None) => out.push(format!("{p}: got {p1}, expected absent")),
                    (None, p2) => {
                        out.push(format!("{p}: got absent, expected {}", p2.unwrap_or(&null)))
                    }
                }
            }
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
            for (i, (p, q)) in x.iter().zip(y).enumerate() {
                diff(&format!("{path}[{i}]"), p, q, out);
            }
        }
        _ => out.push(format!("{path}: got {a}, expected {b}")),
    }
}

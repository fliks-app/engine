//! Matroska per-track statistics tags (`BPS`, `DURATION`, `NUMBER_OF_FRAMES`, `NUMBER_OF_BYTES`).
use super::build::clock_seconds;
use super::ffjson::Tags;
use crate::jsnum::number;

/// Largest relative gap between `BPS` and `NUMBER_OF_BYTES * 8 / DURATION` of a coherent set.
const BPS_TOLERANCE: f64 = 0.05;

/// The four tags are all present and agree: a stale set (kept through an edit) fails the check.
pub fn has_statistics_tags(tags: &Tags) -> bool {
    let get = |k: &str| tags.get_localised(k).map(str::trim);
    let (Some(bps), Some(duration), Some(frames), Some(bytes)) = (
        get("BPS").and_then(number),
        get("DURATION").and_then(clock_seconds),
        get("NUMBER_OF_FRAMES").and_then(number),
        get("NUMBER_OF_BYTES").and_then(number),
    ) else {
        return false;
    };
    if bps <= 0.0 || duration <= 0.0 || frames < 1.0 || bytes <= 0.0 {
        return false;
    }
    (bytes * 8.0 / duration - bps).abs() / bps <= BPS_TOLERANCE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(json: &str) -> Tags {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn a_coherent_set_passes_with_or_without_a_language_suffix() {
        let plain = r#"{"BPS":"800","DURATION":"00:00:10.000000000","NUMBER_OF_FRAMES":"240","NUMBER_OF_BYTES":"1000"}"#;
        let suffixed = plain.replace("\"BPS\"", "\"BPS-eng\"");
        assert!(has_statistics_tags(&tags(plain)));
        assert!(has_statistics_tags(&tags(&suffixed)));
    }

    #[test]
    fn a_missing_or_inconsistent_set_fails() {
        let missing = r#"{"BPS":"800","DURATION":"00:00:10.000000000","NUMBER_OF_FRAMES":"240"}"#;
        let stale = r#"{"BPS":"800","DURATION":"00:00:20.000000000","NUMBER_OF_FRAMES":"240","NUMBER_OF_BYTES":"1000"}"#;
        let zero = r#"{"BPS":"800","DURATION":"00:00:10.000000000","NUMBER_OF_FRAMES":"0","NUMBER_OF_BYTES":"1000"}"#;
        for t in [missing, stale, zero, "{}"] {
            assert!(!has_statistics_tags(&tags(t)), "{t}");
        }
    }
}

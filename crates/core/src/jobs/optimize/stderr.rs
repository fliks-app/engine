//! How ffmpeg's stderr is read: decode errors, failure classes and network drops.
use crate::api::JobErrorClass;

/// Decoding errors per hour of film below which a readable source is `damaged`, not `corrupt`.
const DAMAGED_ERRORS_PER_HOUR: f64 = 10.0;

/// Decoding errors tolerated before `damaged` becomes `corrupt`.
fn damaged_limit(duration_seconds: f64) -> u32 {
    (DAMAGED_ERRORS_PER_HOUR * duration_seconds.max(0.0) / 3600.0).ceil() as u32
}

/// An ffmpeg stderr line that reports a damaged stream; timestamp complaints are benign.
pub fn is_decode_error(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    if l.contains("non monotonically increasing dts")
        || l.contains("non-monotonic dts")
        || l.contains("timestamps are unset")
        || l.contains("past duration too large")
    {
        return false;
    }
    [
        "error while decoding",
        "invalid data found",
        "corrupt",
        "error submitting packet",
        "invalid nal",
        "missing reference picture",
        "concealing",
        "truncated",
        "decode_slice_header",
        "cabac",
        "invalid frame size",
        "unable to decode",
    ]
    .iter()
    .any(|p| l.contains(p))
}

/// The class of a failed ffmpeg run from the end of its stderr.
pub fn classify_stderr(lines: &[String]) -> JobErrorClass {
    let t = lines.join("\n").to_ascii_lowercase();
    let has = |ps: &[&str]| ps.iter().any(|p| t.contains(p));
    if has(&["no space left on device"]) {
        JobErrorClass::Nospace
    } else if has(&["input/output error"]) {
        JobErrorClass::Io
    } else if has(&[
        "permission denied",
        "read-only file system",
        "no such file or directory",
    ]) {
        JobErrorClass::Access
    } else if has(&[
        "moov atom not found",
        "ebml header parsing failed",
        "invalid data found when processing input",
    ]) {
        JobErrorClass::Corrupt
    } else {
        JobErrorClass::Engine
    }
}

/// Whether an ffmpeg failure text is a dropped or refused HTTP read.
pub fn is_network_error(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    [
        "connection reset",
        "connection timed out",
        "connection refused",
        "connection closed",
        "network is unreachable",
        "server returned",
        "http error",
        "end of file",
        "error in the pull function",
        "broken pipe",
    ]
    .iter()
    .any(|p| t.contains(p))
}

/// The `damaged` or `corrupt` failure of a source that showed `errors` decoding errors.
pub fn decode_failure(errors: u32, duration_seconds: f64, excerpt: &str) -> String {
    let class = if errors > damaged_limit(duration_seconds) {
        JobErrorClass::Corrupt
    } else {
        JobErrorClass::Damaged
    };
    class.message(format!("{errors} decoding errors: {excerpt}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dropped_http_read_is_a_network_error() {
        assert!(is_network_error(
            "ffmpeg exited with Some(1): Connection reset by peer"
        ));
        assert!(is_network_error("Server returned 503 Service Unavailable"));
        assert!(!is_network_error(
            "Invalid data found when processing input"
        ));
    }

    #[test]
    fn stderr_is_read_for_errors_and_classes() {
        assert!(is_decode_error(
            "[hevc @ 0x1] Error while decoding stream #0:0: Invalid data found"
        ));
        assert!(!is_decode_error(
            "Application provided invalid, non monotonically increasing dts to muxer"
        ));
        let l = |s: &str| vec![s.to_string()];
        assert_eq!(
            classify_stderr(&l("moov atom not found")),
            JobErrorClass::Corrupt
        );
        assert_eq!(
            classify_stderr(&l("No space left on device")),
            JobErrorClass::Nospace
        );
        assert_eq!(
            classify_stderr(&l("encoder crashed")),
            JobErrorClass::Engine
        );
        let m = decode_failure(3, 3600.0, "x");
        assert!(m.starts_with("damaged: "), "{m}");
        assert_eq!(damaged_limit(7200.0), 20);
        assert!(decode_failure(11, 3600.0, "x").starts_with("corrupt: "));
    }
}

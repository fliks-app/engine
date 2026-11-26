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

/// ffmpeg's teardown lines: they follow the cause of a failure and say nothing about it.
const TEARDOWN: [&str; 6] = [
    "terminating thread with return code",
    "task finished with error code",
    "nothing was written into output file",
    "error sending frames to consumers",
    "could not open encoder before eof",
    "conversion failed",
];

/// The last `n` lines of a failed run that explain it, ffmpeg's teardown lines left out.
pub fn failure_excerpt(lines: &[String], n: usize) -> String {
    let all: Vec<&str> = lines
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    let causes: Vec<&str> = all
        .iter()
        .copied()
        .filter(|l| {
            let l = l.to_ascii_lowercase();
            !TEARDOWN.iter().any(|t| l.contains(t))
        })
        .collect();
    let kept = if causes.is_empty() { all } else { causes };
    kept[kept.len().saturating_sub(n)..].join(" ")
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
    fn the_excerpt_keeps_the_cause_not_the_teardown() {
        let lines: Vec<String> = [
            "[matroska @ 0x1] Subtitle codec none (0) is not supported.",
            "[out#0/matroska @ 0x2] Could not write header (incorrect codec parameters ?): Function not implemented",
            "[vf#0:0 @ 0x3] Error sending frames to consumers: Function not implemented",
            "[vf#0:0 @ 0x3] Task finished with error code: -38 (Function not implemented)",
            "[vf#0:0 @ 0x3] Terminating thread with return code -38 (Function not implemented)",
            "[out#0/matroska @ 0x2] Nothing was written into output file, because at least one of its streams received no packets.",
            "",
        ]
        .map(String::from)
        .into();
        let e = failure_excerpt(&lines, 2);
        assert!(e.starts_with("[matroska @ 0x1] Subtitle codec none"), "{e}");
        assert!(e.contains("Could not write header"), "{e}");
        let only_teardown = vec!["Conversion failed!".to_string()];
        assert_eq!(failure_excerpt(&only_teardown, 2), "Conversion failed!");
        assert_eq!(failure_excerpt(&[], 2), "");
    }

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

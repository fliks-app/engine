//! Whether this ffmpeg pauses on stdin `p`/`u`, from frame counts sampled during a paced run.
//! [ts:backend/src/modules/streaming/transcoding/ffmpeg-pause.ts:44-117]
use crate::api::Platform;

/// A cheap rawvideo run paced in real time (`-re`) with progress on stderr; the run is killed after
/// the verdict. Whole probe gives up after 12 s.
pub const PROBE_ARGS: &[&str] = &[
    "-hide_banner",
    "-loglevel",
    "error",
    "-nostats",
    "-re",
    "-f",
    "lavfi",
    "-i",
    "testsrc=size=320x240:rate=30",
    "-t",
    "600",
    "-c:v",
    "rawvideo",
    "-progress",
    "pipe:2",
    "-f",
    "null",
    "-",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseVerdict {
    /// Still producing frames after `p`: the key does nothing.
    Ignored,
    /// Frozen: send `u` and judge the resumed run with [`pause_step`] again.
    Frozen,
    Works,
    Stalled,
}

/// Step 1 (`after_pause` = frames 1.5 s after the settle point): frozen or ignored.
/// Step 2 (`after_resume` = frames 1 s after `u`): resumed or stalled.
pub fn pause_step(settle: u32, later: u32, resumed: bool) -> PauseVerdict {
    match (resumed, later > settle + 3, later > settle) {
        (false, true, _) => PauseVerdict::Ignored,
        (false, false, _) => PauseVerdict::Frozen,
        (true, _, true) => PauseVerdict::Works,
        (true, _, false) => PauseVerdict::Stalled,
    }
}

/// stdin when the probe confirmed it, else SIGSTOP/SIGCONT, which Windows lacks.
pub fn pause_capability(stdin_works: bool, platform: Platform) -> &'static str {
    match (stdin_works, platform) {
        (true, _) => "stdin",
        (false, Platform::Win32) => "none",
        (false, _) => "signal",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_then_resumed_is_stdin() {
        assert_eq!(pause_step(27, 27, false), PauseVerdict::Frozen);
        assert_eq!(pause_step(27, 57, true), PauseVerdict::Works);
        assert_eq!(pause_step(27, 27, true), PauseVerdict::Stalled);
    }

    #[test]
    fn small_drift_still_counts_as_frozen() {
        assert_eq!(pause_step(27, 30, false), PauseVerdict::Frozen);
        assert_eq!(pause_step(27, 31, false), PauseVerdict::Ignored);
    }

    #[test]
    fn fallback_depends_on_platform() {
        assert_eq!(pause_capability(false, Platform::Linux), "signal");
        assert_eq!(pause_capability(false, Platform::Darwin), "signal");
        assert_eq!(pause_capability(false, Platform::Win32), "none");
        assert_eq!(pause_capability(true, Platform::Win32), "stdin");
    }
}

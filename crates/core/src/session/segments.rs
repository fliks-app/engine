//! The segments a run produced (the in-memory production frontier) and how a request for one
//! is answered. [ts:transcoding/segment-utils.ts] [ts:streaming.controller.ts:2506-2545]
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Segment numbers present for one run's cache dir (flat and `0/` layouts alike).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "Vec<u32>", into = "Vec<u32>")]
pub struct SegmentSet(BTreeSet<u32>);

impl From<Vec<u32>> for SegmentSet {
    fn from(v: Vec<u32>) -> Self {
        Self(v.into_iter().collect())
    }
}
impl From<SegmentSet> for Vec<u32> {
    fn from(s: SegmentSet) -> Self {
        s.0.into_iter().collect()
    }
}

/// Look-ahead of [`SegmentSet::first_missing`] before it gives up.
pub const MAX_LOOKAHEAD: u32 = 2000;

impl SegmentSet {
    pub fn insert(&mut self, seg: u32) {
        self.0.insert(seg);
    }

    pub fn contains(&self, seg: u32) -> bool {
        self.0.contains(&seg)
    }

    /// The segment or its predecessor is there.
    pub fn nearby(&self, seg: u32) -> bool {
        self.contains(seg) || seg > 0 && self.contains(seg - 1)
    }

    /// A segment exists in `[from - lookback, from]`: the encoder is at most `lookback` behind.
    pub fn within_reach(&self, from: u32, lookback: u32) -> bool {
        self.0
            .range(from.saturating_sub(lookback)..=from)
            .next()
            .is_some()
    }

    /// First number at or after `from` that is absent; `None` after [`MAX_LOOKAHEAD`] present ones.
    pub fn first_missing(&self, from: u32) -> Option<u32> {
        (from..from.saturating_add(MAX_LOOKAHEAD)).find(|s| !self.contains(*s))
    }

    pub fn latest(&self) -> Option<u32> {
        self.0.last().copied()
    }

    /// A restart at `from` drops every segment numbered `>= from` (its timeline restarts there).
    pub fn purge_from(&mut self, from: u32) {
        self.0.split_off(&from);
    }
}

/// A parsed segment file name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentName {
    Init(Option<u32>),
    Segment(u32),
}

/// `init.mp4`, `init_<n>.mp4`, `seg-<n>.m4s|ts` with any number of digits. [new]
pub fn parse_segment_name(name: &str) -> Option<SegmentName> {
    let digits = |s: &str| {
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
    };
    if name == "init.mp4" {
        return Some(SegmentName::Init(None));
    }
    if let Some(n) = name
        .strip_prefix("init_")
        .and_then(|r| r.strip_suffix(".mp4"))
    {
        return digits(n).map(|n| SegmentName::Init(Some(n)));
    }
    let n = name
        .strip_prefix("seg-")
        .and_then(|r| r.strip_suffix(".m4s").or_else(|| r.strip_suffix(".ts")))?;
    digits(n).map(SegmentName::Segment)
}

pub const SEGMENT_WAIT_MS: u32 = 60_000;
/// Early companion: the main run covers the same files behind it.
pub const EARLY_WAIT_MS: u32 = 10_000;
/// Segments the early companion writes. [ts:transcoding/constants.ts:43]
pub const EARLY_PROBE_SEGMENTS: u32 = 2;
/// How long an init file gets to become non-empty.
pub const INIT_NON_EMPTY_WAIT_MS: u32 = 5_000;
pub const RETRY_AFTER_SECONDS: u32 = 2;

/// What the wait for a segment file found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SegmentProbe {
    /// The file landed within the wait.
    pub landed: bool,
    /// Size at serve time; `None` when it vanished (a restart removed it).
    pub size: Option<u64>,
    /// The run still produces (the audio route passes `exitCode === null`).
    pub producing: bool,
    /// The run was killed by a seek or a rung switch.
    pub intentionally_killed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum SegmentResponse {
    Serve,
    /// `Retry-After: retry_after_s`: players treat 503 as retryable, 404 as fatal.
    Unavailable {
        retry_after_s: u32,
    },
    NotFound,
}

pub fn segment_response(p: &SegmentProbe) -> SegmentResponse {
    let retry = SegmentResponse::Unavailable {
        retry_after_s: RETRY_AFTER_SECONDS,
    };
    if !p.landed {
        return if p.producing || p.intentionally_killed {
            retry
        } else {
            SegmentResponse::NotFound
        };
    }
    match p.size {
        Some(n) if n > 0 => SegmentResponse::Serve,
        _ => retry,
    }
}

/// A `?sid=` the registry no longer knows is `410` (`session_expired`); no sid passes.
pub fn sid_expired(sid_present: bool, known: bool) -> bool {
    sid_present && !known
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(v: &[u32]) -> SegmentSet {
        v.to_vec().into()
    }

    #[test]
    fn set_queries() {
        let s = set(&[10, 11, 12, 40]);
        assert!(s.nearby(13) && s.nearby(10) && !s.nearby(14) && !s.nearby(9));
        assert!(!SegmentSet::default().nearby(0));
        assert!(s.within_reach(27, 15) && !s.within_reach(28, 15) && !s.within_reach(5, 15));
        assert!(s.within_reach(12, 0) && !s.within_reach(13, 0));
        assert_eq!(s.first_missing(10), Some(13));
        assert_eq!(s.first_missing(13), Some(13));
        assert_eq!(s.latest(), Some(40));
        let full = SegmentSet::from((0..2500).collect::<Vec<_>>());
        assert_eq!(full.first_missing(0), None);
        assert_eq!(full.first_missing(1000), Some(2500));
    }

    #[test]
    fn purge_keeps_below() {
        let mut s = set(&[1, 2, 3, 9]);
        s.purge_from(3);
        assert_eq!(Vec::<u32>::from(s), vec![1, 2]);
    }

    #[test]
    fn names() {
        use SegmentName::*;
        assert_eq!(parse_segment_name("seg-0007.m4s"), Some(Segment(7)));
        assert_eq!(parse_segment_name("seg-12345.ts"), Some(Segment(12345)));
        assert_eq!(parse_segment_name("seg-7.m4s"), Some(Segment(7)));
        assert_eq!(parse_segment_name("init.mp4"), Some(Init(None)));
        assert_eq!(parse_segment_name("init_2.mp4"), Some(Init(Some(2))));
        for bad in [
            "seg-.m4s",
            "seg-1.mp4",
            "init_.mp4",
            "../seg-1.m4s",
            "seg-1.m4s.tmp",
            "seg-a.ts",
        ] {
            assert_eq!(parse_segment_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn responses() {
        let p = |landed, size, producing, killed| {
            segment_response(&SegmentProbe {
                landed,
                size,
                producing,
                intentionally_killed: killed,
            })
        };
        let retry = SegmentResponse::Unavailable { retry_after_s: 2 };
        assert_eq!(p(true, Some(10), false, false), SegmentResponse::Serve);
        assert_eq!(p(true, Some(0), false, false), retry);
        assert_eq!(p(true, None, true, false), retry);
        assert_eq!(p(false, None, true, false), retry);
        assert_eq!(p(false, None, false, true), retry);
        assert_eq!(p(false, None, false, false), SegmentResponse::NotFound);
        assert!(sid_expired(true, false));
        assert!(!sid_expired(true, true) && !sid_expired(false, false));
    }
}

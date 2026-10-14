//! When to pause a run that is far ahead of its viewers, and how.
//! [ts:transcoding/ffmpeg-throttle.service.ts:25-225] [ts:transcoding/ffmpeg-pause.ts:15-40]
use super::{SegmentSet, SessionVariant};
use crate::decide::timeline::real_segment_seconds;
use serde::{Deserialize, Serialize};

/// Start time of segment `n` on the uniform grid, at the real frame rate.
pub fn segment_index_to_seconds(segment: u32, segment_duration: f64, fps: Option<f64>) -> f64 {
    if segment == 0 {
        0.0
    } else {
        f64::from(segment) * real_segment_seconds(segment_duration, fps)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThrottleDecision {
    Pause,
    Resume,
    Noop,
}

/// Pause past the threshold, resume at half of it (hysteresis).
pub fn decide_throttle(
    ahead_seconds: f64,
    paused: bool,
    threshold_seconds: f64,
) -> ThrottleDecision {
    if !paused && ahead_seconds > threshold_seconds {
        ThrottleDecision::Pause
    } else if paused && ahead_seconds <= threshold_seconds / 2.0 {
        ThrottleDecision::Resume
    } else {
        ThrottleDecision::Noop
    }
}

/// What a run produced itself, in content seconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunSpan {
    pub start_seconds: f64,
    pub frontier_seconds: f64,
}

/// A playhead behind the run's start says nothing about this run: it leads by nothing.
pub fn run_lead_seconds(span: &RunSpan, playhead: f64) -> f64 {
    if playhead < span.start_seconds {
        0.0
    } else {
        span.frontier_seconds - playhead
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Viewer {
    pub position: f64,
    pub last_requested_segment: Option<u32>,
    pub pinned: bool,
}

/// How a requested segment index becomes content seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub enum PlayheadGrid {
    Uniform {
        segment_duration: f64,
        fps: Option<f64>,
    },
    /// Remux: content-time start of each segment on the keyframe grid (0 past the end).
    Remux { boundaries: Vec<f64> },
}

impl PlayheadGrid {
    fn seconds(&self, segment: u32) -> f64 {
        match self {
            Self::Uniform {
                segment_duration,
                fps,
            } => segment_index_to_seconds(segment, *segment_duration, *fps),
            Self::Remux { boundaries } => boundaries.get(segment as usize).copied().unwrap_or(0.0),
        }
    }
}

/// Furthest-ahead viewer of a job: a slower sibling must never stall a faster one. `None`
/// when nobody watches (cleanup reaps the run).
pub fn job_playhead_seconds(viewers: &[Viewer], grid: &PlayheadGrid) -> Option<f64> {
    if viewers.is_empty() {
        return None;
    }
    Some(viewers.iter().fold(0.0, |max, v| {
        let requested = v.last_requested_segment.map_or(0.0, |s| grid.seconds(s));
        f64::max(f64::max(max, v.position), requested)
    }))
}

/// Never the bounded early companion, nor a run backing a pinned (download) session.
pub fn is_throttle_eligible(variant: Option<&SessionVariant>, viewers: &[Viewer]) -> bool {
    !matches!(variant, Some(SessionVariant::Early)) && !viewers.iter().any(|v| v.pinned)
}

/// A transcode run's own output: from its `-start_number` to the first segment it has yet to
/// write, never another run's islands in the same dir.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscodeRun {
    pub start_segment: u32,
    /// Where the last tick found the frontier; the scan resumes there.
    pub frontier_hint: Option<u32>,
    pub present: SegmentSet,
    pub segment_duration: f64,
    pub fps: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub enum RunProgress {
    Transcode(TranscodeRun),
    /// The assembler's own span, `None` before its first segment landed.
    Remux {
        start_seconds: Option<f64>,
        frontier_seconds: Option<f64>,
        boundaries: Vec<f64>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PauseCapability {
    Stdin,
    Signal,
    None,
}

impl PauseCapability {
    /// `hw::pause_capability` as a value; anything unknown pauses nothing.
    pub fn parse(s: &str) -> Self {
        match s {
            "stdin" => Self::Stdin,
            "signal" => Self::Signal,
            _ => Self::None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "via", rename_all = "lowercase")]
pub enum PauseCommand {
    /// The key written to ffmpeg's stdin.
    Stdin {
        key: char,
    },
    Signal {
        name: &'static str,
    },
    Nothing,
}

pub fn pause_command(cap: PauseCapability, pause: bool) -> PauseCommand {
    match (cap, pause) {
        (PauseCapability::Stdin, p) => PauseCommand::Stdin {
            key: if p { 'p' } else { 'u' },
        },
        (PauseCapability::Signal, true) => PauseCommand::Signal { name: "SIGSTOP" },
        (PauseCapability::Signal, false) => PauseCommand::Signal { name: "SIGCONT" },
        (PauseCapability::None, _) => PauseCommand::Nothing,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThrottleInput {
    pub capability: PauseCapability,
    pub enabled: bool,
    pub threshold_seconds: f64,
    pub paused: bool,
    pub process_exited: bool,
    pub variant: Option<SessionVariant>,
    /// Live sessions sharing the job (none without a base profile hash).
    pub viewers: Vec<Viewer>,
    pub run: RunProgress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThrottleOutcome {
    pub decision: ThrottleDecision,
    /// New frontier hint to remember (transcode only).
    pub frontier_segment: Option<u32>,
}

/// One tick for one run.
pub fn throttle_step(i: &ThrottleInput) -> ThrottleOutcome {
    let out = |decision, frontier_segment| ThrottleOutcome {
        decision,
        frontier_segment,
    };
    let resume_if_paused = || {
        out(
            if i.paused {
                ThrottleDecision::Resume
            } else {
                ThrottleDecision::Noop
            },
            None,
        )
    };
    if i.capability == PauseCapability::None {
        return out(ThrottleDecision::Noop, None);
    }
    if !i.enabled {
        return resume_if_paused();
    }
    if i.process_exited {
        return out(ThrottleDecision::Noop, None);
    }
    if !is_throttle_eligible(i.variant.as_ref(), &i.viewers) {
        return resume_if_paused();
    }
    let grid = match &i.run {
        RunProgress::Transcode(t) => PlayheadGrid::Uniform {
            segment_duration: t.segment_duration,
            fps: t.fps,
        },
        RunProgress::Remux { boundaries, .. } => PlayheadGrid::Remux {
            boundaries: boundaries.clone(),
        },
    };
    let Some(playhead) = job_playhead_seconds(&i.viewers, &grid) else {
        return out(ThrottleDecision::Noop, None);
    };
    let (span, frontier_segment) = match &i.run {
        RunProgress::Remux {
            start_seconds: Some(s),
            frontier_seconds: Some(f),
            ..
        } => (
            Some(RunSpan {
                start_seconds: *s,
                frontier_seconds: *f,
            }),
            None,
        ),
        RunProgress::Remux { .. } => (None, None),
        RunProgress::Transcode(t) => match t
            .present
            .first_missing(t.frontier_hint.unwrap_or(t.start_segment))
        {
            None => (None, None),
            Some(next) => {
                let span = (next != t.start_segment).then(|| RunSpan {
                    start_seconds: segment_index_to_seconds(
                        t.start_segment,
                        t.segment_duration,
                        t.fps,
                    ),
                    frontier_seconds: segment_index_to_seconds(next, t.segment_duration, t.fps),
                });
                (span, Some(next))
            }
        },
    };
    let decision = span.map_or(ThrottleDecision::Noop, |s| {
        decide_throttle(
            run_lead_seconds(&s, playhead),
            i.paused,
            i.threshold_seconds,
        )
    });
    out(decision, frontier_segment)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ThrottleDecision::*;

    #[test]
    fn hysteresis() {
        assert_eq!(decide_throttle(91.0, false, 90.0), Pause);
        assert_eq!(decide_throttle(90.0, false, 90.0), Noop);
        assert_eq!(decide_throttle(46.0, true, 90.0), Noop);
        assert_eq!(decide_throttle(45.0, true, 90.0), Resume);
        assert_eq!(decide_throttle(200.0, true, 90.0), Noop);
        assert_eq!(decide_throttle(60.0, false, 90.0), Noop);
    }

    #[test]
    fn lead_ignores_a_playhead_before_the_run() {
        let span = RunSpan {
            start_seconds: 1650.0,
            frontier_seconds: 1800.0,
        };
        assert_eq!(run_lead_seconds(&span, 1700.0), 100.0);
        assert_eq!(run_lead_seconds(&span, 1650.0), 150.0);
        assert_eq!(run_lead_seconds(&span, 1600.0), 0.0);
    }

    fn viewer(position: f64, seg: Option<u32>) -> Viewer {
        Viewer {
            position,
            last_requested_segment: seg,
            pinned: false,
        }
    }

    #[test]
    fn playhead_is_the_furthest_viewer() {
        let uni = PlayheadGrid::Uniform {
            segment_duration: 3.0,
            fps: None,
        };
        assert_eq!(job_playhead_seconds(&[], &uni), None);
        assert_eq!(
            job_playhead_seconds(&[viewer(40.0, None), viewer(100.0, None)], &uni),
            Some(100.0)
        );
        assert_eq!(
            job_playhead_seconds(&[viewer(5.0, Some(10))], &uni),
            Some(30.0)
        );
        let remux = PlayheadGrid::Remux {
            boundaries: vec![0.0, 10.0, 20.0, 30.0, 35.0],
        };
        assert_eq!(
            job_playhead_seconds(&[viewer(0.0, Some(4))], &remux),
            Some(35.0)
        );
    }

    #[test]
    fn eligibility() {
        let pinned = Viewer {
            pinned: true,
            ..viewer(0.0, None)
        };
        assert!(!is_throttle_eligible(Some(&SessionVariant::Early), &[]));
        assert!(!is_throttle_eligible(
            Some(&SessionVariant::Main),
            &[pinned]
        ));
        assert!(is_throttle_eligible(
            Some(&SessionVariant::Main),
            &[viewer(0.0, None)]
        ));
        assert!(is_throttle_eligible(None, &[]));
    }

    fn transcode(present: Vec<u32>, hint: Option<u32>, position: f64) -> ThrottleInput {
        ThrottleInput {
            capability: PauseCapability::Signal,
            enabled: true,
            threshold_seconds: 90.0,
            paused: false,
            process_exited: false,
            variant: Some(SessionVariant::Main),
            viewers: vec![viewer(position, None)],
            run: RunProgress::Transcode(TranscodeRun {
                start_segment: 275,
                frontier_hint: hint,
                present: present.into(),
                segment_duration: 6.0,
                fps: None,
            }),
        }
    }

    #[test]
    fn a_run_is_measured_by_its_own_unbroken_output() {
        let at = 275.0 * 6.0;
        let islands = transcode((310..=360).collect(), None, at);
        assert_eq!(
            throttle_step(&islands),
            ThrottleOutcome {
                decision: Noop,
                frontier_segment: Some(275)
            }
        );
        let short = transcode((275..=280).collect(), None, at);
        assert_eq!(throttle_step(&short).decision, Noop);
        let long = transcode((275..=300).collect(), None, at);
        assert_eq!(
            throttle_step(&long),
            ThrottleOutcome {
                decision: Pause,
                frontier_segment: Some(301)
            }
        );
        let unreported = transcode((275..=300).collect(), None, 0.0);
        assert_eq!(throttle_step(&unreported).decision, Noop);
    }

    #[test]
    fn disabled_or_ineligible_runs_resume() {
        let mut i = transcode((275..=300).collect(), None, 0.0);
        i.paused = true;
        i.enabled = false;
        assert_eq!(throttle_step(&i).decision, Resume);
        i.enabled = true;
        i.variant = Some(SessionVariant::Early);
        assert_eq!(throttle_step(&i).decision, Resume);
        i.process_exited = true;
        i.variant = Some(SessionVariant::Main);
        assert_eq!(throttle_step(&i).decision, Noop);
        i.capability = PauseCapability::None;
        i.process_exited = false;
        i.enabled = false;
        assert_eq!(throttle_step(&i).decision, Noop);
    }

    #[test]
    fn pause_by_capability() {
        assert_eq!(
            pause_command(PauseCapability::Stdin, true),
            PauseCommand::Stdin { key: 'p' }
        );
        assert_eq!(
            pause_command(PauseCapability::Stdin, false),
            PauseCommand::Stdin { key: 'u' }
        );
        assert_eq!(
            pause_command(PauseCapability::Signal, true),
            PauseCommand::Signal { name: "SIGSTOP" }
        );
        assert_eq!(
            pause_command(PauseCapability::Signal, false),
            PauseCommand::Signal { name: "SIGCONT" }
        );
        assert_eq!(
            pause_command(PauseCapability::None, true),
            PauseCommand::Nothing
        );
        assert_eq!(PauseCapability::parse("stdin"), PauseCapability::Stdin);
    }
}

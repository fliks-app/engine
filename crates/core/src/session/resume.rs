//! What a segment request does to an existing run: serve, wait, or restart at a segment.
//! [ts:transcoding/transcoding.service.ts:451-537]
use super::SegmentSet;
use serde::{Deserialize, Serialize};

/// Max gap (segments) between the encoder frontier and the request before restarting.
pub const SEEK_WAIT_THRESHOLD: u32 = 15;
pub const REMUX_MAX_WAIT_SEGMENTS: u32 = 3;
pub const REMUX_WAIT_BUFFER_SECONDS: f64 = 3.0;

/// The remux assembler's progress, for the track the request is for. [ts:transcoding/remux-assembler.ts:351-362]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemuxProgress {
    /// First segment of the run (the assembler's plan).
    pub start_segment: u32,
    /// Next segment to produce on that track; `None` before the run's first GOP landed.
    pub frontier: Option<u32>,
    /// Segments per second since open, pauses excluded; `None` before it is measured.
    pub segments_per_second: Option<f64>,
}

impl RemuxProgress {
    /// Servable now: a live run gets a throughput-sized wait window, an exited one a hard bound.
    pub fn can_serve(&self, segment: u32, exited: bool) -> bool {
        let start = self.start_segment;
        if exited {
            return self
                .frontier
                .is_some_and(|f| segment >= start && segment < f);
        }
        if segment < start {
            return false;
        }
        let max = f64::from(REMUX_MAX_WAIT_SEGMENTS);
        let window = self
            .segments_per_second
            .map_or(REMUX_MAX_WAIT_SEGMENTS, |r| {
                // JS Math.round: half rounds up (the rate is never negative).
                (r * REMUX_WAIT_BUFFER_SECONDS + 0.5)
                    .floor()
                    .clamp(1.0, max) as u32
            });
        match self.frontier {
            None => segment - start <= window,
            Some(f) => u64::from(segment) <= u64::from(f) + u64::from(window),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExistingRun {
    /// `None` while the process lives.
    pub exit_code: Option<i32>,
    /// Killed by a signal (exit code stays unset).
    pub signal: bool,
    /// Output still being packaged after the process exited.
    pub output_pending: bool,
    pub start_segment: u32,
    pub remux: bool,
    /// Absent on a remux run whose assembler is gone.
    pub assembler: Option<RemuxProgress>,
}

impl ExistingRun {
    pub fn is_producing(&self) -> bool {
        (self.exit_code.is_none() && !self.signal) || self.output_pending
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveInput {
    pub run: ExistingRun,
    pub requested_segment: u32,
    /// The request's quality is the run's quality.
    pub quality_match: bool,
    /// Segments the run's cache dir holds (unused for remux).
    pub present: SegmentSet,
}

/// The caller spawns again; `kill` stops a live run first, `start_segment` is the run's new
/// start (`None`: unchanged), `purge_cache` removes the crashed run's dir (never a remux one).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Respawn {
    pub kill: bool,
    pub start_segment: Option<u32>,
    pub purge_cache: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum ResumeAction {
    /// The segment is produced.
    Serve,
    /// Attach to the run: the segment is expected.
    Wait,
    Respawn(Respawn),
}

impl ResumeAction {
    fn restart_at(segment: u32) -> Self {
        Self::Respawn(Respawn {
            kill: true,
            start_segment: Some(segment),
            purge_cache: false,
        })
    }
}

pub fn resolve_existing(i: &ResolveInput) -> ResumeAction {
    let (run, seg) = (&i.run, i.requested_segment);
    let start = run.start_segment;
    let remux_serves = |exited| run.assembler.is_some_and(|a| a.can_serve(seg, exited));

    if run.exit_code == Some(0) && i.quality_match {
        let reachable = if run.remux {
            remux_serves(true)
        } else {
            i.present.nearby(seg)
        };
        if !reachable {
            return ResumeAction::Respawn(Respawn {
                kill: false,
                start_segment: Some(seg),
                purge_cache: false,
            });
        }
        let produced = if run.remux {
            remux_serves(true)
        } else {
            i.present.contains(seg)
        };
        return if produced {
            ResumeAction::Serve
        } else {
            ResumeAction::Wait
        };
    }

    if !run.is_producing() {
        // A clean exit here is another quality, not a crash: its cache stays a valid rung.
        let crashed = run.signal || run.exit_code.is_some_and(|c| c != 0);
        return ResumeAction::Respawn(Respawn {
            kill: false,
            start_segment: None,
            purge_cache: crashed && !run.remux,
        });
    }

    if !i.quality_match {
        return ResumeAction::Respawn(Respawn {
            kill: false,
            start_segment: None,
            purge_cache: false,
        });
    }

    if run.remux {
        return if remux_serves(false) {
            let produced = run
                .assembler
                .is_some_and(|a| a.frontier.is_some_and(|f| seg < f));
            if produced {
                ResumeAction::Serve
            } else {
                ResumeAction::Wait
            }
        } else {
            ResumeAction::restart_at(seg)
        };
    }

    if !i.present.nearby(seg) {
        // A run that wrote nothing yet reaches the segments just after its start.
        let run_starting =
            seg >= start && seg - start <= SEEK_WAIT_THRESHOLD && !i.present.within_reach(start, 0);
        return if run_starting || i.present.within_reach(seg, SEEK_WAIT_THRESHOLD) {
            ResumeAction::Wait
        } else {
            ResumeAction::restart_at(seg)
        };
    }

    match i.present.first_missing(seg) {
        Some(gap) if gap < start => ResumeAction::restart_at(gap),
        _ if i.present.contains(seg) => ResumeAction::Serve,
        _ => ResumeAction::Wait,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(start: u32) -> ExistingRun {
        ExistingRun {
            exit_code: None,
            signal: false,
            output_pending: false,
            start_segment: start,
            remux: false,
            assembler: None,
        }
    }

    fn go(run: ExistingRun, seg: u32, quality_match: bool, present: &[u32]) -> ResumeAction {
        resolve_existing(&ResolveInput {
            run,
            requested_segment: seg,
            quality_match,
            present: present.to_vec().into(),
        })
    }

    fn restart(start: Option<u32>, kill: bool) -> ResumeAction {
        ResumeAction::Respawn(Respawn {
            kill,
            start_segment: start,
            purge_cache: false,
        })
    }

    #[test]
    fn a_starting_run_is_waited_on_only_near_its_start() {
        assert_eq!(go(run(0), 0, true, &[]), ResumeAction::Wait);
        assert_eq!(go(run(0), 2, true, &[]), ResumeAction::Wait);
        assert_eq!(go(run(0), 15, true, &[]), ResumeAction::Wait);
        assert_eq!(go(run(10), 3, true, &[]), restart(Some(3), true));
        assert_eq!(go(run(0), 40, true, &[]), restart(Some(40), true));
    }

    #[test]
    fn buffer_ahead_waits_a_real_seek_restarts() {
        let present = [0, 1, 2, 3, 4, 5];
        assert_eq!(go(run(0), 5, true, &present), ResumeAction::Serve);
        assert_eq!(go(run(0), 6, true, &present), ResumeAction::Wait);
        assert_eq!(go(run(0), 20, true, &present), ResumeAction::Wait);
        assert_eq!(go(run(0), 21, true, &present), restart(Some(21), true));
    }

    #[test]
    fn a_cached_segment_behind_a_gap_restarts_at_the_gap() {
        assert_eq!(
            go(run(10), 5, true, &[5, 6, 10, 11]),
            restart(Some(7), true)
        );
        assert_eq!(go(run(5), 5, true, &[5, 6, 10, 11]), ResumeAction::Serve);
    }

    #[test]
    fn dead_runs_respawn_without_a_kill() {
        let mut r = run(0);
        r.signal = true;
        assert_eq!(
            go(r, 5, true, &[]),
            ResumeAction::Respawn(Respawn {
                kill: false,
                start_segment: None,
                purge_cache: true
            })
        );
        let mut r = run(0);
        r.exit_code = Some(1);
        r.remux = true;
        assert_eq!(go(r, 5, true, &[]), restart(None, false));
        // clean exit for another quality keeps its cache
        let mut r = run(0);
        r.exit_code = Some(0);
        assert_eq!(go(r.clone(), 0, false, &[0]), restart(None, false));
        assert_eq!(go(r.clone(), 0, true, &[0]), ResumeAction::Serve);
        assert_eq!(go(r, 9, true, &[0]), restart(Some(9), false));
    }

    #[test]
    fn another_quality_on_a_live_run_is_the_callers_kill() {
        assert_eq!(go(run(0), 0, false, &[0]), restart(None, false));
    }

    #[test]
    fn remux_reachability_is_the_assemblers() {
        let asm = |start, frontier| RemuxProgress {
            start_segment: start,
            frontier,
            segments_per_second: None,
        };
        let remux = |start, frontier, code| ExistingRun {
            remux: true,
            exit_code: code,
            assembler: Some(asm(start, frontier)),
            ..run(start)
        };
        assert_eq!(
            go(remux(0, Some(5), None), 3, true, &[]),
            ResumeAction::Serve
        );
        assert_eq!(
            go(remux(0, Some(5), None), 6, true, &[]),
            ResumeAction::Wait
        );
        assert_eq!(
            go(remux(10, Some(12), None), 3, true, &[3]),
            restart(Some(3), true)
        );
        assert_eq!(
            go(remux(0, Some(5), None), 65, true, &[]),
            restart(Some(65), true)
        );
        assert_eq!(
            go(remux(0, Some(5), Some(0)), 6, true, &[6]),
            restart(Some(6), false)
        );
        assert_eq!(
            go(remux(0, Some(5), Some(0)), 3, true, &[]),
            ResumeAction::Serve
        );
    }

    #[test]
    fn remux_wait_window_follows_throughput() {
        let a = |rate| RemuxProgress {
            start_segment: 0,
            frontier: Some(10),
            segments_per_second: rate,
        };
        assert!(a(Some(0.1)).can_serve(11, false) && !a(Some(0.1)).can_serve(12, false));
        assert!(a(Some(0.5)).can_serve(12, false) && !a(Some(0.5)).can_serve(13, false));
        assert!(a(Some(50.0)).can_serve(13, false) && !a(Some(50.0)).can_serve(14, false));
        assert!(!a(None).can_serve(14, false));
        let no_frontier = RemuxProgress {
            frontier: None,
            ..a(None)
        };
        assert!(no_frontier.can_serve(3, false) && !no_frontier.can_serve(4, false));
    }
}

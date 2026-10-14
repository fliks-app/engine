//! The remux assembler's decisions, bytes in, bytes out: where a run landed, when a GOP is whole,
//! the served init and per-track deltas, and audio grouping (`RemuxSegmentAssembler`). The engine
//! owns the files. [ts:ra:192-745]
use super::Mp4Error;
use super::boxes::{BoxRef, read_box_header, u32_at};
use super::remux::{
    RawFrame, RemuxAssemblyPlan, RemuxEdits, build_fragment, plan_boundary_seconds, remux_edits,
};
use super::timeline::{
    IdMap, big_int, first_tfdt, get, parse_init_tracks, parse_trex_defaults, read_init_edits,
    video_decode_extent, with_init_edits,
};
use crate::jsnum::{js_string, round, to_fixed};

/// Under a frame, above the output `-ss` rounding to a millisecond time base.
const KEYFRAME_MATCH_SECONDS: f64 = 0.002;

/// The run's video track and what adds to its tfdt to give source time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RunVideo {
    pub id: u32,
    pub timescale: u32,
    pub correction: f64,
    /// The same shift in decode time: valid at any keyframe whatever its pts-dts offset.
    pub dts_correction: f64,
    pub trex_default: u32,
}

/// A run once its first GOP landed. [ts:ra:436-518]
#[derive(Debug, Clone, PartialEq)]
pub struct OpenedRun {
    pub video: Option<RunVideo>,
    /// Run-local to source time.
    pub correction: f64,
    pub edits: RemuxEdits,
    /// Ticks added to each track's tfdt.
    pub deltas: IdMap<i128>,
    /// The served init: the run's own with one edit per track.
    pub init: Vec<u8>,
    /// Grid GOP index less ffmpeg's GOP number.
    pub gop_offset: i64,
    /// First segment the run assembles: past the plan's when it landed late.
    pub next: usize,
}

/// A non-video track's edit in seconds: its run's own encoder delay.
fn audio_edit_seconds(run_edits: &IdMap<i128>, id: u32, timescale: u32) -> f64 {
    (get(run_edits, id).unwrap_or(0).max(0) as f64) / f64::from(timescale)
}

/// `init` = the run's init, `moofs` = the first GOP file's `moof` boxes back to back.
pub fn open_run(
    plan: &RemuxAssemblyPlan,
    init: &[u8],
    moofs: &[u8],
) -> Result<OpenedRun, Mp4Error> {
    let tracks = parse_init_tracks(init)?;
    let run_edits = read_init_edits(init)?;
    let video_track = tracks.iter().find(|(_, t)| t.is_video).copied();
    let mut next = plan.start_segment;
    let mut gop_offset = 0;
    // locateRun: the output `-ss` as ffmpeg rounded it, from the first GOP's tfdt.
    let (correction, dts_correction) = match (&plan.gop_pts, &plan.gop_dts, &plan.first_gop) {
        (Some(pts), Some(dts), Some(first_gop)) => {
            let tfdt = match video_track {
                Some((id, _)) => first_tfdt(moofs, id)?,
                None => None,
            };
            let (Some((_, v)), Some(tfdt)) = (video_track, tfdt) else {
                return Err(Mp4Error::Msg("first GOP has no video fragment".into()));
            };
            let out = tfdt as f64 / f64::from(v.timescale);
            let at_pts = out + plan.seek_seconds;
            let Some(at) = pts
                .iter()
                .position(|p| (p - at_pts).abs() < KEYFRAME_MATCH_SECONDS)
            else {
                return Err(Mp4Error::Msg(format!(
                    "first GOP starts at {}s, on no source keyframe",
                    js_string(at_pts)
                )));
            };
            if at != plan.start_number {
                gop_offset = at as i64 - plan.start_number as i64;
                while next + 1 < first_gop.len() && first_gop[next] < at {
                    next += 1;
                }
            }
            (pts[at] - out, dts[at] - out)
        }
        _ => (plan.seek_seconds, plan.seek_seconds),
    };
    let trex = parse_trex_defaults(init)?;
    let video = video_track.map(|(id, t)| RunVideo {
        id,
        timescale: t.timescale,
        correction,
        dts_correction,
        trex_default: get(&trex, id).unwrap_or(0),
    });
    let run_video_edit = video_track.map_or(0.0, |(id, t)| {
        get(&run_edits, id).unwrap_or(0) as f64 / f64::from(t.timescale)
    });
    let edits = remux_edits(
        plan.start,
        plan.first_decode.unwrap_or(plan.start - run_video_edit),
    );
    let edit_of = |id: u32, timescale: u32, is_video: bool| {
        if is_video {
            edits.video
        } else {
            audio_edit_seconds(&run_edits, id, timescale)
        }
    };
    let mut deltas = vec![];
    for (id, t) in &tracks {
        let shift = big_int(round(
            (edit_of(*id, t.timescale, t.is_video) + edits.shift + correction)
                * f64::from(t.timescale),
        ))?;
        let mut d = shift - get(&run_edits, *id).unwrap_or(0);
        // A first frame at 0 whose derived decode time is a tick off.
        if let Some(start) = first_tfdt(moofs, *id)?
            && start + d < 0
        {
            d = -start;
        }
        deltas.push((*id, d));
    }
    let init = with_init_edits(init, |id, t| edit_of(id, t.timescale, t.is_video))?;
    Ok(OpenedRun {
        video,
        correction,
        edits,
        deltas,
        init,
        gop_offset,
        next,
    })
}

/// Whether grid GOP `gop`, whose file's moofs are `moofs`, decodes up to the next keyframe (the
/// last one: the video's end). [ts:ra:400-418]
pub fn gop_complete(
    plan: &RemuxAssemblyPlan,
    video: Option<&RunVideo>,
    gop: usize,
    moofs: &[u8],
) -> Result<bool, Mp4Error> {
    let (Some(dts), Some(bounds), Some(v)) = (&plan.gop_dts, &plan.boundaries, video) else {
        // Uniform fallback: the muxer's rename is the only completion signal.
        return Ok(true);
    };
    let Some(ext) = video_decode_extent(moofs, v.id, v.trex_default)? else {
        return Ok(false);
    };
    let ts = f64::from(v.timescale);
    let target = match dts.get(gop + 1) {
        Some(next) => round((next - v.dts_correction) * ts),
        None => round((bounds.last().copied().unwrap_or(f64::NAN) - v.correction) * ts),
    };
    Ok(ext.end + ext.last_duration / 2 >= big_int(target)?)
}

/// Whether `moof` opens grid GOP `g`'s video (`Ok(false)` without a video fragment); an error
/// when it opens a neighbour, whose samples a file matched by number alone would serve.
/// [ts:ra:557-575]
pub fn check_gop(
    plan: &RemuxAssemblyPlan,
    video: Option<&RunVideo>,
    gop_offset: i64,
    g: usize,
    moof: &[u8],
) -> Result<bool, Mp4Error> {
    let Some(v) = video else { return Ok(true) };
    let Some(tfdt) = first_tfdt(moof, v.id)? else {
        return Ok(false);
    };
    let Some(pts) = &plan.gop_pts else {
        return Ok(true);
    };
    let at = tfdt as f64 / f64::from(v.timescale) + v.correction;
    let off = |k: usize| (at - pts[k]).abs();
    let nearer = [g.checked_sub(1), Some(g + 1)]
        .into_iter()
        .flatten()
        .find(|&k| k < pts.len() && off(k) < off(g));
    match nearer {
        Some(k) => Err(Mp4Error::Msg(format!(
            "GOP file {} starts at {}s, on keyframe {k} ({}s), not the planned {g} ({}s)",
            g as i64 - gop_offset,
            to_fixed(at, 3),
            js_string(pts[k]),
            js_string(pts[g])
        ))),
        None => Ok(true),
    }
}

/// Complete top-level boxes of a file another process appends to, from `from`: a box mid-write
/// is not there yet; size 0 ("to the end") cannot be told complete. [ts:ra:870-896]
pub fn tail_boxes(buf: &[u8], from: usize) -> Vec<BoxRef> {
    let mut out = vec![];
    let mut start = from;
    while start + 8 <= buf.len() && u32_at(buf, start).is_ok_and(|s| s != 0) {
        let head = &buf[start..buf.len().min(start + 16)];
        let Some(b) = read_box_header(head, 0, start, buf.len()) else {
            break;
        };
        start += b.size;
        out.push(b);
    }
    out
}

/// Served segments a frame closed: `(index, bytes)`.
pub type Closed = Vec<(usize, Vec<u8>)>;

/// One audio-only rendition grouped onto the video's segments by each frame's decode time.
/// [ts:ra:157-190,602-745]
#[derive(Debug, Clone, PartialEq)]
pub struct AudioGroup {
    pub track_id: u32,
    pub timescale: u32,
    delta: i128,
    clamped: bool,
    correction_ticks: i128,
    /// Next served segment to close.
    pub served: usize,
    pending: Vec<RawFrame>,
}

/// The growing file's init (its boxes up to `moov`) once the video run opened: the group from
/// the video's first segment, and the served `init_<index>.mp4`.
pub fn open_audio(
    init: &[u8],
    index: usize,
    run: &OpenedRun,
) -> Result<(AudioGroup, Vec<u8>), Mp4Error> {
    let Some((track_id, info)) = parse_init_tracks(init)?.first().copied() else {
        return Err(Mp4Error::Msg(format!(
            "audio rendition {index} init has no parseable track"
        )));
    };
    let run_edits = read_init_edits(init)?;
    let ts = f64::from(info.timescale);
    let edit = audio_edit_seconds(&run_edits, track_id, info.timescale);
    let shift = big_int(round((edit + run.edits.shift + run.correction) * ts))?;
    let group = AudioGroup {
        track_id,
        timescale: info.timescale,
        delta: shift - get(&run_edits, track_id).unwrap_or(0),
        clamped: false,
        correction_ticks: big_int(round(run.correction * ts))?,
        served: run.next,
        pending: vec![],
    };
    Ok((group, with_init_edits(init, |_, _| edit)?))
}

impl AudioGroup {
    /// Segment `i`'s planned boundary in ticks: two runs never disagree on float noise.
    fn boundary_ticks(&self, plan: &RemuxAssemblyPlan, i: usize) -> Result<i128, Mp4Error> {
        big_int(round(
            plan_boundary_seconds(plan, i) * f64::from(self.timescale),
        ))
    }

    pub fn has_more(&self, plan: &RemuxAssemblyPlan) -> bool {
        plan.boundaries
            .as_ref()
            .is_none_or(|b| self.served + 1 < b.len())
    }

    /// Groups one frame; `None` once past the grid (the frame stays unread), else the segments
    /// it closed. A seeked run's leading priming before its first segment is dropped.
    pub fn push(
        &mut self,
        plan: &RemuxAssemblyPlan,
        frame: RawFrame,
    ) -> Result<Option<Closed>, Mp4Error> {
        if !self.clamped {
            if frame.tfdt + self.delta < 0 {
                self.delta = -frame.tfdt;
            }
            self.clamped = true;
        }
        if !self.has_more(plan) {
            return Ok(None);
        }
        let mut closed = vec![];
        let abs = frame.tfdt + self.correction_ticks;
        if self.served > 0 && abs < self.boundary_ticks(plan, self.served)? {
            return Ok(Some(closed));
        }
        while self.has_more(plan) && abs >= self.boundary_ticks(plan, self.served + 1)? {
            closed.extend(self.flush()?);
        }
        if self.has_more(plan) {
            self.pending.push(frame);
        }
        Ok(Some(closed))
    }

    /// The pending frames as segment `served` (none when empty), then the next segment.
    pub fn flush(&mut self) -> Result<Option<(usize, Vec<u8>)>, Mp4Error> {
        let frames = std::mem::take(&mut self.pending);
        let seg = self.served;
        self.served += 1;
        let Some(first) = frames.first() else {
            return Ok(None);
        };
        let samples: Vec<_> = frames.iter().map(|f| f.sample).collect();
        let data: Vec<&[u8]> = frames.iter().map(|f| f.data.as_slice()).collect();
        let built = build_fragment(
            self.track_id,
            seg as u32 + 1,
            first.tfdt + self.delta,
            &samples,
            &data,
        )?;
        Ok(Some((seg, built)))
    }
}

//! Remux run start, assembly plan and audio fragments (`remuxRunStart`, `remux-assembler.ts`).
use super::Mp4Error;
use super::boxes::{find_box, make_box, u32_at};
use super::grid::{DECODE_TIME_TOLERANCE_SECONDS, KeyframeGrid, served_shift};
use super::timeline::{
    RawSample, TRUN_DATA_OFFSET, TRUN_SAMPLE_CTS, TRUN_SAMPLE_DURATION, TRUN_SAMPLE_FLAGS,
    TRUN_SAMPLE_SIZE, read_tfdt, trun_samples,
};
use crate::api::{AudioCodec, AudioPlan};
use serde::{Deserialize, Serialize};

/// Packet grid (seconds) of an encoded track: packets start at `alignedAt - padding + k * frame`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PacketGrid {
    pub frame: f64,
    pub padding: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemuxRunStart {
    /// Source time encoded audio is aligned to.
    pub audio_start_seconds: f64,
    /// Input and output `-ss`; `None` for none.
    pub seek_seconds: Option<f64>,
    /// ffmpeg's number for its first GOP file.
    pub start_number: usize,
}

/// Seek to the segment's first keyframe dts, encoded audio on the packet a run from 0 puts there.
pub fn remux_run_start(
    grid: Option<&KeyframeGrid>,
    start_segment: usize,
    segment_duration: f64,
    origin: f64,
    audio: Option<PacketGrid>,
) -> Result<RemuxRunStart, Mp4Error> {
    let Some(grid) = grid else {
        let start = origin
            + if start_segment == 0 {
                0.0
            } else {
                start_segment as f64 * segment_duration
            };
        return Ok(RemuxRunStart {
            audio_start_seconds: start,
            seek_seconds: (start_segment > 0).then_some(start),
            start_number: start_segment,
        });
    };
    if start_segment >= grid.durations.len() {
        return Err(Mp4Error::Msg(format!(
            "remux segment {start_segment} is past the last one ({})",
            grid.durations.len() as i64 - 1
        )));
    }
    let kf = grid.first_keyframe[start_segment];
    let dts = grid.keyframes[kf].dts;
    if start_segment == 0 {
        return Ok(RemuxRunStart {
            audio_start_seconds: grid.boundaries[0],
            seek_seconds: Some(dts - DECODE_TIME_TOLERANCE_SECONDS),
            start_number: 0,
        });
    }
    // A float-noise sliver past a packet start must not skip that packet.
    let first = grid.boundaries[0];
    let audio_start = match audio {
        Some(a) => first + ((dts - first + a.padding) / a.frame - 1e-9).ceil() * a.frame,
        None => dts,
    };
    Ok(RemuxRunStart {
        audio_start_seconds: audio_start,
        seek_seconds: Some(dts),
        start_number: kf,
    })
}

/// Edits of the served video track, in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RemuxEdits {
    /// The reorder delay: a served segment's tfdt is its first frame.
    pub video: f64,
    /// Added to every source time: lifts a video starting before 0 onto 0.
    pub shift: f64,
}

pub fn remux_edits(start: f64, first_decode: f64) -> RemuxEdits {
    RemuxEdits {
        video: start - first_decode,
        shift: served_shift(start),
    }
}

/// Where a run's GOP files land on the grid; the session and GOP dirs stay with the caller.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemuxAssemblyPlan {
    pub start_segment: usize,
    pub start_number: usize,
    /// The run's output `-ss` (0 for none).
    pub seek_seconds: f64,
    /// GOP number of each segment's first GOP, then the GOP count; `None` on the uniform grid.
    pub first_gop: Option<Vec<usize>>,
    pub gop_pts: Option<Vec<f64>>,
    pub gop_dts: Option<Vec<f64>>,
    /// Source time the served timeline starts at.
    pub start: f64,
    /// Decode time of the first keyframe.
    pub first_decode: Option<f64>,
    pub boundaries: Option<Vec<f64>>,
    pub segment_duration: f64,
    /// Audio-only renditions sharing the run (0 for the inline layout).
    pub audio_renditions: u32,
}

pub fn remux_assembly_plan(
    grid: Option<&KeyframeGrid>,
    start_segment: usize,
    run: &RemuxRunStart,
    origin: f64,
    segment_duration: Option<f64>,
    audio_renditions: Option<u32>,
) -> RemuxAssemblyPlan {
    RemuxAssemblyPlan {
        start_segment,
        start_number: run.start_number,
        seek_seconds: run.seek_seconds.unwrap_or(0.0),
        first_gop: grid.map(|g| g.first_keyframe.clone()),
        gop_pts: grid.map(|g| g.keyframes.iter().map(|k| k.pts).collect()),
        gop_dts: grid.map(|g| g.keyframes.iter().map(|k| k.dts).collect()),
        start: grid.map_or(origin, |g| g.boundaries[0]),
        first_decode: grid.and_then(|g| g.keyframes.first()).map(|k| k.dts),
        boundaries: grid.map(|g| g.boundaries.clone()),
        segment_duration: segment_duration.unwrap_or(3.0),
        audio_renditions: audio_renditions.unwrap_or(0),
    }
}

/// Segment `i`'s planned source-time boundary: the grid's, or `segment_duration` steps from start.
pub fn plan_boundary_seconds(plan: &RemuxAssemblyPlan, i: usize) -> f64 {
    match &plan.boundaries {
        Some(b) => b
            .get(i.min(b.len().wrapping_sub(1)))
            .copied()
            .unwrap_or(f64::NAN),
        None => plan.start + i as f64 * plan.segment_duration,
    }
}

/// The GOP files of `segment` by ffmpeg number, `None` past the grid.
pub fn gops_of(plan: &RemuxAssemblyPlan, segment: usize) -> Option<Vec<usize>> {
    let Some(first) = &plan.first_gop else {
        return Some(vec![segment]);
    };
    if segment + 1 >= first.len() {
        return None;
    }
    Some((first[segment]..first[segment + 1]).collect())
}

/// Served segment file name.
pub fn seg_name(n: usize) -> String {
    format!("seg-{n:04}.m4s")
}

/// One audio frame of a growing `frag_every_frame` file.
#[derive(Debug, Clone, PartialEq)]
pub struct RawFrame {
    pub tfdt: i128,
    pub sample: RawSample,
    pub data: Vec<u8>,
}

fn body_start(b: &[u8]) -> Result<usize, Mp4Error> {
    Ok(if u32_at(b, 0)? == 1 { 16 } else { 8 })
}

/// One (moof, mdat) pair of a single-sample audio fragment.
pub fn parse_frame(moof: &[u8], mdat: &[u8]) -> Result<RawFrame, Mp4Error> {
    let body = body_start(moof)?;
    let traf = find_box(moof, body, moof.len(), b"traf");
    let tfdt = match &traf {
        Some(t) => read_tfdt(moof, t)?,
        None => None,
    };
    let (Some(traf), Some(tfdt)) = (traf, tfdt) else {
        return Err(Mp4Error::Msg(
            "audio fragment missing traf/tfhd/tfdt/trun".into(),
        ));
    };
    let samples = trun_samples(moof, &traf, 0)?;
    if samples.len() != 1 {
        return Err(Mp4Error::Msg(format!(
            "expected 1 sample per audio fragment (frag_every_frame), got {}",
            samples.len()
        )));
    }
    let data = mdat.get(body_start(mdat)?..).unwrap_or_default().to_vec();
    Ok(RawFrame {
        tfdt: tfdt.value,
        sample: samples[0],
        data,
    })
}

const TFHD_BASE_IS_MOOF: u32 = 0x02_0000;

fn fullbox(version: u32, flags: u32) -> [u8; 4] {
    ((version << 24) | flags).to_be_bytes()
}

/// One served audio segment: `samples` in a single moof/trun, then their data.
pub fn build_fragment(
    track_id: u32,
    sequence: u32,
    tfdt: i128,
    samples: &[RawSample],
    data: &[&[u8]],
) -> Result<Vec<u8>, Mp4Error> {
    let use_cts = samples.iter().any(|s| s.cts.is_some());
    // Version 0's cts is unsigned: a negative one only round-trips as version 1.
    let version = u32::from(samples.iter().any(|s| s.cts.unwrap_or(0) < 0));
    let mfhd = make_box(b"mfhd", &[&fullbox(0, 0), &sequence.to_be_bytes()])?;
    let tfhd = make_box(
        b"tfhd",
        &[&fullbox(0, TFHD_BASE_IS_MOOF), &track_id.to_be_bytes()],
    )?;
    let t = u64::try_from(tfdt).map_err(|_| Mp4Error::OutOfRange)?;
    let tfdt_box = make_box(b"tfdt", &[&fullbox(1, 0), &t.to_be_bytes()])?;
    let flags = TRUN_DATA_OFFSET
        | TRUN_SAMPLE_DURATION
        | TRUN_SAMPLE_SIZE
        | TRUN_SAMPLE_FLAGS
        | if use_cts { TRUN_SAMPLE_CTS } else { 0 };
    let build = |data_offset: u32| -> Result<Vec<u8>, Mp4Error> {
        let mut trun = Vec::with_capacity(12 + samples.len() * 16);
        trun.extend_from_slice(&fullbox(version, flags));
        trun.extend_from_slice(&(samples.len() as u32).to_be_bytes());
        trun.extend_from_slice(&data_offset.to_be_bytes());
        for s in samples {
            for v in [s.duration, s.size, s.flags] {
                trun.extend_from_slice(&v.to_be_bytes());
            }
            if use_cts {
                trun.extend_from_slice(&(s.cts.unwrap_or(0) as u32).to_be_bytes());
            }
        }
        let traf = make_box(b"traf", &[&tfhd, &tfdt_box, &make_box(b"trun", &[&trun])?])?;
        make_box(b"moof", &[&mfhd, &traf])
    };
    // `default_base_moof`: the data offset counts from the moof, whose size it does not change.
    let size = build(0)?.len() as u32;
    let mut out = build(size + 8)?;
    out.extend(make_box(b"mdat", data)?);
    Ok(out)
}

/// `remuxAudioGrid`: the packet grid of an encoded track at 48 kHz, `None` for a copy; no plan is
/// the AAC stereo default. [ts:ffmpeg-args.ts:1465-1470] [ts:audio-encode.ts:48-93]
pub fn remux_audio_grid(plan: Option<&AudioPlan>, libfdk_aac: bool) -> Option<PacketGrid> {
    let (codec, channels) = match plan {
        Some(AudioPlan::Copy { .. }) => return None,
        Some(AudioPlan::Transcode { codec, channels }) => (*codec, *channels),
        None => (AudioCodec::Aac, 2),
    };
    let (frame, padding) = match codec {
        AudioCodec::Aac if libfdk_aac && channels <= 6 => (1024.0, 2048.0),
        AudioCodec::Aac => (1024.0, 1024.0),
        AudioCodec::Opus => (960.0, 312.0),
        AudioCodec::Ac3 | AudioCodec::Eac3 => (1536.0, 256.0),
    };
    Some(PacketGrid {
        frame: frame / 48_000.0,
        padding: padding / 48_000.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_grid_follows_the_encoder() {
        let t = |codec, channels| AudioPlan::Transcode { codec, channels };
        let g = |p: Option<&AudioPlan>, fdk| remux_audio_grid(p, fdk).map(|g| (g.frame, g.padding));
        let k = 48_000.0;
        assert_eq!(g(None, false), Some((1024.0 / k, 1024.0 / k)));
        assert_eq!(
            g(Some(&t(AudioCodec::Aac, 2)), true),
            Some((1024.0 / k, 2048.0 / k))
        );
        assert_eq!(
            g(Some(&t(AudioCodec::Aac, 8)), true),
            Some((1024.0 / k, 1024.0 / k))
        );
        assert_eq!(
            g(Some(&t(AudioCodec::Opus, 2)), false),
            Some((960.0 / k, 312.0 / k))
        );
        assert_eq!(
            g(Some(&t(AudioCodec::Eac3, 6)), false),
            Some((1536.0 / k, 256.0 / k))
        );
        let copy = AudioPlan::Copy {
            codec: "aac".into(),
            channels: Some(2),
            bitrate_bps: None,
            profile: None,
        };
        assert_eq!(g(Some(&copy), false), None);
    }
}

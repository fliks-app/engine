//! Init tracks, edit lists and fragment decode times (`timeline.ts`).
use super::Mp4Error;
use super::boxes::{
    BoxRef, boxes, find_box, i32_at, is_one, latin1_eq, make_box, u24_at, u32_at, u64_at,
};
use crate::jsnum::round;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackInfo {
    pub timescale: u32,
    pub is_video: bool,
}

/// Entries in first-insertion order, a repeated key overwriting in place (a JS `Map`).
pub type IdMap<V> = Vec<(u32, V)>;

pub fn get<V: Copy>(m: &IdMap<V>, id: u32) -> Option<V> {
    m.iter().find(|(k, _)| *k == id).map(|(_, v)| *v)
}

fn set<V>(m: &mut IdMap<V>, id: u32, v: V) {
    match m.iter_mut().find(|(k, _)| *k == id) {
        Some(e) => e.1 = v,
        None => m.push((id, v)),
    }
}

/// tkhd: fullbox, creation and modification (4 bytes each in v0, 8 in v1), then track_ID.
fn tkhd_track_id(buf: &[u8], tkhd: &BoxRef) -> Result<u32, Mp4Error> {
    u32_at(
        buf,
        tkhd.payload_start
            + if is_one(buf, tkhd.payload_start) {
                20
            } else {
                12
            },
    )
}

/// `trackId -> { timescale, isVideo }` of an init's `moov`.
pub fn parse_init_tracks(init: &[u8]) -> Result<IdMap<TrackInfo>, Mp4Error> {
    let mut out = vec![];
    let Some(moov) = find_box(init, 0, init.len(), b"moov") else {
        return Ok(out);
    };
    for trak in boxes(init, moov.payload_start, moov.end()) {
        if &trak.kind != b"trak" {
            continue;
        }
        let tkhd = find_box(init, trak.payload_start, trak.end(), b"tkhd");
        let mdia = find_box(init, trak.payload_start, trak.end(), b"mdia");
        let (Some(tkhd), Some(mdia)) = (tkhd, mdia) else {
            continue;
        };
        let id = tkhd_track_id(init, &tkhd)?;
        let Some(mdhd) = find_box(init, mdia.payload_start, mdia.end(), b"mdhd") else {
            continue;
        };
        let timescale = u32_at(
            init,
            mdhd.payload_start
                + if is_one(init, mdhd.payload_start) {
                    20
                } else {
                    12
                },
        )?;
        if timescale == 0 {
            continue;
        }
        let is_video = find_box(init, mdia.payload_start, mdia.end(), b"hdlr")
            .is_some_and(|h| latin1_eq(init, h.payload_start + 8, h.payload_start + 12, b"vide"));
        set(
            &mut out,
            id,
            TrackInfo {
                timescale,
                is_video,
            },
        );
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TfdtInfo {
    pub version: u8,
    /// Byte offset of the value, for an in-place rewrite.
    pub value_offset: usize,
    pub value: i128,
}

/// A traf's own tfdt; version 1 reads signed (a decode time before 0).
pub fn read_tfdt(buf: &[u8], traf: &BoxRef) -> Result<Option<TfdtInfo>, Mp4Error> {
    let Some(b) = find_box(buf, traf.payload_start, traf.end(), b"tfdt") else {
        return Ok(None);
    };
    let version = buf.get(b.payload_start).copied().unwrap_or(0);
    let value_offset = b.payload_start + 4;
    let value = if is_one(buf, b.payload_start) {
        i128::from(u64_at(buf, value_offset)? as i64)
    } else {
        i128::from(u32_at(buf, value_offset)?)
    };
    Ok(Some(TfdtInfo {
        version,
        value_offset,
        value,
    }))
}

struct Frag {
    track_id: u32,
    tfdt: TfdtInfo,
}

fn collect_tfdts(buf: &[u8]) -> Result<Vec<Frag>, Mp4Error> {
    let mut out = vec![];
    for moof in boxes(buf, 0, buf.len()).filter(|b| &b.kind == b"moof") {
        for traf in boxes(buf, moof.payload_start, moof.end()).filter(|b| &b.kind == b"traf") {
            let tfhd = find_box(buf, traf.payload_start, traf.end(), b"tfhd");
            let tfdt = read_tfdt(buf, &traf)?;
            let (Some(tfhd), Some(tfdt)) = (tfhd, tfdt) else {
                continue;
            };
            out.push(Frag {
                track_id: u32_at(buf, tfhd.payload_start + 4)?,
                tfdt,
            });
        }
    }
    Ok(out)
}

/// JS `BigInt(x)` of a `Math.round` result.
pub(super) fn big_int(x: f64) -> Result<i128, Mp4Error> {
    if x.is_finite() {
        // ponytail: saturates past 2^127, where every tfdt write fails anyway
        return Ok(x as i128);
    }
    Err(Mp4Error::Msg(format!(
        "The number {} cannot be converted to a BigInt because it is not an integer",
        crate::jsnum::js_string(x)
    )))
}

fn write_tfdt(buf: &mut [u8], f: &Frag, value: i128) -> Result<(), Mp4Error> {
    let (off, track) = (f.tfdt.value_offset, f.track_id);
    if value < 0 {
        return Err(Mp4Error::Msg(format!(
            "tfdt {value} of track {track} would be negative at offset {off}"
        )));
    }
    if f.tfdt.version == 1 {
        let v = u64::try_from(value).map_err(|_| Mp4Error::OutOfRange)?;
        buf[off..off + 8].copy_from_slice(&v.to_be_bytes());
    } else if let Ok(v) = u32::try_from(value) {
        buf[off..off + 4].copy_from_slice(&v.to_be_bytes());
    } else {
        return Err(Mp4Error::Msg(format!(
            "tfdt {value} exceeds the 32-bit version-0 box at offset {off}"
        )));
    }
    Ok(())
}

/// Move each fragment of a track `ticks_of` knows by that many ticks.
fn move_tfdts(
    mut buf: Vec<u8>,
    frags: &[Frag],
    mut ticks_of: impl FnMut(u32) -> Result<Option<i128>, Mp4Error>,
) -> Result<Vec<u8>, Mp4Error> {
    for f in frags {
        if let Some(t) = ticks_of(f.track_id)? {
            write_tfdt(&mut buf, f, f.tfdt.value.saturating_add(t))?;
        }
    }
    Ok(buf)
}

/// Shift every fragment so segment `index` of a uniform grid decodes at its true content time.
pub fn rewrite_segment_tfdt(
    seg: Vec<u8>,
    tracks: &IdMap<TrackInfo>,
    index: u32,
    duration: f64,
    start_pts: f64,
) -> Result<Vec<u8>, Mp4Error> {
    if tracks.is_empty() {
        return Ok(seg);
    }
    let frags = collect_tfdts(&seg)?;
    let video = frags
        .iter()
        .find(|f| get(tracks, f.track_id).is_some_and(|t| t.is_video));
    let Some(reference) = video.or(frags.first()) else {
        return Ok(seg);
    };
    let Some(ref_ts) = get(tracks, reference.track_id)
        .map(|t| t.timescale)
        .filter(|t| *t != 0)
    else {
        return Ok(seg);
    };
    let seg_start = f64::from(index) * duration + start_pts;
    let mut run_start = seg_start - reference.tfdt.value as f64 / f64::from(ref_ts);
    if video.is_none() {
        // Only the content part is a grid multiple: snap it, keep the start time.
        run_start = round((run_start - start_pts) / duration) * duration + start_pts;
    }
    move_tfdts(seg, &frags, |id| {
        get(tracks, id)
            .filter(|t| t.timescale != 0)
            .map(|t| big_int(round(run_start * f64::from(t.timescale))))
            .transpose()
    })
}

/// Ticks each track's presentation starts into its media: the edit less the empty edits before it.
pub fn read_init_edits(init: &[u8]) -> Result<IdMap<i128>, Mp4Error> {
    let mut out = vec![];
    let Some(moov) = find_box(init, 0, init.len(), b"moov") else {
        return Ok(out);
    };
    let movie_ts = match find_box(init, moov.payload_start, moov.end(), b"mvhd") {
        Some(m) => u32_at(
            init,
            m.payload_start
                + if is_one(init, m.payload_start) {
                    20
                } else {
                    12
                },
        )?,
        None => 0,
    };
    for (id, info) in parse_init_tracks(init)? {
        let mut ticks: i128 = 0;
        let elst = trak_of(init, &moov, id)?
            .and_then(|t| find_box(init, t.payload_start, t.end(), b"edts"))
            .and_then(|e| find_box(init, e.payload_start, e.end(), b"elst"));
        if let Some(elst) = elst {
            let v1 = is_one(init, elst.payload_start);
            let count = u32_at(init, elst.payload_start + 4)?;
            let mut off = elst.payload_start + 8;
            for _ in 0..count {
                let (duration, media_time) = if v1 {
                    (
                        i128::from(u64_at(init, off)?),
                        i128::from(u64_at(init, off + 8)? as i64),
                    )
                } else {
                    (
                        i128::from(u32_at(init, off)?),
                        i128::from(i32_at(init, off + 4)?),
                    )
                };
                if media_time != -1 {
                    ticks += media_time;
                    break;
                }
                if movie_ts == 0 {
                    return Err(Mp4Error::Msg(format!(
                        "track {id} has an empty edit and no movie timescale"
                    )));
                }
                ticks -= duration * i128::from(info.timescale) / i128::from(movie_ts);
                off += if v1 { 20 } else { 12 };
            }
        }
        set(&mut out, id, ticks);
    }
    Ok(out)
}

fn trak_of(buf: &[u8], moov: &BoxRef, id: u32) -> Result<Option<BoxRef>, Mp4Error> {
    for trak in boxes(buf, moov.payload_start, moov.end()).filter(|b| &b.kind == b"trak") {
        if let Some(tkhd) = find_box(buf, trak.payload_start, trak.end(), b"tkhd")
            && tkhd_track_id(buf, &tkhd)? == id
        {
            return Ok(Some(trak));
        }
    }
    Ok(None)
}

/// One edit starting the presentation `ticks` into the media, for a whole fragmented track.
fn single_edit(ticks: i128) -> Result<Vec<u8>, Mp4Error> {
    let v1 = ticks > 0x7fff_ffff;
    let mut entry = vec![0u8; if v1 { 20 } else { 12 }];
    if v1 {
        let t = i64::try_from(ticks).map_err(|_| Mp4Error::OutOfRange)?;
        entry[8..16].copy_from_slice(&t.to_be_bytes());
    } else {
        let t = i32::try_from(ticks).map_err(|_| Mp4Error::OutOfRange)?;
        entry[4..8].copy_from_slice(&t.to_be_bytes());
    }
    let rate = if v1 { 16 } else { 8 };
    entry[rate..rate + 4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    let head = [u8::from(v1), 0, 0, 0, 0, 0, 0, 1];
    make_box(b"edts", &[&make_box(b"elst", &[&head, &entry])?])
}

/// `init` with each parsed track's edits replaced by one starting `seconds_of(id, track)` in.
pub fn with_init_edits(
    init: &[u8],
    mut seconds_of: impl FnMut(u32, &TrackInfo) -> f64,
) -> Result<Vec<u8>, Mp4Error> {
    let tracks = parse_init_tracks(init)?;
    let mut out = vec![];
    for top in boxes(init, 0, init.len()) {
        if &top.kind != b"moov" {
            out.extend_from_slice(&init[top.start..top.end()]);
            continue;
        }
        let mut children: Vec<Vec<u8>> = vec![];
        for b in boxes(init, top.payload_start, top.end()) {
            if &b.kind != b"trak" {
                children.push(init[b.start..b.end()].to_vec());
                continue;
            }
            let tkhd = find_box(init, b.payload_start, b.end(), b"tkhd");
            let info = match tkhd {
                Some(t) => {
                    let id = tkhd_track_id(init, &t)?;
                    get(&tracks, id).map(|i| (id, i))
                }
                None => None,
            };
            let Some((id, info)) = info else {
                children.push(init[b.start..b.end()].to_vec());
                continue;
            };
            let ticks = big_int(round(seconds_of(id, &info) * f64::from(info.timescale)))?;
            let edit = single_edit(ticks)?;
            let mut parts: Vec<&[u8]> = vec![];
            for c in boxes(init, b.payload_start, b.end()).filter(|c| &c.kind != b"edts") {
                parts.push(&init[c.start..c.end()]);
                if &c.kind == b"tkhd" {
                    parts.push(&edit);
                }
            }
            children.push(make_box(b"trak", &parts)?);
        }
        let refs: Vec<&[u8]> = children.iter().map(Vec::as_slice).collect();
        out.extend(make_box(b"moov", &refs)?);
    }
    Ok(out)
}

/// Move every fragment's tfdt by its track's ticks; a track without a delta fails.
pub fn retime_fragments(seg: &[u8], deltas: &IdMap<i128>) -> Result<Vec<u8>, Mp4Error> {
    move_tfdts(seg.to_vec(), &collect_tfdts(seg)?, |id| {
        get(deltas, id)
            .map(Some)
            .ok_or_else(|| Mp4Error::Msg(format!("fragment of track {id}, absent from the init")))
    })
}

/// Decode time of the first fragment of `track_id`.
pub fn first_tfdt(seg: &[u8], track_id: u32) -> Result<Option<i128>, Mp4Error> {
    Ok(collect_tfdts(seg)?
        .into_iter()
        .find(|f| f.track_id == track_id)
        .map(|f| f.tfdt.value))
}

pub const TRUN_DATA_OFFSET: u32 = 0x1;
const TRUN_FIRST_SAMPLE_FLAGS: u32 = 0x4;
pub const TRUN_SAMPLE_DURATION: u32 = 0x100;
pub const TRUN_SAMPLE_SIZE: u32 = 0x200;
pub const TRUN_SAMPLE_FLAGS: u32 = 0x400;
pub const TRUN_SAMPLE_CTS: u32 = 0x800;
const MAX_SAMPLES: u32 = 1 << 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawSample {
    pub duration: u32,
    pub size: u32,
    pub flags: u32,
    pub cts: Option<i32>,
}

/// One traf's samples, each field falling back to the tfhd default, then `trex_default`.
pub fn trun_samples(
    buf: &[u8],
    traf: &BoxRef,
    trex_default: u32,
) -> Result<Vec<RawSample>, Mp4Error> {
    let tfhd = find_box(buf, traf.payload_start, traf.end(), b"tfhd");
    let trun = find_box(buf, traf.payload_start, traf.end(), b"trun");
    let (Some(tfhd), Some(trun)) = (tfhd, trun) else {
        return Ok(vec![]);
    };
    let tf = u24_at(buf, tfhd.payload_start + 1)?;
    let mut o = tfhd.payload_start + 8;
    if tf & 0x1 != 0 {
        o += 8;
    }
    if tf & 0x2 != 0 {
        o += 4;
    }
    let (mut d_duration, mut d_size, mut d_flags) = (trex_default, 0, 0);
    if tf & 0x8 != 0 {
        d_duration = u32_at(buf, o)?;
        o += 4;
    }
    if tf & 0x10 != 0 {
        d_size = u32_at(buf, o)?;
        o += 4;
    }
    if tf & 0x20 != 0 {
        d_flags = u32_at(buf, o)?;
    }
    let flags = u24_at(buf, trun.payload_start + 1)?;
    let count = u32_at(buf, trun.payload_start + 4)?;
    if count > MAX_SAMPLES {
        return Err(Mp4Error::OutOfRange);
    }
    let mut p = trun.payload_start + 8;
    if flags & TRUN_DATA_OFFSET != 0 {
        p += 4;
    }
    let mut first_flags = d_flags;
    if flags & TRUN_FIRST_SAMPLE_FLAGS != 0 {
        first_flags = u32_at(buf, p)?;
        p += 4;
    }
    let mut field = |on: u32, default: u32| -> Result<u32, Mp4Error> {
        if flags & on == 0 {
            return Ok(default);
        }
        let v = u32_at(buf, p)?;
        p += 4;
        Ok(v)
    };
    let mut out = Vec::with_capacity(count.min(4096) as usize);
    for i in 0..count {
        let duration = field(TRUN_SAMPLE_DURATION, d_duration)?;
        let size = field(TRUN_SAMPLE_SIZE, d_size)?;
        let flags_v = field(
            TRUN_SAMPLE_FLAGS,
            if i == 0 { first_flags } else { d_flags },
        )?;
        let cts = (flags & TRUN_SAMPLE_CTS != 0)
            .then(|| field(TRUN_SAMPLE_CTS, 0).map(|v| v as i32))
            .transpose()?;
        out.push(RawSample {
            duration,
            size,
            flags: flags_v,
            cts,
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeExtent {
    /// First traf's tfdt plus every sample duration, in ticks.
    pub end: i128,
    /// Duration of the last sample read.
    pub last_duration: i128,
}

/// `track_id`'s decode extent across the `moof`s of `moofs`.
pub fn video_decode_extent(
    moofs: &[u8],
    track_id: u32,
    trex_default: u32,
) -> Result<Option<DecodeExtent>, Mp4Error> {
    let mut end: Option<i128> = None;
    let mut last = 0;
    for moof in boxes(moofs, 0, moofs.len()).filter(|b| &b.kind == b"moof") {
        for traf in boxes(moofs, moof.payload_start, moof.end()).filter(|b| &b.kind == b"traf") {
            let Some(tfhd) = find_box(moofs, traf.payload_start, traf.end(), b"tfhd") else {
                continue;
            };
            if u32_at(moofs, tfhd.payload_start + 4)? != track_id {
                continue;
            }
            let mut e = match end {
                Some(e) => e,
                None => match read_tfdt(moofs, &traf)? {
                    Some(t) => t.value,
                    None => return Ok(None),
                },
            };
            for s in trun_samples(moofs, &traf, trex_default)? {
                e += i128::from(s.duration);
                last = i128::from(s.duration);
            }
            end = Some(e);
        }
    }
    Ok(end.map(|end| DecodeExtent {
        end,
        last_duration: last,
    }))
}

/// `trackId -> default_sample_duration` of an init's `moov/mvex/trex`.
pub fn parse_trex_defaults(init: &[u8]) -> Result<IdMap<u32>, Mp4Error> {
    let mut out = vec![];
    let mvex = find_box(init, 0, init.len(), b"moov")
        .and_then(|m| find_box(init, m.payload_start, m.end(), b"mvex"));
    let Some(mvex) = mvex else {
        return Ok(out);
    };
    for trex in boxes(init, mvex.payload_start, mvex.end()).filter(|b| &b.kind == b"trex") {
        let id = u32_at(init, trex.payload_start + 4)?;
        set(&mut out, id, u32_at(init, trex.payload_start + 12)?);
    }
    Ok(out)
}

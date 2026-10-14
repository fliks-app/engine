//! ISO-BMFF box walking and Node `Buffer` reads (whole-buffer bounds, as the host reads).
use super::Mp4Error;
use serde::{Deserialize, Serialize};

/// A box in a buffer: `start` and `size` cover the header, `payload_start` follows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoxRef {
    #[serde(rename = "type", with = "fourcc")]
    pub kind: [u8; 4],
    pub start: usize,
    pub size: usize,
    pub payload_start: usize,
}

impl BoxRef {
    pub fn end(&self) -> usize {
        self.start + self.size
    }
}

mod fourcc {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(k: &[u8; 4], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&k.iter().map(|b| char::from(*b)).collect::<String>())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 4], D::Error> {
        let s = String::deserialize(d)?;
        let mut out = [0u8; 4];
        for (o, c) in out.iter_mut().zip(s.chars()) {
            *o = c as u8;
        }
        Ok(out)
    }
}

/// `buf.readUInt32BE(off)`.
pub fn u32_at(buf: &[u8], off: usize) -> Result<u32, Mp4Error> {
    let b = buf
        .get(off..off.checked_add(4).ok_or(Mp4Error::OutOfRange)?)
        .ok_or(Mp4Error::OutOfRange)?;
    Ok(u32::from_be_bytes(b.try_into().expect("4 bytes")))
}

/// `buf.readInt32BE(off)`.
pub fn i32_at(buf: &[u8], off: usize) -> Result<i32, Mp4Error> {
    u32_at(buf, off).map(|v| v as i32)
}

/// `buf.readBigUInt64BE(off)`.
pub fn u64_at(buf: &[u8], off: usize) -> Result<u64, Mp4Error> {
    let b = buf
        .get(off..off.checked_add(8).ok_or(Mp4Error::OutOfRange)?)
        .ok_or(Mp4Error::OutOfRange)?;
    Ok(u64::from_be_bytes(b.try_into().expect("8 bytes")))
}

/// `buf.readUIntBE(off, 3)`.
pub fn u24_at(buf: &[u8], off: usize) -> Result<u32, Mp4Error> {
    let b = buf
        .get(off..off.checked_add(3).ok_or(Mp4Error::OutOfRange)?)
        .ok_or(Mp4Error::OutOfRange)?;
    Ok(u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]))
}

/// `buf[i] === 1`: past the end is undefined, so not 1.
pub fn is_one(buf: &[u8], i: usize) -> bool {
    buf.get(i) == Some(&1)
}

/// `buf.toString('latin1', a, b)` compared to `s`, both ends clamped.
pub fn latin1_eq(buf: &[u8], a: usize, b: usize, s: &[u8]) -> bool {
    let (a, b) = (a.min(buf.len()), b.min(buf.len()));
    a < b && &buf[a..b] == s || a >= b && s.is_empty()
}

/// One box header at `off`; `start`/`end` are the container's own coordinates.
pub fn read_box_header(buf: &[u8], off: usize, start: usize, end: usize) -> Option<BoxRef> {
    let avail = buf.len().saturating_sub(off);
    let mut size = if avail >= 8 {
        u64::from(u32_at(buf, off).ok()?)
    } else {
        0
    };
    let mut payload_start = start + 8;
    if size == 1 && avail >= 16 {
        size = u64_at(buf, off + 8).ok()?;
        payload_start = start + 16;
    } else if size == 0 && avail >= 8 {
        size = end.saturating_sub(start) as u64;
    }
    let size = usize::try_from(size).ok()?;
    if size < 8 || start.checked_add(size)? > end {
        return None;
    }
    let mut kind = [0u8; 4];
    kind.copy_from_slice(&buf[off + 4..off + 8]);
    Some(BoxRef {
        kind,
        start,
        size,
        payload_start,
    })
}

/// The boxes of `buf[start, end)`, up to the first malformed one.
pub fn boxes(buf: &[u8], start: usize, end: usize) -> impl Iterator<Item = BoxRef> + '_ {
    let mut off = start;
    std::iter::from_fn(move || {
        if off.checked_add(8)? > end {
            return None;
        }
        let b = read_box_header(buf, off, off, end)?;
        off += b.size;
        Some(b)
    })
}

pub fn find_box(buf: &[u8], start: usize, end: usize, kind: &[u8; 4]) -> Option<BoxRef> {
    boxes(buf, start, end).find(|b| &b.kind == kind)
}

/// A 32-bit box around `children`.
pub fn make_box(kind: &[u8; 4], children: &[&[u8]]) -> Result<Vec<u8>, Mp4Error> {
    let len: usize = children.iter().map(|c| c.len()).sum();
    let size = u32::try_from(8 + len).map_err(|_| Mp4Error::OutOfRange)?;
    let mut out = Vec::with_capacity(8 + len);
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(kind);
    for c in children {
        out.extend_from_slice(c);
    }
    Ok(out)
}

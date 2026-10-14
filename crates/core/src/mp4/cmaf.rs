//! ffmpeg's DASH-flavoured fMP4 as Apple HLS-fMP4: `hlsf` compat brand, no `styp`, no `sidx`.

const HLSF: &[u8; 4] = b"hlsf";

fn has_hlsf(bytes: &[u8]) -> bool {
    bytes
        .get(16..)
        .is_some_and(|b| b.as_chunks::<4>().0.contains(HLSF))
}

/// Already rewritten: a leading box other than `styp`/`ftyp`, or an ftyp carrying `hlsf`.
pub fn is_cmaf_rewritten(buf: &[u8]) -> bool {
    if buf.len() < 12 {
        return false;
    }
    match &buf[4..8] {
        b"styp" => false,
        b"ftyp" => {
            let size = u32::from_be_bytes(buf[..4].try_into().expect("4 bytes")) as usize;
            has_hlsf(&buf[..size.min(buf.len())])
        }
        _ => true,
    }
}

/// Strip `sidx` and `styp`, add `hlsf`; a size-0, size-1 or truncated box ends it, copied verbatim.
pub fn cmaf_rewrite(buf: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(buf.len() + 4);
    let mut off = 0;
    while off + 8 <= buf.len() {
        let size = u32::from_be_bytes(buf[off..off + 4].try_into().expect("4 bytes")) as usize;
        if size == 0 || size == 1 || off + size > buf.len() {
            out.extend_from_slice(&buf[off..]);
            break;
        }
        let b = &buf[off..off + size];
        match &buf[off + 4..off + 8] {
            b"sidx" | b"styp" => {}
            b"ftyp" if !has_hlsf(b) => {
                // The host writes the size last: a box under 4 bytes loses part of the brand.
                let mut grown = vec![0u8; size + 4];
                grown[..size].copy_from_slice(b);
                grown[size..].copy_from_slice(HLSF);
                grown[..4].copy_from_slice(&((size + 4) as u32).to_be_bytes());
                out.extend(grown);
            }
            _ => out.extend_from_slice(b),
        }
        off += size;
    }
    out
}

/// What the host serves for a segment or init file's bytes: `None` when empty.
pub fn rewrite_cmaf(buf: &[u8]) -> Option<Vec<u8>> {
    if buf.is_empty() {
        return None;
    }
    Some(if is_cmaf_rewritten(buf) {
        buf.to_vec()
    } else {
        cmaf_rewrite(buf)
    })
}

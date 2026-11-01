//! iTunes free-form atoms (`----`) added to the `ilst` of a finished MP4's `moov`: the muxer
//! writes no atom for a narrator, series, ASIN and the like.
use super::Mp4Error;
use super::boxes::{BoxRef, find_box, make_box, read_box_header};

fn msg(m: &str) -> Mp4Error {
    Mp4Error::Msg(m.into())
}

/// `----` with its `mean`, `name` and UTF-8 `data` boxes.
fn atom(name: &str, value: &str) -> Result<Vec<u8>, Mp4Error> {
    let text = |kind: &[u8; 4], head: &[u8], body: &str| make_box(kind, &[head, body.as_bytes()]);
    let mean = text(b"mean", &[0; 4], "com.apple.iTunes")?;
    let name = text(b"name", &[0; 4], name)?;
    let data = text(b"data", &[0, 0, 0, 1, 0, 0, 0, 0], value)?;
    make_box(b"----", &[&mean, &name, &data])
}

/// `moov`, a whole box, with `items` appended to `moov/udta/meta/ilst` and the sizes of those four
/// boxes grown to match. Only valid when nothing refers to bytes after the `ilst` (chunk offsets
/// point into an `mdat` before the `moov`).
pub fn add_items(moov: &[u8], items: &[(&str, String)]) -> Result<Vec<u8>, Mp4Error> {
    let top = read_box_header(moov, 0, 0, moov.len())
        .filter(|b| &b.kind == b"moov" && b.size == moov.len())
        .ok_or_else(|| msg("not a moov box"))?;
    let child = |parent: &BoxRef, skip: usize, kind: &[u8; 4]| {
        find_box(moov, parent.payload_start + skip, parent.end(), kind)
            .ok_or_else(|| msg("moov has no udta/meta/ilst"))
    };
    let udta = child(&top, 0, b"udta")?;
    // `meta` is a full box: four bytes of version and flags precede its children.
    let meta = child(&udta, 0, b"meta")?;
    let ilst = child(&meta, 4, b"ilst")?;
    let mut extra = vec![];
    for (name, value) in items {
        extra.extend(atom(name, value)?);
    }
    let mut out = [&moov[..ilst.end()], &extra, &moov[ilst.end()..]].concat();
    for b in [top, udta, meta, ilst] {
        if b.payload_start != b.start + 8 {
            return Err(msg("a 64-bit box size"));
        }
        let size = u32::try_from(b.size + extra.len()).map_err(|_| Mp4Error::OutOfRange)?;
        out[b.start..b.start + 4].copy_from_slice(&size.to_be_bytes());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp4::boxes::boxes;

    fn moov(tail: &[u8]) -> Vec<u8> {
        let tag = make_box(b"\xA9nam", &[b"x"]).unwrap();
        let ilst = make_box(b"ilst", &[&tag]).unwrap();
        let meta = make_box(b"meta", &[&[0; 4], &ilst, tail]).unwrap();
        let udta = make_box(b"udta", &[&meta]).unwrap();
        make_box(b"moov", &[&make_box(b"mvhd", &[&[0; 4]]).unwrap(), &udta]).unwrap()
    }

    #[test]
    fn items_land_in_the_ilst_and_every_size_grows() {
        let before = moov(&make_box(b"free", &[&[0; 6]]).unwrap());
        let after = add_items(&before, &[("narrator", "Nar".into())]).unwrap();
        let grew = after.len() - before.len();
        assert_eq!(grew, 8 + 28 + 20 + 19, "header, mean, name, data");
        let top = read_box_header(&after, 0, 0, after.len()).unwrap();
        assert_eq!(top.size, after.len());
        let udta = find_box(&after, 8, after.len(), b"udta").unwrap();
        let meta = find_box(&after, udta.payload_start, udta.end(), b"meta").unwrap();
        let kids: Vec<_> = boxes(&after, meta.payload_start + 4, meta.end())
            .map(|b| b.kind)
            .collect();
        assert_eq!(kids, [*b"ilst", *b"free"], "what followed the ilst is kept");
        let ilst = find_box(&after, meta.payload_start + 4, meta.end(), b"ilst").unwrap();
        let items: Vec<_> = boxes(&after, ilst.payload_start, ilst.end()).collect();
        assert_eq!(items.len(), 2);
        let tail = &after[items[1].start..items[1].end()];
        assert_eq!(&tail[4..8], b"----");
        assert!(tail.windows(8).any(|w| w == b"narrator"));
        assert!(tail.ends_with(b"Nar"));
        assert_eq!(udta.end(), meta.end());
    }

    #[test]
    fn a_moov_without_an_ilst_is_refused() {
        let bare = make_box(b"moov", &[&make_box(b"mvhd", &[&[0; 4]]).unwrap()]).unwrap();
        assert!(add_items(&bare, &[("asin", "B0".into())]).is_err());
    }
}

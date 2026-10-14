use super::Rung;

/// Canonical height step of a frame (144 ... 2160), ceilings on both axes; 480 when unknown.
pub fn bucket_resolution_height(width: Option<u32>, height: Option<u32>) -> u32 {
    let (w, h) = (width.unwrap_or(0), height.unwrap_or(0));
    if w == 0 && h == 0 {
        return 480;
    }
    match (w, h) {
        (..=256, ..=192) => 144,
        (..=426, ..=320) => 240,
        (..=640, ..=432) => 360,
        (..=854, ..=576) => 480,
        (..=1280, ..=962) => 720,
        (..=1920, ..=1440) => 1080,
        _ => 2160,
    }
}

/// Whether a frame fits a decoder's max resolution in either orientation; an absent or zero cap
/// axis passes.
pub fn resolution_fits_cap(
    width: Option<u32>,
    height: Option<u32>,
    max_width: Option<f64>,
    max_height: Option<f64>,
) -> bool {
    let (w, h) = (
        f64::from(width.unwrap_or(0)),
        f64::from(height.unwrap_or(0)),
    );
    let (mw, mh) = (max_width.unwrap_or(0.0), max_height.unwrap_or(0.0));
    let (cap_long, cap_short) = (mw.max(mh), mw.min(mh));
    !(cap_long != 0.0 && w.max(h) > cap_long) && !(cap_short != 0.0 && w.min(h) > cap_short)
}

/// Whether a rung is small enough to encode from the source (bucketed heights).
pub fn profile_fits_source(r: &Rung, width: Option<u32>, height: Option<u32>) -> bool {
    bucket_resolution_height(Some(r.max_width), Some(r.max_height))
        <= bucket_resolution_height(width, height)
}

/// Output size of a rung over a source: width capped at the source, height snapped up to even.
/// Height is NaN for a zero-width source, as in the host.
pub fn profile_resolution(r: &Rung, source_width: u32, source_height: u32) -> (f64, f64) {
    let width = f64::from(r.max_width.min(source_width));
    let raw_h = width * f64::from(source_height) / f64::from(source_width);
    let h = (raw_h / 2.0).ceil() * 2.0;
    (width, if h < 2.0 { 2.0 } else { h })
}

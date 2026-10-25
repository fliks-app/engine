//! Transcode quality ladders: rungs, resolution buckets and per-rung bitrate caps.
mod bitrate;
mod resolution;

pub use bitrate::{
    RungBitrateContext, capped_rung_video_bitrate_bps, js_round, resolve_source_video_bitrate_bps,
};
pub(crate) use bitrate::{codec_factor, hevc_main_tier_cap_bps};
pub use resolution::{
    bucket_resolution_height, profile_fits_source, profile_resolution, resolution_fits_cap,
};

/// One rung: bitrates in bps. Ladders are sorted by `max_height`, descending, within a class.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rung {
    pub name: &'static str,
    pub max_width: u32,
    pub max_height: u32,
    pub video_bps: f64,
    pub audio_bps: f64,
}

const fn rung(name: &'static str, w: u32, h: u32, video_bps: f64, audio_bps: f64) -> Rung {
    Rung {
        name,
        max_width: w,
        max_height: h,
        video_bps,
        audio_bps,
    }
}

/// SDR ladder: the full rungs then the low-consumption `eco-*` ones.
pub const SDR_LADDER: &[Rung] = &[
    rung("2160p", 3840, 2160, 20e6, 192e3),
    rung("1080p", 1920, 1080, 8e6, 192e3),
    rung("720p", 1280, 720, 4e6, 128e3),
    rung("480p", 854, 480, 2e6, 96e3),
    rung("360p", 640, 360, 1e6, 64e3),
    rung("240p", 426, 240, 500e3, 64e3),
    rung("144p", 256, 144, 200e3, 48e3),
    rung("eco-2160p", 3840, 2160, 8e6, 192e3),
    rung("eco-1080p", 1920, 1080, 3e6, 192e3),
    rung("eco-720p", 1280, 720, 1500e3, 128e3),
];

/// HDR-preserving ladder; stops at 480p.
pub const HDR_LADDER: &[Rung] = &[
    rung("2160p-hdr", 3840, 2160, 28e6, 192e3),
    rung("1080p-hdr", 1920, 1080, 5500e3, 192e3),
    rung("720p-hdr", 1280, 720, 2800e3, 128e3),
    rung("480p-hdr", 854, 480, 1400e3, 96e3),
    rung("eco-2160p-hdr", 3840, 2160, 12e6, 192e3),
    rung("eco-1080p-hdr", 1920, 1080, 2200e3, 192e3),
];

pub fn ladder(hdr: bool) -> &'static [Rung] {
    if hdr { HDR_LADDER } else { SDR_LADDER }
}

pub fn is_eco(name: &str) -> bool {
    name.starts_with("eco-")
}

/// Rung name asking for the top full rung of whatever ladder the source fits.
pub const BEST_RUNG: &str = "best";

/// `best`, else the exact name, else the nearest rung of the same class (full vs `eco-`) that does not step up.
pub fn resolve_rung<'a>(name: &str, ladder: &'a [Rung]) -> Option<&'a Rung> {
    if name == BEST_RUNG {
        return ladder.iter().find(|r| !is_eco(r.name));
    }
    if let Some(r) = ladder.iter().find(|r| r.name == name) {
        return Some(r);
    }
    let tier = tier_of(name)?;
    let eco = is_eco(name);
    let mut same_class = ladder.iter().filter(|r| is_eco(r.name) == eco);
    let first_fit = same_class.clone().find(|r| f64::from(r.max_height) <= tier);
    first_fit.or_else(|| same_class.next_back())
}

/// The `N` of the first `(\d+)p` in `name`; 0 counts as absent.
fn tier_of(name: &str) -> Option<f64> {
    let b = name.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if b.get(j) == Some(&b'p') {
            return name[i..j].parse::<f64>().ok().filter(|t| *t != 0.0);
        }
        i = j;
    }
    None
}

/// First non-eco rung (the source-resolution one), else the first.
pub fn top_non_eco(ladder: &[Rung]) -> &Rung {
    ladder
        .iter()
        .find(|r| !is_eco(r.name))
        .unwrap_or(&ladder[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn best_is_the_top_full_rung_of_the_given_ladder() {
        assert_eq!(resolve_rung("best", SDR_LADDER).unwrap().name, "2160p");
        let fits_720: Vec<Rung> = SDR_LADDER
            .iter()
            .filter(|r| r.max_height <= 720)
            .copied()
            .collect();
        assert_eq!(resolve_rung("best", &fits_720).unwrap().name, "720p");
        assert_eq!(resolve_rung("best", HDR_LADDER).unwrap().name, "2160p-hdr");
    }
}

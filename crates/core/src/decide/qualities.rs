//! The quality menu and per-rung bitrate hints.
use super::timeline::{DEFAULT_FPS, parse_source_fps};
use crate::api::{BitrateHint, PlayMethod, PlaybackSource, QualityOption};
use crate::ladder::{
    Rung, RungBitrateContext, capped_rung_video_bitrate_bps, is_eco, profile_fits_source,
    resolve_source_video_bitrate_bps, top_non_eco,
};
use std::collections::BTreeMap;

fn ctx<'a>(source: &'a PlaybackSource, output_codec: &'a str) -> RungBitrateContext<'a> {
    RungBitrateContext {
        output_codec,
        source_width: source.width.unwrap_or(0),
        source_height: source.height.unwrap_or(0),
        source_frame_rate: parse_source_fps(source.frame_rate.as_deref()).unwrap_or(DEFAULT_FPS),
        source_video_bitrate_bps: source.video_bit_rate.map(f64::from),
        source_video_codec: &source.video_codec,
    }
}

/// Per-rung bitrate hint on the manifest's scale, keyed by rung id. `rung_audio_bps` is the
/// plan's audio bitrate on a rung (0 without audio).
pub fn transcode_bitrate_by_quality(
    source: &PlaybackSource,
    output_codec: &str,
    ladder: &[Rung],
    rung_audio_bps: impl Fn(&Rung) -> f64,
) -> BTreeMap<String, BitrateHint> {
    let ctx = ctx(source, output_codec);
    ladder
        .iter()
        .map(|r| {
            let video = capped_rung_video_bitrate_bps(r, &ctx);
            let audio = rung_audio_bps(r);
            let hint = BitrateHint {
                video_bitrate_bps: video as u32,
                audio_bitrate_bps: audio as u32,
                total_bitrate_bps: (video + audio) as u32,
            };
            (r.name.to_string(), hint)
        })
        .collect()
}

fn display_label(name: &str) -> String {
    let stripped = name.strip_prefix("eco-").unwrap_or(name);
    let stripped = stripped.strip_suffix("-hdr").unwrap_or(stripped);
    if stripped == "2160p" {
        "4K".into()
    } else {
        stripped.into()
    }
}

/// The server-authoritative quality list: the ladder rungs that fit the source, the top one
/// collapsing into `original` when the video is copied, plus the eco rungs that really reduce it.
pub fn build_quality_list(
    source: &PlaybackSource,
    play_method: PlayMethod,
    video_copy_stream: bool,
    ladder: &[Rung],
    target_codec: &str,
    has_audio: bool,
) -> Vec<QualityOption> {
    let rung_ctx = ctx(source, target_codec);
    let source_total = source.format_bit_rate.map(f64::from).unwrap_or_else(|| {
        f64::from(source.video_bit_rate.unwrap_or(0))
            + f64::from(source.audio_bit_rate.unwrap_or(0))
    });
    let mut available: Vec<Rung> = ladder
        .iter()
        .copied()
        .filter(|r| profile_fits_source(r, source.width, source.height))
        .collect();
    if available.is_empty() {
        available.extend(ladder.last().copied());
    }
    let total_of = |r: &Rung| {
        capped_rung_video_bitrate_bps(r, &rung_ctx) + if has_audio { r.audio_bps } else { 0.0 }
    };
    let top = *top_non_eco(&available);
    // Sorted by the rung's tier, not the displayed height: a scope source's `original` is shorter
    // than its own eco rung's bucket.
    let mut entries: Vec<(u32, QualityOption)> = vec![];
    for r in available.iter().filter(|r| !is_eco(r.name)) {
        let total = total_of(r);
        let option = if r.name == top.name && video_copy_stream {
            QualityOption {
                id: "original".into(),
                label: display_label(top.name),
                height: source.height.filter(|h| *h != 0).unwrap_or(top.max_height),
                width: source.width.filter(|w| *w != 0).unwrap_or(top.max_width),
                total_bitrate_bps: (if source_total > 0.0 {
                    source_total
                } else {
                    total
                }) as u32,
                is_remux: play_method == PlayMethod::DirectStream,
                low_bandwidth: None,
            }
        } else {
            rung_option(r, total, None)
        };
        entries.push((r.max_height, option));
    }

    // An eco rung is only worth a forced re-encode when it shrinks the video itself; compare
    // totals when the source video bitrate is unknown.
    let source_video_bps = resolve_source_video_bitrate_bps(
        source.video_bit_rate.map(f64::from),
        source.format_bit_rate.map(f64::from),
        f64::from(source.audio_bit_rate.unwrap_or(0)),
    );
    let saving_ref = if source_total > 0.0 {
        source_total
    } else {
        total_of(&top)
    };
    for r in available.iter().filter(|r| is_eco(r.name)) {
        let reduces_video = match source_video_bps.filter(|b| *b > 0.0) {
            Some(bps) => capped_rung_video_bitrate_bps(r, &rung_ctx) < bps,
            None => total_of(r) < saving_ref,
        };
        if reduces_video {
            entries.push((r.max_height, rung_option(r, total_of(r), Some(true))));
        }
    }

    entries.sort_by(|(ta, a), (tb, b)| {
        tb.cmp(ta)
            .then(a.low_bandwidth.is_some().cmp(&b.low_bandwidth.is_some()))
            .then(b.total_bitrate_bps.cmp(&a.total_bitrate_bps))
    });
    entries.into_iter().map(|(_, o)| o).collect()
}

fn rung_option(r: &Rung, total: f64, low_bandwidth: Option<bool>) -> QualityOption {
    QualityOption {
        id: r.name.into(),
        label: display_label(r.name),
        height: r.max_height,
        width: r.max_width,
        total_bitrate_bps: total as u32,
        is_remux: false,
        low_bandwidth,
    }
}

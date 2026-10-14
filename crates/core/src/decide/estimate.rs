//! Download size estimate per ladder rung, from the probed source only.
use super::audio::{lower_codec, output_bitrate_bps};
use crate::api::{AudioCodec, AudioPlan, AudioStreamInfo, EstimateSizesRequest, QualityEstimate};
use crate::ladder::{
    RungBitrateContext, SDR_LADDER, capped_rung_video_bitrate_bps, is_eco, profile_fits_source,
    resolve_source_video_bitrate_bps,
};

/// Bitrate a track streams at on a rung of `stereo_bps`: copied when its codec rides fMP4, else
/// Dolby for surround or AAC.
fn track_bitrate_bps(t: &AudioStreamInfo, stereo_bps: f64) -> f64 {
    let codec = lower_codec(t);
    let channels = t.channels.unwrap_or(2);
    let plan = if matches!(codec.as_str(), "aac" | "ac3" | "eac3" | "opus" | "flac") {
        AudioPlan::Copy {
            codec,
            channels: Some(channels),
            bitrate_bps: t.bit_rate,
            profile: None,
        }
    } else if channels >= 6 {
        AudioPlan::Transcode {
            codec: AudioCodec::Eac3,
            channels: channels.min(6),
        }
    } else {
        AudioPlan::Transcode {
            codec: AudioCodec::Aac,
            channels,
        }
    };
    output_bitrate_bps(&plan, stereo_bps)
}

pub fn estimate_sizes(req: &EstimateSizesRequest) -> Vec<QualityEstimate> {
    let info = &req.info;
    let video = info.video.first();
    let (width, height) = (
        video.and_then(|v| v.width).unwrap_or(1920),
        video.and_then(|v| v.height).unwrap_or(1080),
    );
    let source_bitrate = f64::from(info.format_bit_rate.unwrap_or(0));
    let duration = info.duration_seconds.unwrap_or(0.0);
    let audio_sum: f64 = info
        .audio
        .iter()
        .map(|a| f64::from(a.bit_rate.unwrap_or(0)))
        .sum();
    let codec = video.map_or("", |v| v.codec.as_str());
    let ctx = RungBitrateContext {
        output_codec: codec,
        source_width: width,
        source_height: height,
        // JS `Number(s) || 0`: a fraction like 24000/1001 is NaN, so 0.
        source_frame_rate: video
            .and_then(|v| v.frame_rate.as_deref())
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|f| f.is_finite())
            .unwrap_or(0.0),
        source_video_bitrate_bps: resolve_source_video_bitrate_bps(
            video.and_then(|v| v.bit_rate).map(f64::from),
            Some(source_bitrate),
            audio_sum,
        ),
        source_video_codec: codec,
    };
    // The host lists the desktop ladder only, no eco rungs.
    SDR_LADDER
        .iter()
        .filter(|r| !is_eco(r.name) && profile_fits_source(r, Some(width), Some(height)))
        .map(|r| {
            let total = capped_rung_video_bitrate_bps(r, &ctx)
                + info
                    .audio
                    .iter()
                    .map(|t| track_bitrate_bps(t, r.audio_bps))
                    .sum::<f64>();
            let mut size = if duration > 0.0 {
                (total * duration / 8.0).floor()
            } else {
                (req.file_size * (total / source_bitrate.max(total))).floor()
            };
            if req.file_size > 0.0 {
                size = size.min(req.file_size);
            }
            QualityEstimate {
                key: r.name.into(),
                label: r.name.into(),
                estimated_size: size,
            }
        })
        .collect()
}

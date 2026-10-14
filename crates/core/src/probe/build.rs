//! ffprobe JSON to `MediaInfo`: `detectMediaFileInfo` minus the process spawning.
use super::ffjson::{Frame, Main, Stream};
use super::js::{number, parse_float, round, to_fixed3};
use super::lang::{is_image_based, language_from_title, subtitle_flags_from_title};
use crate::api::{
    AudioStreamInfo, Chapter, HdrFormat, HdrMetadata, MediaInfo, SubtitleStreamInfo,
    VideoStreamInfo,
};
use serde_json::Value;
use std::collections::BTreeMap;

/// Seconds of packet timestamps sampled to measure the real frame rate.
pub const FPS_WINDOW_SECONDS: f64 = 60.0;
const MIN_MEASURABLE_SECONDS: f64 = 90.0;
const MIN_SAMPLE_FRAMES: usize = 100;
const FPS_DISAGREEMENT: f64 = 0.001;
const LATE_AUDIO_PARAMS_MARGIN_SECONDS: f64 = 1.0;
/// How far into the file a track's first packet is looked for.
pub const LATE_AUDIO_SEARCH_SECONDS: f64 = 120.0;
const STILL_IMAGE_CODECS: [&str; 5] = ["mjpeg", "png", "bmp", "gif", "webp"];

pub fn parse_main(json: &str) -> Result<Main, String> {
    serde_json::from_str(json).map_err(|e| e.to_string())
}

fn index(s: &Stream) -> u32 {
    s.index.unwrap_or(0.0) as u32
}

/// Programme video streams in container order: cover art never plays and a still beside a
/// moving track is a thumbnail.
pub fn programme_video(streams: &[Stream]) -> Vec<&Stream> {
    let video: Vec<&Stream> = streams
        .iter()
        .filter(|s| {
            s.codec_type.as_deref() == Some("video") && s.disposition.attached_pic != Some(1.0)
        })
        .collect();
    let moving: Vec<&Stream> = video
        .iter()
        .copied()
        .filter(|s| {
            !STILL_IMAGE_CODECS.contains(
                &s.codec_name
                    .clone()
                    .unwrap_or_default()
                    .to_lowercase()
                    .as_str(),
            )
        })
        .collect();
    if moving.is_empty() { video } else { moving }
}

/// Packet-measured frame-rate plan: the programme stream and the window start.
pub fn fps_plan(main: &Main) -> Option<(u32, u32)> {
    let duration = duration_seconds(main).filter(|d| *d != 0.0)?;
    if duration < MIN_MEASURABLE_SECONDS {
        return None;
    }
    let first = programme_video(&main.streams).into_iter().next()?;
    let from = (duration / 2.0 - FPS_WINDOW_SECONDS / 2.0).floor().max(0.0);
    Some((index(first), from as u32))
}

/// The stream whose first frame is probed.
pub fn first_frame_stream(main: &Main) -> Option<u32> {
    programme_video(&main.streams).first().map(|s| index(s))
}

pub fn audio_indices(main: &Main) -> Vec<u32> {
    main.streams
        .iter()
        .filter(|s| s.codec_type.as_deref() == Some("audio"))
        .map(index)
        .collect()
}

pub fn duration_seconds(main: &Main) -> Option<f64> {
    non_empty(&main.format.duration).and_then(number)
}

fn non_empty(v: &Option<String>) -> Option<&str> {
    v.as_deref().filter(|s| !s.is_empty())
}

/// Frame rate from the packet cadence of a mid-file window (`measureFrameRate`).
pub fn measure_fps(packets: &str) -> Option<f64> {
    let mut times: Vec<f64> = packets.split('\n').filter_map(parse_float).collect();
    if times.len() < MIN_SAMPLE_FRAMES {
        return None;
    }
    times.sort_by(f64::total_cmp);
    let span = times[times.len() - 1] - times[0];
    if span <= 0.0 {
        return None;
    }
    let fps = (times.len() - 1) as f64 / span;
    (1.0..=480.0).contains(&fps).then_some(fps)
}

pub(super) fn format_fps(fps: f64) -> String {
    let s = to_fixed3(fps);
    let s = s.trim_end_matches('0');
    s.strip_suffix('.').unwrap_or(s).to_string()
}

fn parse_frame_rate(rate: Option<&str>, avg: Option<&str>) -> Option<String> {
    let normalise = |r: Option<&str>| -> Option<String> {
        let r = r.filter(|r| !r.is_empty() && *r != "0/0")?;
        let mut parts = r.split('/');
        let num = parts.next().and_then(number).filter(|n| *n != 0.0);
        let den = parts.next().and_then(number).filter(|n| *n != 0.0);
        let (Some(num), Some(den)) = (num, den) else {
            return Some(r.to_string());
        };
        let fps = num / den;
        (fps > 0.0).then(|| format_fps(fps))
    };
    normalise(rate).or_else(|| normalise(avg))
}

fn reconcile_frame_rate(declared: Option<String>, measured: Option<f64>) -> Option<String> {
    let (Some(d), Some(m)) = (&declared, measured) else {
        return declared;
    };
    let Some(declared_fps) = number(d).filter(|f| *f > 0.0) else {
        return declared;
    };
    if (m - declared_fps).abs() / m <= FPS_DISAGREEMENT {
        return declared;
    }
    Some(format_fps(m))
}

/// An `h:mm:ss[.fff]` DURATION tag in seconds.
fn clock_seconds(s: &str) -> Option<f64> {
    let mut parts = s.split(':');
    let (h, m, sec) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    let (whole, frac) = sec
        .split_once('.')
        .map_or((sec, None), |(a, b)| (a, Some(b)));
    if !(digits(h) && m.len() == 2 && digits(m) && whole.len() == 2 && digits(whole))
        || frac.is_some_and(|f| !digits(f))
    {
        return None;
    }
    Some(number(h)? * 3600.0 + number(m)? * 60.0 + number(sec)?)
}

/// Where a stream ends: Matroska's `DURATION` tag is the end even for a late track; other
/// containers give a duration from the stream start.
fn stream_end_seconds(s: &Stream) -> Option<f64> {
    if let Some(c) = s.tags.get("DURATION").and_then(clock_seconds) {
        return Some(c);
    }
    let duration = number(non_empty(&s.duration)?)?;
    let start = match non_empty(&s.start_time) {
        Some(v) => number(v)?,
        None => 0.0,
    };
    Some(start + duration)
}

fn start_seconds(s: &Stream) -> Option<f64> {
    non_empty(&s.start_time).and_then(number)
}

fn rotation(s: &Stream) -> f64 {
    let matrix = s
        .side_data_list
        .iter()
        .find_map(|d| d.get("rotation")?.as_f64());
    let raw = matrix.or_else(|| {
        Some(
            s.tags
                .get("rotate")
                .map_or(0.0, |t| number(t).unwrap_or(f64::NAN)),
        )
    });
    raw.filter(|r| r.is_finite()).unwrap_or(0.0)
}

fn derive_hdr(transfer: Option<&str>, primaries: Option<&str>) -> Option<HdrFormat> {
    let bt2020 = primaries == Some("bt2020");
    match transfer? {
        "smpte2084" if bt2020 => Some(HdrFormat::Hdr10),
        "arib-std-b67" if bt2020 => Some(HdrFormat::Hlg),
        _ => None,
    }
}

fn resolve_language(s: &Stream) -> String {
    match s.tags.get("language") {
        Some(l) if !l.is_empty() && l.to_lowercase() != "und" => l.to_string(),
        _ => language_from_title(s.tags.get("title"))
            .unwrap_or("und")
            .to_string(),
    }
}

fn subtitle_flags(s: &Stream) -> (bool, bool) {
    let (forced, hi) = subtitle_flags_from_title(s.tags.get("title"));
    (
        s.disposition.forced == Some(1.0) || forced,
        s.disposition.hearing_impaired == Some(1.0) || hi,
    )
}

/// `scaleMetaValue`: a rational or decimal scaled to its integer SEI unit.
fn scale_meta(raw: Option<&Value>, unit: f64) -> Option<f64> {
    match raw? {
        Value::Number(n) => Some(round(n.as_f64()? * unit)),
        Value::String(s) => {
            let (n, d) = s
                .split_once('/')
                .map_or((s.as_str(), None), |(n, d)| (n, Some(d)));
            let num = number(n)?;
            let den = match d {
                Some(d) => number(d)?,
                None => 1.0,
            };
            (den != 0.0).then(|| round(num / den * unit))
        }
        _ => None,
    }
}

fn side_data_of<'a>(list: &'a [Value], needle: &str) -> Option<&'a Value> {
    list.iter().find(|d| {
        d.get("side_data_type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase()
            .contains(needle)
    })
}

/// `parseHdrStaticMetadata`: HDR10 mastering display and content light level of a frame.
pub fn hdr_static_metadata(side_data: &[Value]) -> Option<HdrMetadata> {
    let md = side_data_of(side_data, "mastering display")?;
    let cl = side_data_of(side_data, "content light");
    let g = |k: &str, unit: f64| scale_meta(md.get(k), unit);
    let (gx, gy, bx, by) = (
        g("green_x", 50000.0)?,
        g("green_y", 50000.0)?,
        g("blue_x", 50000.0)?,
        g("blue_y", 50000.0)?,
    );
    let (rx, ry, wx, wy) = (
        g("red_x", 50000.0)?,
        g("red_y", 50000.0)?,
        g("white_point_x", 50000.0)?,
        g("white_point_y", 50000.0)?,
    );
    let (max_l, min_l) = (g("max_luminance", 10000.0)?, g("min_luminance", 10000.0)?);
    let level = |k: &str| {
        cl.and_then(|c| c.get(k))
            .filter(|v| !v.is_null())
            .and_then(|v| match v {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => number(s),
                _ => None,
            })
            .map_or(0, |v| v as u32)
    };
    Some(HdrMetadata {
        mastering_display: format!(
            "G({gx},{gy})B({bx},{by})R({rx},{ry})WP({wx},{wy})L({max_l},{min_l})"
        ),
        max_cll: level("max_content"),
        max_fall: level("max_average"),
    })
}

/// `hasHdr10PlusSideData`.
pub fn has_hdr10_plus(side_data: &[Value]) -> bool {
    side_data.iter().any(|d| {
        d.get("side_data_type").and_then(Value::as_str)
            == Some("HDR Dynamic Metadata SMPTE2094-40 (HDR10+)")
    })
}

/// A frame timestamp in seconds, exact from its integer ticks and time base.
fn ticks_to_seconds(ticks: Option<f64>, time_base: Option<&str>) -> Option<f64> {
    let mut it = time_base.unwrap_or("").split('/');
    let num = it.next().and_then(number).unwrap_or(f64::NAN);
    let den = it.next().and_then(number).unwrap_or(f64::NAN);
    let ticks = ticks?;
    (num > 0.0 && den > 0.0).then(|| ticks * num / den)
}

/// Facts gathered by the extra tool runs.
#[derive(Default)]
pub struct Extras {
    pub measured_fps: Option<f64>,
    pub first_frame: Option<Frame>,
    /// First packet time of each audio stream that showed one.
    pub audio_firsts: BTreeMap<u32, f64>,
}

/// An audio stream missing its parameters: they need a longer read of the file.
pub struct LateAudio {
    pub reach_seconds: f64,
}

pub struct Built {
    pub info: MediaInfo,
    pub late_audio: Option<LateAudio>,
}

fn u32_of(v: Option<f64>) -> Option<u32> {
    v.map(|v| v as u32)
}

pub fn build(main: &Main, x: &Extras) -> Built {
    let duration = duration_seconds(main);
    let format_start = non_empty(&main.format.start_time).and_then(number);
    let programme = programme_video(&main.streams);
    let format_bit_rate = non_empty(&main.format.bit_rate)
        .and_then(number)
        .filter(|b| *b > 0.0);
    let first_index = programme.first().map(|s| index(s));

    let mut video: Vec<VideoStreamInfo> = programme
        .iter()
        .map(|s| {
            let dovi = s
                .side_data_list
                .iter()
                .find(|d| d.get("dv_profile").is_some_and(Value::is_number));
            let dv = |k: &str| dovi.and_then(|d| d.get(k)).and_then(Value::as_f64);
            let flag = |k: &str| dv(k) == Some(1.0);
            let rot = rotation(s);
            let (w, h) = if rot.abs() % 180.0 == 90.0 {
                (s.height, s.width)
            } else {
                (s.width, s.height)
            };
            let declared = parse_frame_rate(s.r_frame_rate.as_deref(), s.avg_frame_rate.as_deref());
            VideoStreamInfo {
                stream_index: index(s),
                codec: s.codec_name.clone().unwrap_or_else(|| "unknown".into()),
                profile: s.profile.clone(),
                level: s.level.map(|l| l as i32),
                width: u32_of(w),
                height: u32_of(h),
                display_aspect_ratio: s.display_aspect_ratio.clone(),
                pixel_format: s.pix_fmt.clone(),
                frame_rate: reconcile_frame_rate(
                    declared,
                    if Some(index(s)) == first_index {
                        x.measured_fps
                    } else {
                        None
                    },
                ),
                reorder_frames: u32_of(s.has_b_frames),
                avg_frame_rate: s.avg_frame_rate.clone(),
                start_time_seconds: start_seconds(s),
                end_seconds: stream_end_seconds(s),
                bit_rate: u32_of(non_empty(&s.bit_rate).and_then(number)),
                bit_depth: u32_of(non_empty(&s.bits_per_raw_sample).and_then(number)),
                color_space: s.color_space.clone(),
                color_transfer: s.color_transfer.clone(),
                color_primaries: s.color_primaries.clone(),
                color_range: s.color_range.clone(),
                hdr_format: derive_hdr(s.color_transfer.as_deref(), s.color_primaries.as_deref()),
                dv_profile: u32_of(dv("dv_profile")),
                dv_bl_signal_compat_id: u32_of(dv("dv_bl_signal_compatibility_id")),
                dv_level: u32_of(dv("dv_level")),
                dv_rpu_present: Some(flag("rpu_present_flag")),
                dv_bl_present: Some(flag("bl_present_flag")),
                dv_el_present: Some(flag("el_present_flag")),
                ..VideoStreamInfo::default()
            }
        })
        .collect();

    if let (Some(v), Some(s)) = (video.first_mut(), programme.first()) {
        let frame = x.first_frame.as_ref();
        v.first_frame_seconds = frame.map(|f| {
            ticks_to_seconds(f.pts.or(f.best_effort_timestamp), s.time_base.as_deref())
                .unwrap_or(0.0)
        });
        let side = frame.map_or(&[][..], |f| f.side_data_list.as_slice());
        if v.hdr_format == Some(HdrFormat::Hdr10) {
            v.hdr_metadata = hdr_static_metadata(side);
        }
        v.hdr10_plus = Some(has_hdr10_plus(side));
    }

    let mut audio: Vec<AudioStreamInfo> = main
        .streams
        .iter()
        .filter(|s| s.codec_type.as_deref() == Some("audio"))
        .map(|s| AudioStreamInfo {
            stream_index: index(s),
            codec: s.codec_name.clone().unwrap_or_else(|| "unknown".into()),
            language: resolve_language(s),
            title: s.tags.get("title").map(String::from),
            channels: u32_of(s.channels),
            channel_layout: s.channel_layout.clone(),
            sample_rate: u32_of(non_empty(&s.sample_rate).and_then(number)),
            bit_rate: u32_of(non_empty(&s.bit_rate).and_then(number)),
            profile: s.profile.clone(),
            is_default: Some(s.disposition.default == Some(1.0)),
            start_time_seconds: start_seconds(s),
            end_seconds: stream_end_seconds(s),
            commentary: (s.disposition.comment == Some(1.0)).then_some(true),
            audio_description: (s.disposition.visual_impaired == Some(1.0)
                || s.disposition.descriptions == Some(1.0))
            .then_some(true),
            language_tag: None,
        })
        .collect();

    // The header errs early only: a first packet past it is the real start.
    for a in &mut audio {
        if let Some(&first) = x.audio_firsts.get(&a.stream_index)
            && first > a.start_time_seconds.unwrap_or(f64::NEG_INFINITY)
        {
            a.start_time_seconds = Some(first);
        }
    }
    let blind: Vec<&AudioStreamInfo> = audio
        .iter()
        .filter(|a| a.channels.unwrap_or(0) == 0 || a.sample_rate.unwrap_or(0) == 0)
        .collect();
    let late_audio = (!blind.is_empty()).then(|| {
        let latest = blind
            .iter()
            .map(|a| a.start_time_seconds.unwrap_or(0.0))
            .fold(f64::NEG_INFINITY, f64::max);
        LateAudio {
            reach_seconds: (latest - format_start.unwrap_or(0.0)).max(0.0)
                + LATE_AUDIO_PARAMS_MARGIN_SECONDS,
        }
    });

    let subtitles: Vec<SubtitleStreamInfo> = main
        .streams
        .iter()
        .filter(|s| s.codec_type.as_deref() == Some("subtitle"))
        .map(|s| {
            let (forced, hearing_impaired) = subtitle_flags(s);
            SubtitleStreamInfo {
                stream_index: index(s),
                codec: s.codec_name.clone().unwrap_or_else(|| "unknown".into()),
                language: resolve_language(s),
                language_tag: None,
                title: s.tags.get("title").map(String::from),
                forced,
                hearing_impaired,
                is_image_based: is_image_based(s.codec_name.as_deref()),
            }
        })
        .collect();

    let chapters: Vec<Chapter> = main
        .chapters
        .iter()
        .map(|c| {
            (
                non_empty(&c.start_time).map_or(Some(0.0), number),
                non_empty(&c.end_time).map_or(Some(0.0), number),
                c,
            )
        })
        .filter_map(|(start, end, c)| {
            let (start, end) = (start?, end?);
            (end > start).then(|| Chapter {
                start_seconds: start,
                end_seconds: end,
                title: c.tags.get("title").map(String::from),
            })
        })
        .collect();

    let no_streams = video.is_empty() && audio.is_empty();
    let info = MediaInfo {
        video: if no_streams { vec![] } else { video },
        audio,
        subtitles: if no_streams { vec![] } else { subtitles },
        format_bit_rate: u32_of(format_bit_rate),
        format_start_seconds: format_start,
        format_name: main.format.format_name.clone(),
        duration_seconds: duration,
        chapters: Some(chapters),
        error: no_streams.then(|| "No streams detected".to_string()),
        ..MediaInfo::default()
    };
    Built {
        info,
        late_audio: (!no_streams).then_some(late_audio).flatten(),
    }
}

/// Parameters of audio streams that ffprobe reported missing, from a longer read.
pub fn apply_late_audio(info: &mut MediaInfo, stdout: &str) {
    let Ok(found) = serde_json::from_str::<super::ffjson::AudioParams>(stdout) else {
        return;
    };
    for a in &mut info.audio {
        if a.channels.unwrap_or(0) != 0 && a.sample_rate.unwrap_or(0) != 0 {
            continue;
        }
        let Some(s) = found
            .streams
            .iter()
            .find(|s| s.index.map(|i| i as u32) == Some(a.stream_index))
        else {
            continue;
        };
        if let Some(c) = s.channels.filter(|c| *c != 0.0) {
            a.channels = Some(c as u32);
        }
        if let Some(l) = s.channel_layout.as_deref().filter(|l| !l.is_empty()) {
            a.channel_layout = Some(l.into());
        }
        if let Some(r) = non_empty(&s.sample_rate)
            .and_then(number)
            .filter(|r| *r > 0.0)
        {
            a.sample_rate = Some(r as u32);
        }
    }
}

/// The empty error blob the host stores when ffprobe could not read the file.
pub fn failure(message: &str) -> MediaInfo {
    MediaInfo {
        error: Some(message.to_string()),
        ..MediaInfo::default()
    }
}

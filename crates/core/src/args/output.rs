//! Layer 2: what a run writes (HLS VOD, remux GOP files, live window, trick-play frame, plain file).
use super::encode::{
    AudioEnc, DEFAULT_AUDIO_PLAN, audio_copy_args, audio_encode_args, audio_map, audio_stream_args,
    encode, live_input, live_video, probe_args, s, video_map,
};
use super::filters::crop_filter;
use super::{AudioStream, FileArgs, FileFormat, IframeArgs, LiveArgs, RemuxArgs, VodArgs};
use crate::api::{AudioCodec, AudioPlan, HostCaps};
use crate::decide::dolby_vision::has_no_base;
use crate::hw::Device;
use crate::jsnum::{js_string as js, round, seconds, to_fixed};

/// Under `-hls_time` so the muxer cuts on every forced IDR.
const HLS_TIME_MARGIN: f64 = 0.0001;
pub const LIVE_USER_AGENT: &str = "VLC/3.0.20 LibVLC/3.0.20";
/// The top rung's stereo budget.
const REMUX_STEREO_BPS: f64 = 192_000.0;

/// Forward slashes: ffmpeg's HLS muxer derives the fMP4 init directory with POSIX separators.
fn out_path(parts: &[&str]) -> String {
    parts.join("/").replace('\\', "/")
}

/// `Math.max` / `Math.min`: NaN wins.
fn js_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        a.max(b)
    }
}

/// Headroom every MPEG-TS run shares so no timestamp drops below 0.
fn ts_headroom(origin: f64) -> f64 {
    (1.0 - origin).max(0.0)
}

struct Muxer<'a> {
    use_ts: bool,
    hls_time: String,
    start_number: u32,
    output_seek: f64,
    init: &'a str,
    var_stream_map: Option<String>,
    segment: String,
    index: String,
    origin: f64,
    source_timestamps: bool,
    list_size: u32,
    dolby_vision: bool,
}

/// The shared HLS muxer tail. [ts:backend/src/modules/streaming/transcoding/ffmpeg-args.ts:191-255]
fn hls_muxer(m: Muxer) -> Vec<String> {
    let origin = if m.use_ts { 0.0 } else { m.origin };
    let mut a = vec![];
    if !m.use_ts {
        a.extend(s(&["-movflags", "+cmaf", "-avoid_negative_ts", "disabled"]));
    }
    if m.source_timestamps {
        let strict = if m.dolby_vision {
            ":strict=unofficial"
        } else {
            ""
        };
        a.extend([
            "-hls_segment_options".into(),
            format!("movflags=+frag_discont{strict}"),
        ]);
    }
    if !m.source_timestamps && origin != 0.0 && m.output_seek == 0.0 {
        a.extend(["-output_ts_offset".into(), seconds(-origin)]);
    }
    let ts_offset = m.output_seek + ts_headroom(m.origin);
    if m.use_ts && ts_offset > 0.0 {
        a.extend(["-output_ts_offset".into(), seconds(ts_offset)]);
    }
    a.extend([
        "-f".into(),
        "hls".into(),
        "-hls_time".into(),
        m.hls_time,
        "-hls_list_size".into(),
        m.list_size.to_string(),
        "-start_number".into(),
        m.start_number.to_string(),
        "-hls_segment_type".into(),
        if m.use_ts { "mpegts" } else { "fmp4" }.into(),
    ]);
    if !m.use_ts {
        a.extend(["-hls_fmp4_init_filename".into(), m.init.into()]);
    }
    a.extend(s(&["-hls_flags", "independent_segments+temp_file"]));
    if let Some(v) = m.var_stream_map {
        a.extend(["-var_stream_map".into(), v]);
    }
    a.extend(["-hls_segment_filename".into(), m.segment, m.index]);
    a
}

fn plans_for(n: usize, plans: &[AudioPlan]) -> Result<(), String> {
    if plans.len() == n {
        Ok(())
    } else {
        Err(format!("{} audio plans for {n} audio streams", plans.len()))
    }
}

/// One rendition per track for several tracks of a video-only run. [ts:backend/src/modules/streaming/transcoding/audio-layout.ts:20-25]
fn var_stream_map_layout(video_only: bool, streams: Option<&Vec<AudioStream>>) -> bool {
    video_only && streams.is_some_and(|s| s.len() > 1)
}

/// A transcode run into HLS. [ts:backend/src/modules/streaming/transcoding/ffmpeg-args.ts:539-656]
pub fn vod(o: &VodArgs, caps: &HostCaps, dev: &Device) -> Result<Vec<String>, String> {
    let e = encode(o, caps, dev)?;
    let mut a = e.args;
    let plan = o.audio_plan.as_ref().unwrap_or(&DEFAULT_AUDIO_PLAN);
    let audio_args = audio_stream_args("", plan, &e.audio);
    let ext = if o.use_ts { "ts" } else { "m4s" };
    let dir = o.output_dir.as_str();
    let streams = o.audio_streams.as_deref();
    if var_stream_map_layout(o.video_only, o.audio_streams.as_ref()) {
        let streams = streams.unwrap();
        a.extend(["-map".into(), e.video_map]);
        for i in 0..streams.len() {
            a.extend(["-map".into(), audio_map(Some(streams), i)]);
        }
        match &o.audio_track_plans {
            Some(plans) => {
                plans_for(streams.len(), plans)?;
                for (i, p) in plans.iter().enumerate() {
                    a.extend(audio_stream_args(&format!(":{i}"), p, &e.audio));
                }
            }
            None => a.extend(audio_args),
        }
        let mut map = vec!["v:0,agroup:audio".to_string()];
        for (i, st) in streams.iter().enumerate() {
            let lang = st
                .language
                .as_deref()
                .filter(|l| !l.is_empty())
                .unwrap_or("und");
            map.push(format!("a:{i},agroup:audio,language:{lang}"));
        }
        a.extend(hls_muxer(Muxer {
            use_ts: o.use_ts,
            hls_time: seconds(e.real_segment - HLS_TIME_MARGIN),
            start_number: o.start_segment,
            output_seek: e.output_seek,
            init: "init_%v.mp4",
            var_stream_map: Some(map.join(" ")),
            segment: out_path(&[dir, "%v", &format!("seg-%04d.{ext}")]),
            index: out_path(&[dir, "%v", "index.m3u8"]),
            origin: e.origin,
            source_timestamps: false,
            list_size: 0,
            dolby_vision: false,
        }));
        return Ok(a);
    }
    if streams.is_some_and(|s| s.is_empty()) {
        a.extend(["-map".into(), e.video_map, "-an".into()]);
    } else {
        let picked = o.audio_stream_index.unwrap_or(0) as usize;
        a.extend([
            "-map".into(),
            e.video_map,
            "-map".into(),
            audio_map(streams, picked),
        ]);
        a.extend(audio_args);
    }
    a.extend(hls_muxer(Muxer {
        use_ts: o.use_ts,
        hls_time: js(e.segment_duration),
        start_number: o.start_segment,
        output_seek: e.output_seek,
        init: "init.mp4",
        var_stream_map: None,
        segment: out_path(&[dir, &format!("seg-%04d.{ext}")]),
        index: out_path(&[dir, "index.m3u8"]),
        origin: e.origin,
        source_timestamps: false,
        list_size: 0,
        dolby_vision: false,
    }));
    Ok(a)
}

/// A copy-video run cut into one file per GOP. [ts:backend/src/modules/streaming/transcoding/ffmpeg-args.ts:1531-1733]
pub fn remux(o: &RemuxArgs, caps: &HostCaps) -> Result<Vec<String>, String> {
    let mut a = s(&["-hide_banner", "-loglevel", "warning"]);
    a.extend(probe_args(o.trusted_stream_info));
    let seek = o.run.seek_seconds.map(seconds);
    if let Some(ss) = &seek {
        a.extend(s(&["-noaccurate_seek", "-seek_timestamp", "1", "-ss", ss]));
    }
    let audio = AudioEnc {
        stereo_bps: REMUX_STEREO_BPS,
        align_start: o.run.audio_start_seconds,
        end: o.source_end_seconds,
        use_ts: false,
        libfdk_aac: caps.libfdk_aac,
    };
    let plan = o.audio_plan.as_ref().unwrap_or(&DEFAULT_AUDIO_PLAN);
    a.extend(["-i".into(), o.input_path.clone()]);
    a.extend(s(&["-copyts", "-muxdelay", "0", "-muxpreload", "0"]));
    if let Some(ss) = &seek {
        a.extend(["-ss".into(), ss.clone()]);
    }
    let end = o.source_clock_break_seconds.map(seconds);
    if let Some(to) = &end {
        a.extend(["-to".into(), to.clone()]);
    }

    let codec = o.source_video_codec.as_deref();
    let dv_no_base_tag =
        o.dolby_vision && has_no_base(o.source_dv_profile, o.source_dv_bl_signal_compat_id);
    let strip_hdr10_plus = o.dolby_vision && o.hdr10_plus;
    // A P7 remux never carries a DV box, so its enhancement layer is dead weight.
    let strip_dovi_el = o.source_dv_profile == Some(7) && !o.dolby_vision;
    let mut video = vec![];
    if codec == Some("hevc") {
        let mut meta = vec![];
        if strip_dovi_el {
            meta.push("remove_dovi=1");
        }
        if strip_hdr10_plus {
            meta.push("remove_hdr10plus=1");
        }
        let bsf = if meta.is_empty() {
            "hevc_mp4toannexb".to_string()
        } else {
            format!("hevc_mp4toannexb,hevc_metadata={}", meta.join(":"))
        };
        video.extend(s(&[
            "-tag:v",
            if dv_no_base_tag { "dvh1" } else { "hvc1" },
            "-bsf:v",
            &bsf,
            "-max_muxing_queue_size",
            "2048",
        ]));
    }
    if codec == Some("av1") && dv_no_base_tag {
        video.extend(s(&["-tag:v", "dav1"]));
    }
    if codec == Some("av1") && strip_hdr10_plus {
        video.extend(s(&["-bsf:v", "av1_metadata=remove_hdr10plus=1"]));
    }

    let streams = o.audio_streams.as_deref();
    let multi = var_stream_map_layout(o.video_only, o.audio_streams.as_ref());
    let vmap = video_map(o.video_stream_index);
    if multi || streams.is_some_and(|s| s.is_empty()) {
        a.extend(["-map".into(), vmap, "-c:v".into(), "copy".into()]);
        if !multi {
            a.push("-an".into());
        }
    } else {
        a.extend(["-map".into(), vmap]);
        a.extend([
            "-map".into(),
            audio_map(streams, o.audio_stream_index.unwrap_or(0) as usize),
        ]);
        a.extend(s(&["-c:v", "copy"]));
        a.extend(audio_stream_args("", plan, &audio));
    }
    a.extend(video);
    let dir = o.output_dir.as_str();
    a.extend(hls_muxer(Muxer {
        use_ts: false,
        hls_time: if o.grid.is_some() {
            "0".into()
        } else {
            js(o.segment_duration.unwrap_or(3.0))
        },
        start_number: o.run.start_number,
        output_seek: o.run.seek_seconds.unwrap_or(0.0),
        init: "init.mp4",
        var_stream_map: None,
        segment: out_path(&[dir, "gop-%d.m4s"]),
        index: out_path(&[dir, "index.m3u8"]),
        origin: 0.0,
        source_timestamps: true,
        list_size: 1,
        dolby_vision: o.dolby_vision,
    }));
    if multi {
        let streams = streams.unwrap();
        let fallback: Vec<AudioPlan>;
        let plans = match &o.audio_track_plans {
            Some(p) => p,
            None => {
                fallback = vec![plan.clone(); streams.len()];
                &fallback
            }
        };
        plans_for(streams.len(), plans)?;
        for (i, p) in plans.iter().enumerate() {
            if let Some(ss) = &seek {
                a.extend(["-ss".into(), ss.clone()]);
            }
            if let Some(to) = &end {
                a.extend(["-to".into(), to.clone()]);
            }
            a.extend(["-map".into(), audio_map(streams.into(), i)]);
            a.extend(audio_stream_args("", p, &audio));
            a.extend(s(&[
                "-f",
                "mp4",
                "-movflags",
                "+frag_every_frame+delay_moov+default_base_moof+frag_discont",
                "-map_chapters",
                "-1",
            ]));
            a.push(out_path(&[dir, &format!("a{i}.mp4")]));
        }
    }
    Ok(a)
}

/// `-user_agent` (VLC's when empty) and `-headers` of an ffmpeg or ffprobe reading an upstream.
/// [ts:backend/src/modules/livetv/livetv-http.ts:36,64-68]
pub fn http_header_args(
    user_agent: Option<&str>,
    referer: Option<&str>,
    extra: &[(String, String)],
) -> Vec<String> {
    let ua = user_agent
        .filter(|u| !u.is_empty())
        .unwrap_or(LIVE_USER_AGENT);
    let mut a = vec!["-user_agent".to_string(), ua.to_string()];
    let mut headers: String = referer
        .filter(|r| !r.is_empty())
        .map(|r| format!("Referer: {r}\r\n"))
        .unwrap_or_default();
    for (k, v) in extra {
        headers.push_str(&format!("{k}: {v}\r\n"));
    }
    if !headers.is_empty() {
        a.extend(["-headers".into(), headers]);
    }
    a
}

/// A live channel into ffmpeg's sliding-window HLS muxer.
/// [ts:backend/src/modules/livetv/services/live-ffmpeg-args.ts:99-208]
pub fn live(o: &LiveArgs, caps: &HostCaps, dev: &Device) -> Vec<String> {
    let mut a = s(&[
        "-reconnect",
        "1",
        "-reconnect_streamed",
        "1",
        "-reconnect_on_network_error",
        "1",
        "-reconnect_delay_max",
        "5",
        "-rw_timeout",
        "15000000",
    ]);
    a.extend(http_header_args(
        o.user_agent.as_deref(),
        o.referer.as_deref(),
        &o.extra_headers,
    ));
    let probe = js_max(3.0, o.probe_seconds.unwrap_or(3.0));
    a.extend([
        "-analyzeduration".into(),
        js(probe * 1_000_000.0),
        "-probesize".into(),
        js(round(5_000_000.0 * probe / 3.0)),
    ]);
    if o.mode == super::LiveMode::Transcode {
        a.extend(live_input(o.hw_accel, dev));
    }
    a.extend(["-i".into(), o.input_url.clone()]);
    a.extend(s(&["-map", "0:v:0", "-map", "0:a:0?", "-sn", "-dn"]));
    a.extend(live_video(o));
    // A broadcast switches AAC layouts at programme boundaries: ADTS in TS says so, fMP4 cannot.
    if o.audio_codec.as_deref() == Some("aac") && o.use_ts {
        a.extend(audio_copy_args("", "aac", true));
    } else {
        a.extend(audio_encode_args(
            "",
            AudioCodec::Aac,
            2,
            128_000.0,
            caps.libfdk_aac,
        ));
    }
    let list = js_max(1.0, (o.window_minutes * 60.0 / o.segment_seconds).ceil());
    let ext = if o.use_ts { "ts" } else { "m4s" };
    let mut flags =
        "delete_segments+omit_endlist+independent_segments+temp_file+program_date_time".to_string();
    if o.append {
        flags.push_str("+append_list+discont_start");
    }
    a.extend([
        "-f".into(),
        "hls".into(),
        "-hls_time".into(),
        js(o.segment_seconds),
        "-hls_list_size".into(),
        js(list),
        "-hls_delete_threshold".into(),
        "3".into(),
        "-hls_segment_type".into(),
        if o.use_ts { "mpegts" } else { "fmp4" }.into(),
    ]);
    if !o.use_ts {
        a.extend(s(&["-hls_fmp4_init_filename", "init.mp4"]));
    }
    a.extend([
        "-hls_flags".into(),
        flags,
        "-hls_segment_filename".into(),
        format!("{}/seg-%05d.{ext}", o.output_dir),
        format!("{}/index.m3u8", o.output_dir),
    ]);
    a
}

/// One keyframe at `seekSeconds`, MPEG-TS on stdout. [ts:backend/src/modules/streaming/transcoding/iframe-trick-play.ts:44-89]
pub fn iframe(o: &IframeArgs) -> Vec<String> {
    let crop = o
        .crop
        .as_ref()
        .map_or(String::new(), |c| format!("{},", crop_filter(c)));
    let mut a = s(&["-hide_banner", "-loglevel", "error", "-ss"]);
    a.push(to_fixed(o.seek_seconds, 3));
    a.extend(s(&["-copyts", "-i", &o.input_path]));
    a.extend(s(&["-frames:v", "1", "-an", "-sn", "-dn", "-vf"]));
    a.push(format!("{crop}scale={}:{}", o.width, o.height));
    a.extend(s(&[
        "-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p",
    ]));
    if let Some(off) = o
        .timeline_offset_seconds
        .filter(|x| *x != 0.0 && !x.is_nan())
    {
        a.extend(["-output_ts_offset".into(), js(off)]);
    }
    a.extend(s(&[
        "-f",
        "mpegts",
        "-muxdelay",
        "0",
        "-muxpreload",
        "0",
        "pipe:1",
    ]));
    a
}

/// A whole-file conversion: the same layer 1 as HLS, written to one MP4 or MKV. [new]
pub fn file(f: &FileArgs, caps: &HostCaps, dev: &Device) -> Result<Vec<String>, String> {
    let o = &f.encode;
    let e = encode(o, caps, dev)?;
    let mut a = e.args;
    a.extend(["-map".into(), e.video_map]);
    let streams = o.audio_streams.as_deref();
    let plan = o.audio_plan.as_ref().unwrap_or(&DEFAULT_AUDIO_PLAN);
    match streams {
        Some([]) => a.push("-an".into()),
        Some(all) if f.all_audio => {
            let plans: Vec<&AudioPlan> = match &o.audio_track_plans {
                Some(p) => {
                    plans_for(all.len(), p)?;
                    p.iter().collect()
                }
                None => vec![plan; all.len()],
            };
            for i in 0..all.len() {
                a.extend(["-map".into(), audio_map(streams, i)]);
            }
            for (i, p) in plans.into_iter().enumerate() {
                a.extend(audio_stream_args(&format!(":{i}"), p, &e.audio));
            }
        }
        _ => {
            a.extend([
                "-map".into(),
                audio_map(streams, o.audio_stream_index.unwrap_or(0) as usize),
            ]);
            a.extend(audio_stream_args("", plan, &e.audio));
        }
    }
    match f.format {
        FileFormat::Mp4 => a.extend(s(&["-f", "mp4", "-movflags", "+faststart"])),
        FileFormat::Mkv => a.extend(s(&["-f", "matroska"])),
    }
    a.push(f.path.clone());
    Ok(a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Platform;
    use serde_json::json;

    fn request(audio: serde_json::Value) -> VodArgs {
        serde_json::from_value(json!({
            "inputPath": "/media/in.mkv",
            "outputDir": "/unused",
            "hwAccel": "none",
            "profile": { "name": "1080p", "maxWidth": 1920, "maxHeight": 1080,
                         "videoBitrate": "8M", "audioBitrate": "192k" },
            "videoVariant": { "codec": "h264", "bitDepth": 8, "hdr": null },
            "sourceWidth": 1920, "sourceHeight": 1080, "sourceFps": 24,
            "trustedStreamInfo": true,
            "audioStreams": audio,
            "audioTrackPlans": [
                { "mode": "copy", "codec": "eac3" },
                { "mode": "transcode", "codec": "aac", "channels": 2 }
            ]
        }))
        .unwrap()
    }

    fn dev() -> Device {
        Device {
            platform: Platform::Linux,
            render_node: None,
            opencl_device: None,
        }
    }

    #[test]
    fn file_output_reuses_layer_one() {
        let caps = HostCaps::default();
        let o = request(json!([{ "streamIndex": 1 }, { "streamIndex": 2 }]));
        let layer1 = encode(&o, &caps, &dev()).unwrap().args;
        let mp4 = FileArgs {
            encode: o.clone(),
            path: "/out/movie.mp4".into(),
            format: FileFormat::Mp4,
            all_audio: false,
        };
        let got = file(&mp4, &caps, &dev()).unwrap();
        assert_eq!(got[..layer1.len()], layer1[..]);
        assert_eq!(
            got[layer1.len()..],
            s(&[
                "-map",
                "0:v:0",
                "-map",
                "0:1",
                "-c:a",
                "aac",
                "-b:a",
                "192k",
                "-ac",
                "2",
                "-ar",
                "48000",
                "-filter:a",
                "asetpts=PTS-0/TB,aresample=async=1:first_pts=0,asetpts=PTS+0/TB",
                "-f",
                "mp4",
                "-movflags",
                "+faststart",
                "/out/movie.mp4",
            ])[..]
        );
        // The HLS run of the same request shares the whole layer-1 prefix.
        assert_eq!(vod(&o, &caps, &dev()).unwrap()[..layer1.len()], layer1[..]);

        let mkv = FileArgs {
            format: FileFormat::Mkv,
            all_audio: true,
            path: "/out/movie.mkv".into(),
            ..mp4
        };
        let got = file(&mkv, &caps, &dev()).unwrap();
        let tail = got[layer1.len()..].join(" ");
        assert!(tail.starts_with("-map 0:v:0 -map 0:1 -map 0:2 -c:a:0 copy -c:a:1 aac"));
        assert!(tail.ends_with("-f matroska /out/movie.mkv"));
    }

    #[test]
    fn file_output_without_audio() {
        let f = FileArgs {
            encode: request(json!([])),
            path: "/out/silent.mp4".into(),
            format: FileFormat::Mp4,
            all_audio: true,
        };
        let got = file(&f, &HostCaps::default(), &dev()).unwrap();
        let tail = got[got.len() - 7..].join(" ");
        assert_eq!(
            tail,
            "0:v:0 -an -f mp4 -movflags +faststart /out/silent.mp4"
        );
    }
}

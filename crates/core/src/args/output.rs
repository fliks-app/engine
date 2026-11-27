//! Layer 2: what a run writes (HLS VOD, remux GOP files, live window, trick-play frame, plain file).
use super::encode::{
    AudioEnc, DEFAULT_AUDIO_PLAN, audio_copy_args, audio_encode_args, audio_map, audio_stream_args,
    encode, live_filters, live_input, live_video, probe_args, video_map,
};
use super::filters::crop_filter;
use super::{AudioStream, FileArgs, FileFormat, IframeArgs, LiveArgs, RemuxArgs, VodArgs};
use crate::api::{AudioCodec, AudioPlan, HostCaps};
use crate::decide::dolby_vision::has_no_base;
use crate::hw::Device;
use crate::jsnum::{js_string as js, round, seconds, to_fixed};
use crate::live::vetted_input_args;
use crate::strs;

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

fn hls_head(hls_time: String, list_size: String) -> Vec<String> {
    let mut a = strs(&["-f", "hls", "-hls_time"]);
    a.extend([hls_time, "-hls_list_size".into(), list_size]);
    a
}

fn hls_segment_type(use_ts: bool) -> [String; 2] {
    [
        "-hls_segment_type".into(),
        if use_ts { "mpegts" } else { "fmp4" }.into(),
    ]
}

/// The shared HLS muxer tail. [ts:backend/src/modules/streaming/transcoding/ffmpeg-args.ts:191-255]
fn hls_muxer(m: Muxer) -> Vec<String> {
    let origin = if m.use_ts { 0.0 } else { m.origin };
    let mut a = vec![];
    if !m.use_ts {
        a.extend(strs(&[
            "-movflags",
            "+cmaf",
            "-avoid_negative_ts",
            "disabled",
        ]));
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
    a.extend(hls_head(m.hls_time, m.list_size.to_string()));
    a.extend(["-start_number".into(), m.start_number.to_string()]);
    a.extend(hls_segment_type(m.use_ts));
    if !m.use_ts {
        a.extend(["-hls_fmp4_init_filename".into(), m.init.into()]);
    }
    a.extend(strs(&["-hls_flags", "independent_segments+temp_file"]));
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
    let mut a = strs(&["-hide_banner", "-loglevel", "warning"]);
    a.extend(probe_args(o.trusted_stream_info));
    let seek = o.run.seek_seconds.map(seconds);
    if let Some(ss) = &seek {
        a.extend(strs(&[
            "-noaccurate_seek",
            "-seek_timestamp",
            "1",
            "-ss",
            ss,
        ]));
    }
    let audio = AudioEnc {
        stereo_bps: REMUX_STEREO_BPS,
        align_start: o.run.audio_start_seconds,
        end: o.source_end_seconds,
        use_ts: o.use_ts,
        libfdk_aac: caps.libfdk_aac,
        pitch_semitones: 0.0,
    };
    let plan = o.audio_plan.as_ref().unwrap_or(&DEFAULT_AUDIO_PLAN);
    a.extend(["-i".into(), o.input_path.clone()]);
    a.extend(strs(&["-copyts", "-muxdelay", "0", "-muxpreload", "0"]));
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
        video.extend(strs(&[
            "-tag:v",
            if dv_no_base_tag { "dvh1" } else { "hvc1" },
            "-bsf:v",
            &bsf,
            "-max_muxing_queue_size",
            "2048",
        ]));
    }
    // A copy keeps the source's entry: `avc3` and a based `dav1` leave as their codec's own.
    if codec == Some("h264") {
        video.extend(strs(&["-tag:v", "avc1", "-bsf:v", "h264_mp4toannexb"]));
    }
    if codec == Some("av1") {
        video.extend(strs(&[
            "-tag:v",
            if dv_no_base_tag { "dav1" } else { "av01" },
        ]));
    }
    if codec == Some("av1") && strip_hdr10_plus {
        video.extend(strs(&["-bsf:v", "av1_metadata=remove_hdr10plus=1"]));
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
        a.extend(strs(&["-c:v", "copy"]));
        a.extend(audio_stream_args("", plan, &audio));
    }
    a.extend(video);
    let dir = o.output_dir.as_str();
    let ext = if o.use_ts { "ts" } else { "m4s" };
    a.extend(hls_muxer(Muxer {
        use_ts: o.use_ts,
        hls_time: if o.grid.is_some() {
            "0".into()
        } else {
            js(o.segment_duration.unwrap_or(3.0))
        },
        start_number: o.run.start_number,
        output_seek: o.run.seek_seconds.unwrap_or(0.0),
        init: "init.mp4",
        var_stream_map: None,
        segment: out_path(&[dir, &format!("gop-%d.{ext}")]),
        index: out_path(&[dir, "index.m3u8"]),
        origin: 0.0,
        source_timestamps: !o.use_ts,
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
            a.extend(strs(&[
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
    let mut a = strs(&[
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
    a.extend(vetted_input_args(o.vet_proxy.as_deref()));
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
        a.extend(live_input(o, caps, dev));
        let threads = o.encode_threads.max(1).to_string();
        a.extend(["-threads".into(), threads]);
    }
    a.extend(["-i".into(), o.input_url.clone()]);
    a.extend(strs(&["-map", "0:v:0", "-map", "0:a:0?", "-sn", "-dn"]));
    if let Some(vf) = live_filters(o, caps) {
        a.extend(["-vf".into(), vf]);
    }
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
        flags.push_str("+append_list");
    }
    // A run starts its timestamps at the listed duration, not at 0 like each ffmpeg run would.
    if o.timeline.seconds > 0.0 {
        a.extend(["-output_ts_offset".into(), seconds(o.timeline.seconds)]);
    }
    if !o.use_ts {
        // Timestamps in the tfdt, no edit list: MSE drops edit lists, and A/V sync with them.
        // Without one movenc would zero each run's start, offset included, unless told otherwise.
        a.extend(strs(&[
            "-hls_segment_options",
            "movflags=+frag_discont:use_editlist=0:avoid_negative_ts=make_non_negative",
        ]));
    }
    a.extend(hls_head(js(o.segment_seconds), js(list)));
    a.extend(["-hls_delete_threshold".into(), "3".into()]);
    a.extend(hls_segment_type(o.use_ts));
    if !o.use_ts {
        let init = crate::live::init_name(o.run);
        a.extend(["-hls_fmp4_init_filename".into(), init]);
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
    let mut a = strs(&["-hide_banner", "-loglevel", "error", "-ss"]);
    a.push(to_fixed(o.seek_seconds, 3));
    a.extend(strs(&["-copyts", "-i", &o.input_path]));
    a.extend(strs(&["-frames:v", "1", "-an", "-sn", "-dn", "-vf"]));
    a.push(format!("{crop}scale={}:{}", o.width, o.height));
    a.extend(strs(&[
        "-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p",
    ]));
    if let Some(off) = o
        .timeline_offset_seconds
        .filter(|x| *x != 0.0 && !x.is_nan())
    {
        a.extend(["-output_ts_offset".into(), js(off)]);
    }
    a.extend(strs(&[
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
    if let Some(ar) = &f.archive {
        return super::archive::file(f, ar, caps, dev);
    }
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
        FileFormat::Mp4 => a.extend(strs(&["-f", "mp4", "-movflags", "+faststart"])),
        FileFormat::Mkv => a.extend(strs(&["-f", "matroska"])),
    }
    a.push(f.path.clone());
    Ok(a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{HwAccel, Platform};
    use crate::args::live_gpu_decode;
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
            cuda_index: None,
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
            archive: None,
        };
        let got = file(&mp4, &caps, &dev()).unwrap();
        assert_eq!(got[..layer1.len()], layer1[..]);
        assert_eq!(
            got[layer1.len()..],
            strs(&[
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
                "asetpts=PTS-0/TB,aresample=async=1:first_pts=0:rematrix_maxval=3,aformat=channel_layouts=stereo,alimiter=limit=0.977:attack=5:release=50:level=disabled:latency=1,asetpts=PTS+0/TB",
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
    fn nvenc_names_its_card_on_a_multi_gpu_host() {
        let mut o = request(json!([]));
        o.hw_accel = HwAccel::Nvenc;
        let caps = HostCaps {
            hw_accel: HwAccel::Nvenc,
            ..HostCaps::default()
        };
        let argv = |idx| {
            vod(
                &o,
                &caps,
                &Device {
                    cuda_index: idx,
                    ..dev()
                },
            )
            .unwrap()
            .join(" ")
        };
        let one = argv(Some(1));
        assert!(one.contains("-init_hw_device cuda=cu:1"), "{one}");
        assert!(one.contains("-hwaccel_device 1"), "{one}");
        assert!(one.contains("-c:v h264_nvenc -gpu 1"), "{one}");
        let none = argv(None);
        assert!(!none.contains("-gpu") && !none.contains("cu:"), "{none}");
    }

    #[test]
    fn file_output_without_audio() {
        let f = FileArgs {
            encode: request(json!([])),
            path: "/out/silent.mp4".into(),
            format: FileFormat::Mp4,
            all_audio: true,
            archive: None,
        };
        let got = file(&f, &HostCaps::default(), &dev()).unwrap();
        let tail = got[got.len() - 7..].join(" ");
        assert_eq!(
            tail,
            "0:v:0 -an -f mp4 -movflags +faststart /out/silent.mp4"
        );
    }

    fn live_request(mode: &str, hw: &str, threads: u32) -> LiveArgs {
        serde_json::from_value(json!({
            "inputUrl": "http://x/a.ts",
            "outputDir": "/out",
            "segmentSeconds": 2.0,
            "windowMinutes": 15.0,
            "mode": mode,
            "useTs": false,
            "hwAccel": hw,
            "encodeThreads": threads
        }))
        .unwrap()
    }

    #[test]
    fn a_live_run_continues_the_session_timeline() {
        let after =
            |a: &[String], flag: &str| a.iter().position(|x| x == flag).map(|i| a[i + 1].clone());
        let mut r = live_request("remux", "none", 2);
        let first = live(&r, &HostCaps::default(), &dev());
        assert_eq!(after(&first, "-output_ts_offset"), None);
        assert_eq!(
            after(&first, "-hls_segment_options").as_deref(),
            Some("movflags=+frag_discont:use_editlist=0:avoid_negative_ts=make_non_negative")
        );
        r.timeline = crate::live::Timeline {
            next: 7,
            seconds: 13.96,
        };
        r.append = true;
        let appended = live(&r, &HostCaps::default(), &dev());
        assert_eq!(
            after(&appended, "-output_ts_offset").as_deref(),
            Some("13.96")
        );
        assert_eq!(after(&appended, "-start_number"), None);
        r.use_ts = true;
        let ts = live(&r, &HostCaps::default(), &dev());
        assert_eq!(after(&ts, "-hls_segment_options"), None);
        assert_eq!(after(&ts, "-output_ts_offset").as_deref(), Some("13.96"));
    }

    fn vf(hw: &str, interlaced: Option<bool>, bps: Option<f64>, probes: &[&str]) -> Option<String> {
        let mut r = live_request("transcode", hw, 2);
        (r.interlaced, r.video_bitrate_bps) = (interlaced, bps);
        let caps = HostCaps {
            probes: probes.iter().map(|p| p.to_string()).collect(),
            ..HostCaps::default()
        };
        live_filters(&r, &caps)
    }

    #[test]
    fn a_live_transcode_deinterlaces_and_never_enlarges() {
        let bwdif = "bwdif=mode=send_frame:parity=auto:deint=interlaced";
        assert_eq!(vf("none", Some(false), None, &[]), None);
        assert_eq!(vf("none", None, None, &[]).as_deref(), Some(bwdif));
        assert_eq!(
            vf("none", Some(true), Some(3e6), &[]),
            Some(format!("{bwdif},scale=-2:min(ih\\,480)"))
        );
        assert_eq!(
            vf("none", Some(false), Some(1.5e6), &[]).as_deref(),
            Some("scale=-2:min(ih\\,360)")
        );
        assert_eq!(
            vf("vaapi", Some(true), Some(6e6), &["deinterlaceVaapi"]).as_deref(),
            Some(
                "deinterlace_vaapi=mode=default:rate=frame:auto=1,scale_vaapi=w=-2:h=min(ih\\,720)"
            )
        );
        assert_eq!(
            vf("nvenc", Some(true), None, &["bwdifCuda"]).as_deref(),
            Some("hwupload_cuda,bwdif_cuda=mode=send_frame:parity=auto:deint=interlaced")
        );
        // CUDA filters only ever see uploaded frames: NVDEC may have fallen back to software.
        assert_eq!(
            vf("nvenc", Some(false), Some(3e6), &[]).as_deref(),
            Some("hwupload_cuda,scale_cuda=w=-2:h=min(ih\\,480)")
        );
        assert_eq!(
            vf("nvenc", Some(true), Some(3e6), &[]),
            Some(format!(
                "{bwdif},hwupload_cuda,scale_cuda=w=-2:h=min(ih\\,480)"
            ))
        );
        assert_eq!(vf("nvenc", Some(false), None, &[]), None);
        // No GPU deinterlacer: a known interlaced source is decoded and deinterlaced on the CPU,
        // then uploaded (a 10-bit GPU frame does not download to nv12 on QSV); unknown is left.
        assert_eq!(
            vf("vaapi", Some(true), None, &[]),
            Some(format!("{bwdif},format=nv12,hwupload=derive_device=vaapi"))
        );
        assert_eq!(
            vf("qsv", Some(true), Some(3e6), &[]),
            Some(format!(
                "{bwdif},format=nv12,hwupload=extra_hw_frames=16,vpp_qsv=w=-1:h=min(ih\\,480)"
            ))
        );
        let mut r = live_request("transcode", "qsv", 2);
        (r.interlaced, r.video_codec, r.video_bit_depth) =
            (Some(true), Some("hevc".into()), Some(10));
        assert!(!live_gpu_decode(&r, &HostCaps::default()));
        assert_eq!(vf("vaapi", None, None, &[]), None);
        assert_eq!(
            vf("qsv", Some(true), Some(10e6), &["deinterlaceQsv"]).as_deref(),
            Some("vpp_qsv=deinterlace=advanced:rate=frame:w=-1:h=min(ih\\,1080)")
        );
        let remux = live_request("remux", "none", 2);
        assert_eq!(live_filters(&remux, &HostCaps::default()), None);
    }

    #[test]
    fn a_codec_the_gpu_cannot_decode_is_decoded_on_the_cpu_and_uploaded() {
        let bwdif = "bwdif=mode=send_frame:parity=auto:deint=interlaced";
        // An iGPU without AV1, as the boot probe found it.
        let caps = |probes: &[&str]| HostCaps {
            disabled_decoders: vec!["av1_qsv_decode".into(), "av1_vaapi_decode".into()],
            probes: probes.iter().map(|p| p.to_string()).collect(),
            ..HostCaps::default()
        };
        let run = |hw: &str, codec: &str, interlaced, bps, probes: &[&str]| {
            let mut r = live_request("transcode", hw, 2);
            (r.video_codec, r.interlaced, r.video_bitrate_bps) =
                (Some(codec.into()), interlaced, bps);
            let c = caps(probes);
            (live_input(&r, &c, &dev()), live_filters(&r, &c))
        };
        // The device is still initialised for the encoder's filters, no `-hwaccel`.
        let (input, vf) = run("vaapi", "av1", Some(false), Some(3e6), &[]);
        assert_eq!(
            input,
            [
                "-init_hw_device",
                "vaapi=va:/dev/dri/renderD128",
                "-filter_hw_device",
                "va"
            ]
        );
        assert_eq!(
            vf.as_deref(),
            Some("format=nv12,hwupload=derive_device=vaapi,scale_vaapi=w=-2:h=min(ih\\,480)")
        );
        // The encoder takes only VAAPI frames: uploaded even with nothing else to do.
        let (_, vf) = run("vaapi", "av1", Some(false), None, &[]);
        assert_eq!(
            vf.as_deref(),
            Some("format=nv12,hwupload=derive_device=vaapi")
        );
        let (_, vf) = run("vaapi", "av1", None, None, &["deinterlaceVaapi"]);
        assert_eq!(
            vf.as_deref(),
            Some(
                "format=nv12,hwupload=derive_device=vaapi,deinterlace_vaapi=mode=default:rate=frame:auto=1"
            )
        );
        // No GPU deinterlacer: the CPU frames are deinterlaced before the upload.
        let (_, vf) = run("vaapi", "av1", Some(true), None, &[]);
        assert_eq!(
            vf,
            Some(format!("{bwdif},format=nv12,hwupload=derive_device=vaapi"))
        );
        let (input, vf) = run("qsv", "av1", Some(false), Some(3e6), &[]);
        assert!(!input.iter().any(|a| a == "-hwaccel"), "{input:?}");
        assert_eq!(input[input.len() - 2..], ["-filter_hw_device", "qs"]);
        assert_eq!(
            vf.as_deref(),
            Some("format=nv12,hwupload=extra_hw_frames=16,vpp_qsv=w=-1:h=min(ih\\,480)")
        );
        let (_, vf) = run("qsv", "av1", Some(true), None, &[]);
        assert_eq!(
            vf,
            Some(format!("{bwdif},format=nv12,hwupload=extra_hw_frames=16"))
        );
        // A codec it decodes keeps the GPU frames.
        let (input, vf) = run("qsv", "hevc", Some(false), Some(3e6), &[]);
        assert!(input.windows(2).any(|w| w == ["-hwaccel", "qsv"]));
        assert_eq!(vf.as_deref(), Some("vpp_qsv=w=-1:h=min(ih\\,480)"));
        let (input, _) = run("vaapi", "h264", Some(false), None, &[]);
        assert!(input.windows(2).any(|w| w == ["-hwaccel", "vaapi"]));
    }

    #[test]
    fn a_bit_depth_past_the_decoder_or_a_failed_gpu_decode_decodes_on_the_cpu() {
        let caps = HostCaps::default();
        let run = |hw: &str, codec: &str, depth, cpu_decode| {
            let mut r = live_request("transcode", hw, 2);
            (r.video_codec, r.video_bit_depth, r.cpu_decode) =
                (Some(codec.into()), depth, cpu_decode);
            let gpu = live_gpu_decode(&r, &caps);
            let input = live_input(&r, &caps, &dev());
            assert_eq!(input.iter().any(|a| a == "-hwaccel"), gpu, "{input:?}");
            gpu
        };
        // The H.264 decoders stop at 8 bits, HEVC's read 10; unknown is taken for 8.
        assert!(!run("vaapi", "h264", Some(10), false));
        assert!(!run("qsv", "h264", Some(10), false));
        assert!(run("vaapi", "hevc", Some(10), false));
        assert!(run("qsv", "h264", None, false));
        // A channel whose GPU decode failed.
        assert!(!run("vaapi", "h264", None, true));
        assert!(!run("qsv", "hevc", Some(10), true));
        // GPU frames past 8 bits leave the scaler as nv12 for the H.264 encoder.
        let vf = |hw: &str, depth, bps| {
            let mut r = live_request("transcode", hw, 2);
            (r.video_codec, r.video_bit_depth, r.video_bitrate_bps) =
                (Some("hevc".into()), depth, bps);
            live_filters(&r, &caps)
        };
        assert_eq!(
            vf("vaapi", Some(10), Some(3e6)).as_deref(),
            Some("scale_vaapi=w=-2:h=min(ih\\,480):format=nv12")
        );
        assert_eq!(
            vf("vaapi", Some(10), None).as_deref(),
            Some("scale_vaapi=format=nv12")
        );
        assert_eq!(
            vf("qsv", Some(10), Some(3e6)).as_deref(),
            Some("vpp_qsv=w=-1:h=min(ih\\,480):format=nv12")
        );
        assert_eq!(vf("qsv", None, None), None);
        // NVENC's frames are never the GPU's: its decoder falls back by itself.
        let nvenc = live_request("transcode", "nvenc", 2);
        assert!(!live_gpu_decode(&nvenc, &caps));
    }

    #[test]
    fn live_transcode_caps_threads() {
        let caps = HostCaps::default();
        let cpu = live(&live_request("transcode", "none", 2), &caps, &dev());
        let i = cpu.iter().position(|a| a == "-i").unwrap();
        assert_eq!(&cpu[i - 2..i], ["-threads", "2"]);
        let c = cpu.iter().position(|a| a == "-c:v").unwrap();
        assert_eq!(&cpu[c..c + 4], ["-c:v", "libx264", "-threads:v", "2"]);
        let hw = live(&live_request("transcode", "vaapi", 2), &caps, &dev());
        let i = hw.iter().position(|a| a == "-i").unwrap();
        assert_eq!(&hw[i - 2..i], ["-threads", "2"]);
        assert!(!hw.iter().any(|a| a == "-threads:v"));
        let copy = live(&live_request("remux", "none", 2), &caps, &dev());
        assert!(!copy.iter().any(|a| a == "-threads" || a == "-threads:v"));
    }
}

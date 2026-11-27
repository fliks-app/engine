//! Live TV, pure: mode, session key, probe parsing, manifest sniffing, failover walk, playlist
//! token rewrite. Spec: `docs/spec/live.md`; tags relative to `backend/src/modules/livetv/`.
use crate::api::{
    HwAccel, LiveCaps, LiveMode, LiveProbe, PipelineStage, QoeOutput, QoePipeline, QoeSource,
};
use crate::args::{http_header_args, live_h264_encoder};
use crate::jsnum::js_string;
use crate::probe::pix_fmt_depth;
use std::collections::BTreeMap;

/// Tries per upstream before a session gives up on it. [ts:services/livetv-session.service.ts:143]
pub const MAX_FAILURES_PER_UPSTREAM: u32 = 2;
/// [ts:services/live-ffmpeg-args.ts:41]
pub const DEFAULT_TRANSCODE_BITRATE_BPS: f64 = 4_000_000.0;
/// [ts:services/livetv-session.service.ts:216]
const BITRATE_RUNGS: [f64; 5] = [
    1_500_000.0,
    3_000_000.0,
    6_000_000.0,
    10_000_000.0,
    20_000_000.0,
];
const MANIFEST_CONTENT_TYPES: [&str; 7] = [
    "mpegurl",
    "dash+xml",
    "application/xml",
    "text/xml",
    "text/html",
    "application/json",
    "text/plain",
];
const MANIFEST_CONTAINERS: [&str; 3] = ["hls", "applehttp", "dash"];
const PROBE_STREAM_TYPES: [&str; 5] = ["video", "audio", "subtitle", "data", "attachment"];
const MANIFEST_SIGNATURES: [&[u8]; 3] = [b"#EXTM3U", b"<?xml", b"<MPD"];

/// A rung, so nearby requests share one session. [ts:services/livetv-session.service.ts:218-223]
pub fn snap_bitrate(bps: f64) -> f64 {
    BITRATE_RUNGS
        .into_iter()
        .find(|r| *r >= bps)
        .unwrap_or(BITRATE_RUNGS[4])
}

/// `caps` with its bitrate snapped. [ts:services/livetv-session.service.ts:335-337]
pub fn snapped(caps: &LiveCaps) -> LiveCaps {
    LiveCaps {
        max_bitrate_bps: caps.max_bitrate_bps.map(snap_bitrate),
        ..caps.clone()
    }
}

/// Transcoding reaches the first segment sooner, so it wins whenever encoding is free.
/// [ts:services/livetv-session.service.ts:695-704]
pub fn decide_mode(caps: &LiveCaps, fast_zap: Option<bool>, hw: HwAccel) -> LiveMode {
    if caps.direct_play {
        return LiveMode::Direct;
    }
    if caps.max_bitrate_bps.is_some() {
        return LiveMode::Transcode;
    }
    match fast_zap {
        Some(true) => LiveMode::Transcode,
        Some(false) => LiveMode::Remux,
        None if hw != HwAccel::None => LiveMode::Transcode,
        None => LiveMode::Remux,
    }
}

fn mode_name(m: LiveMode) -> &'static str {
    match m {
        LiveMode::Direct => "direct",
        LiveMode::Remux => "remux",
        LiveMode::Transcode => "transcode",
    }
}

/// Viewers share an upstream only when they would decode the same output.
/// [ts:services/livetv-session.service.ts:706-711]
pub fn session_key(channel_id: &str, mode: LiveMode, caps: &LiveCaps) -> String {
    let container = if caps.use_ts { "ts" } else { "fmp4" };
    let bitrate = js_string(caps.max_bitrate_bps.unwrap_or(0.0));
    format!("{channel_id}:{}:{container}:{bitrate}", mode_name(mode))
}

/// The requested mode (the key's) and the snapped caps: a direct request on a manifest is
/// packaged instead. `probe0` is upstream 0's, needed only for a direct request.
/// [ts:services/livetv-session.service.ts:333-358]
pub fn plan(
    caps: &LiveCaps,
    fast_zap: Option<bool>,
    hw: HwAccel,
    probe0: Option<&LiveProbe>,
) -> (LiveMode, LiveCaps) {
    let mut caps = snapped(caps);
    let mut mode = decide_mode(&caps, fast_zap, hw);
    if mode == LiveMode::Direct
        && is_manifest_container(probe0.and_then(|p| p.container.as_deref()))
    {
        caps.direct_play = false;
        mode = decide_mode(&caps, fast_zap, hw);
    }
    (mode, caps)
}

/// Only progressive H.264 survives a copy into fMP4 in every browser: a copied interlaced one
/// combs. [ts:services/livetv-session.service.ts:459-463]
pub fn effective_mode(mode: LiveMode, probed_video: Option<&str>, interlaced: bool) -> LiveMode {
    if mode == LiveMode::Remux && (probed_video != Some("h264") || interlaced) {
        LiveMode::Transcode
    } else {
        mode
    }
}

/// A failover upstream the running init cannot describe needs a fresh transcode.
/// [ts:services/livetv-session.service.ts:880-886]
pub fn forces_transcode(
    probed_video: Option<&str>,
    active_video: Option<&str>,
    interlaced: bool,
) -> bool {
    let codec = probed_video.unwrap_or("");
    codec != "h264" || interlaced || active_video.is_some_and(|a| a != codec)
}

/// ffprobe's `csv=p=0`: a stream row is its codec, its type, then (video) its pixel format and
/// field order (ffprobe's order, not `probe_args`'); the format row has no type, maybe commas.
/// [ts:services/livetv-session.service.ts:170-193]
pub fn parse_probe_output(stdout: &str) -> LiveProbe {
    let mut p = LiveProbe::default();
    // JS `trim()`: \s, which includes \r.
    for line in stdout.split('\n').map(|l| l.trim()) {
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split(',').collect();
        let Some(at) = parts.iter().position(|t| PROBE_STREAM_TYPES.contains(t)) else {
            p.container.get_or_insert_with(|| line.to_string());
            continue;
        };
        let kind = parts[at];
        if kind == "video" && p.video_codec.is_none() {
            p.video_codec = Some(parts[0].to_string());
            p.bit_depth = parts.get(at + 1).and_then(|f| pix_fmt_depth(f));
            p.field_order = parts.get(at + 2).map(|f| f.to_string());
        }
        if kind == "audio" && p.audio_codec.is_none() {
            p.audio_codec = Some(parts[0].to_string());
        }
    }
    p
}

/// A live run's stderr tail shows ffmpeg failing before any decode: the upstream could not be
/// opened or read up to its streams, or holds no video stream.
pub fn input_failed(stderr_tail: &[String]) -> bool {
    stderr_tail
        .iter()
        .any(|l| l.contains("Error opening input") || l.contains("matches no streams"))
}

/// A live run's stderr tail shows its video decoder failing a packet with another error than
/// invalid data: QSV refuses a stream so, and ffmpeg runs on without ever outputting a frame.
pub fn decoder_refused(stderr_tail: &[String]) -> bool {
    stderr_tail.iter().any(|l| {
        l.contains("[vist#")
            && l.contains("Error submitting packet to decoder")
            && !l.contains("Invalid data found when processing input")
    })
}

/// Unknown fails open: an unrecognised container is still worth a direct attempt.
/// [ts:services/livetv-session.service.ts:197-200]
pub fn is_manifest_container(container: Option<&str>) -> bool {
    container.is_some_and(|c| {
        c.split(',')
            .any(|n| MANIFEST_CONTAINERS.contains(&n.trim().to_lowercase().as_str()))
    })
}

/// [ts:services/livetv-session.service.ts:964]
pub fn is_manifest_content_type(content_type: &str) -> bool {
    let ct = content_type.to_lowercase();
    MANIFEST_CONTENT_TYPES.iter().any(|t| ct.contains(t))
}

/// The first bytes are a playlist, after an optional UTF-8 BOM and leading whitespace within 16
/// bytes. [ts:services/livetv-session.service.ts:203-208]
pub fn is_manifest_payload(chunk: &[u8]) -> bool {
    let from = if chunk.starts_with(&[0xef, 0xbb, 0xbf]) {
        3
    } else {
        0
    };
    let head = &chunk[from.min(chunk.len())..(from + 16).min(chunk.len())];
    // latin1 `trimStart()`: ASCII whitespace plus NBSP (0xa0).
    let start = head
        .iter()
        .position(|b| !matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | 0xa0))
        .unwrap_or(head.len());
    MANIFEST_SIGNATURES
        .iter()
        .any(|s| head[start..].starts_with(s))
}

/// `User-Agent` and `Referer` (any case) of an upstream's headers, then the rest in key order.
pub fn split_headers(
    headers: &BTreeMap<String, String>,
) -> (Option<String>, Option<String>, Vec<(String, String)>) {
    let (mut ua, mut referer, mut rest) = (None, None, vec![]);
    for (k, v) in headers {
        match k.to_ascii_lowercase().as_str() {
            "user-agent" => ua = Some(v.clone()),
            "referer" => referer = Some(v.clone()),
            _ => rest.push((k.clone(), v.clone())),
        }
    }
    (ua, referer, rest)
}

/// The codec probe of one upstream. [ts:services/livetv-session.service.ts:746-773]
pub fn probe_args(
    url: &str,
    headers: &BTreeMap<String, String>,
    vet_proxy: Option<&str>,
) -> Vec<String> {
    let (ua, referer, rest) = split_headers(headers);
    let mut a: Vec<String> = [
        "-v",
        "error",
        "-analyzeduration",
        "3000000",
        "-probesize",
        "5000000",
        "-show_entries",
        "format=format_name:stream=codec_type,codec_name,pix_fmt,field_order",
        "-of",
        "csv=p=0",
    ]
    .map(String::from)
    .into();
    a.extend(vetted_input_args(vet_proxy));
    a.extend(http_header_args(ua.as_deref(), referer.as_deref(), &rest));
    a.push(url.into());
    a
}

/// A vetted input only speaks http(s), every request of it through the vetting proxy.
pub fn vetted_input_args(vet_proxy: Option<&str>) -> Vec<String> {
    vet_proxy.map_or(vec![], |p| {
        crate::strs(&[
            "-protocol_whitelist",
            "http,https,tcp,tls,crypto",
            "-http_proxy",
            p,
        ])
    })
}

/// `encodeURIComponent`.
fn encode_uri_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `?token=` after every URI of ffmpeg's playlist: native players rarely send a header.
/// [ts:controllers/livetv.controller.ts:59-82]
pub fn with_token(raw: &str, token: Option<&str>) -> String {
    let Some(token) = token.filter(|t| !t.is_empty()) else {
        return raw.to_string();
    };
    let suffix = format!("?token={}", encode_uri_component(token));
    raw.split('\n')
        .map(|line| {
            if line.starts_with("#EXT-X-MAP") {
                // The first `URI="<1+ chars>"`, as `/URI="([^"]+)"/`.
                let end = line.match_indices("URI=\"").find_map(|(i, _)| {
                    let v = i + 5;
                    line[v..].find('"').filter(|j| *j > 0).map(|j| v + j)
                });
                return match end {
                    Some(end) => format!("{}{suffix}{}", &line[..end], &line[end..]),
                    None => line.to_string(),
                };
            }
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                line.to_string()
            } else {
                format!("{trimmed}{suffix}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The fMP4 init of a session's `run`: one name per run, a player keeps an init by its URL.
pub fn init_name(run: u32) -> String {
    match run {
        0 => "init.mp4".into(),
        n => format!("init-{n}.mp4"),
    }
}

/// `init.mp4`, `init-<digits>.mp4` [new] or `seg-<5+ ASCII digits>.(m4s|ts)`.
/// [ts:controllers/livetv.controller.ts:45-47]
pub fn is_segment_name(name: &str) -> bool {
    let run = name
        .strip_prefix("init-")
        .and_then(|r| r.strip_suffix(".mp4"));
    if name == "init.mp4"
        || run.is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
    {
        return true;
    }
    let Some(rest) = name.strip_prefix("seg-") else {
        return false;
    };
    let Some((digits, ext)) = rest.split_once('.') else {
        return false;
    };
    digits.len() >= 5 && digits.bytes().all(|b| b.is_ascii_digit()) && matches!(ext, "m4s" | "ts")
}

/// The `seg-N` number of a segment file.
pub fn segment_number(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("seg-")?;
    rest[..rest.find('.')?].parse().ok()
}

/// What a session's playlists have listed, each segment counted once by media sequence: the next
/// run numbers on from `next` and starts its timestamps at `seconds`, the listed duration. [new]
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Deserialize)]
pub struct Timeline {
    /// The media sequence number after the last counted segment.
    pub next: u64,
    pub seconds: f64,
}

impl Timeline {
    /// Counts the segments of `playlist` not counted yet.
    pub fn read(&mut self, playlist: &str) {
        let mut seq = 0;
        for line in playlist.lines() {
            if let Some(n) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
                seq = n.trim().parse().unwrap_or(0);
            } else if let Some(d) = line.strip_prefix("#EXTINF:") {
                let d: f64 = d
                    .split(',')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .parse()
                    .unwrap_or(0.0);
                if seq >= self.next {
                    self.seconds += d;
                    self.next = seq + 1;
                }
                seq += 1;
            }
        }
    }
}

/// ffmpeg's playlist with one `#EXT-X-MAP` per run: `inits` holds each run's first media sequence
/// and init, ascending. ffmpeg writes one map, the latest run's, for every listed segment. [new]
pub fn with_inits(raw: &str, inits: &[(u64, String)]) -> String {
    if inits.is_empty() {
        return raw.to_string();
    }
    let mut out = Vec::new();
    let (mut seq, mut mapped) = (0, None);
    for line in raw.split('\n') {
        if line.starts_with("#EXT-X-MAP") {
            continue;
        }
        if let Some(n) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            seq = n.trim().parse().unwrap_or(0);
        } else if line.starts_with("#EXTINF:") {
            let init = inits.iter().rev().find(|(first, _)| *first <= seq);
            if let Some((_, init)) = init
                && mapped != Some(init)
            {
                out.push(format!("#EXT-X-MAP:URI=\"{init}\""));
                mapped = Some(init);
            }
            seq += 1;
        }
        out.push(line.to_string());
    }
    out.join("\n")
}

/// The upstream walk of a session: tries per upstream, round robin from the failed one.
/// [ts:services/livetv-session.service.ts:820-858,1087-1102]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failover {
    pub index: usize,
    pub failures: Vec<u32>,
    pub restarts: u32,
}

impl Failover {
    pub fn new(upstreams: usize) -> Self {
        Self {
            index: 0,
            failures: vec![0; upstreams],
            restarts: 0,
        }
    }

    /// The upstream to try next; `None` once every upstream is spent (the session ends).
    pub fn pick(&mut self) -> Option<usize> {
        let n = self.failures.len();
        if self
            .failures
            .iter()
            .all(|f| *f >= MAX_FAILURES_PER_UPSTREAM)
        {
            return None;
        }
        while self.failures[self.index % n] >= MAX_FAILURES_PER_UPSTREAM {
            self.index += 1;
        }
        Some(self.index % n)
    }

    /// The upstream `pick` gave failed, at startup or while running.
    pub fn failed(&mut self, upstream: usize) {
        self.failures[upstream] += 1;
        self.index += 1;
        self.restarts += 1;
    }
}

/// What the live run does to the stream, for QoE reports. [ts:services/livetv-session.service.ts:625-653]
pub fn qoe_pipeline(
    mode: LiveMode,
    hw: HwAccel,
    video_codec: Option<&str>,
    bitrate_bps: Option<f64>,
) -> QoePipeline {
    let transcode = mode == LiveMode::Transcode;
    QoePipeline {
        source: Some(QoeSource {
            codec: video_codec.map(String::from),
            width: None,
            height: None,
            hdr: None,
        }),
        output: transcode.then(|| QoeOutput {
            codec: Some("h264".into()),
            hdr: None,
            quality: None,
            bitrate: Some(bitrate_bps.unwrap_or(DEFAULT_TRANSCODE_BITRATE_BPS) as u32),
            requested: None,
        }),
        decode: None,
        encode: transcode.then(|| PipelineStage {
            hw: hw != HwAccel::None,
            kind: match hw {
                HwAccel::None => "cpu".into(),
                hw => serde_json::to_value(hw)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_default(),
            },
            name: Some(live_h264_encoder(hw)),
        }),
        tonemap: None,
        burn_in: None,
        crop: None,
        audio_transcode: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_input_failure_is_told_apart_from_a_decode_failure() {
        let tail = |t: &str| t.lines().map(String::from).collect::<Vec<_>>();
        // ffmpeg 8.1.2 with the live input options: refused, 404, silent, not media.
        for t in [
            "[in#0 @ 0x5c8a50796c40] Error opening input: Connection refused\n\
             Error opening input file http://127.0.0.1:18089/x.ts.\n\
             Error opening input files: Connection refused",
            "[http @ 0x6422b35d30c0] HTTP error 404 File not found\n\
             [in#0 @ 0x6422b2eb0140] Error opening input: Server returned 404 Not Found",
            "[in#0 @ 0x5c849fabdc40] Error opening input: Connection timed out",
            "[in#0 @ 0x5a23aed0d040] Error opening input: Invalid data found when processing input",
            // An upstream with no video stream.
            "Stream map '' matches no streams.\nTo ignore this, add a trailing '?' to the map.",
        ] {
            assert!(input_failed(&tail(t)), "{t}");
        }
        // 10-bit H.264 through h264_qsv: the input opened, the decoder failed.
        let decode = "[h264_qsv @ 0x63db92a13240] get_buffer() failed\n\
            [vist#0:0/h264 @ 0x63db9256f8c0] [dec:h264_qsv @ 0x63db92a17140] Decoding error: \
            Invalid argument\nConversion failed!";
        assert!(!input_failed(&tail(decode)));
        assert!(!input_failed(&[]));
    }

    #[test]
    fn each_run_has_an_init_name_the_segment_route_takes() {
        assert_eq!(init_name(0), "init.mp4");
        assert_eq!(init_name(2), "init-2.mp4");
        for n in [0, 1, 12] {
            assert!(is_segment_name(&init_name(n)), "{n}");
        }
        for bad in [
            "init-.mp4",
            "init-a.mp4",
            "init-1.mp4.tmp",
            "init-1",
            "../init-1.mp4",
        ] {
            assert!(!is_segment_name(bad), "{bad}");
        }
    }

    #[test]
    fn a_decoder_refusing_the_stream_is_told_apart_from_damaged_data() {
        let tail = |t: &str| t.lines().map(String::from).collect::<Vec<_>>();
        // ffmpeg 8.1.2, 10-bit H.264 through h264_qsv on a live input: once per keyframe.
        let refused = "[h264_qsv @ 0x6020ab2dec80] Error querying IO surface: unsupported (-3)\n\
            [vist#0:0/h264 @ 0x6020a9ad85c0] [dec:h264_qsv @ 0x6020ab290740] Error submitting \
            packet to decoder: Function not implemented";
        assert!(decoder_refused(&tail(refused)));
        // A stream joined between two keyframes.
        let damaged = "[vist#0:0/h264 @ 0x55d0] [dec:h264 @ 0x55d1] Error submitting packet to \
            decoder: Invalid data found when processing input";
        assert!(!decoder_refused(&tail(damaged)));
        // An audio decoder's trouble says nothing about the GPU.
        let audio = "[aist#0:1/aac_latm @ 0x55d0] [dec:aac_latm @ 0x55d1] Error submitting packet \
            to decoder: Not yet implemented in FFmpeg, patches welcome";
        assert!(!decoder_refused(&tail(audio)));
        assert!(!decoder_refused(&[]));
    }

    #[test]
    fn a_probe_reads_the_field_order_and_bit_depth_of_the_video() {
        // ffprobe 6.1 and 9 on an interlaced H.264 TS: the program lists its streams again.
        let p = parse_probe_output(
            "h264,video,yuv420p,tt\naac,audio\n\nh264,video,yuv420p,tt\naac,audio\nmpegts\n",
        );
        assert_eq!(
            (
                p.video_codec.as_deref(),
                p.audio_codec.as_deref(),
                p.interlaced(),
                p.bit_depth
            ),
            (Some("h264"), Some("aac"), Some(true), None)
        );
        let p = parse_probe_output("mpegts\nhevc,video,yuv420p10le,progressive\n");
        assert_eq!((p.interlaced(), p.bit_depth), (Some(false), Some(10)));
        let wire = serde_json::to_value(&p).unwrap();
        assert_eq!(wire["fieldOrder"], "progressive");
        assert_eq!(wire["bitDepth"], 10);
        let p = parse_probe_output("h264,video\n");
        assert_eq!((p.interlaced(), p.bit_depth), (None, None));
        let p = parse_probe_output("h264,video,unknown,unknown\n");
        assert_eq!((p.interlaced(), p.bit_depth), (None, None));
        assert_eq!(
            effective_mode(LiveMode::Remux, Some("h264"), true),
            LiveMode::Transcode
        );
        assert_eq!(
            effective_mode(LiveMode::Remux, Some("h264"), false),
            LiveMode::Remux
        );
    }

    #[test]
    fn failover_walks_round_robin_and_ends() {
        let mut f = Failover::new(2);
        assert_eq!(f.pick(), Some(0));
        f.failed(0);
        assert_eq!(f.pick(), Some(1));
        f.failed(1);
        f.failed(0);
        assert_eq!(f.pick(), Some(1));
        f.failed(1);
        assert_eq!(f.pick(), None);
        assert_eq!(f.restarts, 4);
    }

    #[test]
    fn headers_split_case_insensitively() {
        let h = BTreeMap::from([
            ("referer".into(), "http://r/".into()),
            ("USER-AGENT".into(), "UA".into()),
            ("X-Token".into(), "t".into()),
        ]);
        assert_eq!(
            probe_args("http://u/", &h, None)[10..],
            [
                "-user_agent",
                "UA",
                "-headers",
                "Referer: http://r/\r\nX-Token: t\r\n",
                "http://u/"
            ]
        );
    }

    #[test]
    fn timeline_counts_each_listed_segment_once() {
        let mut t = Timeline::default();
        t.read("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:2.000000,\nseg-00000.m4s\n#EXTINF:1.960000,\nseg-00001.m4s\n");
        assert_eq!((t.next, t.seconds), (2, 3.96));
        // The window slid and an appended run followed, after a discontinuity.
        t.read("#EXT-X-MEDIA-SEQUENCE:1\n#EXT-X-DISCONTINUITY\n#EXTINF:1.960000,\nseg-00001.m4s\n#EXT-X-DISCONTINUITY\n#EXTINF:2.000000,\n#EXT-X-PROGRAM-DATE-TIME:2026-11-27T17:13:34.600+0100\nseg-00002.m4s\n");
        assert_eq!(t.next, 3);
        assert!((t.seconds - 5.96).abs() < 1e-9, "{t:?}");
        // A playlist restarted at 0 adds nothing.
        t.read("#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:2.000000,\nseg-00000.m4s\n");
        assert_eq!(t.next, 3);
    }

    #[test]
    fn with_inits_maps_each_run_to_its_own_init() {
        let raw = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:4\n#EXT-X-DISCONTINUITY\n#EXT-X-MAP:URI=\"init-2.mp4\"\n\
                   #EXTINF:2.000000,\nseg-00004.m4s\n#EXT-X-DISCONTINUITY\n#EXTINF:2.000000,\nseg-00005.m4s\n\
                   #EXTINF:2.000000,\nseg-00006.m4s\n";
        let inits = [
            (0, "init.mp4".to_string()),
            (3, "init-1.mp4".to_string()),
            (5, "init-2.mp4".to_string()),
        ];
        assert_eq!(
            with_inits(raw, &inits),
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:4\n#EXT-X-DISCONTINUITY\n#EXT-X-MAP:URI=\"init-1.mp4\"\n\
             #EXTINF:2.000000,\nseg-00004.m4s\n#EXT-X-DISCONTINUITY\n#EXT-X-MAP:URI=\"init-2.mp4\"\n\
             #EXTINF:2.000000,\nseg-00005.m4s\n#EXTINF:2.000000,\nseg-00006.m4s\n"
        );
        assert_eq!(with_inits(raw, &[]), raw);
    }

    #[test]
    fn qoe_pipeline_names_the_backend_encoder() {
        let p = qoe_pipeline(LiveMode::Transcode, HwAccel::Vaapi, Some("hevc"), None);
        let e = p.encode.unwrap();
        assert_eq!((e.hw, e.kind.as_str()), (true, "vaapi"));
        assert_eq!(e.name.as_deref(), Some("h264_vaapi"));
        assert_eq!(p.output.unwrap().bitrate, Some(4_000_000));
        assert!(
            qoe_pipeline(LiveMode::Remux, HwAccel::None, None, None)
                .encode
                .is_none()
        );
    }
}

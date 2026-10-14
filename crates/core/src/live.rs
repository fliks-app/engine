//! Live TV, pure: mode, session key, probe parsing, manifest sniffing, failover walk, playlist
//! token rewrite. Spec: `docs/spec/live.md`; tags relative to `backend/src/modules/livetv/`.
use crate::api::{
    HwAccel, LiveCaps, LiveMode, LiveProbe, PipelineStage, QoeOutput, QoePipeline, QoeSource,
};
use crate::args::{http_header_args, live_h264_encoder};
use crate::jsnum::js_string;
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

/// Only H.264 survives a copy into fMP4 in every browser. [ts:services/livetv-session.service.ts:459-463]
pub fn effective_mode(mode: LiveMode, probed_video: Option<&str>) -> LiveMode {
    if mode == LiveMode::Remux && probed_video != Some("h264") {
        LiveMode::Transcode
    } else {
        mode
    }
}

/// A failover upstream the running init cannot describe needs a fresh transcode.
/// [ts:services/livetv-session.service.ts:880-886]
pub fn forces_transcode(probed_video: Option<&str>, active_video: Option<&str>) -> bool {
    let codec = probed_video.unwrap_or("");
    codec != "h264" || active_video.is_some_and(|a| a != codec)
}

/// ffprobe's `csv=p=0`: stream rows end with their type, the format row does not and may hold
/// commas. [ts:services/livetv-session.service.ts:170-193]
pub fn parse_probe_output(stdout: &str) -> LiveProbe {
    let mut p = LiveProbe::default();
    // JS `trim()`: \s, which includes \r.
    for line in stdout.split('\n').map(|l| l.trim()) {
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split(',').collect();
        let kind = parts[parts.len() - 1];
        if !PROBE_STREAM_TYPES.contains(&kind) {
            p.container.get_or_insert_with(|| line.to_string());
            continue;
        }
        if kind == "video" && p.video_codec.is_none() {
            p.video_codec = Some(parts[0].to_string());
        }
        if kind == "audio" && p.audio_codec.is_none() {
            p.audio_codec = Some(parts[0].to_string());
        }
    }
    p
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
pub fn probe_args(url: &str, headers: &BTreeMap<String, String>) -> Vec<String> {
    let (ua, referer, rest) = split_headers(headers);
    let mut a: Vec<String> = [
        "-v",
        "error",
        "-analyzeduration",
        "3000000",
        "-probesize",
        "5000000",
        "-show_entries",
        "format=format_name:stream=codec_type,codec_name",
        "-of",
        "csv=p=0",
    ]
    .map(String::from)
    .into();
    a.extend(http_header_args(ua.as_deref(), referer.as_deref(), &rest));
    a.push(url.into());
    a
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

/// `init.mp4` or `seg-<5+ ASCII digits>.(m4s|ts)`. [ts:controllers/livetv.controller.ts:45-47]
pub fn is_segment_name(name: &str) -> bool {
    if name == "init.mp4" {
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

/// The `seg-N` number of a file ffmpeg wrote (`.tmp` included).
pub fn segment_number(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("seg-")?;
    rest[..rest.find('.')?].parse().ok()
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
            probe_args("http://u/", &h)[10..],
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

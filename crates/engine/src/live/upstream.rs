//! The two transports of an upstream (packaged ffmpeg run, direct tee) and their failover.

use super::vet::{self, Resolver};
use super::{
    DIRECT_CONNECT_TIMEOUT, DIRECT_SNIFF, Direct, FIRST_SEGMENT_POLL, FIRST_SEGMENT_TIMEOUT,
    MAX_REDIRECTS, PLAYLIST_MAX_BYTES, Pipe, STARTUP_ERROR, count_playlist, has_video,
    playlist_mtime, probe, vetted_proxy,
};
use crate::lock::Locked;
use crate::proc::Ffmpeg;
use crate::runtime::{Runtime, can_encode};
use axum::body::Bytes;
use axum::http::{Request, header};
use fliks_engine_core::api::{
    CodecVariant, EngineEvent, LiveMode, LiveProbe, LiveUpstream, VideoCodec,
};
use fliks_engine_core::args;
use fliks_engine_core::args::LiveArgs;
use fliks_engine_core::hls::{HlsPick, pick_variant, resolve_uri};
use fliks_engine_core::ladder::height_for_bitrate;
use fliks_engine_core::live as core_live;
use http_body_util::{BodyExt, Empty};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;
use tokio::time::Instant;

/// http and https with the platform's roots (`SSL_CERT_FILE`/`SSL_CERT_DIR` override them).
pub(super) fn https(vetted: bool) -> HttpsConnector<HttpConnector<Resolver>> {
    let tls = crate::tls_builder();
    let mut http = HttpConnector::new_with_resolver(Resolver { vetted });
    http.enforce_http(false);
    tls.https_or_http().enable_http1().wrap_connector(http)
}

/// An error with its causes: hyper's own text hides a refused certificate.
fn causes(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        out = format!("{out}: {s}");
        src = s.source();
    }
    out
}

/// Upstreams in order until one feeds the pipe; false once all are spent (the caller ends it).
/// [ts:svc:820-858]
pub(super) async fn recover(rt: &Arc<Runtime>, pipe: &Arc<Pipe>) -> bool {
    loop {
        let pos = {
            let mut m = pipe.m.locked();
            if m.ended {
                return false;
            }
            let pos = m.failover.pick();
            if let Some(p) = pos {
                m.current = p;
            }
            pos
        };
        let Some(pos) = pos else { return false };
        let i = pipe.order[pos];
        let ok = if pipe.dir.is_none() {
            connect_direct(rt, pipe, i).await
        } else {
            packaged(rt, pipe, i).await
        };
        if ok {
            attempt(rt, pipe, i, None);
            return true;
        }
        stop_transport(pipe).await;
        let error = pipe.m.locked().last_error.take();
        attempt(
            rt,
            pipe,
            i,
            Some(error.unwrap_or_else(|| STARTUP_ERROR.into())),
        );
        pipe.m.locked().failover.failed(pos);
    }
}

pub(super) fn attempt(rt: &Runtime, pipe: &Pipe, upstream: usize, error: Option<String>) {
    let probe = pipe.m.locked().probes[upstream].clone();
    rt.events.emit(EngineEvent::LiveAttempt {
        capacity_token: pipe.capacity_token.clone(),
        upstream: upstream as u32,
        upstream_url: pipe.upstreams[upstream].url.clone(),
        upstream_priority: pipe.upstreams[upstream].priority,
        error,
        probe,
    });
}

/// How a packaged run started.
#[derive(PartialEq)]
enum Start {
    Ok,
    Failed,
    /// It decoded on the GPU, opened its input and broke before a segment.
    GpuDecodeFailed,
}

/// What ended the wait for a run's first segment.
#[derive(PartialEq)]
enum Wait {
    Segment,
    /// The decoder of a GPU decode run refused the stream.
    Refused,
    /// The run exited or the 20 s ran out.
    Nothing,
}

/// What a run reads of an upstream: its URL, or the pick of its playlist, with the cookies the
/// playlist's answer set.
struct Read {
    pick: HlsPick,
    cookies: Option<String>,
}

/// Upstream `i` probed if unknown, an HLS one read through its pick, the probe retaken on what
/// is read, then `spawn_packaged`, redone once with CPU decode after `GpuDecodeFailed`.
async fn packaged(rt: &Arc<Runtime>, pipe: &Arc<Pipe>, i: usize) -> bool {
    let u = &pipe.upstreams[i];
    let (has, stale, mut probed_on) = {
        let m = pipe.m.locked();
        (
            m.probes[i].clone(),
            m.stale_probes[i],
            m.probe_urls[i].clone(),
        )
    };
    if !has_video(&has) {
        if let Some(p) = probe(rt, u, None).await {
            keep_probe(pipe, i, p, &u.url);
            probed_on = u.url.clone();
        }
    } else if stale {
        probed_on.clear();
    }
    let hls = core_live::is_hls_container(
        pipe.m.locked().probes[i]
            .as_ref()
            .and_then(|p| p.container.as_deref()),
    );
    let mut read = Read {
        pick: HlsPick {
            video: u.url.clone(),
            audio: None,
        },
        cookies: None,
    };
    if hls {
        match pick(rt, pipe, i).await {
            Ok(Some(r)) => read = r,
            // Not a playlist after all: what the probe said of it is stale.
            Ok(None) => probed_on.clear(),
            Err(e) => {
                pipe.m.locked().last_error = Some(e);
                return false;
            }
        }
    }
    if probed_on != read.pick.video
        && let Some(p) = probe(rt, &with_url(u, &read.pick.video), read.cookies.as_deref()).await
    {
        keep_probe(pipe, i, p, &read.pick.video);
    }
    match spawn_packaged(rt, pipe, i, &read).await {
        Start::Ok => true,
        Start::Failed => false,
        Start::GpuDecodeFailed => {
            stop_transport(pipe).await;
            if pipe.m.locked().ended {
                return false;
            }
            pipe.m.locked().cpu_decode = true;
            let ok = spawn_packaged(rt, pipe, i, &read).await == Start::Ok;
            pipe.m.locked().cpu_decode = ok;
            ok
        }
    }
}

fn keep_probe(pipe: &Pipe, i: usize, p: LiveProbe, url: &str) {
    let mut m = pipe.m.locked();
    m.probes[i] = Some(p);
    m.probe_urls[i] = url.into();
    m.stale_probes[i] = false;
}

fn with_url(u: &LiveUpstream, url: &str) -> LiveUpstream {
    LiveUpstream {
        url: url.into(),
        ..u.clone()
    }
}

/// What a run reads from upstream `i`, fetched at each attempt (variant URLs may carry tokens):
/// the pick of a multivariant playlist, else the upstream itself; `None` if it is no playlist.
async fn pick(rt: &Runtime, pipe: &Pipe, i: usize) -> Result<Option<Read>, String> {
    let u = &pipe.upstreams[i];
    let Some((url, cookies, text)) = fetch_playlist(rt, u).await? else {
        return Ok(None);
    };
    let remux = pipe.m.locked().mode == LiveMode::Remux;
    let height = pipe.bitrate.filter(|b| *b > 0.0).map(height_for_bitrate);
    let pick = pick_variant(&text, &url, remux, height).unwrap_or_else(|| HlsPick {
        video: u.url.clone(),
        audio: None,
    });
    let cookies = (!cookies.is_empty()).then(|| cookies.join("\n"));
    Ok(Some(Read { pick, cookies }))
}

/// The playlist `u` answers with, the URL that answered and the cookies set; `None` when it
/// answers something else. One deadline for all of it: a trickling answer holds no attempt.
async fn fetch_playlist(
    rt: &Runtime,
    u: &LiveUpstream,
) -> Result<Option<(String, Vec<String>, String)>, String> {
    let read = async {
        let answer = http_get(rt, u).await?;
        let mut body = answer.res.into_body();
        let mut text = Vec::new();
        loop {
            match tokio::time::timeout(DIRECT_SNIFF, body.frame()).await {
                Ok(Some(Ok(f))) => text.extend_from_slice(&f.into_data().unwrap_or_default()),
                Ok(Some(Err(e))) => return Err(causes(&e)),
                Ok(None) => break,
                Err(_) => return Err("playlist read stalled".into()),
            }
            // A stream (MPEG-TS) is dropped once its first bytes tell.
            if text.len() >= 16 && !core_live::is_manifest_payload(&text) {
                return Ok(None);
            }
            if text.len() > PLAYLIST_MAX_BYTES {
                return Err("playlist larger than 1 MiB".into());
            }
        }
        Ok(core_live::is_manifest_payload(&text).then(|| {
            let text = String::from_utf8_lossy(&text).into_owned();
            (answer.url, answer.cookies, text)
        }))
    };
    tokio::time::timeout(DIRECT_CONNECT_TIMEOUT, read)
        .await
        .map_err(|_| "timeout of 30000ms exceeded".to_string())?
}

/// One ffmpeg run into the pipe's directory, successful once it wrote a segment. [ts:svc:862-927]
async fn spawn_packaged(rt: &Arc<Runtime>, pipe: &Arc<Pipe>, i: usize, read: &Read) -> Start {
    let u = &pipe.upstreams[i];
    let mut vet_proxy = None;
    if u.vetted {
        match vetted_proxy(rt, &u.url).await {
            Ok(p) => vet_proxy = Some(p),
            Err(e) => {
                pipe.m.locked().last_error = Some(e);
                return Start::Failed;
            }
        }
    }
    let Some(mut caps) = rt.caps() else {
        return Start::Failed;
    };
    let dir = pipe.dir.clone().expect("packaged pipe");
    let (mode, p, cpu_decode, run) = {
        let mut m = pipe.m.locked();
        let p = m.probes[i].clone().unwrap_or_default();
        if m.mode == LiveMode::Remux
            && core_live::forces_transcode(
                p.video_codec.as_deref(),
                m.active_codec.as_deref(),
                p.interlaced() == Some(true),
            )
        {
            m.mode = LiveMode::Transcode;
        }
        m.runs += 1;
        (m.mode, p, m.cpu_decode, m.runs - 1)
    };
    count_playlist(&dir, &pipe.m);
    let written = playlist_mtime(&dir);
    let (append, timeline) = {
        let mut m = pipe.m.locked();
        let append = written.is_some() && written != m.foreign_playlist;
        let first = if append { m.timeline.next } else { 0 };
        if !pipe.use_ts {
            m.inits.retain(|(s, _)| *s < first);
            m.inits.push((first, core_live::init_name(run)));
        }
        (append, m.timeline)
    };
    let (ua, referer, extra) = core_live::split_headers(&u.headers);
    let transcode = mode == LiveMode::Transcode;
    let mut dev = rt.device(&caps);
    let mut slot = None;
    let mut device = None;
    if transcode && caps.hw_accel != fliks_engine_core::api::HwAccel::None {
        let sticky = pipe.m.locked().device.clone();
        let h264 = CodecVariant {
            codec: VideoCodec::H264,
            bit_depth: 8,
            hdr: None,
        };
        if let Some(d) = rt
            .gpu_candidates(sticky.as_deref(), None, |d| can_encode(d, &h264))
            .into_iter()
            .next()
        {
            (caps, dev) = (d.detected.host_caps(), d.device(rt.opencl_device()));
            slot = Some(rt.slots.playback_on(Some(&d.id)));
            pipe.m.locked().device = Some(d.id.clone());
            device = Some(d.id);
        } else {
            caps.hw_accel = fliks_engine_core::api::HwAccel::None;
        }
    }
    let hw_accel = if transcode {
        caps.hw_accel
    } else {
        fliks_engine_core::api::HwAccel::None
    };
    let o = LiveArgs {
        input_url: read.pick.video.clone(),
        audio_url: read.pick.audio.clone(),
        cookies: read.cookies.clone(),
        vet_proxy: vet_proxy.clone(),
        output_dir: dir.to_string_lossy().into_owned(),
        segment_seconds: pipe.segment_seconds,
        window_minutes: pipe.window_minutes,
        mode: if transcode {
            args::LiveMode::Transcode
        } else {
            args::LiveMode::Remux
        },
        use_ts: pipe.use_ts,
        user_agent: ua,
        referer,
        extra_headers: extra,
        hw_accel,
        video_bitrate_bps: pipe.bitrate,
        audio_codec: p.audio_codec.clone(),
        container: p.container.clone(),
        append,
        run,
        timeline,
        probe_seconds: Some(pipe.probe_seconds),
        interlaced: p.interlaced(),
        video_codec: p.video_codec.clone(),
        video_bit_depth: p.bit_depth,
        cpu_decode,
        encode_threads: rt.slots.encode_threads(),
    };
    let gpu_decode = args::live_gpu_decode(&o, &caps);
    let argv = args::live(&o, &caps, &dev);
    let before = segments(&dir).await;
    let spawned = if vet_proxy.is_some() {
        Ffmpeg::spawn_vetted(&rt.ffmpeg, &argv).await
    } else {
        Ffmpeg::spawn(&rt.ffmpeg, &argv).await
    };
    let proc = match spawned {
        Ok(p) => Arc::new(p),
        Err(e) => {
            pipe.m.locked().last_error = Some(format!("spawn: {e}"));
            return Start::Failed;
        }
    };
    {
        let mut m = pipe.m.locked();
        if !m.ended {
            m.proc = Some(proc.clone());
            m.slot = Some(slot.unwrap_or_else(|| rt.slots.playback_on(None)));
        }
    }
    if pipe.m.locked().ended {
        proc.kill().await;
        return Start::Failed;
    }
    let wait = first_segment(&dir, &proc, &before, gpu_decode).await;
    let ok = wait == Wait::Segment;
    let tail = proc.stderr_tail();
    // A break past the input, not the window running out (nor a stop: the pipe ended).
    let broke = wait == Wait::Refused || gpu_decode && proc.exited().is_some();
    let start = if ok {
        Start::Ok
    } else if broke && !pipe.m.locked().ended && !core_live::input_failed(&tail) {
        tracing::warn!(
            url = u.url,
            "live run decoding on the gpu broke before a segment, again with cpu decode:\n{}",
            tail.join("\n")
        );
        Start::GpuDecodeFailed
    } else {
        // What the probe said of its input (its container, the host's stored one) may be wrong.
        if core_live::input_failed(&tail) {
            pipe.m.locked().stale_probes[i] = true;
        }
        Start::Failed
    };
    // A hardware run that ran forgives the device; after a failed GPU decode, its CPU decode run
    // gives the attempt's one verdict.
    if let Some(d) = &device {
        match start {
            Start::Ok => rt.slots.device_ok(d),
            Start::Failed => {
                rt.slots.device_failed(d);
                pipe.m.locked().device = None;
            }
            Start::GpuDecodeFailed => {}
        }
    }
    if ok {
        let mut m = pipe.m.locked();
        // The stall window opens here: a TS run writes no playlist before its first cut.
        m.last_segment_at = Instant::now();
        if mode == LiveMode::Remux {
            m.active_codec = p.video_codec;
        }
    }
    start
}

/// Waits 20 s for a `seg-*` entry `before` lacks or dates otherwise; the run exiting ends the
/// wait, as does the decoder of a `gpu_decode` one refusing the stream. [ts:svc:1001-1047]
async fn first_segment(
    dir: &Path,
    proc: &Ffmpeg,
    before: &HashMap<OsString, SystemTime>,
    gpu_decode: bool,
) -> Wait {
    let until = Instant::now() + FIRST_SEGMENT_TIMEOUT;
    loop {
        let now = segments(dir).await;
        if now.iter().any(|(name, at)| before.get(name) != Some(at)) {
            return Wait::Segment;
        }
        if gpu_decode && core_live::decoder_refused(&proc.stderr_tail()) {
            return Wait::Refused;
        }
        if proc.exited().is_some() || Instant::now() >= until {
            return Wait::Nothing;
        }
        tokio::time::sleep(FIRST_SEGMENT_POLL).await;
    }
}

/// The `seg-*` entries and their mtimes, compared for equality only: no clock is trusted.
async fn segments(dir: &Path) -> HashMap<OsString, SystemTime> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("seg-"))
            .filter_map(|e| Some((e.file_name(), e.metadata().ok()?.modified().ok()?)))
            .collect()
    })
    .await
    .unwrap_or_default()
}

/// The answer to a GET: the URL that gave it, the cookies set on the way (`scoped_cookie`).
struct Answer {
    url: String,
    cookies: Vec<String>,
    res: hyper::Response<hyper::body::Incoming>,
}

/// A GET (http or https) following up to 5 redirects, each hop sent the cookies the earlier ones
/// set for its host, as ffmpeg's own requests do; `Err` is the reason to record.
async fn http_get(rt: &Runtime, u: &LiveUpstream) -> Result<Answer, String> {
    let (ua, _, _) = core_live::split_headers(&u.headers);
    let mut url = u.url.clone();
    let mut cookies: Vec<String> = vec![];
    let client = if u.vetted {
        &rt.live.vetted_client
    } else {
        &rt.live.client
    };
    for _ in 0..=MAX_REDIRECTS {
        if u.vetted {
            vet::check_literal(&url)?;
        }
        let mut b = Request::get(&url).header(
            header::USER_AGENT,
            ua.as_deref()
                .filter(|v| !v.is_empty())
                .unwrap_or(args::LIVE_USER_AGENT),
        );
        for (k, v) in u
            .headers
            .iter()
            .filter(|(k, _)| !k.eq_ignore_ascii_case("user-agent"))
        {
            b = b.header(k, v);
        }
        if let Some(c) = core_live::cookie_header(&cookies, &url) {
            b = b.header(header::COOKIE, c);
        }
        let req = b.body(Empty::new()).map_err(|e| e.to_string())?;
        let res = client.request(req).await.map_err(|e| causes(&e))?;
        for c in res.headers().get_all(header::SET_COOKIE) {
            if let Ok(c) = c.to_str() {
                cookies.push(core_live::scoped_cookie(c, &url));
            }
        }
        let status = res.status();
        if status.is_redirection()
            && let Some(loc) = res
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
        {
            url = resolve_uri(&url, loc);
            continue;
        }
        if !status.is_success() {
            return Err(format!(
                "Request failed with status code {}",
                status.as_u16()
            ));
        }
        return Ok(Answer { url, cookies, res });
    }
    Err("Maximum number of redirects exceeded".into())
}

/// Opens the shared upstream of a direct pipe and tees it to the viewers. [ts:svc:929-999]
async fn connect_direct(rt: &Arc<Runtime>, pipe: &Arc<Pipe>, i: usize) -> bool {
    let u = &pipe.upstreams[i];
    let res = match tokio::time::timeout(DIRECT_CONNECT_TIMEOUT, http_get(rt, u)).await {
        Ok(Ok(a)) => a.res,
        Ok(Err(e)) => {
            pipe.m.locked().last_error = Some(e);
            return false;
        }
        Err(_) => {
            pipe.m.locked().last_error = Some("timeout of 30000ms exceeded".into());
            return false;
        }
    };
    let content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("video/mp2t")
        .to_string();
    if core_live::is_manifest_content_type(&content_type) {
        pipe.m.locked().not_streamable = true;
        return false;
    }
    let mut body = res.into_body();
    let first = match tokio::time::timeout(DIRECT_SNIFF, body.frame()).await {
        Err(_) => None,
        Ok(Some(Ok(f))) => f.into_data().ok(),
        Ok(_) => {
            pipe.m.locked().last_error = Some("upstream closed before any data".into());
            return false;
        }
    };
    if first.as_deref().is_some_and(core_live::is_manifest_payload) {
        pipe.m.locked().not_streamable = true;
        return false;
    }
    let down = Arc::new(AtomicBool::new(false));
    let (p, d) = (pipe.clone(), down.clone());
    let reader = tokio::spawn(async move {
        if let Some(c) = first {
            fan_out(&p, c);
        }
        while let Some(Ok(f)) = body.frame().await {
            if let Ok(c) = f.into_data() {
                fan_out(&p, c);
            }
        }
        d.store(true, Ordering::SeqCst);
    });
    let mut m = pipe.m.locked();
    m.content_type = Some(content_type);
    m.last_segment_at = Instant::now();
    m.direct = Some(Direct {
        reader: reader.abort_handle(),
        down,
    });
    if m.ended {
        reader.abort();
        return false;
    }
    true
}

/// A viewer that cannot keep up is dropped, never the upstream. [ts:svc:970-984]
fn fan_out(pipe: &Pipe, chunk: Bytes) {
    let mut m = pipe.m.locked();
    m.last_segment_at = Instant::now();
    m.tees.retain(|(_, tx)| tx.try_send(chunk.clone()).is_ok());
}

/// Kills the process or drops the connection feeding the pipe; viewers and tees stay.
/// [ts:svc:1121-1133]
pub(super) async fn stop_transport(pipe: &Pipe) {
    let (proc, slot, direct) = {
        let mut m = pipe.m.locked();
        (m.proc.take(), m.slot.take(), m.direct.take())
    };
    if let Some(d) = direct {
        d.reader.abort();
    }
    if let Some(p) = proc {
        // No polite stop: ffmpeg would flush what it buffered into the session's playlist.
        p.kill().await;
    }
    drop(slot);
}

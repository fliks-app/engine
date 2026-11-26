//! The two transports of an upstream (packaged ffmpeg run, direct tee) and their failover.

use super::vet::{self, Resolver};
use super::{
    DIRECT_CONNECT_TIMEOUT, DIRECT_SNIFF, Direct, FIRST_SEGMENT_POLL, FIRST_SEGMENT_TIMEOUT,
    MAX_REDIRECTS, Pipe, STARTUP_ERROR, has_video, probe, vetted_proxy,
};
use crate::lock::Locked;
use crate::proc::Ffmpeg;
use crate::runtime::{Runtime, can_encode};
use axum::body::Bytes;
use axum::http::{Request, header};
use fliks_engine_core::api::{CodecVariant, EngineEvent, LiveMode, VideoCodec};
use fliks_engine_core::args;
use fliks_engine_core::args::LiveArgs;
use fliks_engine_core::live as core_live;
use http_body_util::{BodyExt, Empty};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
pub(super) async fn recover(rt: &Arc<Runtime>, pipe: &Arc<Pipe>, mut attempted_once: bool) -> bool {
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
            packaged(rt, pipe, i, attempted_once).await
        };
        attempted_once = true;
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
    /// It decoded on the GPU and exited before a segment.
    GpuDecodeFailed,
}

/// `spawn_packaged`, redone once with CPU decode (the GPU still encodes) when a run that decoded
/// on the GPU exited before a segment; the pipe keeps CPU decode for its later runs when that run
/// starts, else the upstream was at fault. Not after the 20 s window: a dead upstream would double
/// the wait before failover.
async fn packaged(rt: &Arc<Runtime>, pipe: &Arc<Pipe>, i: usize, attempted_once: bool) -> bool {
    match spawn_packaged(rt, pipe, i, attempted_once).await {
        Start::Ok => true,
        Start::Failed => false,
        Start::GpuDecodeFailed => {
            stop_transport(pipe).await;
            if pipe.m.locked().ended {
                return false;
            }
            pipe.m.locked().cpu_decode = true;
            let ok = spawn_packaged(rt, pipe, i, attempted_once).await == Start::Ok;
            pipe.m.locked().cpu_decode = ok;
            ok
        }
    }
}

/// One ffmpeg run into the pipe's directory, successful once a segment exists. [ts:svc:862-927]
async fn spawn_packaged(
    rt: &Arc<Runtime>,
    pipe: &Arc<Pipe>,
    i: usize,
    attempted_once: bool,
) -> Start {
    let u = &pipe.upstreams[i];
    let has = pipe.m.locked().probes[i].clone();
    if !has_video(&has)
        && let Some(p) = probe(rt, u).await
    {
        pipe.m.locked().probes[i] = Some(p);
    }
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
    let (mode, p, fresh, cpu_decode) = {
        let mut m = pipe.m.locked();
        let p = m.probes[i].clone().unwrap_or_default();
        let fresh = m.mode == LiveMode::Remux
            && core_live::forces_transcode(
                p.video_codec.as_deref(),
                m.active_codec.as_deref(),
                p.interlaced() == Some(true),
            );
        if fresh {
            m.mode = LiveMode::Transcode;
        }
        (m.mode, p, fresh, m.cpu_decode)
    };
    let append = !fresh
        && attempted_once
        && tokio::fs::try_exists(dir.join("index.m3u8"))
            .await
            .unwrap_or(false);
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
        input_url: u.url.clone(),
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
        append,
        probe_seconds: Some(pipe.probe_seconds),
        interlaced: p.interlaced(),
        video_codec: p.video_codec.clone(),
        video_bit_depth: p.bit_depth,
        cpu_decode,
        encode_threads: rt.slots.encode_threads(),
    };
    let gpu_decode = args::live_gpu_decode(&o, &caps);
    let argv = args::live(&o, &caps, &dev);
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
            m.last_segment_at = Instant::now();
            m.last_seg = None;
        }
    }
    if pipe.m.locked().ended {
        proc.stop().await;
        return Start::Failed;
    }
    let ok = first_segment(&dir, &proc).await;
    // An exit, not the window running out (nor a stop: the pipe ended).
    let start = if ok {
        Start::Ok
    } else if gpu_decode && proc.exited().is_some() && !pipe.m.locked().ended {
        tracing::warn!(
            url = u.url,
            "live gpu decode failed, cpu decode from here:\n{}",
            proc.stderr_tail().join("\n")
        );
        Start::GpuDecodeFailed
    } else {
        Start::Failed
    };
    // A hardware run that ran forgives the device, as playback's. A failed GPU decode leaves the
    // verdict to its CPU decode run, on the same device: one per attempt, as before.
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
    if ok && mode == LiveMode::Remux {
        pipe.m.locked().active_codec = p.video_codec;
    }
    start
}

/// Any `seg-*` entry within 20 s; the run exiting first fails. [ts:svc:1001-1047]
async fn first_segment(dir: &Path, proc: &Ffmpeg) -> bool {
    let until = Instant::now() + FIRST_SEGMENT_TIMEOUT;
    loop {
        if has_segment(dir).await {
            return true;
        }
        if proc.exited().is_some() || Instant::now() >= until {
            return false;
        }
        tokio::time::sleep(FIRST_SEGMENT_POLL).await;
    }
}

async fn has_segment(dir: &Path) -> bool {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("seg-"))
    })
    .await
    .unwrap_or(false)
}

/// One blocking hop for the whole listing, not one per batch of entries.
pub(super) async fn highest_segment(dir: &Path) -> Option<u32> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| core_live::segment_number(&e.file_name().to_string_lossy()))
            .max()
    })
    .await
    .ok()
    .flatten()
}

/// A GET (http or https) following up to 5 redirects; `Err` is the reason to record.
async fn http_get(
    rt: &Runtime,
    u: &fliks_engine_core::api::LiveUpstream,
) -> Result<hyper::Response<hyper::body::Incoming>, String> {
    let (ua, _, _) = core_live::split_headers(&u.headers);
    let mut url = u.url.clone();
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
        let req = b.body(Empty::new()).map_err(|e| e.to_string())?;
        let res = client.request(req).await.map_err(|e| causes(&e))?;
        let status = res.status();
        if status.is_redirection()
            && let Some(loc) = res
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
        {
            url = if loc.starts_with('/') {
                let uri: hyper::Uri = url
                    .parse()
                    .map_err(|e: hyper::http::uri::InvalidUri| e.to_string())?;
                let scheme = uri.scheme_str().unwrap_or("http");
                let host = uri.authority().map_or("", |a| a.as_str());
                format!("{scheme}://{host}{loc}")
            } else {
                loc.to_string()
            };
            continue;
        }
        if !status.is_success() {
            return Err(format!(
                "Request failed with status code {}",
                status.as_u16()
            ));
        }
        return Ok(res);
    }
    Err("Maximum number of redirects exceeded".into())
}

/// Opens the shared upstream of a direct pipe and tees it to the viewers. [ts:svc:929-999]
async fn connect_direct(rt: &Arc<Runtime>, pipe: &Arc<Pipe>, i: usize) -> bool {
    let u = &pipe.upstreams[i];
    let res = match tokio::time::timeout(DIRECT_CONNECT_TIMEOUT, http_get(rt, u)).await {
        Ok(Ok(r)) => r,
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

/// Stops the process or the connection feeding the pipe; viewers and tees stay. [ts:svc:1121-1133]
pub(super) async fn stop_transport(pipe: &Pipe) {
    let (proc, slot, direct) = {
        let mut m = pipe.m.locked();
        (m.proc.take(), m.slot.take(), m.direct.take())
    };
    if let Some(d) = direct {
        d.reader.abort();
    }
    if let Some(p) = proc {
        p.stop().await;
    }
    drop(slot);
}

//! The two transports of an upstream (packaged ffmpeg run, direct tee) and their failover.

use super::vet::{self, Resolver};
use super::{
    DIRECT_CONNECT_TIMEOUT, DIRECT_SNIFF, Direct, FIRST_SEGMENT_POLL, FIRST_SEGMENT_TIMEOUT,
    MAX_REDIRECTS, Pipe, STARTUP_ERROR, has_video, probe, vetted_proxy,
};
use crate::lock::Locked;
use crate::proc::Ffmpeg;
use crate::runtime::Runtime;
use axum::body::Bytes;
use axum::http::{Request, header};
use fliks_engine_core::api::{EngineEvent, LiveMode};
use fliks_engine_core::args;
use fliks_engine_core::args::LiveArgs;
use fliks_engine_core::live as core_live;
use http_body_util::{BodyExt, Empty};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::connect::HttpConnector;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::time::Instant;

/// http and https with the platform's roots (`SSL_CERT_FILE`/`SSL_CERT_DIR` override them).
pub(super) fn https(vetted: bool) -> HttpsConnector<HttpConnector<Resolver>> {
    let tls = HttpsConnectorBuilder::new()
        .with_native_roots()
        .unwrap_or_else(|e| {
            tracing::warn!("no root certificates ({e}): https upstreams will fail");
            let roots = rustls::RootCertStore::empty();
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            HttpsConnectorBuilder::new().with_tls_config(config)
        });
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
            spawn_packaged(rt, pipe, i, attempted_once).await
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

/// One ffmpeg run into the pipe's directory, successful once a segment exists. [ts:svc:862-927]
async fn spawn_packaged(
    rt: &Arc<Runtime>,
    pipe: &Arc<Pipe>,
    i: usize,
    attempted_once: bool,
) -> bool {
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
                return false;
            }
        }
    }
    let Some(caps) = rt.caps() else { return false };
    let dir = pipe.dir.clone().expect("packaged pipe");
    let (mode, video, audio, fresh) = {
        let mut m = pipe.m.locked();
        let p = m.probes[i].clone().unwrap_or_default();
        let fresh = m.mode == LiveMode::Remux
            && core_live::forces_transcode(p.video_codec.as_deref(), m.active_codec.as_deref());
        if fresh {
            m.mode = LiveMode::Transcode;
        }
        (m.mode, p.video_codec, p.audio_codec, fresh)
    };
    let append = !fresh
        && attempted_once
        && tokio::fs::try_exists(dir.join("index.m3u8"))
            .await
            .unwrap_or(false);
    let (ua, referer, extra) = core_live::split_headers(&u.headers);
    let transcode = mode == LiveMode::Transcode;
    let hw_accel = if transcode {
        caps.hw_accel
    } else {
        fliks_engine_core::api::HwAccel::None
    };
    let argv = args::live(
        &LiveArgs {
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
            audio_codec: audio,
            append,
            probe_seconds: Some(pipe.probe_seconds),
            encode_threads: rt.slots.encode_threads(),
        },
        &caps,
        &rt.device(&caps),
    );
    let spawned = if vet_proxy.is_some() {
        Ffmpeg::spawn_vetted(&rt.ffmpeg, &argv).await
    } else {
        Ffmpeg::spawn(&rt.ffmpeg, &argv).await
    };
    let proc = match spawned {
        Ok(p) => Arc::new(p),
        Err(e) => {
            pipe.m.locked().last_error = Some(format!("spawn: {e}"));
            return false;
        }
    };
    {
        let mut m = pipe.m.locked();
        if !m.ended {
            m.proc = Some(proc.clone());
            m.slot = Some(
                rt.slots
                    .playback(hw_accel == fliks_engine_core::api::HwAccel::None),
            );
            m.last_segment_at = Instant::now();
            m.last_seg = None;
        }
    }
    if pipe.m.locked().ended {
        proc.stop().await;
        return false;
    }
    let ok = first_segment(&dir, &proc).await;
    if ok && mode == LiveMode::Remux {
        pipe.m.locked().active_codec = video;
    }
    ok
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

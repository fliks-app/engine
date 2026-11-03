//! Data plane: the host's `/api/stream/...` routes, byte for byte, authorized by the host
//! resolver. Spec: `docs/spec/runtime.md` section 6.
// The error side of these helpers is the HTTP answer itself.
#![allow(clippy::result_large_err)]

mod direct;
mod hls;
mod item;
mod livetv;
mod subtitles;

pub use self::direct::parse_range;
pub use self::subtitles::with_timestamp_map;

use self::direct::{attachment, base_name, direct, part, ranged_file, sprite};
use self::hls::{
    audio_list, audio_seg, cmcd, iframe_list, iframe_seg, master, run_capture, segment, variant,
};
use self::livetv::{live_direct, live_playlist, live_segment};
use self::subtitles::{
    embedded_download, embedded_list, embedded_vtt, external_download, external_list, external_vtt,
};
use crate::resolve::{Ask, Host, ResolveError, Resolved};
use crate::runtime::{Runtime, Session};
use axum::body::Body;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Router, middleware};
use bytes::BytesMut;
use fliks_engine_core::api::{
    ResolveCredentialSource, ResolveMediaId, ResolvePurpose, ResolvedPart, SessionExpired,
};
use fliks_engine_core::decide::timeline::served_duration;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt};
use tower::ServiceExt;

type Q = Query<HashMap<String, String>>;
type S = State<Arc<Runtime>>;

const PLAYLIST: &str = "application/vnd.apple.mpegurl";

/// The data-plane router, mountable behind the gateway or on its own listener.
pub fn router(rt: Arc<Runtime>) -> Router {
    let base = routes(rt);
    let inner = base.clone();
    base.fallback(move |req: Request| with_path_credential(inner.clone(), req))
        .layer(middleware::map_request(untrusted_source))
}

/// Only the `/cred` rewrite may say a credential came from the path.
const SOURCE_HEADER: &str = "x-engine-credential-source";
const MAX_CREDENTIAL: usize = 8 * 1024;

async fn untrusted_source(mut req: Request) -> Request {
    req.headers_mut().remove(SOURCE_HEADER);
    req
}

fn routes(rt: Arc<Runtime>) -> Router {
    Router::new()
        .route("/api/stream/{id}", get(direct))
        .route("/api/stream/{id}/master.m3u8", get(master))
        .route("/api/stream/{id}/parts/{n}", get(part))
        .route("/api/stream/{id}/item/index.m3u8", get(item::list))
        .route("/api/stream/{id}/item/{seg}", get(item::file))
        .route("/api/stream/{id}/iframe/index.m3u8", get(iframe_list))
        .route("/api/stream/{id}/iframe/{seg}", get(iframe_seg))
        .route("/api/stream/{id}/audio/{idx}/index.m3u8", get(audio_list))
        .route("/api/stream/{id}/audio/{idx}/{seg}", get(audio_seg))
        .route("/api/stream/{id}/thumbnails/{file}", get(sprite))
        .route(
            "/api/stream/{id}/subtitles/embedded/{idx}",
            get(embedded_vtt),
        )
        .route(
            "/api/stream/{id}/subtitles/embedded/{idx}/download",
            get(embedded_download),
        )
        .route(
            "/api/stream/{id}/subtitles/embedded/{idx}/index.m3u8",
            get(embedded_list),
        )
        .route("/api/stream/{id}/subtitles/{sub}", get(external_vtt))
        .route(
            "/api/stream/{id}/subtitles/{sub}/download",
            get(external_download),
        )
        .route(
            "/api/stream/{id}/subtitles/{sub}/index.m3u8",
            get(external_list),
        )
        .route("/api/stream/{id}/{quality}/index.m3u8", get(variant))
        .route("/api/stream/{id}/{quality}/{seg}", get(segment))
        .route("/api/livetv/sessions/{sid}/index.m3u8", get(live_playlist))
        .route("/api/livetv/sessions/{sid}/direct.ts", get(live_direct))
        .route("/api/livetv/sessions/{sid}/{segment}", get(live_segment))
        .layer(middleware::map_response(cors))
        .with_state(rt)
}

/// `/cred/<token>/api/...` is the same route with `?token=<token>`: the credential rides the path
/// for clients that cannot set a header or keep a query (an e-reader's download link).
async fn with_path_credential(inner: Router, mut req: Request) -> Response {
    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        return text(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed");
    }
    match path_credential(req.uri()) {
        Some(uri) => {
            *req.uri_mut() = uri;
            req.headers_mut()
                .insert(SOURCE_HEADER, HeaderValue::from_static("path"));
            inner.oneshot(req).await.into_response()
        }
        None => text(StatusCode::NOT_FOUND, "Not Found"),
    }
}

/// `/cred/<token>/api/stream/<id>...?q` -> `/api/stream/<id>...?q&token=<token>`, replacing any
/// `token` of `q`; no other tail is served.
fn path_credential(uri: &Uri) -> Option<Uri> {
    let (raw, tail) = uri.path().strip_prefix("/cred/")?.split_once('/')?;
    let id = tail.strip_prefix("api/stream/")?;
    let digits = id.split('/').next().filter(|d| !d.is_empty())?;
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let kept = uri
        .query()
        .unwrap_or_default()
        .split('&')
        .filter(|kv| !kv.is_empty() && kv.split('=').next() != Some("token"))
        .map(|kv| format!("{kv}&"))
        .collect::<String>();
    format!("/{tail}?{kept}token={}", encode(&percent_decode(raw)?))
        .parse()
        .ok()
}

fn percent_decode(s: &str) -> Option<String> {
    let (b, mut out, mut i) = (s.as_bytes(), Vec::new(), 0);
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            if !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

async fn cors(mut res: Response) -> Response {
    res.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    res
}

/// A reader as a response body in `chunk`-byte reads, reusing the buffer once a chunk is sent.
pub(crate) fn stream_body<R: AsyncRead + Unpin + Send + 'static>(r: R, chunk: usize) -> Body {
    Body::from_stream(futures_util::stream::try_unfold(
        (r, BytesMut::new()),
        move |(mut r, mut buf)| async move {
            buf.reserve(chunk);
            let n = r.read_buf(&mut buf).await?;
            Ok::<_, std::io::Error>((n > 0).then(|| (buf.split().freeze(), (r, buf))))
        },
    ))
}

fn text(status: StatusCode, body: &str) -> Response {
    (status, body.to_string()).into_response()
}

fn retry() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (header::RETRY_AFTER, "2"),
            (header::CACHE_CONTROL, "no-store, no-cache, must-revalidate"),
        ],
    )
        .into_response()
}

fn expired(sid: Option<&str>) -> Response {
    (
        StatusCode::GONE,
        axum::Json(SessionExpired {
            code: "session_expired".into(),
            sid: sid.map(String::from),
            message: "live session no longer exists".into(),
        }),
    )
        .into_response()
}

fn playlist(body: String) -> Response {
    (
        [
            (header::CONTENT_TYPE, PLAYLIST),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

fn file_id(s: &str) -> Result<u32, Response> {
    s.parse().map_err(|_| {
        text(
            StatusCode::BAD_REQUEST,
            "Validation failed (numeric string is expected)",
        )
    })
}

/// `encodeURIComponent`.
fn encode(s: &str) -> String {
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

/// `?token=..&sid=..` of the request, for every URI a playlist names. [ts:modules/streaming/streaming.controller.ts:175-199]
fn token_param(q: &HashMap<String, String>) -> String {
    let parts: Vec<String> = ["token", "sid"]
        .iter()
        .filter_map(|k| {
            q.get(*k)
                .filter(|v| !v.is_empty())
                .map(|v| format!("{k}={}", encode(v)))
        })
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

/// The credential as received, in the host's extractor order: a `/cred/<token>/` path token, else
/// the `Cookie` header, else a `Bearer` `Authorization` value, else `?token=`, else a `Basic` one
/// (device clients), with where it was found. `None` when it is over `MAX_CREDENTIAL`. [ts:modules/auth/strategies/jwt.strategy.ts:43-47]
fn credential(
    headers: &HeaderMap,
    q: &HashMap<String, String>,
) -> Option<(String, Option<ResolveCredentialSource>)> {
    use ResolveCredentialSource as Src;
    let h = |n| {
        headers
            .get(n)
            .and_then(|v: &HeaderValue| v.to_str().ok())
            .filter(|v| !v.is_empty())
    };
    let scheme = |name: &str| {
        h(header::AUTHORIZATION).filter(|v| {
            v.split_once(' ')
                .is_some_and(|(s, _)| s.eq_ignore_ascii_case(name))
        })
    };
    let query = q.get("token").filter(|t| !t.is_empty());
    let from_path = headers.get(SOURCE_HEADER).is_some_and(|v| v == "path");
    let (cred, src) = if let Some(t) = query.filter(|_| from_path) {
        (t.clone(), Some(Src::Path))
    } else if let Some(c) = h(header::COOKIE) {
        (c.to_string(), Some(Src::Cookie))
    } else if let Some(b) = scheme("bearer") {
        (b.to_string(), Some(Src::Bearer))
    } else if let Some(t) = query {
        (t.clone(), Some(Src::Query))
    } else if let Some(b) = scheme("basic") {
        (b.to_string(), Some(Src::Basic))
    } else {
        (String::new(), None)
    };
    (cred.len() <= MAX_CREDENTIAL).then_some((cred, src))
}

struct Access {
    resolved: Resolved,
    /// The request's `sid`, when it names a session of this file.
    session: Option<Arc<Session>>,
}

/// Unknown sid 410 before any resolver call, then another owner's or file's 403.
async fn access(
    rt: &Arc<Runtime>,
    id: u32,
    purpose: ResolvePurpose,
    headers: &HeaderMap,
    q: &HashMap<String, String>,
    (subtitle_id, stream_index): (Option<u32>, Option<u32>),
) -> Result<Access, Response> {
    let sid = q.get("sid").filter(|s| !s.is_empty());
    let config = rt.current_config();
    let host = config.host_url.clone().map(|url| Host {
        url,
        token: config.host_token.clone(),
    });
    let Some((cred, source)) = credential(headers, q) else {
        return Err(text(StatusCode::BAD_REQUEST, "Credential too long"));
    };
    let ask = Ask {
        media: ResolveMediaId::File(id),
        credential: &cred,
        source,
        purpose,
        subtitle_id,
        stream_index,
    };
    let session = sid.map(|s| rt.session(s));
    if let Some(None) = session {
        return Err(expired(sid.map(String::as_str)));
    }
    let resolved = match rt.resolver.resolve(host.as_ref(), ask).await {
        Ok(r) => r,
        Err(ResolveError::Denied { .. }) => return Err(text(StatusCode::FORBIDDEN, "Forbidden")),
        // Host down: a live session of this file keeps playing (callers map the host path).
        Err(ResolveError::Unavailable) => match &session {
            Some(Some(s)) if s.media_file_id == id => Resolved {
                path: s.req.decide.media.path.clone(),
                owner: s.owner.clone(),
                format: None,
                parts: s
                    .req
                    .decide
                    .media
                    .parts
                    .iter()
                    .map(|p| ResolvedPart {
                        path: p.path.clone(),
                        duration_s: Some(p.duration_s()),
                        start_s: p.start_s,
                        end_s: p.end_s,
                        format: None,
                    })
                    .collect(),
            },
            _ => return Err(retry()),
        },
    };
    let session = match session {
        None | Some(None) => None,
        Some(Some(s)) if s.owner != resolved.owner || s.media_file_id != id => {
            return Err(text(StatusCode::FORBIDDEN, "Forbidden"));
        }
        Some(Some(s)) => Some(s),
    };
    Ok(Access { resolved, session })
}

/// The HLS routes' session: the sid's, else the owner's latest of the file, else 410.
async fn hls(
    rt: &Arc<Runtime>,
    id: u32,
    headers: &HeaderMap,
    q: &HashMap<String, String>,
    segment: Option<u32>,
) -> Result<(Arc<Session>, Resolved), Response> {
    let a = access(rt, id, ResolvePurpose::Stream, headers, q, (None, None)).await?;
    let s = match a.session {
        Some(s) => s,
        None => rt
            .current_session(a.resolved.owner.as_deref(), id)
            .ok_or_else(|| expired(None))?,
    };
    rt.touch(&s, segment);
    Ok((s, a.resolved))
}

fn prefix(id: u32) -> String {
    format!("/api/stream/{id}")
}

fn duration_of(s: &Session, q: &HashMap<String, String>) -> f64 {
    q.get("duration")
        .and_then(|d| d.parse::<f64>().ok())
        .filter(|d| *d > 0.0)
        .map_or(s.plan.duration, |d| served_duration(d, &s.plan.timeline))
}

#[cfg(test)]
mod tests {
    use super::direct::content_type;
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=900-", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=-5000", 1000), Some((0, 999)));
        assert_eq!(parse_range("bytes=990-2000", 1000), Some((990, 999)));
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("bytes=5-1", 1000), None);
        assert_eq!(parse_range("bytes=a-b", 1000), None);
        assert_eq!(parse_range("bytes=0-1,5-6", 1000), Some((0, 1)));
    }

    #[test]
    fn a_path_credential_becomes_the_token_query() {
        let at = |u: &str| path_credential(&u.parse().unwrap()).map(|u| u.to_string());
        assert_eq!(
            at("/cred/a%2Bb/api/stream/7/parts/2").as_deref(),
            Some("/api/stream/7/parts/2?token=a%2Bb")
        );
        assert_eq!(
            at("/cred/k/api/stream/7?download=1&token=old&sid=s").as_deref(),
            Some("/api/stream/7?download=1&sid=s&token=k")
        );
        assert_eq!(at("/cred/k"), None);
        for bad in [
            "/cred/%zz/api/stream/7",
            "/cred/%+1/api/stream/7",
            "/cred/k/api/livetv/sessions/s/index.m3u8",
            "/cred/k/api/stream/info/qualities",
            "/cred/k/api/stream/",
            "/cred/k/v1/config",
        ] {
            assert_eq!(at(bad), None, "{bad}");
        }
    }

    #[test]
    fn timestamp_map_and_token() {
        assert_eq!(
            with_timestamp_map("WEBVTT\n\n00:00.000 --> 00:01.000\nhi\n", 1.5),
            "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:135000,LOCAL:00:00:00.000\n\n00:00.000 --> 00:01.000\nhi\n"
        );
        assert!(with_timestamp_map("WEBVTT\n", -2.25).contains("MPEGTS:0,LOCAL:00:00:02.250"));
        let q: HashMap<String, String> =
            [("token".into(), "a b/c".into()), ("sid".into(), "x".into())].into();
        assert_eq!(token_param(&q), "?token=a%20b%2Fc&sid=x");
        assert_eq!(token_param(&HashMap::new()), "");
    }

    #[test]
    fn credential_order_and_content_type() {
        use ResolveCredentialSource as Src;
        let mut h = HeaderMap::new();
        let q: HashMap<String, String> = [("token".into(), "t".into())].into();
        let cred = |h: &HeaderMap, q: &HashMap<String, String>| credential(h, q).unwrap();
        assert_eq!(cred(&h, &q), ("t".into(), Some(Src::Query)));
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic x"));
        assert_eq!(cred(&h, &q), ("t".into(), Some(Src::Query)));
        assert_eq!(
            cred(&h, &HashMap::new()),
            ("Basic x".into(), Some(Src::Basic))
        );
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer j"));
        assert_eq!(cred(&h, &q), ("Bearer j".into(), Some(Src::Bearer)));
        h.insert(header::COOKIE, HeaderValue::from_static("a=1"));
        assert_eq!(cred(&h, &q), ("a=1".into(), Some(Src::Cookie)));
        assert_eq!(
            cred(&HeaderMap::new(), &HashMap::new()),
            (String::new(), None)
        );
        let long: HashMap<String, String> =
            [("token".into(), "x".repeat(MAX_CREDENTIAL + 1))].into();
        assert!(credential(&HeaderMap::new(), &long).is_none());
        assert_eq!(content_type("/a/b.MKV", None), "video/x-matroska");
        assert_eq!(content_type("/a/b.bin", Some("video/mp4")), "video/mp4");
        assert_eq!(content_type("/a/b.mp4", None), "video/mp4");
        assert_eq!(content_type("/a/b.M4B", None), "audio/mp4");
    }
}

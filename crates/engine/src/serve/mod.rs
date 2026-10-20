//! Data plane: the host's `/api/stream/...` routes, byte for byte, authorized by the host
//! resolver. Spec: `docs/spec/runtime.md` section 6.
// The error side of these helpers is the HTTP answer itself.
#![allow(clippy::result_large_err)]

mod direct;
mod hls;
mod livetv;
mod subtitles;

pub use self::direct::parse_range;
pub use self::subtitles::with_timestamp_map;

use self::direct::{attachment, base_name, direct, ranged_file, sprite};
use self::hls::{
    audio_list, audio_seg, cmcd, iframe_list, iframe_seg, master, run_capture, segment, variant,
};
use self::livetv::{live_direct, live_playlist, live_segment};
use self::subtitles::{
    embedded_download, embedded_list, embedded_vtt, external_download, external_list, external_vtt,
};
use crate::resolve::{Ask, Host, ResolveError, Resolved};
use crate::runtime::{Runtime, Session};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Router, middleware};
use fliks_engine_core::api::{ResolveMediaId, ResolvePurpose, SessionExpired};
use std::collections::HashMap;
use std::sync::Arc;

type Q = Query<HashMap<String, String>>;
type S = State<Arc<Runtime>>;

const PLAYLIST: &str = "application/vnd.apple.mpegurl";

/// The data-plane router, mountable behind the gateway or on its own listener.
pub fn router(rt: Arc<Runtime>) -> Router {
    Router::new()
        .route("/api/stream/{id}", get(direct))
        .route("/api/stream/{id}/master.m3u8", get(master))
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

async fn cors(mut res: Response) -> Response {
    res.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    res
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

/// The credential as received, in the host's extractor order: the `Cookie` header, else a
/// `Bearer` `Authorization` value, else `?token=`. [ts:modules/auth/strategies/jwt.strategy.ts:43-47]
fn credential(headers: &HeaderMap, q: &HashMap<String, String>) -> String {
    let h = |n| {
        headers
            .get(n)
            .and_then(|v: &HeaderValue| v.to_str().ok())
            .filter(|v| !v.is_empty())
    };
    let bearer = h(header::AUTHORIZATION).filter(|v| {
        v.get(..7)
            .is_some_and(|s| s.eq_ignore_ascii_case("bearer "))
    });
    h(header::COOKIE)
        .or(bearer)
        .map(String::from)
        .or_else(|| q.get("token").filter(|t| !t.is_empty()).cloned())
        .unwrap_or_default()
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
    let cred = credential(headers, q);
    let ask = Ask {
        media: ResolveMediaId::File(id),
        credential: &cred,
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
        .unwrap_or(s.plan.duration)
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
        let mut h = HeaderMap::new();
        let q: HashMap<String, String> = [("token".into(), "t".into())].into();
        assert_eq!(credential(&h, &q), "t");
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic x"));
        assert_eq!(credential(&h, &q), "t");
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer j"));
        assert_eq!(credential(&h, &q), "Bearer j");
        h.insert(header::COOKIE, HeaderValue::from_static("a=1"));
        assert_eq!(credential(&h, &q), "a=1");
        assert_eq!(content_type("/a/b.MKV", None), "video/x-matroska");
        assert_eq!(content_type("/a/b.bin", Some("video/mp4")), "video/mp4");
    }
}

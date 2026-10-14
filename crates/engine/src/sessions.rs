//! Control endpoints of the runtime: sessions, revocation, cache and events.
//! Spec: `docs/spec/runtime.md` sections 3, 5 and 7.
use crate::AppState;
use crate::runtime::OpenError;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{StatusCode, header::RETRY_AFTER},
    response::{IntoResponse, Response},
};
use fliks_engine_core::api::{
    CacheDeleteResult, CacheSummary, EngineEvent, HeartbeatRequest, HeartbeatResponse,
    InvalidateRequest, InvalidateResult, OpenSessionRequest, OpenSessionResponse, RevokeRequest,
    RevokeResult, SessionAction, SessionCommand, SessionSnapshot,
};
use serde::Deserialize;
use std::sync::Arc;

type S = State<Arc<AppState>>;

#[utoipa::path(post, path = "/v1/sessions", request_body = OpenSessionRequest, responses(
    (status = 200, body = OpenSessionResponse),
    (status = 400, description = "no config set yet, or an invalid ownerId/path"),
    (status = 503, description = "hardware detection has not finished (Retry-After: 1)")))]
pub async fn open(State(s): S, Json(req): Json<OpenSessionRequest>) -> Response {
    match s.rt.open(req) {
        Ok(r) => Json(r).into_response(),
        Err(OpenError::NoConfig) => (StatusCode::BAD_REQUEST, "no config set yet").into_response(),
        Err(OpenError::BadRequest(m)) => (StatusCode::BAD_REQUEST, m).into_response(),
        Err(OpenError::Detecting) => {
            (StatusCode::SERVICE_UNAVAILABLE, [(RETRY_AFTER, "1")]).into_response()
        }
    }
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[serde(rename_all = "camelCase")]
pub struct ListQuery {
    owner_id: Option<String>,
    media_file_id: Option<u32>,
}

#[utoipa::path(get, path = "/v1/sessions", params(ListQuery),
    responses((status = 200, body = Vec<SessionSnapshot>)))]
pub async fn list(State(s): S, Query(q): Query<ListQuery>) -> Json<Vec<SessionSnapshot>> {
    Json(s.rt.list(q.owner_id.as_deref(), q.media_file_id))
}

#[utoipa::path(get, path = "/v1/sessions/{sid}", responses(
    (status = 200, body = SessionSnapshot), (status = 404, description = "unknown or ended session")))]
pub async fn get_one(State(s): S, Path(sid): Path<String>) -> Response {
    match s.rt.peek(&sid) {
        Some(sess) => Json(s.rt.snapshot(&sess)).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct DeleteQuery {
    /// Default `stopped`.
    reason: Option<String>,
}

#[utoipa::path(delete, path = "/v1/sessions/{sid}", params(DeleteQuery),
    responses((status = 204, description = "gone (idempotent)")))]
pub async fn delete(
    State(s): S,
    Path(sid): Path<String>,
    Query(q): Query<DeleteQuery>,
) -> StatusCode {
    s.rt.stop(&sid, q.reason.as_deref().unwrap_or("stopped"))
        .await;
    StatusCode::NO_CONTENT
}

#[utoipa::path(post, path = "/v1/sessions/{sid}/heartbeat", request_body = HeartbeatRequest, responses(
    (status = 200, body = HeartbeatResponse), (status = 403, description = "not the session's owner")))]
pub async fn heartbeat(
    State(s): S,
    Path(sid): Path<String>,
    Json(hb): Json<HeartbeatRequest>,
) -> Response {
    let Some(sess) = s.rt.session(&sid) else {
        return Json(HeartbeatResponse { session_lost: true }).into_response();
    };
    if hb.owner_id.is_some() && hb.owner_id != sess.owner {
        return StatusCode::FORBIDDEN.into_response();
    }
    s.rt.heartbeat(&sess, &hb);
    Json(HeartbeatResponse {
        session_lost: false,
    })
    .into_response()
}

#[utoipa::path(post, path = "/v1/sessions/{sid}/command", request_body = SessionCommand, responses(
    (status = 204, description = "done"), (status = 404, description = "unknown session"),
    (status = 409, description = "no running job, or the host cannot pause ffmpeg")))]
pub async fn command(
    State(s): S,
    Path(sid): Path<String>,
    Json(c): Json<SessionCommand>,
) -> StatusCode {
    let Some(sess) = s.rt.peek(&sid) else {
        return StatusCode::NOT_FOUND;
    };
    let done = match c.action {
        SessionAction::Stop => s.rt.stop(&sid, "stopped").await,
        SessionAction::PauseJob => s.rt.pause_job(&sess, true).await,
        SessionAction::ResumeJob => s.rt.pause_job(&sess, false).await,
    };
    if done {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::CONFLICT
    }
}

#[utoipa::path(post, path = "/v1/revoke", request_body = RevokeRequest, responses(
    (status = 200, body = RevokeResult), (status = 400, description = "neither ownerId nor mediaFileIds")))]
pub async fn revoke(State(s): S, Json(r): Json<RevokeRequest>) -> Response {
    let files = r.media_file_ids.unwrap_or_default();
    if r.owner_id.is_none() && files.is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let sessions = s.rt.revoke(r.owner_id.as_deref(), &files).await;
    Json(RevokeResult { sessions }).into_response()
}

#[utoipa::path(post, path = "/v1/invalidate", request_body = InvalidateRequest,
    responses((status = 200, body = InvalidateResult)))]
pub async fn invalidate(State(s): S, Json(r): Json<InvalidateRequest>) -> Json<InvalidateResult> {
    let (jobs, cache_entries) = s.rt.invalidate(r.media_file_id).await;
    Json(InvalidateResult {
        jobs,
        cache_entries,
    })
}

#[utoipa::path(get, path = "/v1/cache", responses((status = 200, body = CacheSummary)))]
pub async fn cache(State(s): S) -> Json<CacheSummary> {
    Json(s.rt.cache_summary().await)
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[serde(rename_all = "camelCase")]
pub struct CacheQuery {
    media_file_id: Option<u32>,
    owner_id: Option<String>,
}

#[utoipa::path(delete, path = "/v1/cache", params(CacheQuery),
    responses((status = 200, body = CacheDeleteResult, description = "live entries are kept")))]
pub async fn cache_delete(State(s): S, Query(q): Query<CacheQuery>) -> Json<CacheDeleteResult> {
    Json(CacheDeleteResult {
        removed: s
            .rt
            .cache_delete(q.media_file_id, q.owner_id.as_deref())
            .await,
    })
}

#[utoipa::path(get, path = "/v1/events", responses((status = 200, body = EngineEvent,
    content_type = "text/event-stream", description = "one EngineEvent per `data:` line, event name = `type`")))]
pub async fn events(State(s): S) -> Response {
    s.rt.events.sse().into_response()
}

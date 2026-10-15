//! `POST /v1/intro-detect`: chromaprint intro/outro matching on the blocking pool.
use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use fliks_engine_core::api::{IntroDetectRequest, IntroDetectResponse};
use std::sync::Arc;

use crate::AppState;

#[utoipa::path(post, path = "/v1/intro-detect", operation_id = "intro_detect",
    request_body = IntroDetectRequest,
    responses(
        (status = 200, body = IntroDetectResponse),
        (status = 400, description = "invalid request"),
        (status = 500, description = "worker join failed")))]
pub async fn detect(
    State(_s): State<Arc<AppState>>,
    Json(req): Json<IntroDetectRequest>,
) -> Response {
    match tokio::task::spawn_blocking(move || fliks_engine_core::intro::detect(req)).await {
        Ok(Ok(resp)) => Json(resp).into_response(),
        Ok(Err(msg)) => (StatusCode::BAD_REQUEST, msg).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

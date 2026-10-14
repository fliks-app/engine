//! Control plane of fliks-engine (`/v1/*`, bearer auth).
pub mod cache;
pub mod events;
pub mod gateway;
pub mod hw;
pub mod jobs;
pub mod probe;
pub mod proc;
pub mod remux;
pub mod resolve;
pub mod runtime;
pub mod serve;
pub mod sessions;
pub mod slots;
pub mod supervise;

use axum::{
    Json, Router,
    extract::{Request, State},
    http::{
        HeaderValue, StatusCode,
        header::{AUTHORIZATION, RETRY_AFTER},
    },
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use fliks_engine_core::api::{
    CapabilitiesResponse, DecideBody, Decision, DeviceProfile, EngineConfig, EstimateSizesRequest,
    Fingerprint, Hello, MediaInfo, PROTOCOL_VERSION, QualityEstimate, ResolveRequest,
    ResolveResponse, SessionExpired, SourceScan,
};
use fliks_engine_core::hw::selected_render_node;
use std::sync::Arc;
use utoipa::OpenApi;

pub struct AppState {
    token: String,
    instance_id: String,
    hw: Arc<hw::Hw>,
    /// Sessions, jobs, cache and the config (`None` until the host's first `PUT /v1/config`).
    pub rt: Arc<runtime::Runtime>,
}

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("os rng");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(OpenApi)]
#[openapi(
    info(title = "fliks-engine", version = "1"),
    paths(hello, capabilities, get_config, put_config, decide, estimate_sizes, probe::probe,
        sessions::open, sessions::list, sessions::get_one, sessions::delete, sessions::heartbeat,
        sessions::command, sessions::revoke, sessions::invalidate, sessions::cache,
        sessions::cache_delete, sessions::events, jobs::create, jobs::list, jobs::get_one,
        jobs::delete),
    components(schemas(MediaInfo, DeviceProfile, SourceScan, Fingerprint, ResolveRequest,
        ResolveResponse, SessionExpired)),
    modifiers(&Bearer)
)]
pub struct ApiDoc;

struct Bearer;

impl utoipa::Modify for Bearer {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
        let scheme =
            SecurityScheme::Http(HttpBuilder::new().scheme(HttpAuthScheme::Bearer).build());
        openapi
            .components
            .get_or_insert_default()
            .add_security_scheme("bearer", scheme);
        openapi.security = Some(vec![utoipa::openapi::security::SecurityRequirement::new::<
            _,
            _,
            String,
        >("bearer", [])]);
    }
}

/// Control plane and data plane over a runtime whose cache and ffmpeg come from the environment.
pub fn app(token: String, hw: Arc<hw::Hw>) -> Router {
    let rt = runtime::Runtime::new(hw, cache::Cache::from_env(), hw::ffmpeg_path());
    router(token, rt)
}

/// `/v1/*` behind the bearer, and the data plane (`serve::router`) beside it, resolver-gated.
pub fn router(token: String, rt: Arc<runtime::Runtime>) -> Router {
    let state = Arc::new(AppState {
        token,
        instance_id: random_hex(8),
        hw: rt.hw.clone(),
        rt: rt.clone(),
    });
    let control = Router::new()
        .route("/v1/hello", get(hello))
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/config", get(get_config).put(put_config))
        .route("/v1/decide", post(decide))
        .route("/v1/estimate-sizes", post(estimate_sizes))
        .route("/v1/probe", post(probe::probe))
        .route("/v1/sessions", get(sessions::list).post(sessions::open))
        .route(
            "/v1/sessions/{sid}",
            get(sessions::get_one).delete(sessions::delete),
        )
        .route("/v1/sessions/{sid}/heartbeat", post(sessions::heartbeat))
        .route("/v1/sessions/{sid}/command", post(sessions::command))
        .route("/v1/revoke", post(sessions::revoke))
        .route("/v1/invalidate", post(sessions::invalidate))
        .route(
            "/v1/cache",
            get(sessions::cache).delete(sessions::cache_delete),
        )
        .route("/v1/events", get(sessions::events))
        .route("/v1/jobs", get(jobs::list).post(jobs::create))
        .route("/v1/jobs/{id}", get(jobs::get_one).delete(jobs::delete))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state);
    control.merge(serve::router(rt))
}

async fn auth(
    State(s): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let given = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    // XOR-fold so the comparison time does not leak the matching prefix length.
    let diff = given.len() ^ s.token.len()
        | given
            .bytes()
            .zip(s.token.bytes())
            .fold(0, |a, (x, y)| a | (x ^ y) as usize);
    if diff == 0 {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

#[utoipa::path(get, path = "/v1/hello", responses((status = 200, body = Hello)))]
async fn hello(State(s): State<Arc<AppState>>) -> Json<Hello> {
    Json(Hello {
        engine_version: env!("CARGO_PKG_VERSION").into(),
        protocol_version: PROTOCOL_VERSION,
        features: vec![
            "decide".into(),
            "estimate-sizes".into(),
            "capabilities".into(),
            "probe".into(),
            "sessions".into(),
            "serve".into(),
            "events".into(),
            "revoke".into(),
            "jobs".into(),
        ],
        instance_id: s.instance_id.clone(),
    })
}

/// Detection runs in the background from boot: until the first run ends the host is told to retry.
fn detecting() -> (StatusCode, [(axum::http::HeaderName, HeaderValue); 1]) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(RETRY_AFTER, HeaderValue::from_static("1"))],
    )
}

#[utoipa::path(get, path = "/v1/capabilities", responses(
    (status = 200, body = CapabilitiesResponse),
    (status = 503, description = "hardware detection has not finished (Retry-After: 1)")))]
async fn capabilities(State(s): State<Arc<AppState>>) -> Response {
    match s.hw.snapshot() {
        Some(snap) => Json(CapabilitiesResponse {
            capabilities: snap.capabilities(),
            host_caps: snap.host_caps(),
        })
        .into_response(),
        None => detecting().into_response(),
    }
}

#[utoipa::path(get, path = "/v1/config", responses((status = 200, body = EngineConfig)))]
async fn get_config(State(s): State<Arc<AppState>>) -> Json<EngineConfig> {
    Json(s.rt.config.lock().unwrap().clone().unwrap_or_default())
}

#[utoipa::path(put, path = "/v1/config", request_body = EngineConfig,
    responses((status = 200, body = EngineConfig), (status = 409, description = "revision lower than current")))]
async fn put_config(
    State(s): State<Arc<AppState>>,
    Json(new): Json<EngineConfig>,
) -> Result<Json<EngineConfig>, StatusCode> {
    let mut cur = s.rt.config.lock().unwrap();
    if new.revision < cur.as_ref().map_or(0, |c| c.revision) {
        return Err(StatusCode::CONFLICT);
    }
    *cur = Some(new.clone());
    s.rt.slots.set_size(new.streaming.ffmpeg_slots);
    s.hw.set_render_node(selected_render_node(&new.streaming.gpu_render_node));
    Ok(Json(new))
}

#[utoipa::path(post, path = "/v1/decide", request_body = DecideBody,
    responses((status = 200, body = Decision), (status = 400, description = "no config set yet"),
    (status = 503, description = "no `hostCaps` given and hardware detection has not finished (Retry-After: 1)")))]
async fn decide(State(s): State<Arc<AppState>>, Json(body): Json<DecideBody>) -> Response {
    let Some(config) = s.rt.config.lock().unwrap().clone() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let caps = match (body.host_caps, s.hw.snapshot()) {
        (Some(c), _) => c,
        (None, Some(snap)) => snap.host_caps(),
        (None, None) => return detecting().into_response(),
    };
    Json(fliks_engine_core::decide::decide(
        &body.request,
        &caps,
        &config.streaming,
    ))
    .into_response()
}

#[utoipa::path(post, path = "/v1/estimate-sizes", request_body = EstimateSizesRequest,
    responses((status = 200, body = Vec<QualityEstimate>)))]
async fn estimate_sizes(Json(req): Json<EstimateSizesRequest>) -> Json<Vec<QualityEstimate>> {
    Json(fliks_engine_core::decide::estimate_sizes(&req))
}

#[cfg(test)]
mod openapi_version {
    // The spec version tracks the protocol, not the crate, so release bumps don't drift openapi.json.
    #[test]
    fn matches_protocol() {
        use utoipa::OpenApi;
        assert_eq!(
            super::ApiDoc::openapi().info.version,
            fliks_engine_core::api::PROTOCOL_VERSION.to_string()
        );
    }
}

//! Control plane of fliks-engine (`/v1/*`, bearer auth).
pub mod supervise;

use axum::{
    Json, Router,
    extract::{Request, State},
    http::{StatusCode, header::AUTHORIZATION},
    middleware::{self, Next},
    response::Response,
    routing::get,
};
use fliks_engine_core::api::{
    Capabilities, DeviceProfile, EngineConfig, Hello, MediaInfo, PROTOCOL_VERSION, SourceScan,
};
use std::sync::{Arc, Mutex};
use utoipa::OpenApi;

pub struct AppState {
    token: String,
    instance_id: String,
    config: Mutex<EngineConfig>,
}

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("os rng");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(OpenApi)]
#[openapi(
    info(title = "fliks-engine", version = "1"),
    paths(hello, capabilities, get_config, put_config),
    components(schemas(MediaInfo, DeviceProfile, SourceScan)),
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

pub fn app(token: String) -> Router {
    let state = Arc::new(AppState {
        token,
        instance_id: random_hex(8),
        config: Mutex::new(EngineConfig::default()),
    });
    Router::new()
        .route("/v1/hello", get(hello))
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/config", get(get_config).put(put_config))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state)
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
        features: vec![],
        instance_id: s.instance_id.clone(),
    })
}

#[utoipa::path(get, path = "/v1/capabilities", responses((status = 200, body = Capabilities)))]
async fn capabilities() -> Json<Capabilities> {
    // Real detection lands in M2.
    Json(Capabilities {
        hw_accel: "none".into(),
        encoders: vec![],
        decoders: vec![],
        tonemap_algos: vec![],
        gpus: vec![],
        default_gpu: None,
        platform: std::env::consts::OS.into(),
        pause_capability: "none".into(),
    })
}

#[utoipa::path(get, path = "/v1/config", responses((status = 200, body = EngineConfig)))]
async fn get_config(State(s): State<Arc<AppState>>) -> Json<EngineConfig> {
    Json(s.config.lock().unwrap().clone())
}

#[utoipa::path(put, path = "/v1/config", request_body = EngineConfig,
    responses((status = 200, body = EngineConfig), (status = 409, description = "revision lower than current")))]
async fn put_config(
    State(s): State<Arc<AppState>>,
    Json(new): Json<EngineConfig>,
) -> Result<Json<EngineConfig>, StatusCode> {
    let mut cur = s.config.lock().unwrap();
    if new.revision < cur.revision {
        return Err(StatusCode::CONFLICT);
    }
    *cur = new.clone();
    Ok(Json(new))
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

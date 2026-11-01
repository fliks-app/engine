//! Device credentials (`Authorization: Basic`, a token in the path): resolved by the host like any
//! other credential, forwarded verbatim on host routes, and never logged.
mod runtime_common;
use axum::{Router, extract::Request, http::StatusCode, response::IntoResponse};
use fliks_engine::gateway::{self, RouteTable, Upstream};
use fliks_engine_core::api::{ResolvePurpose, ResolvedPart};
use runtime_common::*;
use std::sync::{Arc, Mutex};
use tracing_subscriber::fmt::MakeWriter;

const BASIC: &str = "Basic ZXJlYWRlcjpzM2NyZXQtcHc=";
const SECRET: &str = "s3cr3t-device-token";

async fn reader_engine() -> Engine {
    let e = engine().await;
    let dir = e.root.join("book");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.mp3"), b"0123456789").unwrap();
    std::fs::write(dir.join("b.mp3"), b"abcdefghij").unwrap();
    e.stub.items.lock().unwrap().insert(
        1,
        ["a.mp3", "b.mp3"]
            .map(|n| ResolvedPart {
                path: dir.join(n).to_string_lossy().into_owned(),
                duration_s: None,
                start_s: None,
                end_s: None,
                format: None,
            })
            .to_vec(),
    );
    e.stub.file(2, &dir.join("a.mp3"));
    e.stub.grant(BASIC, "reader");
    e.stub.grant(SECRET, "reader");
    e
}

fn last(e: &Engine) -> fliks_engine_core::api::ResolveRequest {
    e.stub.calls.lock().unwrap().last().unwrap().clone()
}

async fn basic(e: &Engine, path: &str, auth: Option<&str>) -> Res {
    let h: Vec<(&str, &str)> = auth.map(|a| ("authorization", a)).into_iter().collect();
    http("GET", &format!("{}{path}", e.base), &h, None).await
}

#[tokio::test]
async fn basic_resolves_a_part_and_a_download() {
    let e = reader_engine().await;
    let r = basic(&e, "/api/stream/1/parts/1", Some(BASIC)).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.text(), "abcdefghij");
    let c = last(&e);
    assert_eq!(
        (c.credential.as_str(), c.purpose),
        (BASIC, ResolvePurpose::Stream)
    );
    let r = basic(&e, "/api/stream/2?download=1", Some(BASIC)).await;
    assert_eq!((r.status, r.text().as_str()), (200, "0123456789"));
    assert_eq!(last(&e).purpose, ResolvePurpose::Download);
    // A token in the query still wins over a Basic header.
    let r = basic(
        &e,
        &format!("/api/stream/2?token={SECRET}"),
        Some("Basic bad"),
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(last(&e).credential, SECRET);
}

#[tokio::test]
async fn a_wrong_or_missing_credential_is_denied_and_the_denial_is_cached() {
    let e = reader_engine().await;
    assert_eq!(
        basic(&e, "/api/stream/1/parts/0", Some("Basic bm9wZQ=="))
            .await
            .status,
        403
    );
    assert_eq!(basic(&e, "/api/stream/1/parts/0", None).await.status, 403);
    let calls = e.stub.calls();
    assert_eq!(
        basic(&e, "/api/stream/1/parts/0", Some("Basic bm9wZQ=="))
            .await
            .status,
        403
    );
    assert_eq!(
        e.stub.calls(),
        calls,
        "a repeated bad credential does not reach the host again"
    );
}

#[tokio::test]
async fn a_path_credential_is_the_token_of_the_inner_route() {
    let e = reader_engine().await;
    for (path, token) in [
        (format!("/cred/{SECRET}/api/stream/1/parts/0"), SECRET),
        // A path token replaces a query one; percent-encoding is undone.
        (
            format!("/cred/{SECRET}/api/stream/1/parts/0?token=other"),
            SECRET,
        ),
        ("/cred/a%2Bb%2Fc/api/stream/1/parts/0".into(), "a+b/c"),
    ] {
        e.stub.grant(token, "reader");
        let r = basic(&e, &path, None).await;
        assert_eq!((r.status, r.text().as_str()), (200, "0123456789"), "{path}");
        assert_eq!(last(&e).credential, token);
    }
    let r = http(
        "HEAD",
        &format!("{}/cred/{SECRET}/api/stream/2", e.base),
        &[],
        None,
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(
        basic(&e, "/cred/wrong/api/stream/1/parts/0", None)
            .await
            .status,
        403
    );
    assert_eq!(
        basic(&e, &format!("/cred/{SECRET}/nope"), None)
            .await
            .status,
        404
    );
    assert_eq!(
        basic(&e, &format!("/cred/{SECRET}/api/stream/1/edl"), None)
            .await
            .status,
        404
    );
}

async fn serve(app: Router) -> std::net::SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            l,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
    });
    addr
}

/// A host that answers 401 + `WWW-Authenticate` unless the request carries the right credential.
async fn reader_host(req: Request) -> axum::response::Response {
    let ok = req
        .headers()
        .get("authorization")
        .is_some_and(|v| v == BASIC)
        || req.uri().path().starts_with(&format!("/kobo/{SECRET}/"));
    if ok {
        return format!("host {}", req.uri().path()).into_response();
    }
    (
        StatusCode::UNAUTHORIZED,
        [("www-authenticate", "Basic realm=\"x\"")],
    )
        .into_response()
}

#[tokio::test]
async fn the_gateway_forwards_device_credentials_untouched_and_serves_the_rest() {
    let e = reader_engine().await;
    let up = serve(Router::new().fallback(reader_host)).await;
    let gw = serve(gateway::router(
        RouteTable::fliks(),
        Upstream::parse(&format!("http://{up}")).unwrap(),
        Some(fliks_engine::serve::router(e.rt.clone())),
    ))
    .await;
    let get = |path: String, auth: Option<&'static str>| async move {
        let h: Vec<(&str, &str)> = auth.map(|a| ("authorization", a)).into_iter().collect();
        http("GET", &format!("http://{gw}{path}"), &h, None).await
    };
    // Host routes: the Basic header and the path token reach the host as received.
    let r = get("/opds/catalog".into(), Some(BASIC)).await;
    assert_eq!((r.status, r.text().as_str()), (200, "host /opds/catalog"));
    let r = get(format!("/kobo/{SECRET}/v1/library/sync?x=1"), None).await;
    assert_eq!(
        (r.status, r.text().as_str()),
        (200, format!("host /kobo/{SECRET}/v1/library/sync").as_str())
    );
    // A missing or wrong credential: the host's 401 and its challenge pass through.
    for (path, auth) in [
        ("/opds/catalog", None),
        ("/opds/catalog", Some("Basic bm9wZQ==")),
        ("/kobo/wrong/v1/library/sync", None),
    ] {
        let r = get(path.into(), auth).await;
        assert_eq!(r.status, 401, "{path}");
        assert_eq!(
            r.header("www-authenticate").as_deref(),
            Some("Basic realm=\"x\"")
        );
    }
    // Engine routes: Basic or a path token, resolved by the host's resolver.
    let r = get("/api/stream/1/parts/0".into(), Some(BASIC)).await;
    assert_eq!((r.status, r.text().as_str()), (200, "0123456789"));
    let r = get(format!("/cred/{SECRET}/api/stream/1/parts/1"), None).await;
    assert_eq!((r.status, r.text().as_str()), (200, "abcdefghij"));
    assert_eq!(
        get("/cred/wrong/api/stream/1/parts/1".into(), None)
            .await
            .status,
        403
    );
}

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Logs {
    type Writer = Logs;
    fn make_writer(&'a self) -> Logs {
        self.clone()
    }
}

#[tokio::test]
async fn credentials_never_reach_the_logs() {
    let logs = Logs::default();
    let _g = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_max_level(tracing::Level::TRACE)
            .finish(),
    );
    let e = reader_engine().await;
    // Granted, denied, then a host that fails (the resolver warns) and a dead gateway upstream.
    for path in [
        format!("/cred/{SECRET}/api/stream/1/parts/0"),
        "/cred/leaked-wrong-token/api/stream/1/parts/0".into(),
    ] {
        basic(&e, &path, None).await;
    }
    basic(&e, "/api/stream/1/parts/0", Some(BASIC)).await;
    basic(&e, "/api/stream/1/parts/0", Some("Basic bGVha2VkOndyb25n")).await;
    e.stub.set_down(true);
    basic(&e, "/cred/leaked-down-token/api/stream/1/parts/0", None).await;
    let dead = serve(gateway::router(
        RouteTable::fliks(),
        Upstream::parse("http://127.0.0.1:1").unwrap(),
        None,
    ))
    .await;
    let r = http(
        "GET",
        &format!("http://{dead}/kobo/{SECRET}/v1/library/sync"),
        &[("authorization", BASIC)],
        None,
    )
    .await;
    assert_eq!(r.status, 502);
    let log = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
    assert!(
        log.contains("resolver"),
        "the failing host was logged: {log}"
    );
    for secret in [
        SECRET,
        "ZXJlYWRlcjpzM2NyZXQtcHc=",
        "bGVha2Vk",
        "leaked-wrong-token",
        "leaked-down-token",
    ] {
        assert!(
            !log.contains(secret),
            "{secret} leaked into the logs:\n{log}"
        );
    }
}

async fn seen(
    e: &Engine,
    path: &str,
    h: &[(&str, &str)],
) -> Option<fliks_engine_core::api::ResolveCredentialSource> {
    http("GET", &format!("{}{path}", e.base), h, None).await;
    last(e).credential_source
}

async fn sent(e: &Engine, path: &str, h: &[(&str, &str)]) -> String {
    http("GET", &format!("{}{path}", e.base), h, None).await;
    last(e).credential
}

#[tokio::test]
async fn the_source_of_the_credential_reaches_the_host_and_cannot_be_forged() {
    use fliks_engine_core::api::ResolveCredentialSource as Src;
    let e = reader_engine().await;
    let forged = ("x-engine-credential-source", "path");
    assert_eq!(
        seen(&e, "/api/stream/2", &[("authorization", BASIC)]).await,
        Some(Src::Basic)
    );
    assert_eq!(
        seen(&e, "/api/stream/2", &[("authorization", "Bearer j")]).await,
        Some(Src::Bearer)
    );
    assert_eq!(
        seen(&e, "/api/stream/2", &[("cookie", "a=1")]).await,
        Some(Src::Cookie)
    );
    assert_eq!(
        seen(&e, "/api/stream/2?token=t", &[]).await,
        Some(Src::Query)
    );
    assert_eq!(
        seen(&e, "/api/stream/2?token=t", &[forged]).await,
        Some(Src::Query)
    );
    assert_eq!(
        seen(&e, "/cred/p/api/stream/2", &[forged]).await,
        Some(Src::Path)
    );
    assert_eq!(seen(&e, "/api/stream/2", &[]).await, None);
}

#[tokio::test]
async fn a_cookie_or_bearer_beats_the_path_token_and_basic() {
    let e = reader_engine().await;
    let p = format!("/cred/{SECRET}/api/stream/2");
    assert_eq!(sent(&e, &p, &[("cookie", "access=c")]).await, "access=c");
    assert_eq!(
        sent(&e, &p, &[("authorization", "Bearer j")]).await,
        "Bearer j"
    );
    let both = [("authorization", BASIC), ("cookie", "access=d")];
    assert_eq!(sent(&e, "/api/stream/2", &both).await, "access=d");
}

#[tokio::test]
async fn an_oversized_credential_is_a_400_and_never_asks_the_host() {
    let e = reader_engine().await;
    let big = "x".repeat(8 * 1024 + 1);
    let r = basic(&e, &format!("/api/stream/2?token={big}"), None).await;
    assert_eq!(r.status, 400);
    let r = basic(&e, "/api/stream/2", Some(&format!("Basic {big}"))).await;
    assert_eq!(r.status, 400);
    assert_eq!(e.stub.calls(), 0);
}

#[tokio::test]
async fn only_get_and_head_data_plane_routes_exist_under_cred() {
    let e = reader_engine().await;
    let url = |p: &str| format!("{}/cred/{SECRET}{p}", e.base);
    let r = http("POST", &url("/api/stream/1/parts/0"), &[], None).await;
    assert_eq!(r.status, 405);
    for tail in [
        "/api/stream/info/qualities",
        "/api/stream/info",
        "/api/stream/1/edl",
        "/v1/hello",
        "/api/livetv/sessions/s/index.m3u8",
    ] {
        assert_eq!(
            http("GET", &url(tail), &[], None).await.status,
            404,
            "{tail}"
        );
    }
    assert_eq!(e.stub.calls(), 0);
}

#[tokio::test]
async fn a_traversal_under_cred_reaches_neither_the_host_nor_the_control_plane() {
    let e = reader_engine().await;
    let up = serve(Router::new().fallback(reader_host)).await;
    let gw = serve(gateway::router(
        RouteTable::fliks(),
        Upstream::parse(&format!("http://{up}")).unwrap(),
        Some(fliks_engine::serve::router(e.rt.clone())),
    ))
    .await;
    for tail in [
        "/1/../../../../v1/hello",
        "/1/%2e%2e/%2e%2e/v1/hello",
        "/1/../../../kobo/x/v1",
    ] {
        let r = http(
            "GET",
            &format!("http://{gw}/cred/{SECRET}/api/stream{tail}"),
            &[],
            None,
        )
        .await;
        assert!(r.status == 404 || r.status == 400, "{tail}: {}", r.status);
        assert!(!r.text().starts_with("host"), "{tail}");
    }
    // A POST there is a host route (the table only serves GET and HEAD).
    let r = http(
        "POST",
        &format!("http://{gw}/cred/{SECRET}/api/stream/1/parts/0"),
        &[],
        None,
    )
    .await;
    assert_eq!(r.status, 401);
}

#[test]
fn debug_of_the_wire_types_redacts_the_credential() {
    use fliks_engine_core::api::{ResolveMediaId, ResolveRequest, RevokeRequest};
    let r = ResolveRequest {
        media_id: ResolveMediaId::File(1),
        credential: SECRET.into(),
        credential_source: None,
        purpose: ResolvePurpose::Stream,
        subtitle_id: None,
        stream_index: None,
    };
    let rv = RevokeRequest {
        credential: Some(SECRET.into()),
        ..Default::default()
    };
    for dbg in [format!("{r:?}"), format!("{rv:?}")] {
        assert!(dbg.contains("<redacted>") && !dbg.contains(SECRET), "{dbg}");
    }
}

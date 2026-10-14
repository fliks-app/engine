use axum::{
    Router,
    body::{Body, Bytes},
    extract::Request,
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
};
use fliks_engine::gateway::{self, RouteTable, Upstream};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use std::{
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering::SeqCst},
    },
    time::Duration,
};
use tokio::{net::TcpListener, sync::oneshot};

const CHUNK: usize = 64 * 1024;
static ZEROS: [u8; CHUNK] = [0; CHUNK];

async fn serve(app: Router) -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, app.into_make_service_with_connect_info::<SocketAddr>())
            .await
            .unwrap()
    });
    addr
}

fn gateway_for(upstream: SocketAddr, engine: Option<Router>) -> Router {
    let up = Upstream::parse(&format!("http://{upstream}")).unwrap();
    gateway::router(RouteTable::fliks(), up, engine)
}

async fn send(addr: SocketAddr, mut req: Request) -> Response<hyper::body::Incoming> {
    *req.uri_mut() = format!("http://{addr}{}", req.uri().path_and_query().unwrap())
        .parse()
        .unwrap();
    Client::builder(TokioExecutor::new())
        .build_http()
        .request(req)
        .await
        .unwrap()
}

async fn text(res: Response<hyper::body::Incoming>) -> String {
    String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
}

fn req(method: Method, uri: &str) -> Request {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

/// Echoes what it received as `METHOD URI\nheaders\n\nbody`.
async fn echo(req: Request) -> Response {
    let (p, body) = req.into_parts();
    let body = body.collect().await.unwrap().to_bytes();
    let mut headers: Vec<String> = p
        .headers
        .iter()
        .map(|(k, v)| format!("{k}: {}", v.to_str().unwrap()))
        .collect();
    headers.sort();
    format!(
        "{} {}\n{}\n\n{}",
        p.method,
        p.uri,
        headers.join("\n"),
        String::from_utf8_lossy(&body)
    )
    .into_response()
}

#[tokio::test]
async fn forwards_method_path_query_headers_and_body() {
    let up = serve(Router::new().fallback(echo)).await;
    let gw = serve(gateway_for(up, None)).await;
    for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
        let mut r = Request::builder()
            .method(method.clone())
            .uri("/api/stream/7/playback-info?a=1&b=x%20y")
            .header("x-custom", "kept")
            .header("connection", "x-secret, keep-alive")
            .header("x-secret", "hop")
            .header("x-forwarded-for", "9.9.9.9")
            .body(Body::from("payload"))
            .unwrap();
        r.headers_mut()
            .insert("content-type", "text/plain".parse().unwrap());
        let out = text(send(gw, r).await).await;
        assert!(
            out.starts_with(&format!(
                "{method} /api/stream/7/playback-info?a=1&b=x%20y\n"
            )),
            "{out}"
        );
        assert!(out.ends_with("\n\npayload"), "{out}");
        assert!(out.contains("x-custom: kept") && out.contains("content-type: text/plain"));
        assert!(out.contains(&format!("host: {gw}")), "{out}");
        assert!(out.contains("x-forwarded-for: 9.9.9.9, 127.0.0.1"), "{out}");
        assert!(out.contains("x-forwarded-proto: http"));
        assert!(out.contains(&format!("x-forwarded-host: {gw}")));
        assert!(
            !out.contains("x-secret") && !out.contains("keep-alive"),
            "{out}"
        );
    }
}

#[tokio::test]
async fn response_status_and_headers_pass_through() {
    let up = serve(Router::new().fallback(|| async {
        (
            StatusCode::IM_A_TEAPOT,
            [("x-up", "1"), ("connection", "x-up")],
            "tea",
        )
    }))
    .await;
    let gw = serve(gateway_for(up, None)).await;
    let res = send(gw, req(Method::GET, "/x")).await;
    assert_eq!(res.status(), StatusCode::IM_A_TEAPOT);
    assert!(
        res.headers().get("x-up").is_none(),
        "hop header named by Connection"
    );
    assert_eq!(text(res).await, "tea");
}

fn zero_stream(produced: Arc<AtomicUsize>, total: usize) -> Body {
    Body::from_stream(futures_util::stream::unfold(0, move |i| {
        let produced = produced.clone();
        async move {
            (i < total).then(|| {
                produced.fetch_add(1, SeqCst);
                (Ok::<_, std::io::Error>(Bytes::from_static(&ZEROS)), i + 1)
            })
        }
    }))
}

#[tokio::test]
async fn streams_a_large_download_with_backpressure() {
    let produced = Arc::new(AtomicUsize::new(0));
    let p = produced.clone();
    let total = 16 * 1024; // 1 GiB
    let up = serve(Router::new().fallback(move || {
        let p = p.clone();
        async move { zero_stream(p, total) }
    }))
    .await;
    let gw = serve(gateway_for(up, None)).await;
    let mut body = send(gw, req(Method::GET, "/big")).await.into_body();
    let mut n = body
        .frame()
        .await
        .unwrap()
        .unwrap()
        .data_ref()
        .unwrap()
        .len();
    tokio::time::sleep(Duration::from_millis(500)).await;
    // a buffering proxy would have pulled the whole body by now
    let ahead = produced.load(SeqCst);
    assert!(
        ahead < 512,
        "upstream was read {ahead} chunks ahead of a stalled client"
    );
    while let Some(f) = body.frame().await {
        n += f.unwrap().data_ref().map_or(0, |d| d.len());
    }
    assert_eq!(n, total * CHUNK);
}

#[tokio::test]
async fn streams_a_large_upload_with_backpressure() {
    let total = 16 * 1024;
    let seen = Arc::new(AtomicUsize::new(0));
    let s = seen.clone();
    // The upstream reads nothing until the test has looked at how far the client body got.
    let (go, gate) = oneshot::channel::<()>();
    let gate = Arc::new(Mutex::new(Some(gate)));
    let up = serve(Router::new().fallback(move |req: Request| {
        let s = s.clone();
        let gate = gate.lock().unwrap().take().unwrap();
        async move {
            gate.await.ok();
            let mut b = req.into_body();
            while let Some(f) = b.frame().await {
                s.fetch_add(f.unwrap().data_ref().map_or(0, |d| d.len()), SeqCst);
            }
            s.load(SeqCst).to_string()
        }
    }))
    .await;
    let gw = serve(gateway_for(up, None)).await;
    let produced = Arc::new(AtomicUsize::new(0));
    let r = Request::post("/up")
        .body(zero_stream(produced.clone(), total))
        .unwrap();
    let pending = tokio::spawn(async move { text(send(gw, r).await).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let ahead = produced.load(SeqCst);
    assert!(
        ahead < 512,
        "client body was read {ahead} chunks while upstream stalled"
    );
    go.send(()).unwrap();
    assert_eq!(pending.await.unwrap(), (total * CHUNK).to_string());
}

#[tokio::test]
async fn sse_events_arrive_incrementally() {
    let (tx, rx) = oneshot::channel::<()>();
    let rx = Arc::new(Mutex::new(Some(rx)));
    let up = serve(Router::new().fallback(move || {
        let rx = rx.clone();
        async move {
            let rx = rx.lock().unwrap().take().unwrap();
            let stream = futures_util::stream::unfold((0, Some(rx)), |(i, rx)| async move {
                match i {
                    0 => Some((
                        Ok::<_, std::io::Error>(Bytes::from("data: one\n\n")),
                        (1, rx),
                    )),
                    1 => {
                        // event two is held back until the client has seen event one
                        rx.unwrap().await.ok();
                        Some((Ok(Bytes::from("data: two\n\n")), (2, None)))
                    }
                    _ => None,
                }
            });
            (
                [("content-type", "text/event-stream")],
                Body::from_stream(stream),
            )
        }
    }))
    .await;
    let gw = serve(gateway_for(up, None)).await;
    let mut body = send(gw, req(Method::GET, "/api/events")).await.into_body();
    let first = tokio::time::timeout(Duration::from_secs(60), body.frame())
        .await
        .expect("first event not flushed")
        .unwrap()
        .unwrap();
    assert_eq!(first.data_ref().unwrap(), "data: one\n\n");
    tx.send(()).unwrap();
    let second = body.frame().await.unwrap().unwrap();
    assert_eq!(second.data_ref().unwrap(), "data: two\n\n");
}

#[tokio::test]
async fn websocket_echo_through_the_gateway() {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let mut ws = tokio_tungstenite::accept_async(s).await.unwrap();
                while let Some(Ok(m)) = ws.next().await {
                    if m.is_text() || m.is_binary() {
                        ws.send(m).await.unwrap();
                    }
                }
            });
        }
    });
    let gw = serve(gateway_for(up, None)).await;
    let (mut ws, res) = tokio_tungstenite::connect_async(format!("ws://{gw}/socket?x=1"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SWITCHING_PROTOCOLS);
    for msg in ["hello", "again"] {
        ws.send(msg.into()).await.unwrap();
        assert_eq!(ws.next().await.unwrap().unwrap().into_text().unwrap(), msg);
    }
    ws.close(None).await.unwrap();
}

// A freed ephemeral port can be re-bound by a parallel test; port 1 is reliably refused.
fn dead_addr() -> SocketAddr {
    "127.0.0.1:1".parse().unwrap()
}

fn engine_stub() -> Router {
    Router::new().fallback(any(|req: Request| async move {
        format!("engine {}", req.uri().path())
    }))
}

#[tokio::test]
async fn upstream_down_is_502_and_engine_routes_keep_working() {
    let gw = serve(gateway_for(dead_addr(), Some(engine_stub()))).await;
    assert_eq!(
        send(gw, req(Method::GET, "/api/system/liveness"))
            .await
            .status(),
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(
        send(gw, req(Method::POST, "/api/stream/7/playback-info"))
            .await
            .status(),
        StatusCode::BAD_GATEWAY
    );
    let res = send(gw, req(Method::GET, "/api/stream/7/master.m3u8")).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(text(res).await, "engine /api/stream/7/master.m3u8");
}

#[tokio::test]
async fn engine_and_forwarded_routes_are_told_apart() {
    let up = serve(Router::new().fallback(|req: Request| async move {
        format!("upstream {} {}", req.method(), req.uri().path())
    }))
    .await;
    let gw = serve(gateway_for(up, Some(engine_stub()))).await;
    let cases = [
        (Method::GET, "/api/stream/7", "engine"),
        (Method::HEAD, "/api/stream/7/master.m3u8", "engine"),
        (Method::GET, "/api/stream/7/audio/1/seg3.m4s", "engine"),
        (Method::GET, "/api/stream/7/subtitles/12/download", "engine"),
        (Method::GET, "/api/stream/7/edl", "upstream"),
        (Method::GET, "/api/stream/info/qualities?x=1", "upstream"),
        (Method::POST, "/api/stream/7/master.m3u8", "upstream"),
        (Method::POST, "/api/stream/7/playback-info", "upstream"),
        (Method::DELETE, "/api/stream/sessions/s1", "upstream"),
        (Method::GET, "/api/system/liveness", "upstream"),
    ];
    for (m, path, who) in cases {
        let res = send(gw, req(m.clone(), path)).await;
        if m == Method::HEAD {
            assert_eq!(res.status(), StatusCode::OK, "{m} {path}");
            continue;
        }
        let out = text(res).await;
        assert!(out.starts_with(who), "{m} {path} -> {out}");
    }
}

#[tokio::test]
async fn control_plane_is_not_on_the_public_listener() {
    let up = serve(Router::new().fallback(|req: Request| async move {
        (
            StatusCode::NOT_FOUND,
            format!("upstream {}", req.uri().path()),
        )
    }))
    .await;
    let gw = serve(gateway_for(up, Some(engine_stub()))).await;
    let mut r = req(Method::GET, "/v1/hello");
    r.headers_mut()
        .insert("authorization", "Bearer anything".parse().unwrap());
    let res = send(gw, r).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert_eq!(text(res).await, "upstream /v1/hello");
}

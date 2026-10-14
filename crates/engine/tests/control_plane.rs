mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::cpu_host;
use http_body_util::BodyExt;
use tower::ServiceExt;

async fn call(
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let res = fliks_engine::app("secret".into(), cpu_host())
        .oneshot(req.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

#[tokio::test]
async fn rejects_missing_or_wrong_token() {
    assert_eq!(
        call("GET", "/v1/hello", None, "").await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call("GET", "/v1/hello", Some("nope"), "").await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn hello_ok() {
    let (s, v) = call("GET", "/v1/hello", Some("secret"), "").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["protocolVersion"], 1);
    assert!(v["instanceId"].is_string());
    let features = v["features"].as_array().unwrap();
    assert!(features.iter().any(|f| f == "probe"));
}

#[tokio::test]
async fn config_revision_rule() {
    // One router per call is stateless, so drive a single router through several requests.
    let app = fliks_engine::app("secret".into(), cpu_host());
    let put = |rev: u32| {
        Request::builder()
            .method("PUT")
            .uri("/v1/config")
            .header("content-type", "application/json")
            .header("authorization", "Bearer secret")
            .body(Body::from(format!(
                r#"{{"revision":{rev},"hostUrl":"http://h"}}"#
            )))
            .unwrap()
    };
    assert_eq!(
        app.clone().oneshot(put(2)).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(
        app.clone().oneshot(put(1)).await.unwrap().status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        app.clone().oneshot(put(2)).await.unwrap().status(),
        StatusCode::OK
    );
    let get = Request::builder()
        .uri("/v1/config")
        .header("authorization", "Bearer secret")
        .body(Body::empty())
        .unwrap();
    let body = app
        .oneshot(get)
        .await
        .unwrap()
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["revision"], 2);
}

#[tokio::test]
async fn decide_needs_config_then_matches_the_golden_vector() {
    let app = fliks_engine::app("secret".into(), cpu_host());
    let send = |method: &str, uri: &str, body: String| {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .header("authorization", "Bearer secret")
            .body(Body::from(body))
            .unwrap();
        let app = app.clone();
        async move {
            let res = app.oneshot(req).await.unwrap();
            let status = res.status();
            let bytes = res.into_body().collect().await.unwrap().to_bytes();
            (
                status,
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap_or_default(),
            )
        }
    };
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/decide/android-tv-h264-sdr-mkv-cpu-720p.json"
    );
    let fixture: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    // No hostCaps: the engine's detected capabilities decide, the same as the cpu vector.
    let body = serde_json::json!({ "request": fixture["input"] }).to_string();

    assert_eq!(
        send("POST", "/v1/decide", body.clone()).await.0,
        StatusCode::BAD_REQUEST
    );
    let put = serde_json::json!({ "revision": 1, "streaming": fixture["config"] }).to_string();
    assert_eq!(send("PUT", "/v1/config", put).await.0, StatusCode::OK);
    let (status, got) = send("POST", "/v1/decide", body).await;
    assert_eq!(status, StatusCode::OK);
    let want = &fixture["expected"];
    assert_eq!(
        got["response"]["playMethod"],
        want["response"]["playMethod"]
    );
    assert_eq!(
        got["response"]["transcodeReasons"],
        want["response"]["transcodeReasons"]
    );
    assert_eq!(got["videoVariant"], want["videoVariant"]);
}

#[tokio::test]
async fn estimate_sizes_lists_the_ladder() {
    let body = r#"{"info":{"video":[{"streamIndex":0,"codec":"h264","width":1920,"height":1080}],
        "audio":[],"subtitles":[],"durationSeconds":100},"fileSize":0}"#;
    assert_eq!(
        call("POST", "/v1/estimate-sizes", None, body).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (s, v) = call("POST", "/v1/estimate-sizes", Some("secret"), body).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v[0]["key"], "1080p");
    assert_eq!(v[0]["estimatedSize"].as_f64(), Some(100_000_000.0));
}

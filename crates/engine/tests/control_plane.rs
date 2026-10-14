use axum::{
    body::Body,
    http::{Request, StatusCode},
};
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
    let res = fliks_engine::app("secret".into())
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
}

#[tokio::test]
async fn config_revision_rule() {
    // One router per call is stateless, so drive a single router through several requests.
    let app = fliks_engine::app("secret".into());
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

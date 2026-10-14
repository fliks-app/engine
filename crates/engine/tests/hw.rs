//! Detection plumbing of the control plane: 503 while detecting, re-detection on a GPU re-pin.
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use fliks_engine::hw::{Hw, Snapshot};
use fliks_engine_core::api::{HwAccel, Platform};
use fliks_engine_core::hw::Detected;
use http_body_util::BodyExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tower::ServiceExt;

fn snap(hw: HwAccel, node: &str) -> Snapshot {
    Snapshot {
        detected: Detected {
            hw_accel: hw,
            platform: Platform::Linux,
            libfdk_aac: false,
            vaapi_writes_hdr_metadata: false,
            probes: vec!["vulkan".into()],
            encoders: vec!["libx264".into()],
            decoders: vec!["cpu".into()],
            disabled_encoders: vec![],
            disabled_decoders: vec![],
        },
        gpus: vec![],
        pause_capability: "stdin".into(),
        render_node: node.into(),
    }
}

/// A detector that records the node it ran for and waits for `gate` before answering.
fn gated(gate: Arc<Notify>, ran: Arc<Mutex<Vec<Option<String>>>>) -> Arc<Hw> {
    Hw::new(Arc::new(move |node| {
        let (gate, ran) = (gate.clone(), ran.clone());
        Box::pin(async move {
            ran.lock().unwrap().push(node.clone());
            gate.notified().await;
            snap(
                HwAccel::Qsv,
                node.as_deref().unwrap_or("/dev/dri/renderD128"),
            )
        })
    }))
}

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", "Bearer secret")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

async fn until(hw: &Arc<Hw>, f: impl Fn(&Snapshot) -> bool) {
    for _ in 0..200 {
        if hw.snapshot().as_ref().is_some_and(&f) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("detection did not settle");
}

#[tokio::test]
async fn capabilities_and_decide_wait_for_detection() {
    let (gate, ran) = (Arc::new(Notify::new()), Arc::new(Mutex::new(vec![])));
    let hw = gated(gate.clone(), ran);
    let app = fliks_engine::app("secret".into(), hw.clone());
    hw.start();

    let req = Request::builder()
        .uri("/v1/capabilities")
        .header("authorization", "Bearer secret")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(res.headers()["retry-after"], "1");

    let put = r#"{"revision":1,"streaming":{}}"#;
    assert_eq!(send(&app, "PUT", "/v1/config", put).await.0, StatusCode::OK);
    let decide = r#"{"request":{"media":{"mediaFileId":1,"path":"/a.mkv","ext":".mkv","contentType":"video/x-matroska"},
        "info":{"video":[],"audio":[],"subtitles":[]},"device":{"directPlayProfiles":[]}}}"#;
    assert_eq!(
        send(&app, "POST", "/v1/decide", decide).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );

    gate.notify_one();
    until(&hw, |_| true).await;
    let (s, v) = send(&app, "GET", "/v1/capabilities", "").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["hwAccel"], "qsv");
    assert_eq!(v["pauseCapability"], "stdin");
    assert_eq!(
        send(&app, "POST", "/v1/decide", decide).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn repin_detects_again_and_coalesces() {
    let (gate, ran) = (Arc::new(Notify::new()), Arc::new(Mutex::new(vec![])));
    let hw = gated(gate.clone(), ran.clone());
    let app = fliks_engine::app("secret".into(), hw.clone());
    hw.start();
    while ran.lock().unwrap().is_empty() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let put = |rev: u32, node: &str| {
        format!(r#"{{"revision":{rev},"streaming":{{"gpuRenderNode":"{node}"}}}}"#)
    };
    // `auto` is what the boot run used: no second run. Two pins while it runs: one follow-up.
    send(&app, "PUT", "/v1/config", &put(1, "auto")).await;
    send(&app, "PUT", "/v1/config", &put(2, "/dev/dri/renderD129")).await;
    send(&app, "PUT", "/v1/config", &put(3, "/dev/dri/renderD130")).await;
    gate.notify_one();
    until(&hw, |_| true).await;
    gate.notify_one();
    until(&hw, |s| s.render_node == "/dev/dri/renderD130").await;
    assert_eq!(
        *ran.lock().unwrap(),
        [None, Some("/dev/dri/renderD130".to_string())]
    );
    let (_, v) = send(&app, "GET", "/v1/capabilities", "").await;
    assert_eq!(v["defaultGpu"], serde_json::Value::Null);
}

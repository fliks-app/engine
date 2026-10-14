//! Real tools on the probe corpus against the host's recorded results; a no-op unless
//! `PROBE_CORPUS_DIR` is set (tools: `FFPROBE_PATH`, `FFMPEG_PATH`, `MKVMERGE_PATH`).
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn post(app: &axum::Router, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/v1/probe")
        .header("content-type", "application/json")
        .header("authorization", "Bearer secret")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

/// JSON equality where `180` and `180.0` are the same number.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
        }
        _ => a == b,
    }
}

#[tokio::test]
async fn corpus_matches_the_host() {
    let Ok(corpus) = std::env::var("PROBE_CORPUS_DIR") else {
        eprintln!("PROBE_CORPUS_DIR not set: skipped");
        return;
    };
    let app = fliks_engine::app("secret".into());
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/probe");
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    let (mut n, mut failed) = (0, vec![]);
    for p in entries {
        let f: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let Some(file) = f["file"].as_str() else {
            continue;
        };
        let path = format!("{corpus}/{file}");
        let body = json!({ "media": { "mediaFileId": 1, "path": path }, "crop": true, "mkvTags": true, "moviehash": true });
        let (status, mut got) = post(&app, body).await;
        assert_eq!(status, StatusCode::OK, "{file}");
        // The engine stamps the schema version; the host does not.
        if got["ok"] == true {
            assert_eq!(got["info"]["schemaVersion"], 1, "{file}");
        }
        got["info"].as_object_mut().unwrap().remove("schemaVersion");
        let want = &f["expected"];
        let normalise = |v: &Value| v.to_string().replace(&path, "{path}");
        let got_info: Value = serde_json::from_str(&normalise(&got["info"])).unwrap();
        let mut bad = vec![];
        if got["ok"] != want["ok"] {
            bad.push(format!("ok: got {}, host {}", got["ok"], want["ok"]));
        }
        if !same(&got_info, &want["info"]) {
            bad.push(format!("info: got {got_info}\n  host {}", want["info"]));
        }
        let hash = |v: &Value| v.get("moviehash").filter(|m| !m.is_null()).cloned();
        if hash(&got) != hash(&f) && !hash(&got).zip(hash(&f)).is_some_and(|(a, b)| same(&a, &b)) {
            bad.push(format!(
                "moviehash: got {:?}, host {:?}",
                hash(&got),
                hash(&f)
            ));
        }
        if !bad.is_empty() {
            failed.push(format!("{file}: {}", bad.join("\n  ")));
        }
        n += 1;
    }
    assert!(n > 0, "no corpus fixtures");
    for f in &failed {
        eprintln!("FAIL {f}");
    }
    assert!(
        failed.is_empty(),
        "{} of {n} corpus files differ",
        failed.len()
    );
}

#[tokio::test]
async fn probe_validates_the_request() {
    let app = fliks_engine::app("secret".into());
    let rel = json!({ "media": { "mediaFileId": 1, "path": "-v" }, "crop": false, "mkvTags": false, "moviehash": false });
    assert_eq!(post(&app, rel).await.0, StatusCode::BAD_REQUEST);
    let missing = json!({ "media": { "mediaFileId": 1, "path": "/nonexistent/file.mkv" }, "crop": true, "mkvTags": true, "moviehash": true });
    let (status, v) = post(&app, missing).await;
    assert_eq!(status, StatusCode::OK);
    // ffprobe cannot read it (or is absent): ok false, never "no streams".
    assert_eq!(v["ok"], false);
    assert!(v["error"].is_string());
    assert!(v["info"]["error"].is_string());
    assert!(v.get("moviehash").is_none());
}

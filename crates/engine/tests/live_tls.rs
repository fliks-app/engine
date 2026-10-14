//! Direct play from an `https` upstream: a local TLS provider whose CA the engine trusts through
//! `SSL_CERT_FILE` (its own binary: the variable must be set before any engine starts).
mod runtime_common;
use fliks_engine_core::api::{EngineEvent, HwAccel};
use http_body_util::BodyExt;
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use runtime_common::*;
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::rustls::{
    ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer},
};

/// A CA and a `localhost` certificate it signed: (CA PEM, leaf DER, leaf key DER).
fn pki() -> (String, CertificateDer<'static>, PrivateKeyDer<'static>) {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca = CertificateParams::new(vec![]).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = ca.self_signed(&ca_key).unwrap();
    let key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["localhost".into()])
        .unwrap()
        .signed_by(&key, &ca, &ca_key)
        .unwrap();
    let der = PrivateKeyDer::try_from(key.serialize_der()).unwrap();
    (ca.pem(), leaf.der().clone(), der)
}

/// An HTTPS provider: `/start` redirects to `/live.ts`, which streams forever. Returns its port and
/// the request heads it saw.
async fn provider(
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
) -> (u16, Arc<Mutex<Vec<String>>>) {
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let heads = Arc::new(Mutex::new(vec![]));
    let seen = heads.clone();
    tokio::spawn(async move {
        loop {
            let (tcp, _) = l.accept().await.unwrap();
            let (acceptor, seen) = (acceptor.clone(), seen.clone());
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let mut head = vec![];
                while !head.ends_with(b"\r\n\r\n") {
                    let mut b = [0u8; 1];
                    if tls.read(&mut b).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(b[0]);
                }
                let head = String::from_utf8_lossy(&head).to_lowercase();
                let redirect = head.starts_with("get /start ");
                seen.lock().unwrap().push(head);
                if redirect {
                    let r = "HTTP/1.1 302 Found\r\nLocation: /live.ts\r\nContent-Length: 0\r\n\r\n";
                    let _ = tls.write_all(r.as_bytes()).await;
                    return;
                }
                let r = "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nConnection: close\r\n\r\n";
                let _ = tls.write_all(r.as_bytes()).await;
                for n in 0u32.. {
                    if tls.write_all(format!("TS{n:04}").as_bytes()).await.is_err() {
                        return;
                    }
                    let _ = tls.flush().await;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            });
        }
    });
    (port, heads)
}

#[tokio::test]
async fn direct_play_from_an_https_upstream() {
    let (ca, cert, key) = pki();
    let ca_file = temp_dir("tls").join("ca.pem");
    std::fs::write(&ca_file, ca).unwrap();
    // SAFETY: set before any thread of this binary reads the environment.
    unsafe { std::env::set_var("SSL_CERT_FILE", &ca_file) };
    let fake = fake("live");
    let e = engine_with_tools(
        host(HwAccel::None, "/dev/dri/renderD128", "signal"),
        &fake,
        &fake,
        &fake,
        json!({}),
    )
    .await;
    e.stub.grant("alice-jwt", "alice");
    let mut rx = e.rt.events.subscribe();
    let (port, heads) = provider(cert, key).await;
    let open = |url: String, token: &str| {
        json!({ "channelId": token, "caps": { "directPlay": true }, "ownerId": "alice",
            "capacityToken": token, "upstreams": [{ "url": url, "probedVideoCodec": "h264",
            "probedContainer": "mpegts", "headers": { "User-Agent": "Tuner/1", "X-Key": "k" } }] })
    };
    let r = e
        .control(
            "POST",
            "/v1/live",
            Some(open(format!("https://localhost:{port}/start"), "t")),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json();
    assert_eq!(v["mode"], "direct");
    let url = format!(
        "{}{}?token=alice-jwt",
        e.base,
        v["playPath"].as_str().unwrap()
    );
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build_http::<http_body_util::Empty<axum::body::Bytes>>();
    let res = client.get(url.parse().unwrap()).await.unwrap();
    assert_eq!(res.status(), 200);
    let mut body = res.into_body();
    let mut got = vec![];
    while got.len() < 18 {
        let f = tokio::time::timeout(Duration::from_secs(60), body.frame()).await;
        got.extend(f.unwrap().unwrap().unwrap().into_data().unwrap_or_default());
    }
    assert!(got.starts_with(b"TS"), "{got:?}");
    let heads = heads.lock().unwrap().clone();
    assert_eq!(heads.len(), 2, "{heads:?}");
    assert!(heads[0].starts_with("get /start ") && heads[1].starts_with("get /live.ts "));
    for h in &heads {
        assert!(
            h.contains("user-agent: tuner/1\r\n") && h.contains("x-key: k\r\n"),
            "{h}"
        );
    }

    // A certificate from a CA the engine does not trust fails the attempt.
    let (_, cert, key) = pki();
    let (port, _) = provider(cert, key).await;
    let r = e
        .control(
            "POST",
            "/v1/live",
            Some(open(format!("https://localhost:{port}/live.ts"), "u")),
        )
        .await;
    assert_eq!(r.status, 503, "{}", r.text());
    let attempt = event(&mut rx, 60, |e| {
        matches!(e, EngineEvent::LiveAttempt { capacity_token, error: Some(_), .. } if capacity_token == "u")
    })
    .await;
    let EngineEvent::LiveAttempt {
        error: Some(err), ..
    } = attempt
    else {
        unreachable!()
    };
    assert!(err.contains("invalid peer certificate"), "{err}");
}

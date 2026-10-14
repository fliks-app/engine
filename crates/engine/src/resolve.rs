//! Data-plane access through the host resolver (`POST {hostUrl}/engine/resolve`), cached per
//! (credential hash, media, purpose) for the answer's `ttlS`. Spec: `docs/spec/runtime.md` section 6.
use axum::body::Bytes;
use axum::http::{Request, header};
use fliks_engine_core::api::{ResolveMediaId, ResolvePurpose, ResolveRequest, ResolveResponse};
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const DEFAULT_TTL_S: f64 = 60.0;
const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    credential: u64,
    media: u32,
    purpose: ResolvePurpose,
    subtitle_id: Option<u32>,
    stream_index: Option<u32>,
}

struct Entry {
    until: Instant,
    res: ResolveResponse,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub path: String,
    pub owner: Option<String>,
    pub format: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveError {
    Denied,
    /// The host is unreachable or failing and nothing is cached.
    Unavailable,
}

/// What one data-plane request asks for.
#[derive(Debug, Clone, Copy)]
pub struct Ask<'a> {
    pub media: u32,
    pub credential: &'a str,
    pub purpose: ResolvePurpose,
    pub subtitle_id: Option<u32>,
    pub stream_index: Option<u32>,
}

/// The host callback registered by `PUT /v1/config`.
#[derive(Debug, Clone)]
pub struct Host {
    pub url: String,
    pub token: Option<String>,
}

pub struct Resolver {
    client: Client<HttpConnector, Full<Bytes>>,
    hasher: RandomState,
    cache: Mutex<HashMap<Key, Entry>>,
}

impl Default for Resolver {
    fn default() -> Self {
        Self {
            client: Client::builder(TokioExecutor::new()).build_http(),
            hasher: RandomState::new(),
            cache: Mutex::new(HashMap::new()),
        }
    }
}

fn granted(res: &ResolveResponse) -> Result<Resolved, ResolveError> {
    match (&res.allow, &res.path) {
        (true, Some(p)) => Ok(Resolved {
            path: p.clone(),
            owner: res.owner_id.clone(),
            format: res.format.clone(),
        }),
        (true, None) => Err(ResolveError::Unavailable),
        (false, _) => Err(ResolveError::Denied),
    }
}

impl Resolver {
    pub async fn resolve(
        &self,
        host: Option<&Host>,
        ask: Ask<'_>,
    ) -> Result<Resolved, ResolveError> {
        let key = Key {
            credential: self.hasher.hash_one(ask.credential),
            media: ask.media,
            purpose: ask.purpose,
            subtitle_id: ask.subtitle_id,
            stream_index: ask.stream_index,
        };
        let stale = {
            let cache = self.cache.lock().unwrap();
            match cache.get(&key) {
                Some(e) if e.until > Instant::now() => return granted(&e.res),
                Some(e) => Some(e.res.clone()),
                None => None,
            }
        };
        match self.call(host, &ask).await {
            Some(res) => {
                let ttl = res
                    .ttl_s
                    .filter(|t| *t >= 0.0 && t.is_finite())
                    .unwrap_or(DEFAULT_TTL_S);
                let out = granted(&res);
                self.cache.lock().unwrap().insert(
                    key,
                    Entry {
                        until: Instant::now() + Duration::from_secs_f64(ttl),
                        res,
                    },
                );
                out
            }
            // Host down: an expired grant keeps serving, a denial never turns into one.
            None => match stale {
                Some(res) if res.allow => granted(&res),
                _ => Err(ResolveError::Unavailable),
            },
        }
    }

    /// `None` on any transport error, timeout, non-2xx or unreadable body: never cached.
    async fn call(&self, host: Option<&Host>, ask: &Ask<'_>) -> Option<ResolveResponse> {
        let host = host?;
        let body = serde_json::to_vec(&ResolveRequest {
            media_id: ResolveMediaId::File(ask.media),
            credential: ask.credential.into(),
            purpose: ask.purpose,
            subtitle_id: ask.subtitle_id,
            stream_index: ask.stream_index,
        })
        .ok()?;
        let mut req = Request::post(format!("{}/engine/resolve", host.url.trim_end_matches('/')))
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(t) = &host.token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let req = req.body(Full::new(Bytes::from(body))).ok()?;
        let res = match tokio::time::timeout(TIMEOUT, self.client.request(req)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                tracing::warn!(url = host.url, "resolver unreachable: {e}");
                return None;
            }
            Err(_) => {
                tracing::warn!(url = host.url, "resolver timed out");
                return None;
            }
        };
        if !res.status().is_success() {
            tracing::warn!(status = %res.status(), "resolver failed");
            return None;
        }
        let bytes = Limited::new(res.into_body(), 64 * 1024)
            .collect()
            .await
            .ok()?
            .to_bytes();
        serde_json::from_slice(&bytes).ok()
    }

    /// Drops the cached answers matching every given filter (`POST /v1/revoke`, `/v1/invalidate`).
    pub fn purge(&self, owner: Option<&str>, media: &[u32]) {
        self.cache.lock().unwrap().retain(|k, e| {
            let by_owner = owner.is_none_or(|o| e.res.owner_id.as_deref() == Some(o));
            let by_media = media.is_empty() || media.contains(&k.media);
            !(by_owner && by_media)
        });
    }
}

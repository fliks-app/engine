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
/// An expired grant still answers while the host is down, for this long.
const STALE_GRACE: Duration = Duration::from_secs(3600);
const MAX_ENTRIES: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    credential: u64,
    media: ResolveMediaId,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// `owner`: the user the host denied, when it names one.
    Denied { owner: Option<String> },
    /// The host is unreachable or failing and nothing is cached.
    Unavailable,
}

/// What one data-plane request asks for.
#[derive(Debug, Clone)]
pub struct Ask<'a> {
    /// A media file id, or a live sid (`purpose: live`).
    pub media: ResolveMediaId,
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

/// A sprite grant may come without a path (the engine serves its own), a live one has none.
fn granted(res: &ResolveResponse, purpose: ResolvePurpose) -> Result<Resolved, ResolveError> {
    if !res.allow {
        return Err(ResolveError::Denied {
            owner: res.owner_id.clone(),
        });
    }
    if res.path.is_none() && !matches!(purpose, ResolvePurpose::Sprite | ResolvePurpose::Live) {
        return Err(ResolveError::Unavailable);
    }
    Ok(Resolved {
        path: res.path.clone().unwrap_or_default(),
        owner: res.owner_id.clone(),
        format: res.format.clone(),
    })
}

/// Keeps `cache` under `cap`: drops grants stale past the grace, then the soonest to expire.
fn make_room(cache: &mut HashMap<Key, Entry>, cap: usize, now: Instant) {
    if cache.len() < cap {
        return;
    }
    cache.retain(|_, e| e.until + STALE_GRACE > now);
    while cache.len() >= cap {
        let Some(k) = cache
            .iter()
            .min_by_key(|(_, e)| e.until)
            .map(|(k, _)| k.clone())
        else {
            return;
        };
        cache.remove(&k);
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
            media: ask.media.clone(),
            purpose: ask.purpose,
            subtitle_id: ask.subtitle_id,
            stream_index: ask.stream_index,
        };
        let stale = {
            let cache = self.cache.lock().unwrap();
            match cache.get(&key) {
                Some(e) if e.until > Instant::now() => return granted(&e.res, ask.purpose),
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
                let out = granted(&res, ask.purpose);
                let now = Instant::now();
                let mut cache = self.cache.lock().unwrap();
                make_room(&mut cache, MAX_ENTRIES, now);
                cache.insert(
                    key,
                    Entry {
                        until: now + Duration::from_secs_f64(ttl),
                        res,
                    },
                );
                out
            }
            // Host down: an expired grant keeps serving, a denial never turns into one.
            None => match stale {
                Some(res) if res.allow => granted(&res, ask.purpose),
                _ => Err(ResolveError::Unavailable),
            },
        }
    }

    /// `None` on any transport error, timeout, non-2xx or unreadable body: never cached.
    async fn call(&self, host: Option<&Host>, ask: &Ask<'_>) -> Option<ResolveResponse> {
        let host = host?;
        let body = serde_json::to_vec(&ResolveRequest {
            media_id: ask.media.clone(),
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
            let by_media = media.is_empty()
                || matches!(&k.media, ResolveMediaId::File(m) if media.contains(m));
            !(by_owner && by_media)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(until: Instant) -> Entry {
        Entry {
            until,
            res: ResolveResponse {
                allow: true,
                path: None,
                owner_id: None,
                format: None,
                ttl_s: None,
            },
        }
    }

    fn key(n: u64) -> Key {
        Key {
            credential: n,
            media: ResolveMediaId::File(1),
            purpose: ResolvePurpose::Stream,
            subtitle_id: None,
            stream_index: None,
        }
    }

    #[test]
    fn make_room_drops_long_stale_grants_then_the_soonest_to_expire() {
        let now = Instant::now();
        let mut cache = HashMap::new();
        cache.insert(key(1), entry(now - STALE_GRACE - Duration::from_secs(1)));
        cache.insert(key(2), entry(now + Duration::from_secs(10)));
        cache.insert(key(3), entry(now + Duration::from_secs(5)));
        make_room(&mut cache, 3, now);
        assert_eq!(cache.len(), 2);
        cache.insert(key(4), entry(now + Duration::from_secs(60)));
        make_room(&mut cache, 3, now);
        assert!(!cache.contains_key(&key(3)) && cache.len() == 2);
    }
}

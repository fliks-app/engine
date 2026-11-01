//! Data-plane access through the host resolver (`POST {hostUrl}/engine/resolve`), cached per
//! (credential hash, media, purpose) for the answer's `ttlS`. Spec: `docs/spec/runtime.md` section 6.
use crate::lock::Locked;
use axum::body::Bytes;
use axum::http::{Request, header};
use fliks_engine_core::api::{
    ResolveCredentialSource, ResolveMediaId, ResolvePurpose, ResolveRequest, ResolveResponse,
    ResolvedPart,
};
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
    /// The ordered pieces of an item; `path` is then empty.
    pub parts: Vec<ResolvedPart>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// `owner`: the user the host denied, when it names one.
    Denied { owner: Option<String> },
    /// The host is unreachable or failing and nothing is cached.
    Unavailable,
}

/// What one data-plane request asks for.
#[derive(Clone)]
pub struct Ask<'a> {
    /// A media file id, or a live sid (`purpose: live`).
    pub media: ResolveMediaId,
    pub credential: &'a str,
    pub source: Option<ResolveCredentialSource>,
    pub purpose: ResolvePurpose,
    pub subtitle_id: Option<u32>,
    pub stream_index: Option<u32>,
}

impl std::fmt::Debug for Ask<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ask")
            .field("media", &self.media)
            .field("credential", &"<redacted>")
            .field("source", &self.source)
            .field("purpose", &self.purpose)
            .finish_non_exhaustive()
    }
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

/// A sprite grant may come without a path (the engine serves its own), a live one has none, an
/// item has its parts instead.
fn granted(res: &ResolveResponse, purpose: ResolvePurpose) -> Result<Resolved, ResolveError> {
    if !res.allow {
        return Err(ResolveError::Denied {
            owner: res.owner_id.clone(),
        });
    }
    if res.path.is_none()
        && res.parts.is_empty()
        && !matches!(purpose, ResolvePurpose::Sprite | ResolvePurpose::Live)
    {
        return Err(ResolveError::Unavailable);
    }
    Ok(Resolved {
        path: res.path.clone().unwrap_or_default(),
        owner: res.owner_id.clone(),
        format: res.format.clone(),
        parts: res.parts.clone(),
    })
}

/// Frees room in a full `cache`: drops what is dead (denials once expired, grants past the stale
/// grace), then denials before grants and the soonest to expire, down to 90% so the sweep is amortised.
fn make_room(cache: &mut HashMap<Key, Entry>, cap: usize, now: Instant) {
    if cache.len() < cap {
        return;
    }
    cache.retain(|_, e| {
        e.until
            + if e.res.allow {
                STALE_GRACE
            } else {
                Duration::ZERO
            }
            > now
    });
    let target = cap - (cap / 10).max(1);
    if cache.len() <= target {
        return;
    }
    let mut order: Vec<_> = cache
        .iter()
        .map(|(k, e)| ((e.res.allow, e.until), k.clone()))
        .collect();
    order.sort_unstable_by_key(|(rank, _)| *rank);
    for (_, k) in order.into_iter().take(cache.len() - target) {
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
            let cache = self.cache.locked();
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
                let mut cache = self.cache.locked();
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
            credential_source: ask.source,
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

    /// Drops every cached answer for `credential`, denials included.
    pub fn purge_credential(&self, credential: &str) {
        let hash = self.hasher.hash_one(credential);
        self.cache.locked().retain(|k, _| k.credential != hash);
    }

    /// Drops the cached answers matching every given filter (`POST /v1/revoke`, `/v1/invalidate`).
    pub fn purge(&self, owner: Option<&str>, media: &[u32]) {
        self.cache.locked().retain(|k, e| {
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
                parts: vec![],
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
    fn an_item_grant_needs_parts_instead_of_a_path() {
        let mut res = entry(Instant::now()).res;
        assert_eq!(
            granted(&res, ResolvePurpose::Stream),
            Err(ResolveError::Unavailable)
        );
        res.parts = vec![ResolvedPart {
            path: "/a.mp3".into(),
            duration_s: Some(3.0),
            start_s: None,
            end_s: None,
            format: None,
        }];
        let got = granted(&res, ResolvePurpose::Stream).unwrap();
        assert_eq!((got.path.as_str(), got.parts.len()), ("", 1));
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

    #[test]
    fn an_expired_denial_goes_at_once_but_an_expired_grant_keeps_its_grace() {
        let now = Instant::now();
        let mut cache = HashMap::new();
        let mut denied = entry(now - Duration::from_secs(1));
        denied.res.allow = false;
        cache.insert(key(1), denied);
        cache.insert(key(2), entry(now - Duration::from_secs(1)));
        make_room(&mut cache, 2, now);
        assert!(!cache.contains_key(&key(1)) && cache.contains_key(&key(2)));
    }

    #[test]
    fn make_room_sweeps_down_to_ninety_percent() {
        let now = Instant::now();
        let mut cache = HashMap::new();
        for n in 0..100 {
            cache.insert(key(n), entry(now + Duration::from_secs(n + 1)));
        }
        make_room(&mut cache, 100, now);
        assert_eq!(cache.len(), 90);
        assert!(!cache.contains_key(&key(0)) && cache.contains_key(&key(99)));
    }

    #[test]
    fn make_room_drops_denials_before_grants() {
        let now = Instant::now();
        let mut cache = HashMap::new();
        cache.insert(key(1), entry(now + Duration::from_secs(5)));
        let mut denied = entry(now + Duration::from_secs(60));
        denied.res.allow = false;
        cache.insert(key(2), denied);
        make_room(&mut cache, 2, now);
        assert!(cache.contains_key(&key(1)) && !cache.contains_key(&key(2)));
    }

    #[test]
    fn debug_never_shows_the_credential() {
        let ask = Ask {
            media: ResolveMediaId::File(1),
            credential: "Basic c2VjcmV0",
            source: Some(ResolveCredentialSource::Basic),
            purpose: ResolvePurpose::Download,
            subtitle_id: None,
            stream_index: None,
        };
        assert!(!format!("{ask:?}").contains("c2VjcmV0"));
    }
}

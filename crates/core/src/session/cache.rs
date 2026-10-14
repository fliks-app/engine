//! Transcode cache: layout and eviction policy over an index. [ts:transcoding/transcode-cache.service.ts]
use crate::api::{LifetimeConfig, StreamingConfig};
use serde::{Deserialize, Serialize};

pub const DEFAULT_CACHE_TTL_MS: i64 = 4 * 60 * 60 * 1000;
pub const DEFAULT_CACHE_MAX_BYTES: u64 = 20 * 1024 * 1024 * 1024;

/// Retention and disk budget of the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachePolicy {
    pub ttl_ms: i64,
    pub max_bytes: u64,
}

impl Default for CachePolicy {
    fn default() -> Self {
        Self {
            ttl_ms: DEFAULT_CACHE_TTL_MS,
            max_bytes: DEFAULT_CACHE_MAX_BYTES,
        }
    }
}

impl CachePolicy {
    /// Absent or non-positive settings keep the defaults. [ts:transcoding/transcode-cache.service.ts:66-69]
    pub fn from_config(streaming: &StreamingConfig) -> Self {
        let d = Self::default();
        let positive = |v: Option<f64>| v.filter(|v| *v > 0.0 && v.is_finite());
        Self {
            ttl_ms: positive(streaming.cache_ttl_ms).map_or(d.ttl_ms, |v| v as i64),
            max_bytes: positive(streaming.cache_max_bytes).map_or(d.max_bytes, |v| v as u64),
        }
    }
}

/// GC cadence in ms, with the default for a zero.
pub fn cache_gc_interval_ms(lifetime: &LifetimeConfig) -> u32 {
    lifetime.resolved().cache_gc_interval_ms
}

/// Namespace of the on-disk layout; the profile hash is part of its key. [new]
pub const CACHE_NAMESPACE: &str = "cache/v1";

/// `cache/v1/<u<owner>|anon>/<mediaFileId>/<cacheKey>[/<quality>]`, relative to the data root.
pub fn cache_path(
    owner_id: Option<&str>,
    media_file_id: u32,
    cache_key: &str,
    quality: Option<&str>,
) -> String {
    let owner = owner_id.map_or("anon".to_string(), |o| format!("u{o}"));
    let base = format!("{CACHE_NAMESPACE}/{owner}/{media_file_id}/{cache_key}");
    match quality {
        Some(q) if !q.is_empty() => format!("{base}/{q}"),
        _ => base,
    }
}

/// One (owner, file, profile) directory of the index, as rebuilt from disk before a pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheEntryInfo {
    pub key: String,
    pub total_bytes: u64,
    /// Newest file mtime, ms.
    pub last_access_ms: i64,
    /// A live session's dir is (or is inside) this entry: never evicted.
    pub live: bool,
}

/// Keys to evict, in order: past-TTL entries (index order), then least recently used until the
/// remaining total fits the budget.
pub fn plan_gc(entries: &[CacheEntryInfo], now_ms: i64, policy: &CachePolicy) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let expired = |e: &CacheEntryInfo| !e.live && now_ms - e.last_access_ms > policy.ttl_ms;
    out.extend(entries.iter().filter(|e| expired(e)).map(|e| e.key.clone()));
    let kept: Vec<&CacheEntryInfo> = entries.iter().filter(|e| !expired(e)).collect();
    let mut total: u64 = kept.iter().map(|e| e.total_bytes).sum();
    if total <= policy.max_bytes {
        return out;
    }
    let mut lru: Vec<&CacheEntryInfo> = kept.into_iter().filter(|e| !e.live).collect();
    lru.sort_by_key(|e| e.last_access_ms);
    for e in lru {
        if total <= policy.max_bytes {
            break;
        }
        total = total.saturating_sub(e.total_bytes);
        out.push(e.key.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(key: &str, bytes: u64, last: i64, live: bool) -> CacheEntryInfo {
        CacheEntryInfo {
            key: key.into(),
            total_bytes: bytes,
            last_access_ms: last,
            live,
        }
    }
    const H: i64 = 3_600_000;

    #[test]
    fn ttl_then_lru() {
        let p = CachePolicy {
            ttl_ms: 4 * H,
            max_bytes: 250,
        };
        let idx = [
            e("old", 100, -5 * H, false),
            e("a", 100, 0, false),
            e("b", 100, 100, false),
            e("c", 100, 200, false),
        ];
        assert_eq!(plan_gc(&idx, 0, &p), ["old", "a"]);
        assert_eq!(
            plan_gc(
                &idx[1..],
                0,
                &CachePolicy {
                    max_bytes: 300,
                    ..p
                }
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn ttl_boundary_is_strict() {
        let p = CachePolicy {
            ttl_ms: 4 * H,
            max_bytes: u64::MAX,
        };
        assert!(plan_gc(&[e("x", 1, -4 * H, false)], 0, &p).is_empty());
        assert_eq!(plan_gc(&[e("x", 1, -4 * H - 1, false)], 0, &p), ["x"]);
    }

    #[test]
    fn live_entries_stay_and_still_count() {
        let p = CachePolicy {
            ttl_ms: H,
            max_bytes: 150,
        };
        let idx = [e("live", 100, -9 * H, true), e("a", 100, 0, false)];
        assert_eq!(plan_gc(&idx, 0, &p), ["a"]);
    }

    #[test]
    fn policy_defaults() {
        let mut s = StreamingConfig::default();
        assert_eq!(CachePolicy::from_config(&s), CachePolicy::default());
        s.cache_ttl_ms = Some(-1.0);
        s.cache_max_bytes = Some(5e9);
        assert_eq!(
            CachePolicy::from_config(&s),
            CachePolicy {
                ttl_ms: DEFAULT_CACHE_TTL_MS,
                max_bytes: 5_000_000_000
            }
        );
    }

    #[test]
    fn layout() {
        assert_eq!(
            cache_path(Some("3"), 9, "abc-early", Some("720p")),
            "cache/v1/u3/9/abc-early/720p"
        );
        assert_eq!(cache_path(None, 9, "abc", None), "cache/v1/anon/9/abc");
    }
}

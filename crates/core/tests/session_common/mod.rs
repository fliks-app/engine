//! Replays a `core::session` vector (`{kind, input}`) and returns the host's `expected` shape.
use fliks_engine_core::api::LifetimeConfig;
use fliks_engine_core::session::*;
use serde::Deserialize;
use serde_json::{Value, json};

fn de<T: for<'a> Deserialize<'a>>(v: &Value) -> T {
    serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("input {v}: {e}"))
}

fn variant_of(v: &Value) -> SessionVariant {
    if v["kind"] == "remux" {
        remux_variant(
            v["audioIndex"].as_u64().map(|n| n as u32),
            v["keyframeGrid"].as_bool().unwrap(),
            v["multiAudio"].as_bool().unwrap(),
        )
    } else {
        de(v)
    }
}

fn cache_key(e: &Value) -> String {
    let owner = e["owner"]
        .as_u64()
        .map_or("anon".to_string(), |o| o.to_string());
    format!(
        "{owner}::{}::{}",
        e["mediaFileId"],
        e["profile"].as_str().unwrap()
    )
}

pub fn run(kind: &str, i: &Value) -> Value {
    let cfg = LifetimeConfig::default();
    match kind {
        "keys" => {
            let v = variant_of(&i["variant"]);
            let hash = variant_hash(i["base"].as_str().unwrap(), &v);
            json!({
                "sessionKey": session_key(
                    i["mediaFileId"].as_u64().unwrap() as u32,
                    i["ownerId"].as_u64().map(|o| o.to_string()).as_deref(),
                    &hash,
                ),
                "suffix": variant_suffix(&v),
                "hash": hash,
                "strippedRaw": base_profile_hash(i["raw"].as_str().unwrap()),
                "strippedHash": base_profile_hash(&hash),
            })
        }
        "resolve-existing" => {
            let input: ResolveInput = de(i);
            match resolve_existing(&input) {
                ResumeAction::Serve | ResumeAction::Wait => json!({ "result": "existing" }),
                ResumeAction::Respawn(r) => json!({
                    "result": "respawn",
                    "kill": r.kill,
                    "startSegment": r.start_segment.unwrap_or(input.run.start_segment),
                    "purgeCache": r.purge_cache,
                }),
            }
        }
        "throttle-decide" => json!({
            "decision": decide_throttle(
                i["aheadSeconds"].as_f64().unwrap(),
                i["paused"].as_bool().unwrap(),
                i["thresholdSeconds"].as_f64().unwrap(),
            )
        }),
        "run-lead" => json!({
            "lead": run_lead_seconds(&de(&i["span"]), i["playhead"].as_f64().unwrap())
        }),
        "job-playhead" => {
            let viewers: Vec<Viewer> = de(&i["viewers"]);
            json!({ "playhead": job_playhead_seconds(&viewers, &de(&i["grid"])) })
        }
        "throttle-eligible" => {
            let variant: Option<SessionVariant> = de(&i["variant"]);
            let viewers: Vec<Viewer> = de(&i["viewers"]);
            json!({ "eligible": is_throttle_eligible(variant.as_ref(), &viewers) })
        }
        "throttle-tick" => {
            let input: ThrottleInput = de(i);
            let out = throttle_step(&input);
            let hint = match &input.run {
                RunProgress::Transcode(t) => out.frontier_segment.or(t.frontier_hint),
                RunProgress::Remux { .. } => None,
            };
            json!({ "decision": out.decision, "frontierSegment": hint })
        }
        "cache-gc" => {
            let entries: Vec<CacheEntryInfo> = i["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| CacheEntryInfo {
                    key: cache_key(e),
                    total_bytes: e["totalBytes"].as_u64().unwrap(),
                    last_access_ms: e["lastAccessMs"].as_i64().unwrap(),
                    live: e["live"].as_bool().unwrap(),
                })
                .collect();
            let mut evicted = plan_gc(&entries, i["nowMs"].as_i64().unwrap(), &de(&i["policy"]));
            evicted.sort();
            json!({ "evicted": evicted })
        }
        "job-idle" => {
            let input: JobIdleInput = de(i);
            let (state, action) = job_idle(&input, &cfg);
            json!({
                "state": state,
                "action": if action == JobIdleAction::Keep { "keep" } else { "reap" },
                "kill": action == (JobIdleAction::Reap { kill: true }),
            })
        }
        "live-gc" => {
            let now = i["nowMs"].as_i64().unwrap();
            let states: Vec<&str> = i["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| {
                    let beat = s["lastBeatMs"].as_i64().unwrap();
                    if !live_session_expired(beat, s["pinned"].as_bool().unwrap(), now, &cfg) {
                        "live"
                    } else if tombstone_revivable(beat, now, &cfg) {
                        "revived"
                    } else {
                        "gone"
                    }
                })
                .collect();
            json!({ "states": states })
        }
        "live-cap" => {
            let beats: Vec<i64> = de(&i["lastBeatsMs"]);
            json!({ "evictedIndex": session_to_evict(&beats, &cfg) })
        }
        "sid-gate" => json!({
            "expired": sid_expired(i["sidPresent"].as_bool().unwrap(), i["known"].as_bool().unwrap())
        }),
        other => panic!("unknown session vector kind {other}"),
    }
}

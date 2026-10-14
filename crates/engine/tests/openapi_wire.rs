//! The schema cannot drift from the wire: for every component of `openapi.json`, a sample built
//! from the schema (one per `oneOf` alternative) goes through the Rust type and back, and every key
//! the type serializes must be a property of its schema.
use fliks_engine_core::api::*;
use serde_json::{Map, Value, json};
use utoipa::OpenApi;

fn doc() -> Value {
    serde_json::to_value(fliks_engine::ApiDoc::openapi()).unwrap()
}

fn resolve<'a>(doc: &'a Value, s: &'a Value) -> &'a Value {
    match s.get("$ref").and_then(Value::as_str) {
        Some(r) => resolve(
            doc,
            &doc["components"]["schemas"][r.rsplit('/').next().unwrap()],
        ),
        None => s,
    }
}

fn is_null(s: &Value) -> bool {
    s["type"] == "null"
}

/// A value for `s`; `pick` chooses the `oneOf` alternative of this level (nested ones: the first
/// non-null).
fn sample(doc: &Value, s: &Value, pick: Option<usize>) -> Value {
    let s = resolve(doc, s);
    if let Some(alts) = s
        .get("oneOf")
        .or_else(|| s.get("anyOf"))
        .and_then(Value::as_array)
    {
        let alt = match pick {
            Some(i) => &alts[i],
            None => alts.iter().find(|a| !is_null(a)).unwrap(),
        };
        return sample(doc, alt, None);
    }
    if let Some(all) = s.get("allOf").and_then(Value::as_array) {
        let mut out = Map::new();
        for a in all {
            if let Value::Object(o) = sample(doc, a, None) {
                out.extend(o);
            }
        }
        return Value::Object(out);
    }
    if let Some(e) = s.get("enum").and_then(Value::as_array) {
        return e
            .iter()
            .find(|v| !v.is_null())
            .cloned()
            .unwrap_or(Value::Null);
    }
    let ty = match &s["type"] {
        Value::Array(t) => t
            .iter()
            .find(|t| *t != "null")
            .cloned()
            .unwrap_or(Value::Null),
        t => t.clone(),
    };
    match ty.as_str() {
        Some("integer") => json!(1),
        Some("number") => json!(1.5),
        Some("boolean") => json!(true),
        Some("string") => json!("1"),
        Some("array") => json!([sample(doc, &s["items"], None)]),
        _ => {
            let mut out = Map::new();
            if let Some(p) = s["properties"].as_object() {
                for (k, v) in p {
                    out.insert(k.clone(), sample(doc, v, None));
                }
            }
            if let Some(a) = s.get("additionalProperties").filter(|a| a.is_object()) {
                out.insert("1".into(), sample(doc, a, None));
            }
            Value::Object(out)
        }
    }
}

/// Keys of `v` absent from `s` (any alternative of a `oneOf` may hold them).
fn stray(doc: &Value, s: &Value, v: &Value, path: &str, out: &mut Vec<String>) {
    let s = resolve(doc, s);
    if let Some(alts) = s
        .get("oneOf")
        .or_else(|| s.get("anyOf"))
        .or_else(|| s.get("allOf"))
        .and_then(Value::as_array)
    {
        let all = s.get("allOf").is_some();
        let per: Vec<Vec<String>> = alts
            .iter()
            .map(|a| {
                let mut o = vec![];
                stray(doc, a, v, path, &mut o);
                o
            })
            .collect();
        if all {
            // A key is stray when no part of the composition holds it.
            let first = per.first().cloned().unwrap_or_default();
            out.extend(
                first
                    .into_iter()
                    .filter(|k| per.iter().all(|p| p.contains(k))),
            );
        } else if !per.iter().any(Vec::is_empty) {
            out.extend(per.into_iter().min_by_key(Vec::len).unwrap_or_default());
        }
        return;
    }
    match v {
        Value::Object(o) => {
            let props = s["properties"].as_object();
            let extra = s.get("additionalProperties").filter(|a| a.is_object());
            for (k, x) in o {
                match (props.and_then(|p| p.get(k)), extra) {
                    (Some(ps), _) | (None, Some(ps)) => {
                        stray(doc, ps, x, &format!("{path}.{k}"), out)
                    }
                    (None, None) => out.push(format!("{path}.{k}")),
                }
            }
        }
        Value::Array(a) => {
            for x in a {
                stray(doc, &s["items"], x, &format!("{path}[]"), out);
            }
        }
        _ => {}
    }
}

fn check<T: serde::Serialize + serde::de::DeserializeOwned>(
    doc: &Value,
    name: &str,
) -> Vec<String> {
    let schema = &doc["components"]["schemas"][name];
    assert!(!schema.is_null(), "{name}: no schema");
    let alts = schema
        .get("oneOf")
        .and_then(Value::as_array)
        .map_or(1, Vec::len);
    let mut errs = vec![];
    for i in 0..alts {
        let input = sample(doc, schema, (alts > 1).then_some(i));
        let out = match serde_json::from_value::<T>(input.clone()) {
            Ok(t) => serde_json::to_value(t).unwrap(),
            Err(e) => {
                errs.push(format!(
                    "{name}#{i}: the schema's sample does not read: {e}\n  {input}"
                ));
                continue;
            }
        };
        let mut s = vec![];
        stray(doc, schema, &out, name, &mut s);
        errs.extend(
            s.into_iter()
                .map(|k| format!("{name}#{i}: {k} not in the schema")),
        );
    }
    errs
}

macro_rules! wire {
    ($doc:expr, $($t:ident),* $(,)?) => {{
        let mut errs = vec![];
        let names = vec![$(stringify!($t)),*];
        $(errs.extend(check::<$t>($doc, stringify!($t)));)*
        (names, errs)
    }};
}

#[test]
fn every_wire_key_is_in_its_schema() {
    let doc = doc();
    let (names, errs) = wire!(
        &doc,
        AudioCodec,
        AudioPlan,
        AudioStreamInfo,
        AudioTrackPlan,
        AutoQualityMode,
        BgJobState,
        BitrateHint,
        BurnInKind,
        BurnInSpec,
        CacheDeleteResult,
        CacheEntry,
        CacheSummary,
        Capabilities,
        CapabilitiesResponse,
        Chapter,
        ClientInfo,
        CodecCondition,
        CodecVariant,
        CropInfo,
        DecideBody,
        DecideMedia,
        DecideRequest,
        DecideSelection,
        Decision,
        DeviceProfile,
        DeviceType,
        DirectPlayProfile,
        EngineConfig,
        EngineEvent,
        EstimateSizesRequest,
        Fingerprint,
        FingerprintWindow,
        Gpu,
        HdrFormat,
        HdrMetadata,
        HeartbeatRequest,
        HeartbeatResponse,
        Hello,
        Hints,
        HostCaps,
        HwAccel,
        InvalidateRequest,
        InvalidateResult,
        JobAccepted,
        JobKind,
        JobPriority,
        JobRequest,
        JobResult,
        JobSnapshot,
        JobState,
        JobStats,
        Keyframe,
        LifetimeConfig,
        LiveCaps,
        LiveError,
        LiveMode,
        LiveProbe,
        LiveSnapshot,
        LiveTvConfig,
        LiveUpstream,
        MediaInfo,
        MediaRef,
        MovieHash,
        MuxFlavour,
        OpenLiveRequest,
        OpenLiveResponse,
        OpenSessionRequest,
        OpenSessionResponse,
        PipelineStage,
        QoeOutput,
        QoePipeline,
        QoeSource,
        Platform,
        PlayMethod,
        PlaybackInfo,
        PlaybackSource,
        ProbeRequest,
        ProbeResult,
        QsvPreset,
        QualityEstimate,
        QualityOption,
        ResolveMediaId,
        ResolvePurpose,
        ResolveRequest,
        ResolveResponse,
        RevokeRequest,
        RevokeResult,
        SessionAction,
        SessionCommand,
        SessionExpired,
        SessionKind,
        SessionSnapshot,
        SourceScan,
        SpriteMeta,
        StreamingConfig,
        SubtitlePrewarm,
        SubtitleStreamInfo,
        SubtitleTrack,
        SubtitleTrackKind,
        TonemapAlgo,
        TonemapCurve,
        Track,
        TranscodeReason,
        VideoCodec,
        VideoStreamInfo,
    );
    let mut all: Vec<&str> = doc["components"]["schemas"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    all.retain(|n| !names.contains(n));
    assert!(all.is_empty(), "schemas without a wire check: {all:?}");
    assert!(errs.is_empty(), "{}", errs.join("\n"));
}

#[test]
fn operation_ids_are_unique() {
    let mut seen = std::collections::HashSet::new();
    for (path, item) in doc()["paths"].as_object().unwrap() {
        for (method, op) in item.as_object().unwrap() {
            let id = op["operationId"].as_str().unwrap();
            assert!(
                seen.insert(id),
                "duplicate operationId {id} ({method} {path})"
            );
        }
    }
}

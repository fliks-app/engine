//! Golden packaging calls (`fixtures/mp4/*.json`; `BLESS=1` rewrites `expected`).
mod common;

use common::{Fixture, blessing, diff};
use fliks_engine_core::api::{Keyframe, MediaInfo, SourceScan};
use fliks_engine_core::mp4::{self, BoxRef, IdMap, Mp4Error, RemuxAssemblyPlan, TrackInfo};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::path::PathBuf;

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn unb64(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes().filter(|c| *c != b'=') {
        let v = B64.iter().position(|x| *x == c).expect("base64") as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

fn b64(b: &[u8]) -> Value {
    let mut s = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = c
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, x)| n | u32::from(*x) << (16 - 8 * i));
        for i in 0..4 {
            s.push(if i <= c.len() {
                B64[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    json!(s)
}

fn bytes(v: &Value) -> Vec<u8> {
    unb64(v.as_str().expect("base64 string"))
}

fn de<T: DeserializeOwned>(v: &Value) -> T {
    serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("{e}: {v}"))
}

fn f(v: &Value) -> f64 {
    v.as_f64()
        .unwrap_or_else(|| panic!("number expected, got {v}"))
}

fn n(v: &Value) -> usize {
    v.as_u64()
        .unwrap_or_else(|| panic!("index expected, got {v}")) as usize
}

fn ticks(v: &Value) -> i128 {
    v.as_str().expect("ticks string").parse().expect("ticks")
}

fn id_map<V>(v: &Value, val: impl Fn(&Value) -> V) -> IdMap<V> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|e| (e[0].as_u64().unwrap() as u32, val(&e[1])))
        .collect()
}

fn header(b: Option<BoxRef>) -> Value {
    b.map_or(Value::Null, |b| {
        let mut v = json!(b);
        v.as_object_mut().unwrap().remove("start");
        v
    })
}

fn grid(v: &Value) -> Option<mp4::KeyframeGrid> {
    (!v.is_null()).then(|| de(v))
}

fn plan(v: &Value) -> RemuxAssemblyPlan {
    de(v)
}

fn run(fn_: &str, a: &Value) -> Result<Value, Mp4Error> {
    let tracks = |v: &Value| id_map(v, de::<TrackInfo>);
    Ok(match fn_ {
        "readBoxHeader" => header(mp4::read_box_header(
            &bytes(&a["buf"]),
            n(&a["off"]),
            n(&a["start"]),
            n(&a["end"]),
        )),
        "findBox" => {
            let kind: [u8; 4] = s(&a["type"]).as_bytes().try_into().unwrap();
            json!(mp4::find_box(
                &bytes(&a["buf"]),
                n(&a["start"]),
                n(&a["end"]),
                &kind
            ))
        }
        "parseInitTracks" => json!(mp4::parse_init_tracks(&bytes(&a["init"]))?),
        "rewriteSegmentTfdt" => b64(&mp4::rewrite_segment_tfdt(
            bytes(&a["seg"]),
            &tracks(&a["tracks"]),
            a["segIndex"].as_u64().unwrap() as u32,
            f(&a["segDuration"]),
            a["startPts"].as_f64().unwrap_or(0.0),
        )?),
        "readInitEdits" => {
            let e = mp4::read_init_edits(&bytes(&a["init"]))?;
            json!(
                e.iter()
                    .map(|(id, t)| json!([id, t.to_string()]))
                    .collect::<Vec<_>>()
            )
        }
        "withInitEdits" => {
            // A recorded NaN or undefined is JSON null, which JS multiplies as 0.
            let secs = id_map(&a["seconds"], |v| v.as_f64().unwrap_or(0.0));
            let lookup = |id: u32, _: &TrackInfo| {
                secs.iter()
                    .find(|(k, _)| *k == id)
                    .map_or(f64::NAN, |e| e.1)
            };
            b64(&mp4::with_init_edits(&bytes(&a["init"]), lookup)?)
        }
        "retimeFragments" => b64(&mp4::retime_fragments(
            &bytes(&a["seg"]),
            &id_map(&a["deltas"], ticks),
        )?),
        "firstTfdt" => json!(
            mp4::first_tfdt(&bytes(&a["seg"]), a["trackId"].as_u64().unwrap() as u32)?
                .map(|t| t.to_string())
        ),
        "readTfdt" => {
            let t = mp4::read_tfdt(&bytes(&a["buf"]), &de(&a["traf"]))?;
            t.map_or(Value::Null, |t| json!({ "version": t.version, "valueOffset": t.value_offset, "value": t.value.to_string() }))
        }
        "trunSamples" => json!(mp4::trun_samples(
            &bytes(&a["buf"]),
            &de(&a["traf"]),
            a["trexDefault"].as_u64().unwrap() as u32
        )?),
        "videoDecodeExtent" => {
            let e = mp4::video_decode_extent(
                &bytes(&a["moofs"]),
                a["trackId"].as_u64().unwrap() as u32,
                a["trexDefault"].as_u64().unwrap() as u32,
            )?;
            e.map_or(Value::Null, |e| json!({ "end": e.end.to_string(), "lastDuration": e.last_duration.to_string() }))
        }
        "parseTrexDefaults" => json!(mp4::parse_trex_defaults(&bytes(&a["init"]))?),
        "makeBox" => {
            let kind: [u8; 4] = s(&a["type"]).as_bytes().try_into().unwrap();
            let children: Vec<Vec<u8>> = a["children"]
                .as_array()
                .unwrap()
                .iter()
                .map(bytes)
                .collect();
            let refs: Vec<&[u8]> = children.iter().map(Vec::as_slice).collect();
            b64(&mp4::make_box(&kind, &refs)?)
        }
        "cmafRewrite" => b64(&mp4::cmaf_rewrite(&bytes(&a["buf"]))),
        "isCmafRewritten" => json!(mp4::is_cmaf_rewritten(&bytes(&a["buf"]))),
        "readAndRewriteCmaf" => {
            mp4::rewrite_cmaf(bytes(&a["buf"])).map_or(Value::Null, |b| b64(&b))
        }
        "computeSegmentGrid" => {
            let k: Vec<Keyframe> = de(&a["keyframes"]);
            let frame = a["frameSeconds"].as_f64().unwrap_or(1.0 / 24.0);
            json!(mp4::compute_segment_grid(
                &k,
                f(&a["origin"]),
                f(&a["end"]),
                f(&a["segDur"]),
                frame
            ))
        }
        "gridSegmentIndex" => {
            let b: Vec<f64> = de(&a["boundaries"]);
            json!(mp4::grid_segment_index(
                &b,
                f(&a["seconds"]),
                f(&a["origin"])
            ))
        }
        "remuxSegmentGrid" => {
            let scan = (!a["scan"].is_null()).then(|| SourceScan {
                keyframes: de(&a["scan"]["keyframes"]),
                end: f(&a["scan"]["end"]),
                break_seconds: None,
                audio_config_changes: Default::default(),
                schema_version: None,
            });
            json!(mp4::remux_segment_grid(
                scan.as_ref(),
                f(&a["origin"]),
                f(&a["segDur"]),
                a["frameRate"].as_str()
            ))
        }
        "sourceTimeline" => {
            let si: Option<MediaInfo> = (!a["si"].is_null()).then(|| de(&a["si"]));
            json!(mp4::source_timeline(si.as_ref()))
        }
        "inputSeekSeconds" => json!(mp4::input_seek_seconds(
            f(&a["contentSeconds"]),
            &de(&a["timeline"])
        )),
        "servedShift" => json!(mp4::served_shift(f(&a["timeline"]["origin"]))),
        "servedOrigin" => json!(mp4::served_origin(f(&a["timeline"]["origin"]))),
        "cueOffsetSeconds" => json!(mp4::cue_offset_seconds(&de(&a["timeline"]))),
        "remuxRunStart" => json!(mp4::remux_run_start(
            grid(&a["grid"]).as_ref(),
            n(&a["startSegment"]),
            f(&a["segmentDuration"]),
            f(&a["origin"]),
            (!a["audioGrid"].is_null()).then(|| de(&a["audioGrid"])),
        )?),
        "remuxEdits" => json!(mp4::remux_edits(f(&a["start"]), f(&a["firstDecode"]))),
        "remuxAssemblyPlan" => json!(mp4::remux_assembly_plan(
            grid(&a["grid"]).as_ref(),
            n(&a["startSegment"]),
            &de(&a["run"]),
            f(&a["origin"]),
            a["segmentDuration"].as_f64(),
            a["audioRenditions"].as_u64().map(|x| x as u32),
        )),
        "planBoundarySeconds" => {
            let x = mp4::plan_boundary_seconds(&plan(&a["plan"]), n(&a["i"]));
            if x.is_nan() { Value::Null } else { json!(x) }
        }
        "gopsOf" => json!(mp4::gops_of(&plan(&a["plan"]), n(&a["segment"]))),
        "parseFrame" => {
            let fr = mp4::parse_frame(&bytes(&a["moof"]), &bytes(&a["mdat"]))?;
            json!({ "tfdt": fr.tfdt.to_string(), "sample": fr.sample, "data": b64(&fr.data) })
        }
        "buildFragment" => {
            let data: Vec<Vec<u8>> = a["data"].as_array().unwrap().iter().map(bytes).collect();
            let refs: Vec<&[u8]> = data.iter().map(Vec::as_slice).collect();
            b64(&mp4::build_fragment(
                a["trackId"].as_u64().unwrap() as u32,
                a["sequenceNumber"].as_u64().unwrap() as u32,
                ticks(&a["tfdt"]),
                &de::<Vec<_>>(&a["samples"]),
                &refs,
            )?)
        }
        other => panic!("unknown fn {other}"),
    })
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

/// Mismatch text, `None` when the port reproduces `expected` (value or error).
fn check(v: &Value) -> Option<String> {
    let want = &v["expected"];
    match (run(s(&v["fn"]), &v["args"]), want.get("error")) {
        (Ok(got), None) => {
            let mut d = vec![];
            diff("", &got, &want["ok"], &mut d);
            (!d.is_empty()).then(|| d.join("\n  "))
        }
        (Err(Mp4Error::OutOfRange), Some(_)) if want.get("code").is_some() => None,
        (Err(Mp4Error::Msg(m)), Some(e)) if want.get("code").is_none() && e == &json!(m) => None,
        (got, _) => Some(format!("got {got:?}, expected {want}")),
    }
}

/// What `expected` becomes: the value, or the error (a Node `code` is kept, else a RangeError).
fn blessed(v: &Value) -> Value {
    match run(s(&v["fn"]), &v["args"]) {
        Ok(got) => json!({ "ok": got }),
        Err(Mp4Error::Msg(m)) => json!({ "error": m, "type": "Error" }),
        Err(Mp4Error::OutOfRange) => {
            let old = &v["expected"];
            if old.get("code").is_some() {
                old.clone()
            } else {
                json!({ "error": "out of range", "type": "RangeError", "code": "ERR_OUT_OF_RANGE" })
            }
        }
    }
}

fn report(what: &str, n: usize, failed: &[String]) {
    for f in failed.iter().take(10) {
        eprintln!("FAIL {f}");
    }
    assert!(n > 0, "{what}: nothing replayed");
    assert!(failed.is_empty(), "{what}: {} of {n} fail", failed.len());
}

#[test]
fn mp4_fixtures_reproduce_the_host() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/mp4");
    let (mut n, mut failed) = (0, vec![]);
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let mut fx = Fixture::load(&p);
        if let Some(m) = check(&fx.root) {
            if blessing() {
                let expected = blessed(&fx.root);
                fx.bless(None, expected);
                fx.save();
                n += 1;
                continue;
            }
            failed.push(format!("{}: {m}", p.file_name().unwrap().to_string_lossy()));
        }
        n += 1;
    }
    report("fixtures/mp4", n, &failed);
}

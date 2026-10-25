//! Golden host-detection vectors (`fixtures/hw/*.json`, `BLESS=1` rewrites `expected`): the
//! recorded ffmpeg and sysfs answers go through the pure detection and must give the TS's
//! `Capabilities`, `HostCaps` and the same set of ffmpeg commands.
mod common;

use common::{Fixture, blessing, diff};
use fliks_engine_core::hw::{
    Cmd, DEFAULT_RENDER_NODE, Detected, Device, GpuInfo, GpuRaw, Outcome, PauseVerdict, Runner,
    describe_gpu, detect, pause_capability, pause_step, platform_from_os, selected_render_node,
};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

pub fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = std::pin::pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

const TMP: &str = "TMPDIR";
const PID: u32 = 4242;

/// Fixture argv carries `{tmp}/` and `{pid}` where the run has its temp dir and pid.
fn norm(a: &str) -> String {
    let a = match a.strip_prefix(TMP) {
        Some(rest) => format!("{{tmp}}/{}", &rest[1..]),
        None => a.to_string(),
    };
    a.replace(&format!("-{PID}."), "-{pid}.")
}

struct Replay {
    calls: HashMap<String, (bool, String, String)>,
    seen: Mutex<BTreeSet<String>>,
}

fn key(args: &[String]) -> String {
    args.join("\u{1f}")
}

impl Runner for Replay {
    async fn run(&self, cmd: &Cmd) -> Outcome {
        let args: Vec<String> = cmd.args.iter().map(|a| norm(a)).collect();
        self.seen
            .lock()
            .unwrap()
            .insert(format!("{}\u{1e}{}", key(&args), cmd.timeout_ms));
        let (ok, stdout, stderr) = self
            .calls
            .get(&key(&args))
            .unwrap_or_else(|| panic!("no recorded call for ffmpeg {}", args.join(" ")))
            .clone();
        Outcome { ok, stdout, stderr }
    }

    async fn write(&self, _path: &str, _bytes: &[u8]) -> bool {
        true
    }

    async fn remove(&self, _path: &str) {}
}

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/hw")
}

fn fixtures() -> Vec<(String, Value)> {
    let mut out: Vec<_> = std::fs::read_dir(dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .map(|p| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            (
                name,
                serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap(),
            )
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn str_of(v: &Value) -> Option<String> {
    v.as_str().map(String::from)
}

/// `enumerateGpus()` over the fixture's `/dev/dri` listing and sysfs readings.
fn gpus(env: &Value) -> Vec<GpuInfo> {
    let mut names: Vec<String> = env["driEntries"]
        .as_array()
        .map(|a| a.iter().filter_map(str_of).collect())
        .unwrap_or_default();
    names.sort();
    names
        .iter()
        .filter(|n| {
            n.strip_prefix("renderD")
                .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        })
        .map(|n| {
            let d = &env["drm"][n.as_str()];
            let raw = GpuRaw {
                vendor_id: str_of(&d["vendor"]),
                device_id: str_of(&d["device"]),
                boot_vga: str_of(&d["bootVga"]),
            };
            describe_gpu(n, &format!("/dev/dri/{n}"), &raw)
        })
        .collect()
}

/// The pause probe's three frame samples (1.5 s, 3 s, 4 s after `p`) read the way the engine does.
fn stdin_pause_works(samples: &Value) -> bool {
    let Some(s) = samples.as_array() else {
        return false;
    };
    let n = |i: usize| s[i].as_u64().unwrap() as u32;
    match pause_step(n(0), n(1), false) {
        PauseVerdict::Frozen => pause_step(n(0), n(2), true) == PauseVerdict::Works,
        _ => false,
    }
}

fn run(f: &Value) -> (Detected, Vec<GpuInfo>, String, BTreeSet<String>) {
    let env = &f["env"];
    let platform = platform_from_os(env["platform"].as_str().unwrap());
    let calls = f["calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let args: Vec<String> = c["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap().to_string())
                .collect();
            (
                key(&args),
                (
                    c["exit"] == 0,
                    c["stdout"].as_str().unwrap_or("").to_string(),
                    c["stderr"].as_str().unwrap_or("").to_string(),
                ),
            )
        })
        .collect();
    let runner = Replay {
        calls,
        seen: Mutex::new(BTreeSet::new()),
    };
    let node = selected_render_node(env["gpuRenderNode"].as_str().unwrap());
    let dev = Device {
        platform,
        render_node: node,
        opencl_device: None,
    };
    let d = block_on(detect(&dev, TMP, PID, &runner));
    let pause = pause_capability(stdin_pause_works(&f["pause"]["samples"]), platform);
    (
        d,
        gpus(env),
        pause.to_string(),
        runner.seen.into_inner().unwrap(),
    )
}

#[test]
fn replays_every_fixture() {
    let all = fixtures();
    assert!(!all.is_empty(), "fixtures/hw is empty");
    for (name, f) in &all {
        let (d, gpus, pause, seen) = run(f);
        let want = &f["expected"];
        let mut out = vec![];
        let host_caps = serde_json::to_value(d.host_caps()).unwrap();
        diff("hostCaps", &host_caps, &want["hostCaps"], &mut out);
        let node = selected_render_node(f["env"]["gpuRenderNode"].as_str().unwrap())
            .unwrap_or_else(|| DEFAULT_RENDER_NODE.to_string());
        let capabilities = serde_json::to_value(d.capabilities(&gpus, &node, &pause)).unwrap();
        diff(
            "capabilities",
            &capabilities,
            &want["capabilities"],
            &mut out,
        );
        let expected: BTreeSet<String> = want["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                let args: Vec<String> = c["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|a| a.as_str().unwrap().to_string())
                    .collect();
                format!("{}\u{1e}{}", key(&args), c["timeoutMs"])
            })
            .collect();
        for c in seen.difference(&expected) {
            out.push(format!(
                "command only in Rust: {}",
                c.replace('\u{1f}', " ")
            ));
        }
        for c in expected.difference(&seen) {
            out.push(format!("command only in TS: {}", c.replace('\u{1f}', " ")));
        }
        if !out.is_empty() && blessing() {
            let commands: Vec<Value> = seen
                .iter()
                .map(|c| {
                    let (args, ms) = c.split_once('\u{1e}').unwrap();
                    let args: Vec<&str> = args.split('\u{1f}').collect();
                    json!({ "args": args, "timeoutMs": ms.parse::<u64>().unwrap() })
                })
                .collect();
            let mut fx = Fixture::load(&dir().join(name));
            fx.bless(
                None,
                json!({ "capabilities": capabilities, "commands": commands, "hostCaps": host_caps }),
            );
            fx.save();
            continue;
        }
        assert!(out.is_empty(), "{name}:\n  {}", out.join("\n  "));
    }
}

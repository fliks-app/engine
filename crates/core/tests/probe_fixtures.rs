//! Golden probes (`fixtures/probe/*.json`): the recorded output of ffprobe, mkvmerge and
//! ffmpeg of one file, replayed through `probe()`, must give the host's `MediaInfo`.
mod common;

use common::diff;
use fliks_engine_core::probe::{Outcome, ProbeOpts, ToolError, Tools, probe};
use serde_json::{Value, json};
use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

/// Every future of the replay is ready at once: poll it to completion without a runtime.
fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

struct Recorded {
    cmd: String,
    args: Vec<String>,
    stdout: String,
    stderr: String,
    fail: Option<Value>,
    stopped: bool,
    used: bool,
}

struct Replay {
    calls: Mutex<Vec<Recorded>>,
    unmatched: Mutex<Vec<String>>,
    /// Streamed runs read further, or less far, than the host read.
    overreads: Mutex<Vec<String>>,
    file_size: u64,
}

impl Replay {
    fn new(f: &Value) -> Self {
        let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
        let calls = f["calls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| Recorded {
                cmd: s(&c["cmd"]),
                args: c["args"].as_array().unwrap().iter().map(s).collect(),
                stdout: s(&c["stdout"]),
                stderr: s(&c["stderr"]),
                fail: c.get("fail").cloned(),
                stopped: c["stopped"].as_bool() == Some(true),
                used: false,
            })
            .collect();
        Self {
            calls: Mutex::new(calls),
            unmatched: Mutex::default(),
            overreads: Mutex::default(),
            file_size: f["fileSize"].as_u64().unwrap(),
        }
    }

    /// The first unused recorded call with these arguments: the same arguments, in any order of runs.
    fn take(&self, cmd: &str, args: &[String]) -> Option<(String, String, Option<Value>, bool)> {
        let mut calls = self.calls.lock().unwrap();
        let hit = calls
            .iter_mut()
            .find(|c| !c.used && c.cmd == cmd && c.args == args);
        match hit {
            Some(c) => {
                c.used = true;
                Some((
                    c.stdout.clone(),
                    c.stderr.clone(),
                    c.fail.clone(),
                    c.stopped,
                ))
            }
            None => {
                self.unmatched
                    .lock()
                    .unwrap()
                    .push(format!("{cmd} {args:?}"));
                None
            }
        }
    }

    fn run(&self, cmd: &str, args: &[String]) -> Result<String, ToolError> {
        let Some((stdout, stderr, fail, _)) = self.take(cmd, args) else {
            return Err(ToolError {
                message: "no recorded call".into(),
                stderr: "no recorded call".into(),
                ..ToolError::default()
            });
        };
        match fail {
            None => Ok(stdout),
            Some(f) => Err(ToolError {
                message: if f["enoent"].as_bool() == Some(true) {
                    format!("spawn {cmd} ENOENT")
                } else {
                    format!("Command failed: {cmd} {}\n{stderr}", args.join(" "))
                },
                stderr,
                stdout,
                missing: f["enoent"].as_bool() == Some(true),
            }),
        }
    }
}

impl Tools for Replay {
    fn ffprobe(
        &self,
        args: Vec<String>,
        _: u64,
    ) -> impl Future<Output = Result<String, ToolError>> + Send {
        std::future::ready(self.run("ffprobe", &args))
    }

    fn ffprobe_lines(
        &self,
        args: Vec<String>,
        _: u64,
        on_line: &mut (dyn FnMut(&str) -> bool + Send),
    ) -> impl Future<Output = Result<(), ToolError>> + Send {
        let stopped = self
            .calls
            .lock()
            .unwrap()
            .iter()
            .find(|c| !c.used && c.cmd == "ffprobe" && c.args == args)
            .is_some_and(|c| c.stopped);
        let out = self.run("ffprobe", &args);
        let (stdout, err) = match out {
            Ok(s) => (s, None),
            Err(e) => (e.stdout.clone(), Some(e)),
        };
        let mut stopped_by_us = false;
        for line in stdout.lines().filter(|l| !l.is_empty()) {
            if !on_line(line) {
                stopped_by_us = true;
                break;
            }
        }
        if stopped_by_us != stopped {
            let what = if stopped {
                "read to the end"
            } else {
                "stopped early"
            };
            self.overreads
                .lock()
                .unwrap()
                .push(format!("{args:?}: {what}"));
        }
        std::future::ready(if stopped_by_us {
            Ok(())
        } else {
            err.map_or(Ok(()), Err)
        })
    }

    fn mkvmerge(
        &self,
        args: Vec<String>,
    ) -> impl Future<Output = Result<String, ToolError>> + Send {
        std::future::ready(self.run("mkvmerge", &args))
    }

    fn ffmpeg_samples(
        &self,
        args: Vec<Vec<String>>,
    ) -> impl Future<Output = Vec<Result<String, ToolError>>> + Send {
        // The host reads the log of a successful run from stderr.
        let out = args
            .iter()
            .map(|a| match self.take("ffmpeg", a) {
                Some((_, stderr, None, _)) => Ok(stderr),
                Some((_, stderr, Some(_), _)) => Err(ToolError {
                    stderr,
                    ..ToolError::default()
                }),
                None => Err(ToolError::default()),
            })
            .collect();
        std::future::ready(out)
    }

    fn file_size(&self) -> impl Future<Output = Option<u64>> + Send {
        std::future::ready(Some(self.file_size))
    }
}

fn strip_key(v: &mut Value, key: &str) {
    match v {
        Value::Object(m) => {
            m.remove(key);
            m.values_mut().for_each(|x| strip_key(x, key));
        }
        Value::Array(a) => a.iter_mut().for_each(|x| strip_key(x, key)),
        _ => {}
    }
}

fn got(o: &Outcome) -> Value {
    let mut v = json!({ "ok": o.ok, "info": o.info });
    if let Some(e) = &o.error {
        v["error"] = json!(e);
    }
    v
}

#[test]
fn probe_reproduces_the_host() {
    // PROBE_FIXTURES_DIR: a larger throwaway run of the host harness (see docs/tests.md).
    let dir = std::env::var_os("PROBE_FIXTURES_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/probe"),
        PathBuf::from,
    );
    let (mut n, mut failed) = (0, vec![]);
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for p in entries {
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        if !name.ends_with(".json") || name.starts_with('_') {
            continue;
        }
        let f: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let tools = Replay::new(&f);
        let opts = ProbeOpts {
            crop: f["opts"]["crop"].as_bool().unwrap(),
            mkv_tags: f["opts"]["mkvTags"].as_bool().unwrap(),
            // The fixtures were recorded with crop forced to software.
            crop_decode: None,
        };
        let outcome = block_on(probe(&tools, "{path}", &opts));
        let mut expected = f["expected"].clone();
        expected.as_object_mut().unwrap().remove("nulls");
        let mut d = vec![];
        diff("", &got(&outcome), &expected, &mut d);
        let unmatched = tools.unmatched.lock().unwrap().clone();
        if !unmatched.is_empty() {
            d.push(format!("unrecorded tool runs: {unmatched:?}"));
        }
        d.extend(tools.overreads.lock().unwrap().iter().cloned());
        let unused = tools
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| !c.used)
            .count();
        if unused > 0 {
            d.push(format!("{unused} recorded tool runs not made"));
        }
        if !d.is_empty() {
            failed.push(format!("{name}\n  {}", d.join("\n  ")));
        }
        n += 1;
        // `mkvTags: false` never runs mkvmerge and leaves the tags out.
        let mkv_runs = f["calls"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["cmd"] == "mkvmerge")
            .count();
        if mkv_runs > 0 {
            let tools = Replay::new(&f);
            let off = block_on(probe(
                &tools,
                "{path}",
                &ProbeOpts {
                    mkv_tags: false,
                    ..opts.clone()
                },
            ));
            let unused = tools
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| !c.used)
                .count();
            let mut want = f["expected"]["info"].clone();
            strip_key(&mut want, "languageTag");
            if unused != mkv_runs || !common::same(&serde_json::to_value(&off.info).unwrap(), &want)
            {
                failed.push(format!(
                    "{name}: mkvTags=false still reads tags ({unused} unused runs)"
                ));
            }
        }
    }
    assert!(n > 0, "fixtures/probe is empty");
    for f in failed.iter().take(40) {
        eprintln!("FAIL {f}");
    }
    assert!(failed.is_empty(), "probe: {} of {n} fail", failed.len());
}

//! The boot probe chain: pick a backend, then probe decoders, encoders and tone-map paths.
//! [ts:backend/src/modules/streaming/transcoding/transcoding.service.ts:140-170,297-375]
use super::decoders::{DECODERS, DecodeCodec};
use super::device::Device;
use super::encoders::ENCODERS;
use super::parse;
use super::probe::{self, Cmd};
use crate::api::{Capabilities, Gpu, HostCaps, HwAccel, Platform};
use std::future::Future;
use std::path::Path;

/// What one ffmpeg run did: `ok` is exit status 0 within the deadline.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// IO the chain needs; the engine spawns ffmpeg and deletes sample files, tests replay fixtures.
pub trait Runner: Sync {
    fn run(&self, cmd: &Cmd) -> impl Future<Output = Outcome> + Send;
    fn remove(&self, path: &str) -> impl Future<Output = ()> + Send;
}

/// The probe results, as `HostCaps` reads them plus the lists `Capabilities` reports.
#[derive(Debug, Clone, PartialEq)]
pub struct Detected {
    pub hw_accel: HwAccel,
    pub platform: Platform,
    pub libfdk_aac: bool,
    pub vaapi_writes_hdr_metadata: bool,
    pub probes: Vec<String>,
    pub encoders: Vec<String>,
    pub decoders: Vec<String>,
    pub disabled_encoders: Vec<String>,
    pub disabled_decoders: Vec<String>,
}

fn sample_path(tmp: &str, name: &str) -> String {
    Path::new(tmp).join(name).to_string_lossy().into_owned()
}

#[derive(Default)]
struct Flags {
    tonemap_opencl: bool,
    tonemap_opencl_crop: bool,
    tonemap_opencl_hdr10: bool,
    qsv_opencl: bool,
    qsv_opencl_hdr10: bool,
    vpp_qsv: bool,
    vulkan: bool,
    cuda: bool,
    cuda_hdr10: bool,
    opencl: bool,
    opencl_hdr10: bool,
    amf_opencl: bool,
}

/// Runs the whole chain. Failures only turn a flag off; nothing here aborts. Serial where the TS
/// parallelises CPU probes (the result does not depend on it).
pub async fn detect<R: Runner>(dev: &Device, tmp_dir: &str, pid: u32, r: &R) -> Detected {
    let platform = dev.platform;
    let mut hw_accel = HwAccel::None;
    for (hw, cmd) in probe::hw_tests(dev) {
        if r.run(&cmd).await.ok {
            hw_accel = hw;
            break;
        }
    }

    let libfdk_aac = has_libfdk(r).await;
    let decoders = probe_decoders(dev, tmp_dir, pid, r).await;
    let (encoders, ihd) = probe_encoders(dev, hw_accel, r).await;
    let f = probe_tonemap(dev, hw_accel, tmp_dir, pid, r).await;

    let ok_decoders: Vec<&str> = DECODERS
        .iter()
        .zip(&decoders)
        .filter(|(_, ok)| **ok)
        .map(|(d, _)| d.id)
        .collect();
    let ok_encoders: Vec<&str> = ENCODERS
        .iter()
        .zip(&encoders)
        .filter(|(_, ok)| **ok)
        .map(|(e, _)| e.id)
        .collect();
    let split = |ids: Vec<&str>, all: Vec<&str>| -> (Vec<String>, Vec<String>) {
        let on = ids.iter().map(|s| s.to_string()).collect();
        let off = all
            .into_iter()
            .filter(|i| !ids.contains(i))
            .map(String::from)
            .collect();
        (on, off)
    };
    let (enc_on, enc_off) = split(ok_encoders, ENCODERS.iter().map(|e| e.id).collect());
    let (dec_on, dec_off) = split(ok_decoders, DECODERS.iter().map(|d| d.id).collect());
    let names = [
        ("tonemapOpencl", f.tonemap_opencl),
        ("tonemapOpenclCrop", f.tonemap_opencl_crop),
        ("tonemapOpenclHdr10", f.tonemap_opencl_hdr10),
        ("qsvOpencl", f.qsv_opencl),
        ("qsvOpenclHdr10", f.qsv_opencl_hdr10),
        ("vppQsv", f.vpp_qsv),
        ("vulkan", f.vulkan),
        ("cuda", f.cuda),
        ("cudaHdr10", f.cuda_hdr10),
        ("opencl", f.opencl),
        ("openclHdr10", f.opencl_hdr10),
        ("amfOpencl", f.amf_opencl),
    ];
    Detected {
        hw_accel,
        platform,
        libfdk_aac,
        vaapi_writes_hdr_metadata: ihd,
        probes: names
            .into_iter()
            .filter(|(_, on)| *on)
            .map(|(n, _)| n.to_string())
            .collect(),
        encoders: enc_on,
        decoders: dec_on,
        disabled_encoders: enc_off,
        disabled_decoders: dec_off,
    }
}

async fn has_libfdk<R: Runner>(r: &R) -> bool {
    let o = r.run(&probe::encoders_listing()).await;
    o.ok && parse::has_libfdk_aac(&o.stdout)
}

/// One flag per decoder, in `DECODERS` order. [ts:backend/src/modules/streaming/transcoding/codec/decoder-probe.ts:33-100]
async fn probe_decoders<R: Runner>(dev: &Device, tmp: &str, pid: u32, r: &R) -> Vec<bool> {
    let mut samples: Vec<(DecodeCodec, Option<String>)> = vec![];
    for d in DECODERS {
        let Some(c) = d.codec else { continue };
        if samples.iter().any(|(k, _)| *k == c) {
            continue;
        }
        let path = sample_path(tmp, &probe::decoder_sample_name(c, pid));
        let made = r.run(&probe::decoder_sample_cmd(c, &path)).await.ok;
        if !made {
            r.remove(&path).await;
        }
        samples.push((c, made.then_some(path)));
    }
    let sample = |c: DecodeCodec| {
        samples
            .iter()
            .find(|(k, _)| *k == c)
            .and_then(|(_, p)| p.clone())
    };
    let mut out = vec![];
    for d in DECODERS {
        let path = sample(d.codec.unwrap_or(DecodeCodec::H264));
        let ok = match (d.supported(dev.platform), path) {
            (true, Some(p)) => r.run(&probe::decoder_cmd(d, dev, &p)).await.ok,
            _ => false,
        };
        out.push(ok);
    }
    for (_, p) in samples
        .iter()
        .filter_map(|(c, p)| p.as_ref().map(|p| (c, p)))
    {
        r.remove(p).await;
    }
    out
}

/// One flag per encoder, plus whether VAAPI runs Intel's iHD driver.
/// [ts:backend/src/modules/streaming/transcoding/codec/encoder-probe.ts:112-185]
async fn probe_encoders<R: Runner>(dev: &Device, hw: HwAccel, r: &R) -> (Vec<bool>, bool) {
    let todo = |e: &super::Encoder| probe::probeable(hw, e.hw);
    let want_ihd = ENCODERS
        .iter()
        .any(|e| todo(e) && e.hw == HwAccel::Vaapi && e.ten_bit);
    let ihd = want_ihd && {
        let o = r.run(&probe::vaapi_driver_cmd(dev)).await;
        o.ok && parse::vaapi_driver_is_intel_ihd(&o.stderr)
    };
    let mut out = vec![];
    for e in ENCODERS {
        let ok =
            todo(e) && e.supported(dev.platform) && r.run(&probe::encoder_cmd(e, dev)).await.ok;
        out.push(ok);
    }
    (out, ihd)
}

/// Tone-map probes by detected backend. Each probe that needs the HDR sample makes it first and
/// fails closed with it; the sample is deleted whatever happens.
/// [ts:backend/src/modules/streaming/transcoding/transcoding.service.ts:323-375]
async fn probe_tonemap<R: Runner>(dev: &Device, hw: HwAccel, tmp: &str, pid: u32, r: &R) -> Flags {
    let mut f = Flags::default();
    let win = dev.platform == Platform::Win32;
    let sample = |name: &str| sample_path(tmp, &probe::hdr_sample_name(name, pid));
    // Makes the sample; the caller removes it.
    let synth = |p: String| async move { r.run(&probe::hdr_sample_cmd(&p)).await.ok };

    if hw == HwAccel::Qsv {
        let p = sample("vpp-qsv-tonemap");
        f.vpp_qsv = synth(p.clone()).await && r.run(&probe::vpp_qsv_cmd(dev, &p)).await.ok;
        r.remove(&p).await;
        if win {
            let p = sample("qsv-opencl");
            let [sdr, hdr10] = probe::qsv_opencl_cmds(&p);
            if synth(p.clone()).await && r.run(&sdr).await.ok {
                f.qsv_opencl = true;
                f.qsv_opencl_hdr10 = r.run(&hdr10).await.ok;
            }
            r.remove(&p).await;
        }
    }
    if matches!(hw, HwAccel::Qsv | HwAccel::Vaapi) && !win {
        let p = sample("tonemap-opencl");
        if synth(p.clone()).await {
            let [plain, crop, hdr10] = probe::tonemap_opencl_cmds(hw, dev, &p);
            if r.run(&plain).await.ok {
                f.tonemap_opencl = true;
                f.tonemap_opencl_crop = r.run(&crop).await.ok;
                f.tonemap_opencl_hdr10 = r.run(&hdr10).await.ok;
            }
        }
        r.remove(&p).await;
    }
    if matches!(hw, HwAccel::Nvenc | HwAccel::Amf) {
        let p = sample("opencl-tonemap");
        if synth(p.clone()).await {
            let [sdr, hdr10] = probe::opencl_tonemap_cmds(hw, dev, &p);
            if r.run(&sdr).await.ok {
                f.opencl = true;
                f.opencl_hdr10 = r.run(&hdr10).await.ok;
            }
        }
        r.remove(&p).await;
    }
    if hw == HwAccel::Vaapi && dev.platform == Platform::Linux {
        let p = sample("vulkan-tonemap");
        f.vulkan = synth(p.clone()).await && r.run(&probe::vulkan_cmd(dev, &p)).await.ok;
        r.remove(&p).await;
    }
    if hw == HwAccel::Nvenc {
        let [sdr, hdr10] = probe::cuda_cmds();
        if r.run(&sdr).await.ok {
            f.cuda = true;
            f.cuda_hdr10 = r.run(&hdr10).await.ok;
        }
    }
    if hw == HwAccel::Amf {
        let p = sample("amf-opencl");
        if synth(p.clone()).await {
            let [sdr, hdr10] = probe::amf_opencl_cmds(&p);
            f.amf_opencl = r.run(&sdr).await.ok && r.run(&hdr10).await.ok;
        }
        r.remove(&p).await;
    }
    f
}

impl Detected {
    pub fn host_caps(&self) -> HostCaps {
        HostCaps {
            hw_accel: self.hw_accel,
            platform: self.platform,
            libfdk_aac: self.libfdk_aac,
            vaapi_writes_hdr_metadata: self.vaapi_writes_hdr_metadata,
            probes: self.probes.clone(),
            disabled_encoders: self.disabled_encoders.clone(),
            disabled_decoders: self.disabled_decoders.clone(),
        }
    }

    /// Tone-map algorithms this host can run; `auto` is always there.
    /// [ts:backend/src/modules/streaming/transcoding/transcoding.service.ts:377-395]
    pub fn tonemap_algos(&self) -> Vec<String> {
        let has = |p: &str| self.probes.iter().any(|x| x == p);
        let mut out = vec!["auto".to_string()];
        if matches!(self.hw_accel, HwAccel::Qsv | HwAccel::Vaapi) {
            out.push("vaapi".into());
        }
        if self.hw_accel == HwAccel::Qsv && has("vppQsv") {
            out.push("qsv".into());
        }
        if has("tonemapOpencl") {
            out.push("opencl".into());
        }
        out
    }

    /// `gpus` and `pause_capability` come from outside the ffmpeg chain; `default_gpu` is the
    /// node `auto` resolves to, only meaningful when there is a node to pick.
    pub fn capabilities(
        &self,
        gpus: &[super::GpuInfo],
        render_node: &str,
        pause_capability: &str,
    ) -> Capabilities {
        Capabilities {
            hw_accel: hw_name(self.hw_accel).into(),
            encoders: self.encoders.clone(),
            decoders: self.decoders.clone(),
            tonemap_algos: self.tonemap_algos(),
            gpus: gpus.iter().map(Gpu::from).collect(),
            default_gpu: (!gpus.is_empty()).then(|| render_node.to_string()),
            platform: platform_name(self.platform).into(),
            pause_capability: pause_capability.into(),
        }
    }
}

fn hw_name(h: HwAccel) -> &'static str {
    match h {
        HwAccel::Vaapi => "vaapi",
        HwAccel::Nvenc => "nvenc",
        HwAccel::Qsv => "qsv",
        HwAccel::Amf => "amf",
        HwAccel::Videotoolbox => "videotoolbox",
        HwAccel::None => "none",
    }
}

fn platform_name(p: Platform) -> &'static str {
    match p {
        Platform::Linux => "linux",
        Platform::Darwin => "darwin",
        Platform::Win32 => "win32",
    }
}

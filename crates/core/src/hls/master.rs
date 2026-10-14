//! The master playlist: audio and subtitle renditions, then the remux variant or a rung ladder.
use super::codecs::{self, plan_codec};
use crate::api::{AudioCodec, AudioPlan, CodecVariant, HdrFormat, VideoCodec};
use crate::decide::audio::{REMUX_STEREO_BPS, output_bitrate_bps};
use crate::jsnum::{js_string, round, to_fixed, truthy};
use crate::ladder::{
    HDR_LADDER, Rung, RungBitrateContext, SDR_LADDER, capped_rung_video_bitrate_bps,
    profile_fits_source, profile_resolution, resolve_rung,
};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioStreamMeta {
    pub language: Option<String>,
    pub language_tag: Option<String>,
    pub title: Option<String>,
    pub channels: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubtitleKind {
    Embedded,
    External,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleRendition {
    pub kind: SubtitleKind,
    /// Stream index (embedded) or subtitle file id (external).
    pub key: u32,
    pub language: Option<String>,
    pub language_tag: Option<String>,
    pub name: String,
    #[serde(default)]
    pub forced: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HdrPassThrough {
    pub hdr_format: HdrFormat,
    pub hdr_variant: CodecVariant,
}

/// Inputs of the master; absent fields take the host's defaults.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MasterOptions {
    /// Root every URI hangs off (the host's `/api/stream/{id}`).
    pub prefix: String,
    /// Query suffix appended to every URI.
    pub token_param: String,
    pub source_width: u32,
    pub source_height: u32,
    pub remux_width: Option<u32>,
    pub remux_height: Option<u32>,
    pub include_remux: bool,
    /// Non-empty: EXT-X-MEDIA renditions; empty: an audio-less source; absent: muxed audio.
    pub audio_streams: Option<Vec<AudioStreamMeta>>,
    pub muxed_audio: Option<AudioStreamMeta>,
    pub audio_plans: Option<Vec<AudioPlan>>,
    pub only_quality: Option<String>,
    pub default_audio_index: i64,
    pub hdr_pass_through: Option<HdrPassThrough>,
    pub can_emit_hdr_ladder: bool,
    pub sdr_variant: Option<CodecVariant>,
    pub source_frame_rate: Option<f64>,
    pub subtitle_renditions: Option<Vec<SubtitleRendition>>,
    pub source_video_bitrate_bps: Option<f64>,
    pub source_video_codec: Option<String>,
    pub dedupes_audio_by_language: bool,
    pub supports_abr: Option<bool>,
    pub remux_codecs: Option<String>,
    pub remux_supplemental_codecs: Option<String>,
    pub format_bit_rate: Option<f64>,
    pub source_hdr_format: Option<HdrFormat>,
    pub i_frame_trick_play_segment_seconds: Option<f64>,
}

const DEFAULT_PLAN: AudioPlan = AudioPlan::Transcode {
    codec: AudioCodec::Aac,
    channels: 2,
};
const SDR_H264: CodecVariant = CodecVariant {
    codec: VideoCodec::H264,
    bit_depth: 8,
    hdr: None,
};
const REMUX_FALLBACK_BANDWIDTH_BPS: f64 = 8_000_000.0;
const IFRAME_CODEC: &str = "avc1.4d401f";

/// Remux BANDWIDTH: the copied video plus the largest audio rendition, else the container total.
pub fn remux_bandwidth_bps(
    video_bps: Option<f64>,
    format_bps: Option<f64>,
    plans: &[AudioPlan],
) -> f64 {
    if let Some(v) = video_bps.filter(|v| truthy(*v)) {
        let audio = plans.iter().fold(0.0, |m, p| {
            crate::jsnum::max(m, output_bitrate_bps(p, REMUX_STEREO_BPS))
        });
        return round(v) + audio;
    }
    let total = round(format_bps.filter(|f| truthy(*f)).unwrap_or(0.0));
    if truthy(total) {
        total
    } else {
        REMUX_FALLBACK_BANDWIDTH_BPS
    }
}

fn frame_rate_attr(fps: f64) -> String {
    let s = if fps.fract() == 0.0 {
        js_string(fps)
    } else {
        to_fixed(fps, 3)
    };
    format!(",FRAME-RATE={s}")
}

/// NAME per rendition: title, else language, else `und`; repeats get ` #2`, ` #3`...
fn unique_names<'a>(
    streams: impl Iterator<Item = (Option<&'a str>, Option<&'a str>)>,
) -> Vec<String> {
    let mut seen: HashMap<String, u32> = HashMap::new();
    streams
        .map(|(title, language)| {
            let base = [title, language]
                .into_iter()
                .flatten()
                .find(|s| !s.is_empty())
                .unwrap_or("und")
                .to_string();
            let n = seen.entry(base.clone()).or_default();
            *n += 1;
            if *n == 1 {
                base
            } else {
                format!("{base} #{n}")
            }
        })
        .collect()
}

fn rendition_language(tag: &Option<String>, language: &Option<String>) -> String {
    [tag, language]
        .into_iter()
        .flatten()
        .find(|s| !s.is_empty())
        .map_or("und".into(), Clone::clone)
}

fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().filter(|s| !s.is_empty())
}

fn has_audio_identity(a: &AudioStreamMeta) -> bool {
    non_empty(&a.title).is_some()
        || non_empty(&a.language_tag).is_some()
        || non_empty(&a.language).is_some_and(|l| l != "und")
}

/// One index per language: the picked track wins its own, every other keeps its first.
fn keep_one_per_language(streams: &[AudioStreamMeta], picked: usize) -> HashSet<usize> {
    let lang = |i: usize| non_empty(&streams[i].language).unwrap_or("und");
    let mut by_lang: HashMap<&str, usize> = HashMap::from([(lang(picked), picked)]);
    for i in 0..streams.len() {
        by_lang.entry(lang(i)).or_insert(i);
    }
    by_lang.into_values().collect()
}

struct Ctx<'a> {
    o: &'a MasterOptions,
    plans: Vec<AudioPlan>,
    no_audio: bool,
    codecs_tail: String,
    frame_rate_attr: String,
    audio_attr: &'static str,
    subs_attr: &'static str,
    fps: f64,
}

impl Ctx<'_> {
    fn uri(&self, path: &str) -> String {
        format!("{}/{path}{}", self.o.prefix, self.o.token_param)
    }

    fn rung_audio_bps(&self, r: &Rung) -> f64 {
        if self.no_audio {
            return 0.0;
        }
        self.plans
            .iter()
            .map(|p| output_bitrate_bps(p, r.audio_bps))
            .fold(f64::NEG_INFINITY, crate::jsnum::max)
    }

    fn push_remux(&self, lines: &mut Vec<String>) {
        let o = self.o;
        let plans: &[AudioPlan] = if self.no_audio { &[] } else { &self.plans };
        let bw = js_string(remux_bandwidth_bps(
            o.source_video_bitrate_bps,
            o.format_bit_rate,
            plans,
        ));
        let w = o.remux_width.unwrap_or(o.source_width);
        let h = o.remux_height.unwrap_or(o.source_height);
        let range = match o.source_hdr_format {
            Some(HdrFormat::Hlg) => ",VIDEO-RANGE=HLG",
            Some(HdrFormat::Hdr10) => ",VIDEO-RANGE=PQ",
            None => "",
        };
        let codecs = non_empty(&o.remux_codecs);
        let codecs_attr = codecs.map_or(String::new(), |c| {
            format!(",CODECS=\"{c}{}\"", self.codecs_tail)
        });
        let supplemental = match (codecs, non_empty(&o.remux_supplemental_codecs)) {
            (Some(_), Some(s)) => format!(",SUPPLEMENTAL-CODECS=\"{s}\""),
            _ => String::new(),
        };
        lines.push(format!(
            "#EXT-X-STREAM-INF:BANDWIDTH={bw},AVERAGE-BANDWIDTH={bw},RESOLUTION={w}x{h}{range}{},NAME=\"remux\"{codecs_attr}{supplemental}{}{},CLOSED-CAPTIONS=NONE",
            self.frame_rate_attr, self.audio_attr, self.subs_attr
        ));
        lines.push(self.uri("remux/index.m3u8"));
    }

    fn push_ladder(
        &self,
        lines: &mut Vec<String>,
        rungs: &[Option<&Rung>],
        variant: &CodecVariant,
        range: &str,
    ) {
        let o = self.o;
        let bitrate_ctx = RungBitrateContext {
            output_codec: crate::decide::codec::codec_name(variant.codec),
            source_width: o.source_width,
            source_height: o.source_height,
            source_frame_rate: self.fps,
            source_video_bitrate_bps: o.source_video_bitrate_bps,
            source_video_codec: o.source_video_codec.as_deref().unwrap_or(""),
        };
        // An empty collapsed ladder, where the TS throws, emits no rung.
        for r in rungs.iter().flatten() {
            let (w, h) = profile_resolution(r, o.source_width, o.source_height);
            let avg = capped_rung_video_bitrate_bps(r, &bitrate_ctx) + self.rung_audio_bps(r);
            let bw = round(avg * 1.5);
            let codec = codecs::video(variant, w, h, self.fps);
            lines.push(format!(
                "#EXT-X-STREAM-INF:BANDWIDTH={},AVERAGE-BANDWIDTH={},RESOLUTION={}x{}{range}{},NAME=\"{}\",CODECS=\"{codec}{}\"{}{},CLOSED-CAPTIONS=NONE",
                js_string(bw),
                js_string(avg),
                js_string(w),
                js_string(h),
                self.frame_rate_attr,
                r.name,
                self.codecs_tail,
                self.audio_attr,
                self.subs_attr
            ));
            lines.push(self.uri(&format!("{}/index.m3u8", r.name)));
        }
    }

    fn push_iframe(&self, lines: &mut Vec<String>) {
        let Some(secs) = self
            .o
            .i_frame_trick_play_segment_seconds
            .filter(|s| truthy(*s))
        else {
            return;
        };
        let (w, h) = iframe_resolution(
            f64::from(self.o.source_width),
            f64::from(self.o.source_height),
        );
        let bw = round(120_000.0 * 8.0 / crate::jsnum::max(1.0, secs));
        lines.push(format!(
            "#EXT-X-I-FRAME-STREAM-INF:BANDWIDTH={},RESOLUTION={}x{},CODECS=\"{IFRAME_CODEC}\",URI=\"{}\"",
            js_string(bw),
            js_string(w),
            js_string(h),
            self.uri("iframe/index.m3u8")
        ));
    }
}

/// Trick-play output size: source aspect, height capped at 720, both axes even.
pub fn iframe_resolution(w: f64, h: f64) -> (f64, f64) {
    let even = |n: f64| crate::jsnum::max(2.0, round(n / 2.0) * 2.0);
    if h <= 720.0 {
        (even(w), even(h))
    } else {
        (even(w * 720.0 / h), 720.0)
    }
}

/// The first rung that fits the source, else the first.
fn top_fitting<'a>(ladder: &[&'a Rung], w: u32, h: u32) -> Option<&'a Rung> {
    ladder
        .iter()
        .find(|r| profile_fits_source(r, Some(w), Some(h)))
        .or(ladder.first())
        .copied()
}

/// HDR ladder rungs carry `-hdr`.
fn hdr_rung_name(q: &str, hdr: bool) -> String {
    if hdr && !q.ends_with("-hdr") {
        format!("{q}-hdr")
    } else {
        q.to_string()
    }
}

/// Rung a `startQuality` pin lands on in this source's master (what a prewarm spawns).
pub fn pinned_rung_name(only_quality: &str, w: u32, h: u32, hdr: bool) -> String {
    let name = hdr_rung_name(only_quality, hdr);
    let fits = |r: &&Rung| profile_fits_source(r, Some(w), Some(h));
    let ladder: Vec<Rung> = crate::ladder::ladder(hdr)
        .iter()
        .filter(fits)
        .copied()
        .collect();
    resolve_rung(&name, &ladder).map_or(name, |r| r.name.to_string())
}

/// A pinned or non-ABR client gets one rung; an unresolvable pin keeps the ladder.
fn apply_pin<'a>(ladder: Vec<&'a Rung>, o: &MasterOptions, hdr: bool) -> Vec<Option<&'a Rung>> {
    let (w, h) = (o.source_width, o.source_height);
    match non_empty(&o.only_quality) {
        None if o.supports_abr.unwrap_or(true) => ladder.into_iter().map(Some).collect(),
        None | Some("remux" | "original") => vec![top_fitting(&ladder, w, h)],
        Some(q) => {
            let name = hdr_rung_name(q, hdr);
            let owned: Vec<Rung> = ladder.iter().map(|r| **r).collect();
            match resolve_rung(&name, &owned).map(|r| r.name) {
                Some(n) => vec![ladder.iter().find(|r| r.name == n).copied()],
                None => ladder.into_iter().map(Some).collect(),
            }
        }
    }
}

pub fn master_playlist(o: &MasterOptions) -> String {
    let streams = o.audio_streams.as_deref();
    let multi_audio = streams.is_some_and(|s| !s.is_empty());
    let no_audio = streams.is_some_and(<[_]>::is_empty);
    let plans = match &o.audio_plans {
        Some(p) if !p.is_empty() => p.clone(),
        _ => vec![DEFAULT_PLAN],
    };
    let output_codec = plan_codec(&plans[0]).to_string();
    let codecs_tail = match codecs::audio_group(&plans) {
        Some(c) if !no_audio => format!(",{c}"),
        _ => String::new(),
    };
    let fps = o.source_frame_rate.unwrap_or(24.0);
    let subs = o.subtitle_renditions.as_deref().filter(|s| !s.is_empty());
    let mut lines: Vec<String> = vec![
        "#EXTM3U".into(),
        "#EXT-X-VERSION:7".into(),
        "#EXT-X-INDEPENDENT-SEGMENTS".into(),
    ];
    let uri = |path: String| format!("{}/{path}{}", o.prefix, o.token_param);

    if let Some(streams) = streams.filter(|_| multi_audio) {
        let picked = usize::try_from(o.default_audio_index)
            .ok()
            .filter(|i| *i < streams.len())
            .unwrap_or(0);
        let names = unique_names(
            streams
                .iter()
                .map(|a| (a.title.as_deref(), a.language.as_deref())),
        );
        let publish = o
            .dedupes_audio_by_language
            .then(|| keep_one_per_language(streams, picked));
        for (i, a) in streams.iter().enumerate() {
            if publish.as_ref().is_some_and(|p| !p.contains(&i)) {
                continue;
            }
            let default = if i == picked { "YES" } else { "NO" };
            let channels = plans
                .get(i)
                .and_then(codecs::channels_attr)
                .unwrap_or_else(|| {
                    codecs::rendition_channels(&output_codec, a.channels).to_string()
                });
            lines.push(format!(
                "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"{}\",LANGUAGE=\"{}\",DEFAULT={default},AUTOSELECT={default},CHANNELS=\"{channels}\",URI=\"{}\"",
                names[i],
                rendition_language(&a.language_tag, &a.language),
                uri(format!("audio/{i}/index.m3u8"))
            ));
        }
    }
    let muxed = o
        .muxed_audio
        .as_ref()
        .filter(|a| !multi_audio && !no_audio && has_audio_identity(a));
    if let Some(a) = muxed {
        let name = &unique_names(std::iter::once((a.title.as_deref(), a.language.as_deref())))[0];
        let channels = codecs::channels_attr(&plans[0])
            .unwrap_or_else(|| codecs::rendition_channels(&output_codec, a.channels).to_string());
        lines.push(format!(
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"{name}\",LANGUAGE=\"{}\",DEFAULT=YES,AUTOSELECT=YES,CHANNELS=\"{channels}\"",
            rendition_language(&a.language_tag, &a.language)
        ));
    }
    if let Some(subs) = subs {
        let names = unique_names(
            subs.iter()
                .map(|s| (Some(s.name.as_str()), s.language.as_deref())),
        );
        for (s, name) in subs.iter().zip(&names) {
            let path = match s.kind {
                SubtitleKind::Embedded => format!("subtitles/embedded/{}", s.key),
                SubtitleKind::External => format!("subtitles/{}", s.key),
            };
            lines.push(format!(
                "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"{name}\",LANGUAGE=\"{}\",DEFAULT=NO,AUTOSELECT=NO,FORCED={},URI=\"{}\"",
                rendition_language(&s.language_tag, &s.language),
                if s.forced { "YES" } else { "NO" },
                uri(format!("{path}/index.m3u8"))
            ));
        }
    }

    let ctx = Ctx {
        o,
        plans,
        no_audio,
        codecs_tail,
        frame_rate_attr: frame_rate_attr(fps),
        audio_attr: if multi_audio || muxed.is_some() {
            ",AUDIO=\"audio\""
        } else {
            ""
        },
        subs_attr: if subs.is_some() {
            ",SUBTITLES=\"subs\""
        } else {
            ""
        },
        fps,
    };
    let pinned = non_empty(&o.only_quality).is_some();
    let (w, h) = (Some(o.source_width), Some(o.source_height));
    if o.include_remux && !pinned {
        ctx.push_remux(&mut lines);
    } else if let Some(hdr) = &o.hdr_pass_through {
        if o.can_emit_hdr_ladder {
            let base: Vec<&Rung> = HDR_LADDER
                .iter()
                .filter(|r| profile_fits_source(r, w, h))
                .collect();
            let range = match hdr.hdr_format {
                HdrFormat::Hlg => ",VIDEO-RANGE=HLG",
                HdrFormat::Hdr10 => ",VIDEO-RANGE=PQ",
            };
            ctx.push_ladder(
                &mut lines,
                &apply_pin(base, o, true),
                &hdr.hdr_variant,
                range,
            );
        }
    } else {
        let mut base: Vec<&Rung> = SDR_LADDER
            .iter()
            .filter(|r| profile_fits_source(r, w, h))
            .collect();
        if base.is_empty() {
            base.push(&SDR_LADDER[SDR_LADDER.len() - 1]);
        }
        let variant = o.sdr_variant.unwrap_or(SDR_H264);
        ctx.push_ladder(&mut lines, &apply_pin(base, o, false), &variant, "");
    }
    ctx.push_iframe(&mut lines);
    lines.join("\n")
}

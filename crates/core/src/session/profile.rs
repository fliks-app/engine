//! Cache profile hash of a session (`cache/v1`) and the session fields it derives from.
//! New in the engine: stable, not the host's sha1. [ts:transcoding/profile-hash.ts] [ts:transcoding/session-profile.ts]
use crate::api::{CodecVariant, HdrFormat, MuxFlavour, TonemapCurve, VideoCodec};
use crate::decide::dolby_vision::has_no_base;
use serde::{Deserialize, Serialize};

/// Raise when the segment layout (edit lists, tfdt origin, audio alignment) changes, so cached
/// segments of the old layout never mix in. [ts:transcoding/profile-hash.ts:79]
pub const SEGMENT_TIMELINE_VERSION: u32 = 1;

/// Everything that decides the bytes of the segments written for one playback: two playbacks
/// with equal profiles share a cache dir, any difference forks it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackProfile {
    pub video_codec: VideoCodec,
    pub video_bit_depth: u32,
    pub hdr: Option<HdrFormat>,
    pub audio_codec: String,
    pub audio_channels: u32,
    pub audio_copy: bool,
    /// Encoder binary of a transcode (`aac` vs `libfdk_aac`); `copy` otherwise.
    pub audio_encoder_id: String,
    /// Per-rendition mask of a var_stream_map group: `c` or `t<channels><encoder>` each.
    pub audio_track_modes: Option<String>,
    pub mux: MuxFlavour,
    /// Multi-audio: video-only segments plus one rendition per track.
    pub var_stream_map: bool,
    pub segment_duration_ms: u32,
    /// Client class (`browser`, `tizen`, ...): player quirks that change segment bytes.
    pub tv_platform: String,
    /// Source timeline the run is cut against.
    pub origin: f64,
    pub format_start: f64,
    pub source_version: Option<String>,
    pub dolby_vision: bool,
    /// A no-base Dolby Vision source tone-mapped with its RPU applied.
    pub dv_tonemap: bool,
    /// ... towards HDR10 instead of SDR.
    pub dv_tonemap_hdr10: bool,
    pub hdr10_plus_strip: bool,
    pub dovi_el_strip: bool,
    /// Only for an SDR-output tone-map: a copy or an HDR10 reshape runs no curve.
    pub tonemap_curve: Option<TonemapCurve>,
}

fn name<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

/// An f64 as its IEEE bits (`-0` folded into `0`): no formatting drift between versions.
fn bits(v: f64) -> String {
    format!("{:016x}", (v + 0.0).to_bits())
}

impl PlaybackProfile {
    /// `cache/v1|key=value|...`, fixed order, every key always present (a new field is appended,
    /// so it can never alias an old layout).
    pub fn canonical(&self) -> String {
        let b = |v: bool| u8::from(v);
        let (ac, am, ae) = if self.var_stream_map {
            // The mask hashes every rendition: the picked track's own settings must not fork it.
            (String::new(), String::new(), String::new())
        } else {
            (
                self.audio_channels.to_string(),
                if self.audio_copy { "copy" } else { "transcode" }.to_string(),
                self.audio_encoder_id.clone(),
            )
        };
        let atm = if self.var_stream_map {
            self.audio_track_modes.clone().unwrap_or_default()
        } else {
            String::new()
        };
        [
            super::CACHE_NAMESPACE.to_string(),
            format!("v={}", name(&self.video_codec)),
            format!("vd={}", self.video_bit_depth),
            format!("h={}", self.hdr.map_or("null".into(), |h| name(&h))),
            format!("a={}", self.audio_codec),
            format!("atm={atm}"),
            format!("ac={ac}"),
            format!("am={am}"),
            format!("ae={ae}"),
            format!("mux={}", name(&self.mux)),
            format!(
                "al={}",
                if self.var_stream_map {
                    "var-stream-map"
                } else {
                    "inline"
                }
            ),
            format!("sd={}", self.segment_duration_ms),
            format!("tv={}", self.tv_platform),
            format!("tl={SEGMENT_TIMELINE_VERSION}"),
            format!("o={}", bits(self.origin)),
            format!("fs={}", bits(self.format_start)),
            format!("sv={}", self.source_version.clone().unwrap_or_default()),
            format!("dv={}", b(self.dolby_vision)),
            format!("dvt={}", b(self.dv_tonemap)),
            format!("dvt10={}", b(self.dv_tonemap && self.dv_tonemap_hdr10)),
            format!("h10p={}", b(self.hdr10_plus_strip)),
            format!("dvel={}", b(self.dovi_el_strip)),
            format!(
                "tc={}",
                self.tonemap_curve.map_or(String::new(), |c| name(&c))
            ),
        ]
        .join("|")
    }
}

/// FNV-1a 64, 16 hex digits: a directory name, not a security boundary.
pub fn profile_hash(p: &PlaybackProfile) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in p.canonical().bytes() {
        h ^= u64::from(byte);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// One audio output as the hash sees it; `encoder_id` comes from `core::args`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LayoutAudio {
    pub codec: String,
    pub channels: u32,
    pub copy: bool,
    pub encoder_id: String,
}

/// What playback-info freezes on the session; every request rebuilds its cache hash from it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionLayout {
    pub use_ts: bool,
    pub audio_stream_count: u32,
    pub audio_plan: LayoutAudio,
    pub audio_track_plans: Option<Vec<LayoutAudio>>,
    pub video_variant: Option<CodecVariant>,
    pub timeline_origin: f64,
    pub timeline_format_start: Option<f64>,
    pub source_version: Option<String>,
    pub dolby_vision: bool,
    pub tonemapping: bool,
    pub tonemap_curve: TonemapCurve,
    pub source_dv_profile: Option<u32>,
    pub source_dv_bl_signal_compat_id: Option<u32>,
    pub source_hdr10_plus: bool,
}

/// [ts:transcoding/profile-hash.ts:140-186]; a source with several audio tracks is a
/// var_stream_map layout (the session is video-only).
pub fn profile_from_layout(l: &SessionLayout, segment_duration_ms: u32) -> PlaybackProfile {
    let v = l.video_variant;
    let track = |p: &LayoutAudio| {
        if p.copy {
            "c".to_string()
        } else {
            format!("t{}{}", p.channels, p.encoder_id)
        }
    };
    PlaybackProfile {
        video_codec: v.map_or(VideoCodec::H264, |v| v.codec),
        video_bit_depth: v.map_or(8, |v| v.bit_depth),
        hdr: v.and_then(|v| v.hdr),
        audio_codec: l.audio_plan.codec.clone(),
        audio_channels: l.audio_plan.channels,
        audio_copy: l.audio_plan.copy,
        audio_encoder_id: if l.audio_plan.copy {
            "copy".into()
        } else {
            l.audio_plan.encoder_id.clone()
        },
        audio_track_modes: l
            .audio_track_plans
            .as_ref()
            .map(|t| t.iter().map(track).collect()),
        mux: if l.use_ts {
            MuxFlavour::Ts
        } else {
            MuxFlavour::Fmp4
        },
        var_stream_map: l.audio_stream_count > 1,
        segment_duration_ms,
        tv_platform: "browser".into(),
        origin: l.timeline_origin,
        format_start: l.timeline_format_start.unwrap_or(l.timeline_origin),
        source_version: l.source_version.clone(),
        dolby_vision: l.dolby_vision,
        dv_tonemap: l.tonemapping
            && has_no_base(l.source_dv_profile, l.source_dv_bl_signal_compat_id),
        dv_tonemap_hdr10: v.is_some_and(|v| v.hdr == Some(HdrFormat::Hdr10)),
        hdr10_plus_strip: l.dolby_vision && l.source_hdr10_plus,
        dovi_el_strip: l.source_dv_profile == Some(7) && !l.dolby_vision,
        tonemap_curve: (l.tonemapping && v.is_none_or(|v| v.hdr.is_none()))
            .then_some(l.tonemap_curve),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> SessionLayout {
        SessionLayout {
            use_ts: false,
            audio_stream_count: 1,
            audio_plan: LayoutAudio {
                codec: "aac".into(),
                channels: 2,
                copy: false,
                encoder_id: "aac".into(),
            },
            audio_track_plans: None,
            video_variant: Some(CodecVariant {
                codec: VideoCodec::H264,
                bit_depth: 8,
                hdr: None,
            }),
            timeline_origin: 0.0,
            timeline_format_start: None,
            source_version: None,
            dolby_vision: false,
            tonemapping: false,
            tonemap_curve: TonemapCurve::Mobius,
            source_dv_profile: None,
            source_dv_bl_signal_compat_id: None,
            source_hdr10_plus: false,
        }
    }

    #[test]
    fn canonical_snapshot() {
        let p = profile_from_layout(&layout(), 3000);
        assert_eq!(
            p.canonical(),
            "cache/v1|v=h264|vd=8|h=null|a=aac|atm=|ac=2|am=transcode|ae=aac|mux=fmp4|al=inline|sd=3000|tv=browser|tl=1\
             |o=0000000000000000|fs=0000000000000000|sv=|dv=0|dvt=0|dvt10=0|h10p=0|dvel=0|tc="
        );
    }

    #[test]
    fn hash_snapshot_is_stable() {
        // Changing these values orphans every cached title: raise CACHE_NAMESPACE instead.
        assert_eq!(
            profile_hash(&profile_from_layout(&layout(), 3000)),
            "10d572465a29a073"
        );
        let mut hdr = layout();
        hdr.video_variant = Some(CodecVariant {
            codec: VideoCodec::Hevc,
            bit_depth: 10,
            hdr: Some(HdrFormat::Hdr10),
        });
        hdr.use_ts = true;
        hdr.timeline_origin = 1.4;
        assert_eq!(
            profile_hash(&profile_from_layout(&hdr, 6000)),
            "a65baad0ad9840d1"
        );
    }

    #[test]
    fn every_input_forks_the_hash() {
        let base = profile_from_layout(&layout(), 3000);
        let h = profile_hash(&base);
        let mut seen = vec![h.clone()];
        let edits: &[fn(&mut PlaybackProfile)] = &[
            |p| p.video_codec = VideoCodec::Hevc,
            |p| p.video_bit_depth = 10,
            |p| p.hdr = Some(HdrFormat::Hlg),
            |p| p.audio_codec = "ac3".into(),
            |p| p.audio_channels = 6,
            |p| p.audio_copy = true,
            |p| p.audio_encoder_id = "libfdk_aac".into(),
            |p| p.mux = MuxFlavour::Ts,
            |p| p.var_stream_map = true,
            |p| p.segment_duration_ms = 6000,
            |p| p.tv_platform = "tizen".into(),
            |p| p.origin = 0.001,
            |p| p.format_start = 2.0,
            |p| p.source_version = Some("v2".into()),
            |p| p.dolby_vision = true,
            |p| p.dv_tonemap = true,
            |p| p.hdr10_plus_strip = true,
            |p| p.dovi_el_strip = true,
            |p| p.tonemap_curve = Some(TonemapCurve::Hable),
        ];
        for e in edits {
            let mut p = base.clone();
            e(&mut p);
            let h = profile_hash(&p);
            assert!(!seen.contains(&h), "{}", p.canonical());
            seen.push(h);
        }
    }

    #[test]
    fn a_group_ignores_the_picked_track_and_only_counts_hdr10_with_a_dv_tonemap() {
        let mut a = profile_from_layout(&layout(), 3000);
        a.var_stream_map = true;
        a.audio_track_modes = Some("ct6ac3".into());
        let mut b = a.clone();
        (b.audio_channels, b.audio_copy, b.audio_encoder_id) = (6, true, "copy".into());
        assert_eq!(profile_hash(&a), profile_hash(&b));
        b.audio_track_modes = Some("ct2aac".into());
        assert_ne!(profile_hash(&a), profile_hash(&b));
        let mut c = a.clone();
        c.dv_tonemap_hdr10 = true;
        assert_eq!(profile_hash(&a), profile_hash(&c));
    }

    #[test]
    fn negative_zero_hashes_like_zero() {
        let mut a = profile_from_layout(&layout(), 3000);
        let h = profile_hash(&a);
        a.origin = -0.0;
        assert_eq!(profile_hash(&a), h);
    }

    #[test]
    fn layout_rules() {
        let mut l = layout();
        l.tonemapping = true;
        l.source_dv_profile = Some(5);
        l.audio_stream_count = 3;
        l.audio_track_plans = Some(vec![
            LayoutAudio {
                codec: "aac".into(),
                channels: 2,
                copy: true,
                encoder_id: "copy".into(),
            },
            LayoutAudio {
                codec: "ac3".into(),
                channels: 6,
                copy: false,
                encoder_id: "ac3".into(),
            },
        ]);
        let p = profile_from_layout(&l, 3000);
        assert!(p.dv_tonemap && p.var_stream_map && p.tonemap_curve == Some(TonemapCurve::Mobius));
        assert_eq!(p.audio_track_modes.as_deref(), Some("ct6ac3"));
        assert_eq!(p.format_start, 0.0);
        l.source_dv_profile = Some(7);
        let p = profile_from_layout(&l, 3000);
        assert!(!p.dv_tonemap && p.dovi_el_strip);
    }
}

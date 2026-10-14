//! Keyframe scan of a whole source file. [ts:modules/streaming/services/{source-scan,video-packets}.ts]
use super::v;
use crate::api::{Keyframe, MediaInfo, SourceScan};
use crate::decide::source::is_mpeg_ts;
use crate::jsnum::{js_string, number, parse_float};
use std::collections::{BTreeMap, HashMap};

/// Slowest storage a whole-file scan still waits out; the packet read times out after
/// `max(SCAN_MIN_TIMEOUT_MS, size / SCAN_MIN_BYTES_PER_SECOND)`.
pub const SCAN_MIN_BYTES_PER_SECOND: f64 = 10.0 * 1024.0 * 1024.0;
pub const SCAN_MIN_TIMEOUT_MS: u64 = 300_000;
pub const REORDER_TIMEOUT_MS: u64 = 30_000;
/// Past the latest first audio packet, how far the ADTS read probes.
const ADTS_PROBE_MARGIN_SECONDS: f64 = 5.0;
const ADTS_MIN_FRAME_BYTES: usize = 7;
/// ffmpeg's backward discontinuity: a decode time 0.1 s behind the last one.
const CLOCK_BACKWARD_SECONDS: f64 = 0.1;
/// Longer than a reception dropout: a forward jump past it is a restarted clock.
const MAX_DROPOUT_SECONDS: f64 = 300.0;

pub fn scan_timeout_ms(size: f64) -> u64 {
    ((size / SCAN_MIN_BYTES_PER_SECOND) * 1000.0).max(SCAN_MIN_TIMEOUT_MS as f64) as u64
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoRef {
    pub stream_index: u32,
    pub reorder_frames: Option<u32>,
    pub avg_frame_rate: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AacTrack {
    pub stream_index: u32,
    pub start_time_seconds: Option<f64>,
}

/// What one scan reads: the first video stream and, on MPEG-TS, every AAC track.
#[derive(Debug, Clone, PartialEq)]
pub struct ScanPlan {
    pub video: VideoRef,
    pub mpeg_ts: bool,
    pub format_start: f64,
    pub aac: Vec<AacTrack>,
}

impl ScanPlan {
    pub fn new(info: &MediaInfo, file_path: &str) -> Result<Self, String> {
        let v = info.video.first().ok_or("no video stream to scan")?;
        let mpeg_ts = is_mpeg_ts(info, file_path);
        let aac = if mpeg_ts {
            info.audio
                .iter()
                .filter(|a| a.codec == "aac")
                .map(|a| AacTrack {
                    stream_index: a.stream_index,
                    start_time_seconds: a.start_time_seconds,
                })
                .collect()
        } else {
            vec![]
        };
        Ok(Self {
            video: VideoRef {
                stream_index: v.stream_index,
                reorder_frames: v.reorder_frames,
                avg_frame_rate: v.avg_frame_rate.clone(),
            },
            mpeg_ts,
            format_start: info.format_start_seconds.unwrap_or(0.0),
            aac,
        })
    }

    /// With AAC tracks the file's bytes go to both ffprobe (stdin) and ffmpeg; else ffprobe opens it.
    pub fn piped(&self) -> bool {
        !self.aac.is_empty()
    }
}

/// ffprobe's `key=value|...` compact line; a field without `=` keeps its text as value and loses its last char as key.
fn fields(line: &str) -> HashMap<String, String> {
    line.split('|')
        .map(|kv| match kv.find('=') {
            Some(i) => (kv[..i].to_string(), kv[i + 1..].to_string()),
            None => {
                let mut k = kv.to_string();
                k.pop();
                (k, kv.to_string())
            }
        })
        .collect()
}

fn nan(s: Option<&String>) -> f64 {
    s.and_then(|s| number(s)).unwrap_or(f64::NAN)
}

/// The reorder lead ffmpeg starts Matroska's derived decode times with, truncated to whole microseconds.
pub fn lead_seconds(reorder_frames: f64, avg_frame_rate: Option<&str>) -> f64 {
    let mut parts = avg_frame_rate.unwrap_or("0/0").split('/');
    let mut next = || {
        parts
            .next()
            .map_or(f64::NAN, |p| number(p).unwrap_or(f64::NAN))
    };
    let (n, d) = (next(), next());
    if n > 0.0 && d > 0.0 {
        (reorder_frames * 1e6 * d / n).trunc() / 1e6
    } else {
        0.0
    }
}

/// The lead when the probe already knows it; else read it from the file with `reorder_args`.
pub fn known_lead(v: &VideoRef) -> Option<f64> {
    match (v.reorder_frames, v.avg_frame_rate.as_deref()) {
        (Some(r), Some(a)) if !a.is_empty() => Some(lead_seconds(r as f64, Some(a))),
        _ => None,
    }
}

/// The lead from the first line `reorder_args` prints (0 when it prints none).
pub fn lead_from_probe(line: &str) -> f64 {
    let f = fields(line);
    let b = nan(f.get("has_b_frames"));
    lead_seconds(
        if b.is_nan() { 0.0 } else { b },
        f.get("avg_frame_rate").map(String::as_str),
    )
}

pub fn reorder_args(video: &VideoRef, file: &str) -> Vec<String> {
    let mut a = v(&["-v", "error", "-select_streams"]);
    a.push(video.stream_index.to_string());
    a.extend(v(&[
        "-show_entries",
        "stream=has_b_frames,avg_frame_rate",
        "-of",
        "compact=p=0",
    ]));
    a.push(file.into());
    a
}

/// Every video packet, none decoded; `file` is read from stdin (`pipe:0`) when the plan is piped.
pub fn packet_args(plan: &ScanPlan, file: &str) -> Vec<String> {
    let mut a = v(&["-v", "error", "-select_streams"]);
    a.push(plan.video.stream_index.to_string());
    a.extend(v(&[
        "-show_entries",
        "packet=pts_time,dts_time,duration_time,flags",
        "-of",
        "compact=p=0",
    ]));
    a.push(if plan.piped() { "pipe:0" } else { file }.into());
    a
}

/// ffmpeg copying each AAC track as raw ADTS on `pipe:3+i` from the file on stdin. The demuxer
/// needs every track's parameters first: probe past the latest first packet.
pub fn adts_args(plan: &ScanPlan, size: u64) -> Vec<String> {
    let reach = plan
        .aac
        .iter()
        .map(|a| a.start_time_seconds.unwrap_or(plan.format_start) - plan.format_start)
        .fold(f64::NEG_INFINITY, f64::max);
    let analyze = ((reach.max(0.0) + ADTS_PROBE_MARGIN_SECONDS) * 1e6).ceil();
    let mut a = v(&["-v", "error", "-analyzeduration"]);
    a.push(js_string(analyze));
    a.extend(v(&["-probesize"]));
    a.push(size.to_string());
    a.extend(v(&["-f", "mpegts", "-i", "pipe:0"]));
    for (i, t) in plan.aac.iter().enumerate() {
        a.extend(v(&["-map"]));
        a.push(format!("0:{}", t.stream_index));
        a.extend(v(&["-c", "copy", "-f", "data"]));
        a.push(format!("pipe:{}", 3 + i));
    }
    a
}

/// Video packets in decode order, as a copy gets them: decode times Matroska leaves unknown
/// start `lead` under the first pts, as ffmpeg's do.
#[derive(Debug, Clone)]
pub struct PacketReader {
    lead: f64,
    detect_break: bool,
    keyframes: Vec<Keyframe>,
    end: f64,
    next: Option<f64>,
    last: Option<(f64, f64)>,
    break_seconds: Option<f64>,
}

impl PacketReader {
    /// `detect_break` on MPEG-TS only.
    pub fn new(lead: f64, detect_break: bool) -> Self {
        Self {
            lead,
            detect_break,
            keyframes: vec![],
            end: f64::NEG_INFINITY,
            next: None,
            last: None,
            break_seconds: None,
        }
    }

    /// False once the clock broke: nothing after it belongs to the timeline.
    pub fn add(&mut self, pts: f64, dts: f64, dur: f64, key: bool) -> bool {
        if self.break_seconds.is_some() {
            return false;
        }
        // A packet the demuxer gives no time to has none a copy could cut on.
        if !pts.is_finite() {
            return true;
        }
        let decode = if dts.is_finite() {
            dts
        } else {
            self.next.unwrap_or(pts - self.lead)
        };
        if let (true, Some((ld, lu))) = (self.detect_break, self.last) {
            let expected = ld + lu;
            if decode < ld - CLOCK_BACKWARD_SECONDS || decode - expected > MAX_DROPOUT_SECONDS {
                self.break_seconds = Some(self.end);
                return false;
            }
        }
        self.next = Some(decode + dur);
        self.last = Some((decode, dur));
        self.end = self.end.max(pts + dur);
        if key {
            self.keyframes.push(Keyframe { pts, dts: decode });
        }
        true
    }

    /// One compact packet line; false to stop reading.
    pub fn feed_line(&mut self, line: &str) -> bool {
        if line.is_empty() {
            return true;
        }
        let f = fields(line);
        let time = |k: &str| f.get(k).and_then(|s| parse_float(s)).unwrap_or(f64::NAN);
        let dur = time("duration_time");
        let key = f.get("flags").is_some_and(|s| s.contains('K'));
        self.add(
            time("pts_time"),
            time("dts_time"),
            if dur.is_nan() { 0.0 } else { dur },
            key,
        )
    }

    /// The scan once every line was fed (or the clock broke). Audio config changes are by stream index.
    pub fn finish(self, audio_config_changes: BTreeMap<u32, bool>) -> Result<SourceScan, String> {
        if !self.end.is_finite() {
            return Err("no video packet with a time".into());
        }
        Ok(SourceScan {
            keyframes: self.keyframes,
            end: self.end,
            break_seconds: self.break_seconds,
            audio_config_changes,
            schema_version: None,
        })
    }
}

/// Follows ADTS frames across chunks, noting a header whose configuration differs from the first one's.
#[derive(Debug, Clone, Default)]
pub struct AdtsConfigWalker {
    pub changed: bool,
    config: Option<u32>,
    carry: Vec<u8>,
}

impl AdtsConfigWalker {
    pub fn push(&mut self, chunk: &[u8]) {
        let b: Vec<u8> = [self.carry.as_slice(), chunk].concat();
        let mut i = 0;
        while i + ADTS_MIN_FRAME_BYTES <= b.len() {
            // Syncword, then layer 0.
            if b[i] != 0xff || (b[i + 1] & 0xf6) != 0xf0 {
                i += 1;
                continue;
            }
            let length = (usize::from(b[i + 3] & 0x03) << 11)
                | (usize::from(b[i + 4]) << 3)
                | usize::from(b[i + 5] >> 5);
            if length < ADTS_MIN_FRAME_BYTES {
                i += 1;
                continue;
            }
            if i + length > b.len() {
                break;
            }
            // Profile and sampling index with the private bit masked, and the channel configuration.
            let config = (u32::from(b[i + 2] & 0xfd) << 8) | u32::from(b[i + 3] & 0xc0);
            match self.config {
                None => self.config = Some(config),
                Some(c) if c != config => self.changed = true,
                _ => {}
            }
            i += length;
        }
        self.carry = b[i..].to_vec();
    }
}

/// `audioConfigChanges`: a track whose headers could not be read (`ok` false: ffmpeg failed to
/// start or exited non-zero) is reported as possibly changing.
pub fn audio_config_changes(
    aac: &[AacTrack],
    walkers: &[AdtsConfigWalker],
    ok: bool,
) -> BTreeMap<u32, bool> {
    aac.iter()
        .zip(walkers)
        .map(|(t, w)| (t.stream_index, !ok || w.changed))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_scales_with_size_past_the_floor() {
        assert_eq!(scan_timeout_ms(1e6), 300_000);
        assert_eq!(
            scan_timeout_ms(100.0 * 1024.0 * 1024.0 * 1024.0),
            10_240_000
        );
    }

    #[test]
    fn a_stream_with_no_timed_packet_has_no_scan() {
        let mut r = PacketReader::new(0.0, false);
        r.feed_line("pts_time=N/A|dts_time=N/A|duration_time=N/A|flags=K__");
        assert!(r.finish(Default::default()).is_err());
    }

    #[test]
    fn a_failed_adts_read_flags_every_track() {
        let aac = [AacTrack {
            stream_index: 1,
            start_time_seconds: None,
        }];
        let w = [AdtsConfigWalker::default()];
        assert!(audio_config_changes(&aac, &w, false)[&1]);
        assert!(!audio_config_changes(&aac, &w, true)[&1]);
    }
}

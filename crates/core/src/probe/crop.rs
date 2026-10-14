//! Letterbox detection from ffmpeg `cropdetect` samples (`detectCrop`).
use crate::api::CropInfo;

/// Positions of the samples, as fractions of the duration.
const FRACTIONS: [f64; 6] = [0.05, 0.15, 0.3, 0.45, 0.6, 0.8];

/// A file without a known duration is sampled as if it lasted ten minutes.
pub fn timestamps(duration_seconds: Option<f64>) -> Vec<u32> {
    let dur = duration_seconds.unwrap_or(600.0);
    FRACTIONS.iter().map(|f| (dur * f).floor() as u32).collect()
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub w: u32,
    pub h: u32,
    pub x: u32,
    pub y: u32,
}

/// `crop=W:H:X:Y` of the last line of ffmpeg's log that has one.
pub fn parse_sample(stderr: &str) -> Option<Sample> {
    stderr.split('\n').rev().find_map(|line| {
        line.match_indices("crop=").find_map(|(i, _)| {
            let mut nums = [0u32; 4];
            let mut rest = &line[i + 5..];
            for (k, n) in nums.iter_mut().enumerate() {
                let len = rest.bytes().take_while(u8::is_ascii_digit).count();
                *n = rest[..len].parse().ok()?;
                rest = &rest[len..];
                if k < 3 {
                    rest = rest.strip_prefix(':')?;
                }
            }
            Some(Sample {
                w: nums[0],
                h: nums[1],
                x: nums[2],
                y: nums[3],
            })
        })
    })
}

/// The loosest box over the samples (the union of content seen), kept only when it removes at
/// least 40 px on one axis. `width`/`height` are the displayed size of the stream.
pub fn aggregate(
    samples: &[Option<Sample>],
    width: Option<u32>,
    height: Option<u32>,
) -> Option<CropInfo> {
    let mut best: Option<Sample> = None;
    for s in samples.iter().flatten() {
        if best.is_none_or(|b| u64::from(s.w) * u64::from(s.h) > u64::from(b.w) * u64::from(b.h)) {
            best = Some(*s);
        }
    }
    let Sample { w, h, x, y } = best?;
    let removed = |full: Option<u32>, kept: u32, offset: u32| match full {
        Some(f) if f != 0 => i64::from(f) - i64::from(kept),
        _ => i64::from(offset) * 2,
    };
    if removed(height, h, y) < 40 && removed(width, w, x) < 40 {
        return None;
    }
    Some(CropInfo {
        width: w,
        height: h,
        x,
        y,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_is_the_last_crop_line() {
        let log = "[Parsed_cropdetect_1 @ 0x1] x1:0 crop=480:192:0:40\nframe=1\n[Parsed_cropdetect_1 @ 0x1] crop=480:190:0:40\nsize=N/A";
        assert_eq!(
            parse_sample(log),
            Some(Sample {
                w: 480,
                h: 190,
                x: 0,
                y: 40
            })
        );
        assert_eq!(parse_sample("crop=480:x"), None);
    }

    #[test]
    fn small_bars_are_ignored() {
        let s = Some(Sample {
            w: 480,
            h: 254,
            x: 0,
            y: 8,
        });
        assert_eq!(aggregate(&[s], Some(480), Some(270)), None);
        assert_eq!(aggregate(&[s], None, None), None);
        let big = Some(Sample {
            w: 480,
            h: 190,
            x: 0,
            y: 40,
        });
        assert!(aggregate(&[None, big], Some(480), Some(270)).is_some());
        // Largest area wins, the first on a tie.
        assert_eq!(
            aggregate(&[big, s], Some(480), Some(400)).unwrap().height,
            254
        );
    }
}

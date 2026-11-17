//! Decodes the audio of a file to raw s16le PCM. Spec: `docs/spec/jobs.md` section 10b.
use super::merge::{absolute, plain};
use crate::strs;

pub const DEFAULT_RATE: u32 = 24_000;
pub const DEFAULT_CHANNELS: u32 = 1;

pub fn validate(output: &str, rate: Option<u32>, channels: Option<u32>) -> Result<(), String> {
    if !plain(output) || !absolute(output) {
        return Err("Pcm output must be an absolute path without control characters".into());
    }
    if !(8000..=48000).contains(&rate.unwrap_or(DEFAULT_RATE)) {
        return Err("Pcm rate must be 8000 to 48000".into());
    }
    if !(1..=2).contains(&channels.unwrap_or(DEFAULT_CHANNELS)) {
        return Err("Pcm channels must be 1 or 2".into());
    }
    Ok(())
}

pub fn decode_args(input: &str, rate: u32, channels: u32, output: &str) -> Vec<String> {
    let mut a = strs(&["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"]);
    a.push(input.into());
    a.extend(strs(&["-map", "0:a:0", "-vn", "-sn", "-dn", "-ac"]));
    a.push(channels.to_string());
    a.push("-ar".into());
    a.push(rate.to_string());
    a.extend(strs(&["-f", "s16le"]));
    a.push(output.into());
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_path_rate_and_channels() {
        assert!(validate("/a/b.pcm", None, None).is_ok());
        assert!(validate("/a/b.pcm", Some(8000), Some(2)).is_ok());
        assert!(validate("/a/b.pcm", Some(48000), Some(1)).is_ok());
        for (out, rate, ch) in [
            ("b.pcm", None, None),
            ("/a/b\n.pcm", None, None),
            ("/a/b.pcm", Some(7999), None),
            ("/a/b.pcm", Some(48001), None),
            ("/a/b.pcm", None, Some(0)),
            ("/a/b.pcm", None, Some(3)),
        ] {
            assert!(validate(out, rate, ch).is_err(), "{out} {rate:?} {ch:?}");
        }
    }

    #[test]
    fn decodes_the_first_audio_stream_to_s16le() {
        let a = decode_args("/in.flac", 24000, 1, "/out.tmp").join(" ");
        assert!(a.ends_with("-ac 1 -ar 24000 -f s16le /out.tmp"), "{a}");
        assert!(a.contains("-map 0:a:0 -vn"));
    }
}

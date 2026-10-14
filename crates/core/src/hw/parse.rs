//! Readers of ffmpeg's text output.

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `\blibfdk_aac\b` over `ffmpeg -encoders`. [ts:backend/src/modules/streaming/transcoding/audio-encoder-probe.ts:31]
pub fn has_libfdk_aac(listing: &str) -> bool {
    const W: &str = "libfdk_aac";
    listing.match_indices(W).any(|(i, _)| {
        !listing[..i].chars().next_back().is_some_and(is_word)
            && !listing[i + W.len()..].chars().next().is_some_and(is_word)
    })
}

/// `/VAAPI driver:\s*Intel iHD/i` over the verbose device-init log.
/// [ts:backend/src/modules/streaming/transcoding/codec/encoder-probe.ts:70]
pub fn vaapi_driver_is_intel_ihd(log: &str) -> bool {
    let low = log.to_lowercase();
    low.match_indices("vaapi driver:")
        .any(|(i, m)| low[i + m.len()..].trim_start().starts_with("intel ihd"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libfdk_needs_a_word_boundary() {
        assert!(has_libfdk_aac(" A....D libfdk_aac   Fraunhofer FDK AAC\n"));
        assert!(!has_libfdk_aac(" A....D libfdk_aac_extra  x\n"));
        assert!(!has_libfdk_aac(" A....D xlibfdk_aac  x\n"));
        assert!(has_libfdk_aac("a libfdk_aac_extra libfdk_aac."));
    }

    #[test]
    fn ihd_is_case_insensitive_and_skips_whitespace() {
        assert!(vaapi_driver_is_intel_ihd(
            "[x] VAAPI driver: Intel iHD driver for ..."
        ));
        assert!(vaapi_driver_is_intel_ihd("vaapi driver:\n  intel IHD"));
        assert!(!vaapi_driver_is_intel_ihd(
            "VAAPI driver: Mesa Gallium driver"
        ));
        assert!(!vaapi_driver_is_intel_ihd(
            "VAAPI driver: Intel i965 driver"
        ));
    }
}

//! Session map keys and variants. [ts:transcoding/session-key.ts] [ts:transcoding/variant.ts]
use serde::{Deserialize, Serialize};

/// `<media>-u<owner>-<cacheKey>`, `anon` without an owner.
pub fn session_key(media_file_id: u32, owner_id: Option<&str>, cache_key: &str) -> String {
    match owner_id {
        Some(o) => format!("{media_file_id}-u{o}-{cache_key}"),
        None => format!("{media_file_id}-anon-{cache_key}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub enum SessionVariant {
    Main,
    /// Companion producing seg-0/seg-1 while the main run seeks.
    Early,
    /// DirectStream; `audio_index` is `None` when every rendition is published.
    Remux {
        audio_index: Option<u32>,
        keyframe_grid: bool,
    },
}

/// A multi-audio source never forks on the picked track; a single-audio one defaults to 0.
pub fn remux_variant(
    audio_index: Option<u32>,
    keyframe_grid: bool,
    multi_audio: bool,
) -> SessionVariant {
    SessionVariant::Remux {
        audio_index: if multi_audio {
            None
        } else {
            Some(audio_index.unwrap_or(0))
        },
        keyframe_grid,
    }
}

pub fn variant_suffix(v: &SessionVariant) -> String {
    match v {
        SessionVariant::Main => String::new(),
        SessionVariant::Early => "-early".into(),
        SessionVariant::Remux {
            audio_index,
            keyframe_grid,
        } => format!(
            "-remux{}{}",
            audio_index.map(|a| format!("-a{a}")).unwrap_or_default(),
            if *keyframe_grid { "" } else { "-u" }
        ),
    }
}

pub fn variant_hash(base: &str, v: &SessionVariant) -> String {
    format!("{base}{}", variant_suffix(v))
}

/// Whether `s` is exactly `-early` or `-remux[-a<digits>][-u]`.
fn is_variant_suffix(s: &str) -> bool {
    if s == "-early" {
        return true;
    }
    let Some(mut rest) = s.strip_prefix("-remux") else {
        return false;
    };
    if let Some(r) = rest.strip_prefix("-a") {
        let digits = r.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return false;
        }
        rest = &r[digits..];
    }
    rest.is_empty() || rest == "-u"
}

/// Strips the leftmost trailing variant suffix: the base hash the live-session registry tracks.
pub fn base_profile_hash(cache_key: &str) -> &str {
    cache_key
        .match_indices('-')
        .find(|(i, _)| is_variant_suffix(&cache_key[*i..]))
        .map_or(cache_key, |(i, _)| &cache_key[..i])
}

/// A concurrent playback of the same content forks onto its own job.
pub fn with_instance_suffix(base: &str, instance_suffix: Option<&str>) -> String {
    format!("{base}{}", instance_suffix.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(session_key(7, Some("3"), "abc"), "7-u3-abc");
        assert_eq!(session_key(7, None, "abc"), "7-anon-abc");
    }

    #[test]
    fn suffixes_round_trip() {
        for v in [
            SessionVariant::Main,
            SessionVariant::Early,
            remux_variant(Some(2), true, false),
            remux_variant(None, false, false),
            remux_variant(Some(2), false, true),
        ] {
            assert_eq!(base_profile_hash(&variant_hash("0123abcd", &v)), "0123abcd");
        }
        assert_eq!(
            variant_suffix(&remux_variant(None, false, false)),
            "-remux-a0-u"
        );
        assert_eq!(
            variant_suffix(&remux_variant(Some(5), true, true)),
            "-remux"
        );
    }

    #[test]
    fn base_hash_only_strips_a_whole_suffix() {
        assert_eq!(base_profile_hash("h-remux-a-u"), "h-remux-a-u");
        assert_eq!(base_profile_hash("h-remux-a1-x"), "h-remux-a1-x");
        assert_eq!(base_profile_hash("h-early-early"), "h-early");
        assert_eq!(base_profile_hash("h~i12ab-early"), "h~i12ab");
    }
}

//! BCP-47 language tags from `mkvmerge -J` (`mkv-language-tags.ts`).
use crate::api::MediaInfo;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, PartialEq, Clone, Copy)]
enum Kind {
    Audio,
    Subtitle,
}

/// `^[a-z]{2,3}(-[a-z0-9]{2,8})+$` (case-insensitive): a language with a region, script or variant.
fn qualified(tag: &str) -> bool {
    let mut parts = tag.split('-');
    let lang_ok = parts
        .next()
        .is_some_and(|l| (2..=3).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_alphabetic()));
    let rest: Vec<&str> = parts.collect();
    lang_ok
        && !rest.is_empty()
        && rest
            .iter()
            .all(|p| (2..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// Track id (= ffprobe stream index) to its tag, for audio and subtitle tracks only.
fn parse(json: &str) -> BTreeMap<u32, (Kind, String)> {
    let mut out = BTreeMap::new();
    let Ok(Value::Object(root)) = serde_json::from_str::<Value>(json) else {
        return out;
    };
    let Some(Value::Array(tracks)) = root.get("tracks") else {
        return out;
    };
    for t in tracks {
        let kind = match t.get("type").and_then(Value::as_str) {
            Some("audio") => Kind::Audio,
            Some("subtitles") => Kind::Subtitle,
            _ => continue,
        };
        let id = t.get("id").and_then(Value::as_f64);
        let tag = t
            .pointer("/properties/language_ietf")
            .and_then(Value::as_str);
        let (Some(id), Some(tag)) = (id, tag) else {
            continue;
        };
        if !qualified(tag) || tag.to_ascii_lowercase().starts_with("und-") {
            continue;
        }
        out.insert(id as u32, (kind, tag.to_string()));
    }
    out
}

/// Whether the probe reads tags for this container (`/matroska/` on the format name).
pub fn is_matroska(format_name: Option<&str>) -> bool {
    format_name.unwrap_or("").contains("matroska")
}

/// Sets `language_tag` on the streams whose mkvmerge track has the same id and type; returns
/// the number of tags found (the host keeps a non-empty result of a failed run).
pub fn apply(info: &mut MediaInfo, mkvmerge_json: &str) -> usize {
    let tags = parse(mkvmerge_json);
    for a in &mut info.audio {
        if let Some((Kind::Audio, t)) = tags.get(&a.stream_index) {
            a.language_tag = Some(t.clone());
        }
    }
    for s in &mut info.subtitles {
        if let Some((Kind::Subtitle, t)) = tags.get(&s.stream_index) {
            s.language_tag = Some(t.clone());
        }
    }
    tags.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_tags() {
        for ok in ["es-419", "pt-BR", "zh-Hant", "de-DE-1996", "fil-PH"] {
            assert!(qualified(ok), "{ok}");
        }
        for no in [
            "en",
            "eng",
            "und",
            "e-US",
            "es-4",
            "es-123456789",
            "es-",
            "engl-US",
        ] {
            assert!(!qualified(no), "{no}");
        }
    }
}

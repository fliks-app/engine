//! Subtitle OCR argv, language resolution and validation. Spec: `docs/spec/subtitle-jobs.md` section 2.
use super::v;
use crate::api::{MediaInfo, SubtitleStreamInfo};

pub const EXTRACT_TIMEOUT_MS: u64 = 900_000;
pub const OCR_TOOL_TIMEOUT_MS: u64 = 1_800_000;
pub const MAX_STREAM_INDEX: u32 = 999;

const OCR_CODECS: &[&str] = &["hdmv_pgs_subtitle", "dvd_subtitle"];

fn two_letter(s: &str) -> Option<String> {
    let t = s.trim();
    (t.len() == 2 && t.bytes().all(|b| b.is_ascii_alphabetic())).then(|| t.to_ascii_lowercase())
}

/// Request language, else the track tag, else English (PGS/VobSub tracks are often `und`).
pub fn resolved_language(request: Option<&str>, track_language: &str) -> String {
    two_letter(request.unwrap_or(""))
        .or_else(|| two_letter(track_language))
        .unwrap_or_else(|| "en".into())
}

pub fn tesseract_pack(ietf: &str) -> &'static str {
    match ietf {
        "en" => "eng",
        "fr" => "fra",
        "es" => "spa",
        "de" => "deu",
        "it" => "ita",
        "pt" => "por",
        "nl" => "nld",
        "sv" => "swe",
        "da" => "dan",
        "no" => "nor",
        "fi" => "fin",
        "pl" => "pol",
        "cs" => "ces",
        "sk" => "slk",
        "hu" => "hun",
        "ro" => "ron",
        "el" => "ell",
        "ru" => "rus",
        "uk" => "ukr",
        "bg" => "bul",
        "sr" => "srp",
        "hr" => "hrv",
        "tr" => "tur",
        "ar" => "ara",
        "he" => "heb",
        "fa" => "fas",
        "hi" => "hin",
        "th" => "tha",
        "vi" => "vie",
        "id" => "ind",
        "ja" => "jpn",
        "ko" => "kor",
        "zh" => "chi_sim",
        _ => "eng",
    }
}

pub fn validate_ocr<'a>(
    info: &'a MediaInfo,
    stream_index: u32,
    language: Option<&str>,
) -> Result<&'a SubtitleStreamInfo, String> {
    if stream_index > MAX_STREAM_INDEX {
        return Err(format!("Invalid stream index: above {MAX_STREAM_INDEX}"));
    }
    if let Some(lang) = language.filter(|s| !s.is_empty())
        && two_letter(lang).is_none()
    {
        return Err("SubtitleOcr language must be two ASCII letters".into());
    }
    let track = info
        .subtitles
        .iter()
        .find(|s| s.stream_index == stream_index)
        .ok_or_else(|| "SubtitleOcr needs stream index in info.subtitles".to_string())?;
    if !track.is_image_based {
        return Err("SubtitleOcr needs an image-based subtitle track".into());
    }
    if !OCR_CODECS.contains(&track.codec.as_str()) {
        return Err(format!(
            "OCR is not available for \"{}\" subtitles",
            track.codec
        ));
    }
    Ok(track)
}

fn matroska_ext(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    let ext = base.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(ext.as_str(), "mkv" | "mka" | "webm")
}

pub fn require_matroska_vobsub(path: &str) -> Result<(), String> {
    if matroska_ext(path) {
        Ok(())
    } else {
        Err("VobSub OCR requires a Matroska (.mkv) source".into())
    }
}

pub fn pgs_sup_path(base: &str, ietf: &str) -> String {
    format!("{base}.{ietf}.sup")
}

pub fn pgs_extract_argv(path: &str, stream_index: u32, sup: &str) -> Vec<String> {
    let mut a = v(&["-y", "-i"]);
    a.push(path.into());
    a.push("-map".into());
    a.push(format!("0:{stream_index}"));
    a.extend(v(&["-c:s", "copy"]));
    a.push(sup.into());
    a
}

pub fn pgsrip_argv(ietf: &str, sup: &str) -> Vec<String> {
    v(&["--language", ietf, sup])
}

pub fn mkvextract_argv(path: &str, stream_index: u32, idx: &str) -> Vec<String> {
    let mut a = v(&["tracks"]);
    a.push(path.into());
    a.push(format!("{stream_index}:{idx}"));
    a
}

pub fn subtile_ocr_argv(pack: &str, base: &str) -> Vec<String> {
    let mut a = v(&["-l", pack, "-o"]);
    a.push(format!("{base}.srt"));
    a.push(format!("{base}.idx"));
    a
}

pub fn pgs_missing_srt_error(ietf: &str, stderr: &str, stdout: &str) -> String {
    let raw = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    let tail: String = raw.chars().take(300).collect();
    format!("pgsrip produced no SRT for language \"{ietf}\" — {tail}")
}

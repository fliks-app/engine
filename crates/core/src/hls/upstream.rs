//! A live upstream's multivariant playlist: the one variant (and audio rendition) a run reads.
//! Spec: `docs/spec/live.md` (packaged attempt). [new]

/// The media playlists a run reads: a variant and, when its audio is a separate rendition, it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsPick {
    pub video: String,
    pub audio: Option<String>,
}

struct Variant {
    uri: String,
    height: Option<u32>,
    bandwidth: Option<u64>,
    codecs: Option<String>,
    audio: Option<String>,
    range: Option<String>,
}

struct Rendition {
    group: String,
    uri: Option<String>,
    default: bool,
    autoselect: bool,
}

const AUDIO_CODECS: [&str; 13] = [
    "mp4a", "ac-3", "ec-3", "ec+3", "ac-4", "dts", "mha1", "mhm1", "opus", "flac", "fLaC", "alac",
    "mp3",
];

/// The variant a run reads (rules in `live.md`, packaged attempt) from the multivariant playlist
/// `text` answered at `base`; `None` for a media playlist or one without a variant holding video.
pub fn pick_variant(text: &str, base: &str, remux: bool, height: Option<u32>) -> Option<HlsPick> {
    let (variants, renditions) = parse(text, base);
    let video: Vec<&Variant> = variants.iter().filter(|v| has_video(v)).collect();
    // The live transcode has no tonemap, and a remux copies: SDR when there is one.
    let mut pool = prefer(video, |v| v.range.as_deref().is_none_or(|r| r == "SDR"));
    if remux {
        pool = prefer(pool, is_h264);
    }
    let up = |v: &&Variant| (v.height.unwrap_or(0), aac(v), v.bandwidth.unwrap_or(0));
    let down = |v: &&Variant| (v.height.unwrap_or(0), !aac(v), v.bandwidth.unwrap_or(0));
    let covering = height.filter(|_| !remux).and_then(|h| {
        pool.iter()
            .copied()
            .filter(|v| v.height.is_some_and(|vh| vh >= h))
            .min_by_key(down)
    });
    let chosen = match covering {
        Some(v) => v,
        None => pool.iter().copied().rev().max_by_key(up)?,
    };
    let audio = chosen.audio.as_ref().and_then(|g| {
        let group: Vec<&Rendition> = renditions.iter().filter(|r| &r.group == g).collect();
        let r = group
            .iter()
            .find(|r| r.default)
            .or_else(|| group.iter().find(|r| r.autoselect))
            .or_else(|| group.first())?;
        r.uri.clone()
    });
    Some(HlsPick {
        video: chosen.uri.clone(),
        audio,
    })
}

/// The variants of `pool` that `keep` holds, or all of them when it holds none.
fn prefer(pool: Vec<&Variant>, keep: impl Fn(&Variant) -> bool) -> Vec<&Variant> {
    let kept: Vec<_> = pool.iter().copied().filter(|v| keep(v)).collect();
    if kept.is_empty() { pool } else { kept }
}

fn codecs(v: &Variant) -> impl Iterator<Item = &str> {
    v.codecs.iter().flat_map(|c| c.split(',')).map(str::trim)
}

fn is_audio(codec: &str) -> bool {
    AUDIO_CODECS.iter().any(|a| codec.starts_with(a))
}

fn has_video(v: &Variant) -> bool {
    v.codecs.is_none() || codecs(v).any(|c| !is_audio(c))
}

/// Its audio, if any is listed, is AAC: copied into TS, where another codec is encoded.
fn aac(v: &Variant) -> bool {
    codecs(v)
        .filter(|c| is_audio(c))
        .all(|c| c.starts_with("mp4a"))
}

fn is_h264(v: &Variant) -> bool {
    codecs(v).any(|c| c.starts_with("avc1") || c.starts_with("avc3"))
}

/// Only http and https: a playlist could name any URL ffmpeg opens, `file:` included.
fn http(uri: String) -> Option<String> {
    let lower = uri.to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://")).then_some(uri)
}

fn parse(text: &str, base: &str) -> (Vec<Variant>, Vec<Rendition>) {
    let (mut variants, mut renditions) = (vec![], vec![]);
    let mut pending: Option<Vec<(String, String)>> = None;
    for line in text.lines().map(str::trim) {
        if let Some(a) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(attributes(a));
        } else if let Some(a) = line.strip_prefix("#EXT-X-MEDIA:") {
            let a = attributes(a);
            let get = |k: &str| a.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
            if get("TYPE").as_deref() == Some("AUDIO") {
                let uri = get("URI").map(|u| http(resolve_uri(base, &u)));
                if uri == Some(None) {
                    continue;
                }
                renditions.push(Rendition {
                    group: get("GROUP-ID").unwrap_or_default(),
                    uri: uri.flatten(),
                    default: get("DEFAULT").as_deref() == Some("YES"),
                    autoselect: get("AUTOSELECT").as_deref() == Some("YES"),
                });
            }
        } else if !line.is_empty()
            && !line.starts_with('#')
            && let Some(a) = pending.take()
        {
            let get = |k: &str| a.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
            let Some(uri) = http(resolve_uri(base, line)) else {
                continue;
            };
            variants.push(Variant {
                uri,
                height: get("RESOLUTION")
                    .and_then(|r| r.split_once('x').and_then(|(_, h)| h.parse().ok())),
                bandwidth: get("BANDWIDTH").and_then(|b| b.parse().ok()),
                codecs: get("CODECS"),
                audio: get("AUDIO"),
                range: get("VIDEO-RANGE"),
            });
        }
    }
    (variants, renditions)
}

/// `NAME=value` pairs of an attribute list, quotes removed; a quoted value may hold commas.
fn attributes(list: &str) -> Vec<(String, String)> {
    let (mut out, mut rest) = (vec![], list);
    while let Some((name, after)) = rest.split_once('=') {
        let (value, next) = match after.strip_prefix('"') {
            Some(q) => match q.split_once('"') {
                Some((v, n)) => (v, n.strip_prefix(',').unwrap_or(n)),
                None => (q, ""),
            },
            None => after.split_once(',').unwrap_or((after, "")),
        };
        out.push((name.trim().to_string(), value.trim().to_string()));
        rest = next;
    }
    out
}

/// `reference` resolved against `base` (RFC 3986 section 5.2).
pub fn resolve_uri(base: &str, reference: &str) -> String {
    let scheme_end = |s: &str| {
        s.find(':')
            .filter(|i| {
                s[..*i]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
            })
            .filter(|i| *i > 0 && !s[..*i].contains('/'))
    };
    if scheme_end(reference).is_some() {
        return reference.to_string();
    }
    let Some(colon) = scheme_end(base) else {
        return reference.to_string();
    };
    let scheme = &base[..colon];
    let rest = &base[colon + 1..];
    if let Some(r) = reference.strip_prefix("//") {
        return format!("{scheme}://{r}");
    }
    let (authority, path_query) = match rest.strip_prefix("//") {
        Some(r) => r.split_at(r.find(['/', '?', '#']).unwrap_or(r.len())),
        None => ("", rest),
    };
    let path = path_query.split(['?', '#']).next().unwrap_or("");
    let root = if rest.starts_with("//") {
        format!("{scheme}://{authority}")
    } else {
        format!("{scheme}:")
    };
    if reference.starts_with('?') {
        return format!("{root}{path}{reference}");
    }
    let (ref_path, suffix) =
        reference.split_at(reference.find(['?', '#']).unwrap_or(reference.len()));
    let merged = if ref_path.starts_with('/') {
        ref_path.to_string()
    } else if path.is_empty() && rest.starts_with("//") {
        format!("/{ref_path}")
    } else {
        format!(
            "{}{ref_path}",
            &path[..path.rfind('/').map_or(0, |i| i + 1)]
        )
    };
    let mut out: Vec<&str> = vec![];
    let segments: Vec<&str> = merged.split('/').collect();
    for (i, s) in segments.iter().enumerate() {
        match *s {
            "." => {}
            ".." => {
                if out.len() > 1 {
                    out.pop();
                }
            }
            s => out.push(s),
        }
        if matches!(*s, "." | "..") && i == segments.len() - 1 {
            out.push("");
        }
    }
    format!("{root}{}{suffix}", out.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://cdn.example/live/ch1/master.m3u8?token=a";
    const MASTER: &str = "#EXTM3U\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"en\",AUTOSELECT=YES,URI=\"audio/en.m3u8\"\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"fr\",DEFAULT=YES,URI=\"audio/fr.m3u8\"\n\
        #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS=\"avc1.4d401e,mp4a.40.2\",AUDIO=\"aud\"\n\
        v360.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1280x720,CODECS=\"avc1.4d401f,mp4a.40.2\",AUDIO=\"aud\"\n\
        /live/ch1/v720.m3u8?token=b\n\
        #EXT-X-STREAM-INF:BANDWIDTH=6000000,RESOLUTION=1920x1080,CODECS=\"hvc1.1.6.L120.90,mp4a.40.2\",AUDIO=\"aud\"\n\
        v1080.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=128000,CODECS=\"mp4a.40.2\"\n\
        audio-only.m3u8\n\
        #EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=100000,RESOLUTION=1920x1080,URI=\"iframes.m3u8\"\n";

    #[test]
    fn a_remux_reads_the_highest_h264_variant_with_its_default_audio() {
        let p = pick_variant(MASTER, BASE, true, None).unwrap();
        assert_eq!(p.video, "https://cdn.example/live/ch1/v720.m3u8?token=b");
        assert_eq!(
            p.audio.as_deref(),
            Some("https://cdn.example/live/ch1/audio/fr.m3u8")
        );
    }

    #[test]
    fn a_transcode_reads_the_lowest_variant_covering_its_height() {
        let low = pick_variant(MASTER, BASE, false, Some(360)).unwrap();
        assert_eq!(low.video, "https://cdn.example/live/ch1/v360.m3u8");
        let mid = pick_variant(MASTER, BASE, false, Some(480)).unwrap();
        assert_eq!(mid.video, "https://cdn.example/live/ch1/v720.m3u8?token=b");
        // Taller than every variant, or no cap: the highest, whatever its codec.
        for h in [Some(2160), None] {
            let top = pick_variant(MASTER, BASE, false, h).unwrap();
            assert_eq!(top.video, "https://cdn.example/live/ch1/v1080.m3u8");
        }
    }

    #[test]
    fn missing_attributes_and_muxed_audio() {
        // No RESOLUTION: by bandwidth; no CODECS: taken for video; a rendition with no URI is
        // in the variant, which then carries its audio.
        let m = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"x\",DEFAULT=YES\n\
                 #EXT-X-STREAM-INF:BANDWIDTH=900000,AUDIO=\"a\"\nlo.m3u8\n\
                 #EXT-X-STREAM-INF:BANDWIDTH=2000000,AUDIO=\"a\"\nhi.m3u8\n\
                 #EXT-X-STREAM-INF:PROGRAM-ID=1\nbare.m3u8\n";
        let p = pick_variant(m, "http://h/p/m.m3u8", true, None).unwrap();
        assert_eq!(
            p,
            HlsPick {
                video: "http://h/p/hi.m3u8".into(),
                audio: None
            }
        );
    }

    #[test]
    fn a_media_playlist_or_audio_only_master_picks_nothing() {
        let media = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\nseg1.ts\n";
        assert_eq!(pick_variant(media, BASE, true, None), None);
        let audio = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1,CODECS=\"mp4a.40.2\"\na.m3u8\n";
        assert_eq!(pick_variant(audio, BASE, true, None), None);
    }

    #[test]
    fn sdr_and_aac_are_preferred_and_odd_audio_is_audio() {
        let m = "#EXTM3U\n\
            #EXT-X-STREAM-INF:BANDWIDTH=9000000,RESOLUTION=1920x1080,CODECS=\"avc1.640028,mp4a.40.2\",VIDEO-RANGE=PQ\n\
            pq.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=6200000,RESOLUTION=1920x1080 ,CODECS=\"avc1.640028,ec-3\"\n\
            ec3.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=6000000,RESOLUTION=1920x1080 ,CODECS=\"avc1.640028,mp4a.40.2\",VIDEO-RANGE=SDR\n\
            sdr.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=9900000,CODECS=\"dtsx,mha1.0C,ec+3\"\n\
            audio.m3u8\n";
        for (remux, height) in [(true, None), (false, None), (false, Some(1080))] {
            let p = pick_variant(m, "http://h/m.m3u8", remux, height).unwrap();
            assert_eq!(p.video, "http://h/sdr.m3u8", "{remux} {height:?}");
        }
    }

    #[test]
    fn only_http_uris_are_read() {
        let m = "#EXTM3U\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"x\",DEFAULT=YES,URI=\"file:///etc/a.m3u8\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=9000000,RESOLUTION=1920x1080,AUDIO=\"a\"\n\
            file:///etc/passwd\n\
            #EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=640x360,AUDIO=\"a\"\n\
            HTTPS://cdn/v.m3u8\n";
        let p = pick_variant(m, "http://h/m.m3u8", true, None).unwrap();
        assert_eq!(
            p,
            HlsPick {
                video: "HTTPS://cdn/v.m3u8".into(),
                audio: None
            }
        );
    }

    #[test]
    fn uris_resolve_against_the_final_url() {
        let b = "http://h:8080/a/b/c.m3u8?x=1";
        for (r, want) in [
            ("d.m3u8", "http://h:8080/a/b/d.m3u8"),
            ("../d.m3u8?t=2", "http://h:8080/a/d.m3u8?t=2"),
            ("./e/../d.m3u8", "http://h:8080/a/b/d.m3u8"),
            ("/root.m3u8", "http://h:8080/root.m3u8"),
            ("//cdn/x.m3u8", "http://cdn/x.m3u8"),
            ("https://o/y.m3u8", "https://o/y.m3u8"),
            ("?y=2", "http://h:8080/a/b/c.m3u8?y=2"),
        ] {
            assert_eq!(resolve_uri(b, r), want, "{r}");
        }
        assert_eq!(resolve_uri("http://h", "v.m3u8"), "http://h/v.m3u8");
        assert_eq!(resolve_uri("http://h?q", "v.m3u8"), "http://h/v.m3u8");
    }
}

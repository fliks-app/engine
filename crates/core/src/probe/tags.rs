//! Descriptive tags and cover of an audio file, as ffprobe prints the ID3, MP4 and Vorbis keys.
use super::ffjson::{Stream, Tags};
use crate::api::{CoverArt, MediaTags};

/// Tag sources in priority order: the container's, then the first audio stream's (Ogg only).
struct Sources<'a>(&'a [&'a Tags]);

impl Sources<'_> {
    /// The first non-empty value, trying the keys in order and, per key, the sources in order.
    fn text(&self, keys: &[&str]) -> Option<String> {
        keys.iter()
            .find_map(|k| {
                self.0
                    .iter()
                    .find_map(|t| t.get(k).map(str::trim).filter(|v| !v.is_empty()))
            })
            .map(String::from)
    }

    /// `n` or `n/total`, the total also read from its own keys.
    fn position(&self, keys: &[&str], total_keys: &[&str]) -> (Option<u32>, Option<u32>) {
        let (n, total) = self
            .text(keys)
            .map_or((None, None), |v| match v.split_once('/') {
                Some((n, t)) => (digits(n), digits(t)),
                None => (digits(&v), None),
            });
        let total = total.or_else(|| self.text(total_keys).as_deref().and_then(digits));
        (n, total)
    }
}

fn digits(s: &str) -> Option<u32> {
    let s = s.trim();
    s[..s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len())]
        .parse()
        .ok()
}

/// First run of four digits: `2021`, `2021-03-04`, `March 4, 2021`.
fn year(s: &str) -> Option<u32> {
    let b = s.as_bytes();
    let digit = |i: usize| b.get(i).is_some_and(u8::is_ascii_digit);
    (0..b.len())
        .find(|i| (0..4).all(|k| digit(i + k)) && !digit(i + 4) && (*i == 0 || !digit(i - 1)))
        .and_then(|i| s[i..i + 4].parse().ok())
}

/// Only Ogg keeps the file's comments on its stream; elsewhere a stream tag names the track.
pub fn media_tags(
    format: &Tags,
    format_name: Option<&str>,
    audio: Option<&Tags>,
) -> Option<MediaTags> {
    let ogg = format_name.is_some_and(|n| n.split(',').any(|f| f == "ogg"));
    let all: Vec<&Tags> = std::iter::once(format)
        .chain(audio.filter(|_| ogg))
        .collect();
    let t = Sources(&all);
    let (track, track_total) =
        t.position(&["track", "tracknumber"], &["tracktotal", "totaltracks"]);
    let (disc, disc_total) = t.position(&["disc", "discnumber"], &["disctotal", "totaldiscs"]);
    let tags = MediaTags {
        title: t.text(&["title"]),
        album: t.text(&["album"]),
        artist: t.text(&["artist", "author"]),
        album_artist: t.text(&["album_artist", "albumartist"]),
        composer: t.text(&["composer"]),
        narrator: t.text(&["narrator", "reader"]),
        genre: t.text(&["genre"]),
        publisher: t.text(&["publisher", "label"]),
        description: t.text(&["description", "synopsis", "comment"]),
        // A stream's own language is `audio[].language`; an MP4 stream always carries one.
        language: Sources(&all[..1]).text(&["language"]),
        series: t.text(&["series", "mvnm", "movement_name", "grouping"]),
        series_part: t.text(&[
            "series-part",
            "series_part",
            "seriespart",
            "mvin",
            "movement",
        ]),
        asin: t.text(&["asin", "audible_asin", "cdek"]),
        isbn: t.text(&["isbn"]),
        year: t
            .text(&["date", "year", "originaldate"])
            .as_deref()
            .and_then(year),
        track,
        track_total,
        disc,
        disc_total,
    };
    (tags != MediaTags::default()).then_some(tags)
}

/// The first embedded picture.
pub fn cover(streams: &[Stream]) -> Option<CoverArt> {
    let s = streams.iter().find(|s| {
        s.codec_type.as_deref() == Some("video") && s.disposition.attached_pic == Some(1.0)
    })?;
    Some(CoverArt {
        stream_index: s.index.unwrap_or(0.0) as u32,
        codec: s.codec_name.clone().unwrap_or_else(|| "unknown".into()),
        width: s.width.map(|w| w as u32),
        height: s.height.map(|h| h as u32),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(json: &str) -> Tags {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn year_takes_the_first_four_digit_run() {
        for (s, want) in [
            ("2021", Some(2021)),
            ("2021-03-04", Some(2021)),
            ("March 4, 2021", Some(2021)),
            ("12345", None),
            ("99", None),
        ] {
            assert_eq!(year(s), want, "{s}");
        }
    }

    #[test]
    fn positions_read_a_fraction_or_their_own_total_keys() {
        let t = tags(r#"{"TRACK":"3/12","disc":"1","DISCTOTAL":"2"}"#);
        let m = media_tags(&t, None, None).unwrap();
        assert_eq!(
            (m.track, m.track_total, m.disc, m.disc_total),
            (Some(3), Some(12), Some(1), Some(2))
        );
    }

    #[test]
    fn only_ogg_falls_back_to_the_stream_and_blank_values_are_absent() {
        let format = tags(r#"{"title":"  ","album":"Container"}"#);
        let stream = tags(r#"{"title":"Stream","album":"Stream","SERIES":"S"}"#);
        let m = media_tags(&format, Some("ogg"), Some(&stream)).unwrap();
        assert_eq!(m.title.as_deref(), Some("Stream"));
        assert_eq!(m.album.as_deref(), Some("Container"));
        assert_eq!(m.series.as_deref(), Some("S"));
        // A Matroska or MP4 stream tag is a track name, not the file's title.
        let m = media_tags(&format, Some("matroska,webm"), Some(&stream)).unwrap();
        assert_eq!((m.title, m.series), (None, None));
        assert_eq!(media_tags(&tags(r#"{"encoder":"x"}"#), None, None), None);
    }

    #[test]
    fn a_multi_valued_tag_stays_the_one_string_ffprobe_joined() {
        let t = tags(r#"{"artist":"Ada Example; Bob Reader","narrator":"Bob Reader/Cy Voice"}"#);
        let m = media_tags(&t, None, None).unwrap();
        assert_eq!(m.artist.as_deref(), Some("Ada Example; Bob Reader"));
        assert_eq!(m.narrator.as_deref(), Some("Bob Reader/Cy Voice"));
    }
}

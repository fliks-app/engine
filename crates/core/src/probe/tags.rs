//! Descriptive tags and cover of an audio file, as ffprobe prints the ID3, MP4 and Vorbis keys.
use super::ffjson::{Stream, Tags};
use crate::api::{CoverArt, MediaTags, Performer};

/// Tag sources in priority order: the container's, then the first audio stream's (Ogg only).
struct Sources<'a>(&'a [&'a Tags]);

impl Sources<'_> {
    fn list(&self, keys: &[&str]) -> Vec<String> {
        self.text(keys)
            .as_deref()
            .map(split_multi)
            .unwrap_or_default()
    }

    fn float(&self, keys: &[&str]) -> Option<f64> {
        self.text(keys).as_deref().and_then(leading_float)
    }

    /// The track or album gain: ReplayGain text, else Opus `R128_*` converted.
    fn gain(&self, rg: &str, r128: &str) -> Option<f64> {
        self.float(&[rg]).or_else(|| {
            self.text(&[r128])
                .and_then(|v| v.parse().ok())
                .map(r128_to_rg)
        })
    }

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

/// A multi-valued tag ffprobe printed as one string: split on `;`, blanks dropped.
pub fn split_multi(v: &str) -> Vec<String> {
    v.split(';')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect()
}

/// `Name` or `Name (instrument)`.
pub fn performer(v: &str) -> Option<Performer> {
    let v = v.trim();
    let (name, instrument) = match v.strip_suffix(')').and_then(|r| r.rsplit_once('(')) {
        Some((n, i)) if !n.trim().is_empty() && !i.trim().is_empty() => {
            (n.trim(), Some(i.trim().to_string()))
        }
        _ => (v, None),
    };
    (!name.is_empty()).then(|| Performer {
        name: name.into(),
        instrument,
    })
}

/// Opus `R128_*_GAIN` (Q7.8 dB against -23 LUFS) as a ReplayGain dB against -18 LUFS.
pub fn r128_to_rg(q78: i32) -> f64 {
    f64::from(q78) / 256.0 + 5.0
}

/// A leading float: `-6.5 dB`, `0.98`.
pub fn leading_float(s: &str) -> Option<f64> {
    let s = s.trim();
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '-' | '+' | '.')))
        .unwrap_or(s.len());
    s[..end].parse().ok().filter(|f: &f64| f.is_finite())
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
        artists: t.list(&["artists", "artist"]),
        album_artists: t.list(&[
            "albumartists",
            "album_artists",
            "album_artist",
            "albumartist",
        ]),
        genres: t.list(&["genre"]),
        musicbrainz_recording_id: t.text(&["musicbrainz_trackid", "musicbrainz track id"]),
        musicbrainz_track_id: t
            .text(&["musicbrainz_releasetrackid", "musicbrainz release track id"]),
        musicbrainz_release_id: t.text(&["musicbrainz_albumid", "musicbrainz album id"]),
        musicbrainz_release_group_id: t
            .text(&["musicbrainz_releasegroupid", "musicbrainz release group id"]),
        musicbrainz_work_id: t.text(&["musicbrainz_workid", "musicbrainz work id"]),
        musicbrainz_artist_ids: t.list(&["musicbrainz_artistid", "musicbrainz artist id"]),
        musicbrainz_album_artist_ids: t
            .list(&["musicbrainz_albumartistid", "musicbrainz album artist id"]),
        work: t.text(&["work", "worktitle"]),
        movement: t.text(&["movementname", "movement_name", "mvnm"]),
        movement_number: t
            .position(&["movement", "mvin"], &["movementtotal", "mvc"])
            .0,
        movement_total: t
            .position(&["movement", "mvin"], &["movementtotal", "mvc"])
            .1,
        conductor: t.text(&["conductor"]),
        performers: t
            .list(&["performer"])
            .iter()
            .filter_map(|p| performer(p))
            .collect(),
        isrc: t.text(&["isrc", "tsrc"]),
        original_date: t.text(&["originaldate", "originalyear", "tdor"]),
        release_type: t.text(&[
            "musicbrainz_albumtype",
            "musicbrainz album type",
            "releasetype",
        ]),
        release_status: t.text(&[
            "musicbrainz_albumstatus",
            "musicbrainz album status",
            "releasestatus",
        ]),
        compilation: t
            .text(&["compilation", "tcmp", "cpil"])
            .map(|v| v.trim() == "1"),
        disc_subtitle: t.text(&["discsubtitle", "tsst", "setsubtitle"]),
        media: t.text(&["media", "tmed"]),
        barcode: t.text(&["barcode"]),
        catalog_number: t.text(&["catalognumber"]),
        label: t.text(&["label", "publisher"]),
        replay_gain_track_gain: t.gain("replaygain_track_gain", "r128_track_gain"),
        replay_gain_track_peak: t.float(&["replaygain_track_peak"]),
        replay_gain_album_gain: t.gain("replaygain_album_gain", "r128_album_gain"),
        replay_gain_album_peak: t.float(&["replaygain_album_peak"]),
        lyrics: t.text(&["lyrics", "unsyncedlyrics", "unsynced lyrics", "uslt"]),
        synced_lyrics: None,
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

    #[test]
    fn music_keys_split_multi_values_and_convert_opus_gain() {
        let t = tags(
            r#"{"artists":"Ada Example; Bob Reader","MUSICBRAINZ_TRACKID":"r1","MUSICBRAINZ_ARTISTID":"a1;a2",
            "performer":"Cy Voice (viola)","movement":"2/4","compilation":"1","R128_TRACK_GAIN":"-256",
            "REPLAYGAIN_ALBUM_GAIN":"-6.5 dB","lyrics":"la","MUSICBRAINZ_ALBUMSTATUS":"official"}"#,
        );
        let m = media_tags(&t, None, None).unwrap();
        assert_eq!(m.artists, ["Ada Example", "Bob Reader"]);
        assert_eq!(m.musicbrainz_recording_id.as_deref(), Some("r1"));
        assert_eq!(m.musicbrainz_artist_ids, ["a1", "a2"]);
        assert_eq!(m.performers[0].instrument.as_deref(), Some("viola"));
        assert_eq!((m.movement_number, m.movement_total), (Some(2), Some(4)));
        assert_eq!(m.compilation, Some(true));
        assert_eq!(m.replay_gain_track_gain, Some(4.0));
        assert_eq!(m.replay_gain_album_gain, Some(-6.5));
        assert_eq!(m.release_status.as_deref(), Some("official"));
        assert_eq!(m.lyrics.as_deref(), Some("la"));
    }
}

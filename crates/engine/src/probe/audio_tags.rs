//! Audio tags read with lofty on top of ffprobe's: multi-value fidelity, SYLT, Opus `R128_*`.
use fliks_engine_core::api::MediaTags;
use fliks_engine_core::probe::{performer, r128_to_rg, split_multi};
use lofty::file::{AudioFile, TaggedFile, TaggedFileExt};
use lofty::id3::v2::{Frame, SynchronizedTextFrame, TimestampFormat};
use lofty::tag::{ItemKey, Tag};
use std::path::Path;

/// A lofty value replaces the ffprobe one; an absent one leaves it.
fn pick<T: Default + PartialEq>(base: &mut T, over: T) {
    if over != T::default() {
        *base = over;
    }
}

/// `ffprobe` tags completed with lofty's; the file is read blocking.
pub fn overlay(path: &Path, base: &mut MediaTags) {
    let Ok(file) = lofty::read_from_path(path) else {
        return;
    };
    if let Some(tag) = primary(&file) {
        apply(tag, base);
    }
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("mp3"))
    {
        pick(&mut base.synced_lyrics, sylt(path));
    }
}

fn primary(file: &TaggedFile) -> Option<&Tag> {
    file.primary_tag().or_else(|| file.first_tag())
}

fn apply(tag: &Tag, b: &mut MediaTags) {
    let strings = |k: ItemKey| -> Vec<String> {
        tag.get_strings(k)
            .flat_map(|s| s.split('\0'))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect()
    };
    let one = |k: ItemKey| strings(k).into_iter().next();
    let many = |keys: &[ItemKey]| -> Vec<String> {
        keys.iter()
            .map(|k| strings(k.clone()))
            .find(|v| !v.is_empty())
            .map(|v| v.iter().flat_map(|s| split_multi(s)).collect())
            .unwrap_or_default()
    };
    let ids = |k: ItemKey| strings(k).iter().flat_map(|s| split_multi(s)).collect();
    pick(
        &mut b.artists,
        many(&[ItemKey::TrackArtists, ItemKey::TrackArtist]),
    );
    pick(
        &mut b.album_artists,
        many(&[ItemKey::AlbumArtists, ItemKey::AlbumArtist]),
    );
    pick(&mut b.genres, many(&[ItemKey::Genre]));
    pick(
        &mut b.musicbrainz_recording_id,
        one(ItemKey::MusicBrainzRecordingId),
    );
    pick(
        &mut b.musicbrainz_track_id,
        one(ItemKey::MusicBrainzTrackId),
    );
    pick(
        &mut b.musicbrainz_release_id,
        one(ItemKey::MusicBrainzReleaseId),
    );
    pick(
        &mut b.musicbrainz_release_group_id,
        one(ItemKey::MusicBrainzReleaseGroupId),
    );
    pick(&mut b.musicbrainz_work_id, one(ItemKey::MusicBrainzWorkId));
    pick(
        &mut b.musicbrainz_artist_ids,
        ids(ItemKey::MusicBrainzArtistId),
    );
    pick(
        &mut b.musicbrainz_album_artist_ids,
        ids(ItemKey::MusicBrainzReleaseArtistId),
    );
    pick(&mut b.work, one(ItemKey::Work));
    pick(&mut b.movement, one(ItemKey::Movement));
    let (n, total) = one(ItemKey::MovementNumber).map_or((None, None), |v| fraction(&v));
    pick(&mut b.movement_number, n);
    pick(
        &mut b.movement_total,
        total.or_else(|| one(ItemKey::MovementTotal).and_then(|v| v.trim().parse().ok())),
    );
    pick(&mut b.conductor, one(ItemKey::Conductor));
    pick(
        &mut b.performers,
        strings(ItemKey::Performer)
            .iter()
            .filter_map(|p| performer(p))
            .collect(),
    );
    pick(&mut b.isrc, one(ItemKey::Isrc));
    pick(&mut b.original_date, one(ItemKey::OriginalReleaseDate));
    pick(&mut b.release_type, one(ItemKey::MusicBrainzReleaseType));
    pick(
        &mut b.compilation,
        one(ItemKey::FlagCompilation).map(|v| v == "1"),
    );
    pick(&mut b.disc_subtitle, one(ItemKey::SetSubtitle));
    pick(&mut b.media, one(ItemKey::OriginalMediaType));
    pick(&mut b.barcode, one(ItemKey::Barcode));
    pick(&mut b.catalog_number, one(ItemKey::CatalogNumber));
    pick(&mut b.label, one(ItemKey::Label));
    let float = |k: ItemKey| one(k).and_then(|v| fliks_engine_core::probe::leading_float(&v));
    let r128 = |k: ItemKey| one(k).and_then(|v| v.trim().parse().ok()).map(r128_to_rg);
    pick(
        &mut b.replay_gain_track_gain,
        float(ItemKey::ReplayGainTrackGain).or_else(|| r128(ItemKey::R128TrackGain)),
    );
    pick(
        &mut b.replay_gain_track_peak,
        float(ItemKey::ReplayGainTrackPeak),
    );
    pick(
        &mut b.replay_gain_album_gain,
        float(ItemKey::ReplayGainAlbumGain).or_else(|| r128(ItemKey::R128AlbumGain)),
    );
    pick(
        &mut b.replay_gain_album_peak,
        float(ItemKey::ReplayGainAlbumPeak),
    );
    pick(
        &mut b.lyrics,
        one(ItemKey::UnsyncLyrics).or_else(|| one(ItemKey::Lyrics)),
    );
}

/// `n` or `n/total`.
fn fraction(v: &str) -> (Option<u32>, Option<u32>) {
    match v.split_once('/') {
        Some((n, t)) => (n.trim().parse().ok(), t.trim().parse().ok()),
        None => (v.trim().parse().ok(), None),
    }
}

/// The first SYLT frame in millisecond units as LRC lines.
fn sylt(path: &Path) -> Option<String> {
    use lofty::config::ParseOptions;
    let mut f = std::fs::File::open(path).ok()?;
    let mpeg = lofty::mpeg::MpegFile::read_from(&mut f, ParseOptions::new()).ok()?;
    let frame = mpeg.id3v2()?.into_iter().find_map(|fr| match fr {
        Frame::Binary(b) if b.id().as_str() == "SYLT" => {
            SynchronizedTextFrame::parse(&b.data, b.flags()).ok()
        }
        _ => None,
    })?;
    lrc(&frame)
}

fn lrc(f: &SynchronizedTextFrame<'_>) -> Option<String> {
    if f.timestamp_format != TimestampFormat::MS {
        return None;
    }
    let lines: Vec<String> = f
        .content
        .iter()
        .map(|(ms, text)| {
            let (m, s, cs) = (ms / 60_000, ms / 1000 % 60, ms % 1000 / 10);
            format!("[{m:02}:{s:02}.{cs:02}]{}", text.trim().replace('\n', " "))
        })
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lofty::id3::v2::{BinaryFrame, FrameId, Id3v2Tag, SyncTextContentType};
    use lofty::tag::TagExt;
    use lofty::tag::TagItem;

    fn mp3(name: &str, tag: Tag, sylt: Option<Frame<'static>>) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "fliks-audio-tags-{}-{name}.mp3",
            std::process::id()
        ));
        let mut audio = Vec::new();
        for _ in 0..8 {
            audio.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
            audio.extend(std::iter::repeat_n(0u8, 413));
        }
        std::fs::write(&path, audio).unwrap();
        let mut id3: Id3v2Tag = tag.into();
        if let Some(f) = sylt {
            id3.insert(f);
        }
        id3.save_to_path(&path, lofty::config::WriteOptions::default())
            .unwrap();
        path
    }

    #[test]
    fn id3_tags_fill_the_music_fields_and_sylt_becomes_lrc() {
        let mut tag = Tag::new(lofty::tag::TagType::Id3v2);
        for (k, v) in [
            (ItemKey::TrackArtist, "Ada Example"),
            (ItemKey::TrackArtist, "Bob Reader"),
            (ItemKey::MusicBrainzRecordingId, "rec-1"),
            (ItemKey::Work, "Work One"),
            (ItemKey::Movement, "Largo"),
            (ItemKey::MovementNumber, "2/4"),
            (ItemKey::Conductor, "Cy Voice"),
            (ItemKey::Isrc, "XX1234500001"),
            (ItemKey::ReplayGainTrackGain, "-7.25 dB"),
            (ItemKey::UnsyncLyrics, "la la"),
        ] {
            tag.push(TagItem::new(k, lofty::tag::ItemValue::Text(v.into())));
        }
        let sylt = SynchronizedTextFrame::new(
            lofty::TextEncoding::UTF8,
            *b"eng",
            TimestampFormat::MS,
            SyncTextContentType::Lyrics,
            None,
            vec![(1500, "one".into()), (61_230, "two".into())],
        );
        let bytes = sylt
            .as_bytes(lofty::config::WriteOptions::default())
            .unwrap();
        let frame = Frame::Binary(BinaryFrame::new(FrameId::new("SYLT").unwrap(), bytes));
        let path = mp3("full", tag, Some(frame));
        let mut m = MediaTags {
            artists: vec!["ffprobe".into()],
            title: Some("kept".into()),
            ..MediaTags::default()
        };
        overlay(&path, &mut m);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(m.title.as_deref(), Some("kept"));
        assert_eq!(m.artists, ["Ada Example", "Bob Reader"]);
        assert_eq!(m.musicbrainz_recording_id.as_deref(), Some("rec-1"));
        assert_eq!(m.work.as_deref(), Some("Work One"));
        assert_eq!((m.movement_number, m.movement_total), (Some(2), Some(4)));
        assert_eq!(m.conductor.as_deref(), Some("Cy Voice"));
        assert_eq!(m.isrc.as_deref(), Some("XX1234500001"));
        assert_eq!(m.replay_gain_track_gain, Some(-7.25));
        assert_eq!(m.lyrics.as_deref(), Some("la la"));
        assert_eq!(
            m.synced_lyrics.as_deref(),
            Some("[00:01.50]one\n[01:01.23]two")
        );
    }

    #[test]
    fn an_unreadable_file_leaves_the_ffprobe_tags() {
        let mut m = MediaTags {
            album: Some("a".into()),
            ..MediaTags::default()
        };
        overlay(Path::new("/nonexistent/x.mp3"), &mut m);
        assert_eq!(m.album.as_deref(), Some("a"));
    }
}

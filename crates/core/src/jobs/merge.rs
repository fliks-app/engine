//! Merges the parts of an audiobook into one `.m4b`: plan, ffmpeg argv, chapters and tags.
//! Spec: `docs/spec/jobs.md` section 10.
use crate::api::{Chapter, CoverArt, MediaFamily, MediaTags, MergePart};
use crate::strs;
use serde::Serialize;
use std::fmt::Write;
use std::path::Path;

/// A re-encode opens one input per part.
pub const MAX_PARTS: usize = 500;

/// Characters of a Windows command line (the limit is 32767 with the program's own path).
pub const WINDOWS_ARGV_LIMIT: usize = 30_000;

/// Ceiling of an unpaused run: realtime plus an hour. The stall watchdog ends a hung run sooner.
pub fn timeout_ms(total_seconds: f64) -> u64 {
    3_600_000_u64.saturating_add((total_seconds.max(0.0) * 1000.0) as u64)
}

/// Characters the arguments take on a command line, separators included.
pub fn argv_chars(argv: &[String]) -> usize {
    argv.iter().map(|a| a.chars().count() + 1).sum()
}

/// A path ffmpeg or the concat list could read as something else: a newline starts a directive.
fn plain(path: &str) -> bool {
    !path.chars().any(char::is_control)
}

pub fn validate(parts: &[MergePart], output: &str) -> Result<(), String> {
    if parts.is_empty() || parts.len() > MAX_PARTS {
        return Err(format!("Merge needs 1 to {MAX_PARTS} parts"));
    }
    if !plain(output)
        || !Path::new(output).is_absolute()
        || !output.to_ascii_lowercase().ends_with(".m4b")
    {
        return Err("Merge output must be an absolute .m4b path without control characters".into());
    }
    for (i, p) in parts.iter().enumerate() {
        let n = i + 1;
        if !plain(&p.path) || !Path::new(&p.path).is_absolute() {
            return Err(format!(
                "Merge part {n} path must be absolute, without control characters"
            ));
        }
        if p.info.family() != Some(MediaFamily::Audio) {
            return Err(format!("Merge part {n} is not an audio file"));
        }
        if !p
            .info
            .duration_seconds
            .is_some_and(|d| d.is_finite() && d > 0.0)
        {
            return Err(format!("Merge part {n} has no duration"));
        }
    }
    Ok(())
}

/// How the audio goes into the `.m4b`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(
    tag = "mode",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Encoding {
    /// Every part is the same AAC: the packets are copied.
    Copy,
    /// Anything else: AAC at a bit rate close to the sources', in the first part's sample rate.
    Aac {
        sample_rate: u32,
        channels: u32,
        bit_rate: u32,
    },
}

/// Lossless codecs: their bit rate says nothing about what speech needs.
fn lossless(codec: &str) -> bool {
    codec.starts_with("pcm_")
        || matches!(codec, "flac" | "alac" | "wavpack" | "ape" | "tta" | "tak")
}

pub fn encoding(parts: &[MergePart]) -> Encoding {
    let first = |p: &MergePart| p.info.audio.first().cloned();
    let lead = first(&parts[0]);
    let family = |p: &MergePart| p.info.format_name.clone();
    let same = lead.as_ref().is_some_and(|l| {
        l.codec == "aac"
            && l.sample_rate.is_some()
            && l.channels.is_some()
            && family(&parts[0]).is_some()
            && parts.iter().all(|p| {
                family(p) == family(&parts[0])
                    && first(p).is_some_and(|a| {
                        (
                            &a.codec,
                            a.sample_rate,
                            a.channels,
                            &a.profile,
                            a.stream_index,
                        ) == (
                            &l.codec,
                            l.sample_rate,
                            l.channels,
                            &l.profile,
                            l.stream_index,
                        )
                    })
            })
    });
    if same {
        return Encoding::Copy;
    }
    let streams = || parts.iter().filter_map(first);
    let channels = streams()
        .filter_map(|a| a.channels)
        .max()
        .unwrap_or(2)
        .clamp(1, 2);
    // A lossless source counts as 64 kb/s per channel, a lossy one as its own rate.
    let speech =
        |a: &crate::api::AudioStreamInfo| 64_000 * a.channels.unwrap_or(channels).clamp(1, 2);
    let bit_rate = streams()
        .map(|a| {
            if lossless(&a.codec) {
                speech(&a)
            } else {
                a.bit_rate.unwrap_or_else(|| speech(&a))
            }
        })
        .max()
        .unwrap_or(64_000 * channels)
        .clamp(32_000, 128_000 * channels);
    Encoding::Aac {
        sample_rate: lead
            .and_then(|a| a.sample_rate)
            .unwrap_or(44_100)
            .min(96_000),
        channels,
        bit_rate,
    }
}

fn duration(p: &MergePart) -> f64 {
    p.info.duration_seconds.unwrap_or(0.0)
}

/// Where each part starts on the merged timeline: the sum of the parts before it.
pub fn part_starts(parts: &[MergePart]) -> Vec<f64> {
    parts
        .iter()
        .scan(0.0, |at, p| {
            let start = *at;
            *at += duration(p);
            Some(start)
        })
        .collect()
}

pub fn total_seconds(parts: &[MergePart]) -> f64 {
    parts.iter().map(duration).sum()
}

/// The part's own chapters moved to its place in the merged file, else one chapter for the part
/// titled from its tags.
pub fn chapters(parts: &[MergePart]) -> Vec<Chapter> {
    let mut out: Vec<Chapter> = vec![];
    for (i, (p, at)) in parts.iter().zip(part_starts(parts)).enumerate() {
        let len = duration(p);
        let own: Vec<_> = p.info.chapters.iter().flatten().collect();
        if own.is_empty() {
            let title = p.info.tags.as_ref().and_then(|t| t.title.clone());
            out.push(Chapter {
                start_seconds: at,
                end_seconds: at + len,
                title: Some(title.unwrap_or_else(|| format!("Part {}", i + 1))),
            });
            continue;
        }
        for c in own {
            let start = c.start_seconds.clamp(0.0, len);
            let end = c.end_seconds.clamp(start, len);
            if end <= start {
                continue;
            }
            let title = c.title.as_deref().map(str::trim).filter(|t| !t.is_empty());
            out.push(Chapter {
                start_seconds: at + start,
                end_seconds: at + end,
                title: Some(title.map_or_else(|| format!("Chapter {}", out.len() + 1), Into::into)),
            });
        }
    }
    out
}

/// The request's tags win, then the first part that has the field. The book title is the album of
/// the parts (their own titles name chapters); track and disc numbers mean nothing once merged.
pub fn merged_tags(parts: &[MergePart], over: Option<&MediaTags>) -> MediaTags {
    let pick = |f: fn(&MediaTags) -> Option<String>| {
        over.and_then(f).or_else(|| {
            parts
                .iter()
                .filter_map(|p| p.info.tags.as_ref())
                .find_map(f)
        })
    };
    let title = over
        .and_then(|t| t.title.clone())
        .or_else(|| pick(|t| t.album.clone()))
        .or_else(|| pick(|t| t.title.clone()));
    let year = over.and_then(|t| t.year).or_else(|| {
        parts
            .iter()
            .filter_map(|p| p.info.tags.as_ref()?.year)
            .next()
    });
    MediaTags {
        album: over
            .and_then(|t| t.album.clone().or_else(|| t.title.clone()))
            .or_else(|| pick(|t| t.album.clone()))
            .or_else(|| title.clone()),
        title,
        artist: pick(|t| t.artist.clone()),
        album_artist: pick(|t| t.album_artist.clone()),
        composer: pick(|t| t.composer.clone()),
        narrator: pick(|t| t.narrator.clone()),
        genre: pick(|t| t.genre.clone()),
        publisher: pick(|t| t.publisher.clone()),
        description: pick(|t| t.description.clone()),
        language: pick(|t| t.language.clone()),
        series: pick(|t| t.series.clone()),
        series_part: pick(|t| t.series_part.clone()),
        asin: pick(|t| t.asin.clone()),
        isbn: pick(|t| t.isbn.clone()),
        year,
        ..MediaTags::default()
    }
}

/// A value escaped for an ffmetadata file. A trailing backslash would escape the line break, so
/// it is dropped; NUL cannot be stored.
fn escape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.trim_end_matches('\\').chars().filter(|c| *c != '\0') {
        if matches!(c, '=' | ';' | '#' | '\\' | '\n' | '\r') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The ffmetadata file: the tags the MP4 muxer writes by name, then the chapters.
pub fn ffmetadata(tags: &MediaTags, chapters: &[Chapter]) -> String {
    let mut out = String::from(";FFMETADATA1\n");
    let year = tags.year.map(|y| y.to_string());
    let named = [
        ("title", &tags.title),
        ("album", &tags.album),
        ("artist", &tags.artist),
        ("album_artist", &tags.album_artist),
        ("composer", &tags.composer),
        ("genre", &tags.genre),
        ("description", &tags.description),
        ("date", &year),
    ];
    for (k, v) in named {
        if let Some(v) = v {
            let _ = writeln!(out, "{k}={}", escape(v));
        }
    }
    for c in chapters {
        let ms = |s: f64| (s * 1000.0).round() as i64;
        let _ = write!(
            out,
            "[CHAPTER]\nTIMEBASE=1/1000\nSTART={}\nEND={}\n",
            ms(c.start_seconds),
            ms(c.end_seconds)
        );
        if let Some(t) = &c.title {
            let _ = writeln!(out, "title={}", escape(t));
        }
    }
    out
}

/// The tags the MP4 muxer has no atom for; they are added as iTunes free-form atoms.
pub fn freeform_tags(tags: &MediaTags) -> Vec<(&'static str, String)> {
    [
        ("narrator", &tags.narrator),
        ("publisher", &tags.publisher),
        ("language", &tags.language),
        ("series", &tags.series),
        ("series-part", &tags.series_part),
        ("asin", &tags.asin),
        ("isbn", &tags.isbn),
    ]
    .into_iter()
    .filter_map(|(k, v)| Some((k, v.clone()?)))
    .collect()
}

/// The concat demuxer's list: one quoted path per line.
pub fn concat_list(paths: &[String]) -> String {
    paths
        .iter()
        .map(|p| format!("file '{}'\n", p.replace('\'', r"'\''")))
        .collect()
}

fn first_cover(parts: &[MergePart]) -> Option<(usize, &CoverArt)> {
    parts
        .iter()
        .enumerate()
        .find_map(|(i, p)| Some((i, p.info.cover.as_ref()?)))
}

/// The filter graph of a re-encode, read from a file (`-filter_complex_script`) so that the
/// argv does not grow with it: every part's first audio stream concatenated, then one `aformat`.
pub fn filter_graph(count: usize, sample_rate: u32, channels: u32) -> String {
    let layout = if channels == 1 { "mono" } else { "stereo" };
    let inputs: String = (0..count).map(|i| format!("[{i}:a:0]")).collect();
    format!(
        "{inputs}concat=n={count}:v=0:a=1,aformat=sample_rates={sample_rate}:channel_layouts={layout}[out]\n"
    )
}

/// One ffmpeg run: `paths` are the parts' files on the engine's host, in order, `graph` the file
/// of `filter_graph`. Progress goes to stdout (`progress_seconds`); the output is the temporary
/// file, written as an `ipod` mux (the `.m4b` flavour) whatever its name.
pub fn merge_args(
    parts: &[MergePart],
    paths: &[String],
    encoding: &Encoding,
    files: &MergeFiles,
    output: &str,
) -> Vec<String> {
    let (list, meta, graph) = (&files.list, &files.meta, &files.graph);
    let mut a = strs(&[
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostats",
        "-progress",
        "pipe:1",
        "-n",
    ]);
    let cover = first_cover(parts);
    // Inputs: the concat list, the metadata, the cover's part; or every part, then the metadata.
    let (meta_at, cover_at) = match encoding {
        Encoding::Copy => {
            a.extend(strs(&[
                "-f", "concat", "-safe", "0", "-i", list, "-i", meta,
            ]));
            let at = cover.map(|(i, _)| {
                a.extend(strs(&["-i", &paths[i]]));
                2
            });
            (1, at)
        }
        Encoding::Aac { .. } => {
            for p in paths {
                a.extend(strs(&["-i", p]));
            }
            a.extend(strs(&["-i", meta]));
            (paths.len(), cover.map(|(i, _)| i))
        }
    };
    match encoding {
        Encoding::Copy => a.extend(strs(&["-map", "0:a:0"])),
        Encoding::Aac { .. } => a.extend(strs(&["-filter_complex_script", graph, "-map", "[out]"])),
    }
    if let (Some((_, art)), Some(at)) = (cover, cover_at) {
        a.extend(strs(&["-map"]));
        a.push(format!("{at}:{}", art.stream_index));
    }
    a.extend(strs(&["-map_metadata"]));
    a.push(meta_at.to_string());
    a.extend(strs(&["-map_chapters"]));
    a.push(meta_at.to_string());
    match encoding {
        Encoding::Copy => a.extend(strs(&["-c:a", "copy"])),
        Encoding::Aac { bit_rate, .. } => {
            a.extend(strs(&["-c:a", "aac", "-b:a"]));
            a.push(bit_rate.to_string());
        }
    }
    if let Some((_, art)) = cover {
        if matches!(art.codec.as_str(), "mjpeg" | "png") {
            a.extend(strs(&["-c:v", "copy"]));
        } else {
            a.extend(strs(&["-c:v", "mjpeg", "-q:v", "2"]));
        }
        a.extend(strs(&["-disposition:v:0", "attached_pic"]));
    }
    a.extend(strs(&["-f", "ipod"]));
    a.push(output.into());
    a
}

/// The files `merge_args` reads, in the job's scratch directory.
pub struct MergeFiles {
    pub list: String,
    pub meta: String,
    pub graph: String,
}

/// Seconds of output written so far, from a `-progress` line (`out_time_us=` is microseconds).
pub fn progress_seconds(line: &str) -> Option<f64> {
    let us: f64 = line.strip_prefix("out_time_us=")?.trim().parse().ok()?;
    Some(us / 1e6)
}

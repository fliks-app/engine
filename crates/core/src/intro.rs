//! Intro and outro segments from chromaprint hashes.
use crate::api::{
    Chapter, Fingerprint, IntroDetectKind, IntroDetectRequest, IntroDetectResponse, IntroEpisode,
    IntroSegment, IntroSegmentSource,
};

const HAMMING_THRESHOLD: u32 = 6;
const MAX_LAG_SAMPLES: i32 = 1440;
const MAX_PLAUSIBLE_INTRO_SECONDS: f64 = 120.0;
const MAX_PLAUSIBLE_INTRO_START: f64 = 300.0;
const CLUSTER_BUCKET_SECONDS: f64 = 15.0;
const LEADING_LOGO_GUARD: f64 = 5.0;
const SECONDS_PER_SAMPLE: f64 = 0.1238;
const REF_MARGIN_SECONDS: f64 = 30.0;
const DEFAULT_MIN_SEGMENT_SECONDS: f64 = 15.0;

pub fn detect(req: IntroDetectRequest) -> Result<IntroDetectResponse, String> {
    validate(&req)?;
    let min_segment = req
        .min_segment_seconds
        .unwrap_or(DEFAULT_MIN_SEGMENT_SECONDS);
    let lookup = req.lookup_seconds;
    let is_intro = req.kind == IntroDetectKind::Intro;

    let mut chapter_results: Vec<IntroSegment> = Vec::new();
    let mut chapter_starts: Vec<f64> = Vec::new();
    let mut chapter_lengths: Vec<f64> = Vec::new();
    let mut reference_peer: Option<(usize, f64, f64)> = None;

    for (idx, ep) in req.episodes.iter().enumerate() {
        let chapters = ep.chapters.as_deref().unwrap_or(&[]);
        if let Some(hit) = chapter_hit(req.kind, chapters, ep.duration_seconds) {
            chapter_results.push(IntroSegment {
                media_file_id: ep.media_file_id,
                start_seconds: hit.0,
                end_seconds: hit.1,
                confidence: 1.0,
                source: IntroSegmentSource::Chapter,
            });
            chapter_starts.push(hit.0);
            chapter_lengths.push(hit.1 - hit.0);
            if reference_peer.is_none() {
                reference_peer = Some((idx, hit.0, hit.1));
            }
        }
    }

    let chapter_ids: std::collections::HashSet<u32> =
        chapter_results.iter().map(|r| r.media_file_id).collect();

    let (reference_min, reference_max, reference_length) = if is_intro && chapter_starts.len() >= 2
    {
        (
            Some(
                (chapter_starts.iter().copied().fold(f64::INFINITY, f64::min) - REF_MARGIN_SECONDS)
                    .max(0.0),
            ),
            Some(
                chapter_starts
                    .iter()
                    .copied()
                    .fold(f64::NEG_INFINITY, f64::max)
                    + REF_MARGIN_SECONDS,
            ),
            Some(median(chapter_lengths.clone())),
        )
    } else {
        (None, None, None)
    };

    let mut reference_results: Vec<IntroSegment> = Vec::new();
    let mut reference_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();

    if let Some((peer_idx, peer_start, peer_end)) = reference_peer {
        let peer = &req.episodes[peer_idx];
        let peer_fp = fingerprint_for(peer, req.kind);
        let ref_hashes = ref_hashes_from_window(peer_fp, peer_start, peer_end, is_intro);
        if ref_hashes.len() >= 8 {
            let max_search = if is_intro {
                MAX_PLAUSIBLE_INTRO_START
            } else {
                lookup
            };
            for ep in &req.episodes {
                if chapter_ids.contains(&ep.media_file_id) {
                    continue;
                }
                let target_fp = fingerprint_for(ep, req.kind);
                if let Some(m) = find_by_reference(&ref_hashes, target_fp, max_search) {
                    let (start, end) = if is_intro {
                        (m.start_seconds, m.end_seconds)
                    } else {
                        (
                            target_fp.base_offset_sec + m.start_seconds,
                            target_fp.base_offset_sec + m.end_seconds,
                        )
                    };
                    reference_ids.insert(ep.media_file_id);
                    reference_results.push(IntroSegment {
                        media_file_id: ep.media_file_id,
                        start_seconds: start,
                        end_seconds: end,
                        confidence: m.confidence,
                        source: IntroSegmentSource::Reference,
                    });
                }
            }
        }
    }

    let peer_media_id = reference_peer.map(|(i, _, _)| req.episodes[i].media_file_id);

    let pairwise_targets: Vec<usize> = req
        .episodes
        .iter()
        .enumerate()
        .filter(|(_, ep)| {
            if chapter_ids.contains(&ep.media_file_id) {
                return false;
            }
            if reference_ids.contains(&ep.media_file_id) {
                return false;
            }
            if !is_intro && peer_media_id == Some(ep.media_file_id) {
                return false;
            }
            true
        })
        .map(|(i, _)| i)
        .collect();

    let mut pairwise_results: Vec<IntroSegment> = Vec::new();

    if is_intro || pairwise_targets.len() >= 2 {
        std::thread::scope(|s| {
            let mut handles = Vec::new();
            for &target_idx in &pairwise_targets {
                if is_intro && peer_media_id == Some(req.episodes[target_idx].media_file_id) {
                    continue;
                }
                let req_ref = &req;
                let chapter_ids_ref = &chapter_ids;
                let reference_ids_ref = &reference_ids;
                handles.push(s.spawn(move || {
                    pairwise_one(
                        req_ref,
                        target_idx,
                        PairwiseCtx {
                            min_segment,
                            lookup,
                            is_intro,
                            chapter_ids: chapter_ids_ref,
                            reference_ids: reference_ids_ref,
                            peer_media_id,
                            reference_min,
                            reference_max,
                            reference_length,
                        },
                    )
                }));
            }
            for h in handles {
                if let Some(seg) = h.join().ok().flatten() {
                    pairwise_results.push(seg);
                }
            }
        });
    }

    // Preserve request order within each source tier.
    reference_results.sort_by_key(|r| {
        req.episodes
            .iter()
            .position(|e| e.media_file_id == r.media_file_id)
            .unwrap_or(usize::MAX)
    });
    pairwise_results.sort_by_key(|r| {
        req.episodes
            .iter()
            .position(|e| e.media_file_id == r.media_file_id)
            .unwrap_or(usize::MAX)
    });

    let mut results = chapter_results;
    results.extend(reference_results);
    results.extend(pairwise_results);
    Ok(IntroDetectResponse { results })
}

fn validate(req: &IntroDetectRequest) -> Result<(), String> {
    if !req.lookup_seconds.is_finite() || req.lookup_seconds <= 0.0 {
        return Err("lookupSeconds must be positive".into());
    }
    if req.kind == IntroDetectKind::Outro {
        for ep in &req.episodes {
            if !ep.duration_seconds.is_finite() || ep.duration_seconds <= 0.0 {
                return Err(format!(
                    "episode {} needs a positive durationSeconds for outro detection",
                    ep.media_file_id
                ));
            }
        }
    }
    for ep in &req.episodes {
        let missing = match req.kind {
            IntroDetectKind::Intro => ep.intro_fingerprint.is_none(),
            IntroDetectKind::Outro => ep.outro_fingerprint.is_none(),
        };
        if missing {
            return Err(format!(
                "episode {} missing {} fingerprint",
                ep.media_file_id,
                if req.kind == IntroDetectKind::Intro {
                    "intro"
                } else {
                    "outro"
                }
            ));
        }
    }
    Ok(())
}

fn fingerprint_for(ep: &IntroEpisode, kind: IntroDetectKind) -> &Fingerprint {
    match kind {
        IntroDetectKind::Intro => ep.intro_fingerprint.as_ref().unwrap(),
        IntroDetectKind::Outro => ep.outro_fingerprint.as_ref().unwrap(),
    }
}

fn chapter_hit(kind: IntroDetectKind, chapters: &[Chapter], duration: f64) -> Option<(f64, f64)> {
    match kind {
        IntroDetectKind::Intro => extract_intro_from_chapters(chapters),
        IntroDetectKind::Outro => extract_outro_from_chapters(chapters, duration),
    }
}

fn extract_intro_from_chapters(chapters: &[Chapter]) -> Option<(f64, f64)> {
    for c in chapters {
        let title = c.title.as_deref().unwrap_or("");
        if title.is_empty() || !title_matches_intro(title) {
            continue;
        }
        if c.end_seconds <= c.start_seconds {
            continue;
        }
        let len = c.end_seconds - c.start_seconds;
        if len < 10.0 || len > 180.0 {
            continue;
        }
        if c.start_seconds > 360.0 {
            continue;
        }
        return Some((c.start_seconds, c.end_seconds));
    }
    None
}

fn extract_outro_from_chapters(chapters: &[Chapter], file_duration: f64) -> Option<(f64, f64)> {
    for c in chapters.iter().rev() {
        let title = c.title.as_deref().unwrap_or("");
        if title.is_empty() || !title_matches_outro(title) {
            continue;
        }
        if c.end_seconds <= c.start_seconds {
            continue;
        }
        let len = c.end_seconds - c.start_seconds;
        if len < 10.0 || len > 300.0 {
            continue;
        }
        if file_duration > 0.0 && c.start_seconds < file_duration * 0.6 {
            continue;
        }
        return Some((c.start_seconds, c.end_seconds));
    }
    None
}

fn is_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn word_boundary_before(bytes: &[u8], start: usize) -> bool {
    start == 0 || !is_word_char(bytes[start - 1])
}

fn word_boundary_after(bytes: &[u8], end: usize) -> bool {
    end >= bytes.len() || !is_word_char(bytes[end])
}

fn matches_phrase(title: &str, phrase: &str) -> bool {
    let title_l = title.to_lowercase();
    let phrase_l = phrase.to_lowercase();
    let tb = title_l.as_bytes();
    let pb = phrase_l.as_bytes();
    if pb.is_empty() || pb.len() > tb.len() {
        return false;
    }
    for i in 0..=tb.len() - pb.len() {
        if tb[i..i + pb.len()] != *pb {
            continue;
        }
        if word_boundary_before(tb, i) && word_boundary_after(tb, i + pb.len()) {
            return true;
        }
    }
    false
}

fn title_matches_intro(title: &str) -> bool {
    const PHRASES: &[&str] = &[
        "opening credits",
        "main titles",
        "main title",
        "opening",
        "intro",
        "theme",
        "titles",
        "title",
    ];
    PHRASES.iter().any(|p| matches_phrase(title, p))
}

fn title_matches_outro(title: &str) -> bool {
    if matches_end_credits(title) {
        return true;
    }
    const PHRASES: &[&str] = &["outro", "ending", "credits", "credit", "closing"];
    PHRASES.iter().any(|p| matches_phrase(title, p))
}

/// `end` + one character + `credit` or `credits`, word-bounded as a whole.
fn matches_end_credits(title: &str) -> bool {
    let title_l = title.to_lowercase();
    let tb = title_l.as_bytes();
    let mut i = 0;
    while i + 3 < tb.len() {
        if tb[i..].starts_with(b"end") {
            let start = i;
            let after = i + 3;
            if after >= tb.len() {
                break;
            }
            let rem_start = after + 1;
            if rem_start > tb.len() {
                break;
            }
            for suffix in ["credit", "credits"] {
                let sb = suffix.as_bytes();
                let end = rem_start + sb.len();
                if end <= tb.len()
                    && tb[rem_start..end] == *sb
                    && word_boundary_before(tb, start)
                    && word_boundary_after(tb, end)
                {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

fn ref_hashes_from_window(
    fp: &Fingerprint,
    start_sec: f64,
    end_sec: f64,
    is_intro: bool,
) -> Vec<i32> {
    let (rel_start, rel_end) = if is_intro {
        (
            seconds_to_samples(start_sec).min(fp.hashes.len()),
            seconds_to_samples(end_sec).min(fp.hashes.len()),
        )
    } else {
        let base = fp.base_offset_sec;
        (
            seconds_to_samples((start_sec - base).max(0.0)).min(fp.hashes.len()),
            seconds_to_samples((end_sec - base).max(0.0)).min(fp.hashes.len()),
        )
    };
    if rel_end <= rel_start {
        return vec![];
    }
    fp.hashes[rel_start..rel_end].to_vec()
}

struct RefMatch {
    start_seconds: f64,
    end_seconds: f64,
    confidence: f64,
}

fn find_by_reference(
    ref_hashes: &[i32],
    target: &Fingerprint,
    max_start_seconds: f64,
) -> Option<RefMatch> {
    if ref_hashes.len() < 8 {
        return None;
    }
    let max_start_idx = seconds_to_samples(max_start_seconds)
        .min(target.hashes.len().saturating_sub(ref_hashes.len()));
    if target.hashes.len() < ref_hashes.len() {
        return None;
    }
    let mut best_offset = None;
    let mut best_matches = 0usize;
    for off in 0..=max_start_idx {
        let mut matches = 0usize;
        for (i, &rh) in ref_hashes.iter().enumerate() {
            if popcount32(rh, target.hashes[off + i]) <= HAMMING_THRESHOLD {
                matches += 1;
            }
        }
        if matches > best_matches {
            best_matches = matches;
            best_offset = Some(off);
        }
    }
    let off = best_offset?;
    let conf = best_matches as f64 / ref_hashes.len() as f64;
    if conf < 0.3 {
        return None;
    }
    Some(RefMatch {
        start_seconds: samples_to_seconds(off),
        end_seconds: samples_to_seconds(off + ref_hashes.len()),
        confidence: conf,
    })
}

struct RawMatch {
    a_start: usize,
    length: usize,
    hamming_avg: f64,
}

fn find_all_matches(a: &[i32], b: &[i32], min_segment_seconds: f64) -> Vec<RawMatch> {
    let min_len = 8_usize.max(seconds_to_samples(min_segment_seconds));
    let mut out = Vec::new();
    for lag in -MAX_LAG_SAMPLES..=MAX_LAG_SAMPLES {
        let lag = lag as isize;
        let a_start = (0_isize).max(-lag) as usize;
        let b_end = b.len() as isize - lag;
        let a_end = if b_end <= 0 {
            a_start
        } else {
            a.len().min(b_end as usize)
        };
        if a_start >= a_end {
            continue;
        }
        let mut run_start = None;
        let mut run_len = 0usize;
        let mut run_ham = 0u64;
        let mut flush = |start: Option<usize>, len: usize, ham: u64| {
            if len >= min_len
                && let Some(s) = start
            {
                out.push(RawMatch {
                    a_start: s,
                    length: len,
                    hamming_avg: ham as f64 / len as f64,
                });
            }
        };
        for (off, ha) in a[a_start..a_end].iter().enumerate() {
            let i = a_start + off;
            let j = (i as isize + lag) as usize;
            let ham = popcount32(*ha, b[j]);
            if ham <= HAMMING_THRESHOLD {
                if run_start.is_none() {
                    run_start = Some(i);
                    run_len = 0;
                    run_ham = 0;
                }
                run_len += 1;
                run_ham += ham as u64;
            } else {
                flush(run_start, run_len, run_ham);
                run_start = None;
                run_len = 0;
                run_ham = 0;
            }
        }
        flush(run_start, run_len, run_ham);
    }
    out
}

#[derive(Clone)]
struct MatchCandidate {
    start: f64,
    length: f64,
    conf: f64,
}

struct PairwiseCtx<'a> {
    min_segment: f64,
    lookup: f64,
    is_intro: bool,
    chapter_ids: &'a std::collections::HashSet<u32>,
    reference_ids: &'a std::collections::HashSet<u32>,
    peer_media_id: Option<u32>,
    reference_min: Option<f64>,
    reference_max: Option<f64>,
    reference_length: Option<f64>,
}

fn pairwise_one(
    req: &IntroDetectRequest,
    target_idx: usize,
    ctx: PairwiseCtx<'_>,
) -> Option<IntroSegment> {
    let PairwiseCtx {
        min_segment,
        lookup,
        is_intro,
        chapter_ids,
        reference_ids,
        peer_media_id,
        reference_min,
        reference_max,
        reference_length,
    } = ctx;
    let target = &req.episodes[target_idx];
    let a = fingerprint_for(target, req.kind);
    let mut matches: Vec<MatchCandidate> = Vec::new();

    for (j, ep) in req.episodes.iter().enumerate() {
        if j == target_idx {
            continue;
        }
        if chapter_ids.contains(&ep.media_file_id)
            && (!is_intro || peer_media_id != Some(ep.media_file_id))
        {
            continue;
        }
        // Intros still match against episodes already placed by the reference search; outros do not.
        if !is_intro && reference_ids.contains(&ep.media_file_id) {
            continue;
        }
        if !is_intro && peer_media_id == Some(ep.media_file_id) {
            continue;
        }
        let b = fingerprint_for(ep, req.kind);
        for m in find_all_matches(&a.hashes, &b.hashes, min_segment) {
            let start = samples_to_seconds(m.a_start);
            let length = samples_to_seconds(m.length);
            if length > MAX_PLAUSIBLE_INTRO_SECONDS {
                continue;
            }
            if is_intro && start > MAX_PLAUSIBLE_INTRO_START {
                continue;
            }
            matches.push(MatchCandidate {
                start,
                length,
                conf: 1.0 - m.hamming_avg / HAMMING_THRESHOLD as f64 / 4.0,
            });
        }
    }

    if matches.is_empty() {
        return None;
    }

    let bucket = CLUSTER_BUCKET_SECONDS;
    let mut clusters: Vec<(f64, Vec<MatchCandidate>, usize)> = Vec::new();
    let mut next_order = 0usize;
    for m in matches {
        let key = (m.start / bucket).round() * bucket;
        if let Some(c) = clusters
            .iter_mut()
            .find(|(k, _, _)| (*k - key).abs() < f64::EPSILON)
        {
            c.1.push(m);
        } else {
            clusters.push((key, vec![m], next_order));
            next_order += 1;
        }
    }
    clusters.sort_by_key(|(_, _, o)| *o);

    let mut best_cluster: &[MatchCandidate] = &[];
    let mut pick_from_reference = false;

    if is_intro {
        if let (Some(rmin), Some(rmax)) = (reference_min, reference_max) {
            for (key, arr, _) in &clusters {
                if *key < rmin || *key > rmax {
                    continue;
                }
                if arr.len() > best_cluster.len() {
                    best_cluster = arr;
                    pick_from_reference = true;
                }
            }
            if best_cluster.len() < 2 {
                pick_from_reference = false;
            }
        }
        if best_cluster.len() < 2 {
            for (key, arr, _) in &clusters {
                if *key < LEADING_LOGO_GUARD {
                    continue;
                }
                if arr.len() > best_cluster.len() {
                    best_cluster = arr;
                    pick_from_reference = false;
                }
            }
        }
        if best_cluster.len() < 2 {
            for (_, arr, _) in &clusters {
                if arr.len() > best_cluster.len() {
                    best_cluster = arr;
                    pick_from_reference = false;
                }
            }
        }
    } else {
        for (_, arr, _) in &clusters {
            if arr.len() > best_cluster.len() {
                best_cluster = arr;
            }
        }
    }

    if best_cluster.len() < 2 {
        return None;
    }

    let start_med = median(best_cluster.iter().map(|m| m.start).collect());
    let mut length_med = median(best_cluster.iter().map(|m| m.length).collect());
    let conf_med = clamp(
        median(best_cluster.iter().map(|m| m.conf).collect()),
        0.0,
        1.0,
    );

    if is_intro {
        if pick_from_reference
            && let Some(ref_len) = reference_length
            && length_med < ref_len * 0.7
        {
            length_med = ref_len;
        }
        let start_seconds = start_med.max(0.0);
        let end_seconds = (start_med + length_med).min(lookup);
        if end_seconds - start_seconds < min_segment {
            return None;
        }
        Some(IntroSegment {
            media_file_id: target.media_file_id,
            start_seconds,
            end_seconds,
            confidence: conf_med,
            source: IntroSegmentSource::Pairwise,
        })
    } else {
        let base = a.base_offset_sec;
        let abs_start = base + start_med.max(0.0);
        let abs_end = base + start_med + length_med;
        Some(IntroSegment {
            media_file_id: target.media_file_id,
            start_seconds: abs_start,
            end_seconds: abs_end,
            confidence: conf_med,
            source: IntroSegmentSource::Pairwise,
        })
    }
}

fn popcount32(a: i32, b: i32) -> u32 {
    ((a as u32) ^ (b as u32)).count_ones()
}

fn seconds_to_samples(seconds: f64) -> usize {
    (seconds / SECONDS_PER_SAMPLE).round() as usize
}

fn samples_to_seconds(samples: usize) -> f64 {
    samples as f64 * SECONDS_PER_SAMPLE
}

fn median(mut values: Vec<f64>) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let m = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[m - 1] + values[m]) / 2.0
    } else {
        values[m]
    }
}

fn clamp(n: f64, min: f64, max: f64) -> f64 {
    n.max(min).min(max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intro_chapter_title() {
        assert!(title_matches_intro("Main Titles"));
        assert!(title_matches_intro("02 - Opening Credits"));
        assert!(!title_matches_intro("Recap"));
    }

    #[test]
    fn outro_chapter_rejects_head() {
        let chapters = vec![
            Chapter {
                start_seconds: 0.0,
                end_seconds: 60.0,
                title: Some("Opening Credits".into()),
            },
            Chapter {
                start_seconds: 2400.0,
                end_seconds: 2520.0,
                title: Some("End Credits".into()),
            },
        ];
        let hit = extract_outro_from_chapters(&chapters, 2520.0);
        assert!(hit.is_some());
        assert!((hit.unwrap().0 - 2400.0).abs() < f64::EPSILON);
    }

    #[test]
    fn pairwise_stretches_short_match_to_chapter_length() {
        let start = 30.0;
        let chapter_len = 60.0;
        let at = seconds_to_samples(start);
        let chapter_fp = {
            let end = seconds_to_samples(start + chapter_len);
            let mut hashes = vec![9; end + 8];
            hashes[at..end].fill(1);
            Fingerprint {
                hashes,
                duration_seconds: 120.0,
                base_offset_sec: 0.0,
            }
        };
        let chapter_ep = |id: u32| IntroEpisode {
            media_file_id: id,
            duration_seconds: 2400.0,
            chapters: Some(vec![Chapter {
                start_seconds: start,
                end_seconds: start + chapter_len,
                title: Some("Intro".into()),
            }]),
            intro_fingerprint: Some(chapter_fp.clone()),
            outro_fingerprint: None,
        };
        let short = |id: u32, salt: i32| {
            let mut hashes = vec![salt; at + 16];
            hashes[at..at + 8].fill(7);
            IntroEpisode {
                media_file_id: id,
                duration_seconds: 2400.0,
                chapters: None,
                intro_fingerprint: Some(Fingerprint {
                    hashes,
                    duration_seconds: 40.0,
                    base_offset_sec: 0.0,
                }),
                outro_fingerprint: None,
            }
        };
        let res = detect(IntroDetectRequest {
            kind: IntroDetectKind::Intro,
            lookup_seconds: 600.0,
            min_segment_seconds: Some(1.0),
            episodes: vec![
                chapter_ep(1),
                chapter_ep(2),
                short(3, 3),
                short(4, 4),
                short(5, 5),
            ],
        })
        .unwrap();
        let stretched: Vec<_> = res
            .results
            .iter()
            .filter(|r| r.source == IntroSegmentSource::Pairwise)
            .collect();
        assert_eq!(stretched.len(), 3);
        for seg in stretched {
            let span = seg.end_seconds - seg.start_seconds;
            assert!(
                (span - chapter_len).abs() < 0.01,
                "file {} span {span} was not stretched to {chapter_len}",
                seg.media_file_id
            );
        }
    }

    #[test]
    fn validate_lookup() {
        let req = IntroDetectRequest {
            kind: IntroDetectKind::Intro,
            lookup_seconds: 0.0,
            min_segment_seconds: None,
            episodes: vec![],
        };
        assert!(validate(&req).is_err());
    }
}

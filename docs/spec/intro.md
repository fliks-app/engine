# Spec: intro and outro detection (M8)

`POST /v1/intro-detect`: the pure matching half of the host's intro-detection service, moved into `core::intro` and served as one control-plane route. The host collects the fingerprints (it already runs the `Fingerprint` job of `jobs.md` section 7), sends them as data with each episode's chapters and duration, and persists the returned segments as markers; the engine holds no library state. Announced in `hello.features` as `intro-detect` (always present: no tool, pure computation over the fingerprints the host already paid for). Tags are relative to `fliks/backend/src/` (`intro` = `modules/markers/intro-detection.service.ts`). Plan: sections 3, 5, 14 (M8).

## 1. Wire (`core::api::intro`)

```
POST /v1/intro-detect
  <- IntroDetectRequest { kind: "intro" | "outro", lookupSeconds: f64, minSegmentSeconds: Option<f64>,
                          episodes: [IntroEpisode] }
     IntroEpisode { mediaFileId: u32, durationSeconds: f64, chapters: Option<Vec<Chapter>>,
                    introFingerprint: Option<Fingerprint>, outroFingerprint: Option<Fingerprint> }
  -> { results: [IntroSegment] }     // only the episodes that resolved
     IntroSegment { mediaFileId: u32, startSeconds: f64, endSeconds: f64, confidence: f64,
                    source: "chapter" | "reference" | "pairwise" }
```

- `lookupSeconds` is the host's `introMaxLookupMinutes` × 60 (the fingerprints were taken with the same window; it clamps the intro's end). `minSegmentSeconds` defaults to 15. [ts:intro:144-150,67]
- `kind: "intro"` matches head fingerprints (`introFingerprint` required on every episode the host wants matched); `kind: "outro"` matches tail fingerprints (`outroFingerprint`). Episodes with a `manual` marker are simply not sent (the host never overwrites them). [ts:intro:155-163,568-576]
- 400 when: an episode's required fingerprint is missing, `lookupSeconds` ≤ 0, or `kind: "outro"` is sent with a `durationSeconds` ≤ 0 episode (the outro window is positioned from it).
- Route on the control plane (`/v1`), bearer auth like the others; computation runs on `spawn_blocking` with the pair loop parallelised over std threads (a 30-episode season is ~10^10 popcounts; the TS ran the same work on the Node event loop). [new: threading]

## 2. Algorithm (`core::intro`, one port, two kinds)

Constants [ts:intro:60-81]: `HAMMING_THRESHOLD` 6, `MAX_LAG_SAMPLES` 1440 (~180 s at chromaprint's fixed 8.13 hash/s), `MAX_PLAUSIBLE_INTRO_SECONDS` 120, `MAX_PLAUSIBLE_INTRO_START` 300, `CLUSTER_BUCKET_SECONDS` 15, `LEADING_LOGO_GUARD` 5, `SECONDS_PER_SAMPLE` 0.1238, `REF_MARGIN_SECONDS` 30. Samples ↔ seconds through `SECONDS_PER_SAMPLE` only (never `Fingerprint::duration_seconds`, which is the file's length, not the window's). [ts:intro:905-914]

1. **Chapter shortcut** (per episode, in file order): `kind: "intro"` scans chapters for the first whose title matches `\b(intro|opening|opening credits|main title|main titles|theme|titles?)\b` (case-insensitive) with 10 ≤ len ≤ 180 and start ≤ 360; `kind: "outro"` scans backwards for `\b(outro|ending|end.?credits?|credits?|closing)\b` with 10 ≤ len ≤ 300 and start ≥ 0.6 × `durationSeconds` (when duration > 0). A hit is a result with `confidence: 1`, `source: "chapter"`; the first hit is the **reference peer**. [ts:intro:165-246,578-635,993-1052]
2. **Reference window** (intros only): with ≥ 2 chapter hits, `[min(starts) − 30, max(starts) + 30]` and `median(lengths)`; outros skip it (their pairwise step handles the absolute offset via `baseOffsetSec`). [ts:intro:216-235]
3. **Reference search**: the peer's hashes inside its chapter window (intros: indices = seconds × samples; outros: minus the fingerprint's `baseOffsetSec`) slide over each remaining episode's fingerprint (`findByReference`, offsets 0..`min(len − refLen, secondsToSamples(maxStart))`, best offset wins, ≥ 30 % of the reference hashes matching the threshold, else no result). Intros report window-relative seconds; outros add the target's `baseOffsetSec` back. Result `source: "reference"`. [ts:intro:301-353,691-742,923-957]
4. **Pairwise fallback** over the episodes still unresolved (intros also skip the reference peer itself): `findAllMatches` slides A against B over lags −1440..+1440, collecting *all* runs of `popcount(a[i] ^ b[i+lag]) ≤ 6` of at least `max(8, secondsToSamples(minSegmentSeconds))` samples; per match, seconds are window-relative for intros (`baseOffsetSec` added for outros), a match is dropped when its length exceeds `MAX_PLAUSIBLE_INTRO_SECONDS` or (intros only) its start exceeds `MAX_PLAUSIBLE_INTRO_START`, and its confidence is `1 − hammingAvg / 6 / 4`. [ts:intro:368-403,744-773,855-903]
5. **Clustering and consensus**: candidates bucket by `round(start / 15) × 15`; three-tier pick (intros): biggest cluster inside the reference window (needs ≥ 2 votes), else biggest cluster at start ≥ 5 s (leading-logo guard), else biggest cluster overall; still < 2 votes: no result. Outros: biggest cluster with ≥ 2 votes, no tiers. Result: `median(start)`, `median(length)` (intros stretched to the reference window's median length when picked from its tier and shorter than 0.7 × it), `median(confidence)` clamped to [0, 1]; end clamped to `lookupSeconds` (intros); below `minSegmentSeconds` after clamping: no result. `source: "pairwise"`. [ts:intro:404-508,774-813,479-488]
6. Episodes the host sent but that resolved to nothing are simply absent from `results` (the host computes its own skipped count).

`popcount32`, `median` and `clamp` are the service's helpers, ported as-is. [ts:intro:971-986]

## 3. What stays host-side

Episode/season/file queries, the `Fingerprint` job submissions (host's `FINGERPRINT_CONCURRENCY = 4` batching loop), manual-marker protection, marker upsert (delete non-manual, insert), `confidence` clamping is in the engine, the detected/skipped counters and all activity events. [ts:intro:120-163,248-299,490-507]

## 4. Fixtures

`fixtures/intro/*.json`: the TS's own logs give no vectors, so the corpus is built from synthetic fingerprints (known intro at a known offset, logo at 0 s, cold-open variance, reference-window pick and stretch, outro absolute offsets through `baseOffsetSec`) plus golden vectors recorded from the TS service by extraction before the host's code is deleted (same harness rule as the M4 cleanup: run the TS pure parts once, commit the vectors, they become Rust-owned). [ts:intro:855-1052]

## 5. Deviations

- A control-plane route, not a job: the work spawns no process and holds no slot, so the job system's queue/pause/watchdog buy nothing; the plan's word "job" is read as "optional feature announced in `hello.features`". [new]
- The engine returns segments, not markers: marker rows, manual-marker protection and counters are the host's (`jobs.md` section 7 kept matching out of `Fingerprint` for the same reason).
- The TS logs one line per episode decision; the engine returns `source` per segment (chapter/reference/pairwise) so the host can log equivalently.

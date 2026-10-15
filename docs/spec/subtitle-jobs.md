# Spec: subtitle OCR and sync jobs (M8)

Two optional job kinds on the M5 job system (`engine::jobs`, `core::jobs`): `SubtitleOcr` turns an embedded image-based subtitle track into SRT text, `SubtitleSync` re-times a sidecar subtitle against the audio. Both announce themselves in `hello.features` only when their tools are on PATH (`subtitle-ocr`, `subtitle-sync`); a host that does not use them pays nothing. Results travel as wire data (the host persists the files), so M9's no-shared-filesystem rule holds. Tags are relative to `fliks/backend/src/` (`ocr` = `modules/subtitles/subtitle-ocr.service.ts`, `sync` = `modules/subtitles/subtitle-sync.service.ts`). Plan: sections 5, 14 (M8).

## 1. Wire (`core::api::jobs`)

- `JobKind::SubtitleOcr { info: MediaInfo, streamIndex: u32, language: Option<String> }`. [ts:ocr:119-143]
- `JobKind::SubtitleSync { info: MediaInfo, subtitle: SubtitleText, reference: SyncReference, maxOffsetSeconds: Option<f64>, noFixFramerate: Option<bool>, gss: Option<bool> }`. [ts:sync:37-48]
- `SubtitleText { format: String, text: String }` — the sidecar's lowercased extension (`srt`, `sub`, `ass`, `ssa`, `vtt`) and its content; the engine stages it to a temp file, the tools dispatch on the extension. Text is UTF-8 on the wire: the host re-encodes non-UTF-8 sidecars before submitting (it already owns `reencodeToUtf8`). [ts:sync:396-414]
- `SyncReference`, tagged by `type`: `{ type: "auto" }` (the media file itself), `{ type: "audioStream", index: u32 }`, `{ type: "subtitleStream", index: u32 }` (ffmpeg stream index in the media file), `{ type: "file", path: String }` (absolute, under the media root: the host resolves it before submitting). [ts:sync:225-252]
- Validation in `engine::jobs::submit`, 400 otherwise [ts:ocr:129-143] [ts:sync:95-97,219-222]:
  - OCR: `streamIndex` ≤ 999 and present in `info.subtitles`; the track is image-based; its codec is `hdmv_pgs_subtitle` or `dvd_subtitle` (DVB, XSUB and text tracks rejected before any work); `language` if present is a 2-letter IETF code.
  - Sync: `subtitle.text` non-empty, `subtitle.format` one of `srt|sub|ass|ssa|vtt`; `reference.file` is absolute.
- Results: `JobResult::SubtitleOcr { srt: String }` (the tools always produce SRT), `JobResult::SubtitleSync { subtitle: SubtitleText }` (ffsubsync and alass keep the input format), `JobResult::SubtitleToSrt { srt: String }`. [ts:ocr:303] [ts:sync:254-269]
- `hello.features` gains `subtitle-ocr` when the PGS path (`pgsrip`) or the VobSub path (`subtile-ocr` and `mkvextract`) is complete, and `subtitle-sync` when `ffsubsync` is on PATH (the alass fallback is compiled in, it needs no feature).

## 2. SubtitleOcr (`core::jobs::ocr`, `engine::jobs::ocr`)

Input: `media.path`, `info`, `streamIndex`, `language`. Output: the SRT text; no file in the engine cache (the host names, cleans and stores it as a library sidecar). [ts:ocr:224-261]

- Language: the request's `language`, else the track's 2-letter `language` tag, else `en` (these tracks are usually untagged `und`). It drives both the tool's language and the staged file name. The tesseract pack name comes from the ISO 639-1 → 639-2/T map of `TESSERACT_LANG` (33 entries), default `eng`. [ts:ocr:58-68,309-316]
- Staging under the engine cache: `tmp/ocr/<jobId>/ocr-<streamIndex>-<nano>/`, every artefact removed in the end (sup/idx/sub/srt/pngs). [ts:ocr:317,366-391]
- PGS (`hdmv_pgs_subtitle`): one ffmpeg run extracts the track (`-y -i <path> -map 0:<streamIndex> -c:s copy <base>.<ietf>.sup`, timeout 15 min), then one `pgsrip --language <ietf> <sup>` run (timeout 30 min). The SRT is the one file the run produced matching `<base>` prefix + `.srt` (pgsrip names it `<base>.<lang>.srt` in babelfish's form); none is `pgsrip produced no SRT for language "<ietf>" — <first 300 bytes of stderr/stdout>`. [ts:ocr:320-338,371-379]
- VobSub (`dvd_subtitle`): the media file must be Matroska (`.mkv`, `.mka`, `.webm` case-insensitive), else `VobSub OCR requires a Matroska (.mkv) source`. One `mkvextract tracks <path> <streamIndex>:<base>.idx` run (15 min); a non-zero exit is fatal only when the paired `.sub` is missing (mkvextract exits non-zero on warnings too). Then one `subtile-ocr -l <tesseract pack> -o <base>.srt <base>.idx` run (30 min). [ts:ocr:341-363]
- Every run goes through `Ctx::pipeline` (one background slot for the whole job, pause, watchdog, stop on cancel); the kind's own run timeouts are the extract's 900 s and the OCR tool's 1800 s of unpaused wall time (a dense 2 h track can take past 10 min). Progress: `pct` at stage boundaries only (extract done = 20), the tools batch the whole track and report nothing. [ts:ocr:30-34]
- Errors carry the tool name and are bounded (the watchdog's stderr tail, capped like the other kinds). The host keeps the FAILED row logic (its auto-pass needs the row to not re-arm a 30-minute OCR every sweep) — not ported. [ts:ocr:290-299]

Golden argv: `fixtures/jobs/ocr/*.json` (both codecs' extract and OCR argv, language fallback and pack mapping, VobSub non-mkv rejection, missing-SRT error). The `pgsrip`/`subtile-ocr`/`mkvextract` runs are canned (no real OCR in CI): the fixture records the argv and the produced file, the harness replays a fake binary like the other jobs.

## 3. SubtitleSync (`core::jobs::sync`, `engine::jobs::sync`)

Input: `media.path`, `info`, `subtitle`, `reference`, options. Output: the resynced subtitle text. [ts:sync:254-346]

- The engine stages the subtitle text to `<tmp>/sync/<jobId>/in.<format>`, output `out.<format>`, reference subtitle text (when given) to `ref.<format>`, removes them at the end. The media path (or the reference file) is read in place.
- ffsubsync argv: `<ref> -i <in> -o <out>` plus, when the reference is a stream, `--reference-stream stream:<index>`; `--max-offset-seconds <n>` when set; `--no-fix-framerate` when set; `--gss` when set. Timeout 900 s. [ts:sync:254-269,29-35]
- The retry matrix is the job's algorithm, decided in `core` and visible in fixtures:
  1. ffsubsync with the request's reference. Success: done.
  2. On failure matching `unable to detect speech` (case-insensitive) **and** the reference was `auto`: retry with `--reference-stream stream:<first audio stream index>` taken from `info.audio` (absent: skip to 3). Success: done.
  3. alass, compiled in (`alass-core` + `alass-util`, GPL-3.0, AGPL-compatible): the reference is the media file — one ffmpeg run extracts and resamples the audio (16-bit mono PCM on stdout, one background slot), `alass_util` builds the voice-activity reference timespans (webrtc-vad), `alass_core` aligns (offsets, splits, framerate), the engine applies the offsets to the parsed cues (`subparse`) and re-serialises. A subtitle-file reference aligns against that file's cues (no ffmpeg). A `subtitleStream` reference is extracted to SRT with one ffmpeg run, then aligned against those cues. Failure detail: `ffsubsync stderr: <…>\nalass error: <…>`, bounded. [ts:sync:284-345] [assumed: the alass-crate path reproduces the `alass` CLI's behaviour closely enough on the reference corpus — validated in PR 2 before the ffsubsync retry legs are kept]
- `auto` means ffsubsync probes the media file itself (it runs its own ffprobe); for the alass leg the engine extracts the first audio stream unless the reference names one. The engine's ffprobe/ffmpeg come from `FFPROBE_PATH`/`FFMPEG_PATH`. [ts:sync:276-281] [new: stream selection for the alass leg]
- The embedded-only rejection (`Cannot sync an embedded subtitle`), the `locked` flag, the queue, notifications and the `synced`/`syncFailed` row updates stay host-side: they are library state. [ts:sync:95-101,348-374]

Golden argv: `fixtures/jobs/sync/*.json` (the ffsubsync argv per reference shape and per option, the unable-to-detect-speech retry, the alass fallback leg's ffmpeg extract argv, staged file names, error text). The ffsubsync runs are canned; the alass leg is tested on a real 20 s chirp with a shifted SRT (real ffmpeg, no fpcalc).

## 4. Concurrency

The job system's own budget replaces the TS's dedicated gates: at most 2 background jobs run at once (`jobs.md` section 2) and every tool run holds one background slot (`streaming.ffmpegSlots`), so `subtitle_ocr_max_concurrency` (default 1) and sync's `MAX_CONCURRENT = 2` are not ported. [ts:ocr:83-96,178-196] [ts:sync:35,151-186]

## 5. Deviations

- Results travel as wire data, not as written files: the TS OCR wrote the SRT next to the video and the sync edited the sidecar in place; the host now does both from the returned text (M9: the engine may not share a filesystem with the host).
- Temp staging moves from `/tmp/fliks-ocr` to the engine cache (`tmp/ocr`, `tmp/sync`), so the watchdog's disk rules and cleanup apply.
- The alass fallback is the `alass-core`/`alass-util` crate, not the `alass` CLI binary the TS spawned; a subtitle-stream reference (audio-less) aligns against the reference subtitle's cues, which the CLI cannot do. [new]
- A `subtitleStream` reference is extracted from the container with one ffmpeg run (SRT to the job staging dir); only the sidecar `in.<format>` is written from wire data.
- `subtitle_ocr_max_concurrency` and the sync queue's `MAX_CONCURRENT` are dropped for the job system's budget (section 4).
- The OCR's PROCESSING/FAILED row bookkeeping, `subtitle.list_changed`/`subtitle.downloaded`/`subtitle.failed` events and `subtitle_ocr_delete_source` stay host-side (library state, no media tool).

## 6. SubtitleToSrt (`core::jobs::srt`, `engine::jobs::srt`)

Input: `media.path`, `info`, `source`. Output: SRT text. The host still reads an SRT sidecar itself. [ts:modules/subtitles/subtitle-translation.service.ts:409-437]

- `source` is `{ type: "file", path }` (absolute non-SRT sidecar) or `{ type: "stream", index }` (embedded text track of the job's media). A stream index above 999, missing from `info.subtitles`, or image-based is a 400. A relative file path is a 400.
- One ffmpeg run, timeout 120 s: `-y -i <path> -f srt pipe:1`, plus `-map 0:<index>` for a stream. The SRT is ffmpeg's stdout. No feature flag: ffmpeg is already required.
- Providers, batching, API keys, the placeholder row and the progress events stay host-side. [ts:modules/subtitles/subtitle-translation.service.ts:266-307]

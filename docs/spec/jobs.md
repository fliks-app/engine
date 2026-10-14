# Spec: background jobs (M5)

Work the host asks for outside playback: one generic job system (`engine::jobs`) with an extensible kind enum, a priority, cancellation, progress and a disk check, sharing one ffmpeg slot budget (`engine::slots`) with playback. Kinds: `Sprite` (the reference), `Scan`, `Fingerprint`, `SubtitleExtract`. Pure parts in `core::jobs`, IO in the engine. Tags are relative to `fliks/backend/src/` (`thumb` = `modules/streaming/thumbnail.service.ts`, `ext` = `modules/streaming/thumbnail-extractors/`, `slots` = `common/utils/ffmpeg-slots.ts`). Plan: sections 5, 14 (M5), M9.

## 1. Wire (`core::api::jobs`)

- `POST /v1/jobs` `JobRequest { kind, media: MediaRef, priority?, force? }` -> `{ jobId }`. `kind` is tagged by `type`: `{ type: "Sprite", info }`, `{ type: "Scan", info }`, `{ type: "Fingerprint", window: { type: "head" } | { type: "outro", durationSeconds }, maxLookupMinutes? }`, `{ type: "SubtitleExtract", info }` (`info` = `MediaInfo`). An identical request (same `kind` and `media`, priority and `force` ignored) still queued or running answers its `jobId`: one run, one `BgJobDone`. 400 on a relative path or an unusable request (sprite without a positive `info.durationSeconds`, scan without a video stream, an outro fingerprint without a positive `durationSeconds`, subtitle extraction with no text track in `info.subtitles` or a stream index above 999). [new]
- `GET /v1/jobs` -> `[JobSnapshot]` (oldest first), `GET /v1/jobs/{id}` -> `JobSnapshot` or 404. `JobSnapshot { jobId, kind, mediaFileId, priority, state, pct, createdMs, startedMs?, endedMs?, result?, error? }`; `state`: `queued`, `running`, `paused` (every run it holds is preempted), `done`, `failed`, `cancelled`. Ended jobs stay listed: the last 200. [new]
- `DELETE /v1/jobs/{id}`: 204 (a queued job leaves the queue, a running one is stopped through `proc` section 1's stop, an ended one is untouched), 404 when unknown. [new]
- Events: `BgJobProgress { jobId, pct }` at most once per second per job while it runs (`pct` 0 to 100, f64); `BgJobDone { jobId, state, result?, error? }` once per job (`state` done/failed/cancelled; `result` on done, `error` otherwise). Results are data (`JobResult`, tagged by `type`: `Sprite { sprite }`, `Scan { scan }`, `Fingerprint { fingerprint }`, `SubtitleExtract { streams }`): files a job writes stay in the engine's cache and are served by the data plane, never handed to the host as a path (M9: no shared filesystem). [new]
- `hello.features` gains `jobs`, `scan`, `fingerprint`, `subtitle-extract`.

## 2. Queue and lifecycle

- At most 2 jobs run at once (`SPRITE_CONCURRENCY` [ts:thumb:56]); the others wait in a queue ordered by `priority` (`high`, `normal` default, `low`), then arrival. [ts:thumb:245-270] (FIFO only there)
- A job that writes files first checks the free space of the cache root's filesystem: below `streaming.jobMinFreeBytes` (default 1 GiB, 0 disables) it ends `failed` with `not enough disk space: <free> bytes free under <dir>, jobMinFreeBytes is <min>`. Checked once the job runs, before it writes (an existing sprite, or subtitle tracks all cached, answered without a write skip it). [new]
- Cancellation (DELETE, `POST /v1/invalidate` of its file, engine shutdown) stops the job's ffmpeg runs (SIGTERM, SIGKILL after 2 s), removes its temporary files and ends it `cancelled`. Shutdown awaits every job. [new]
- Invalidate also deletes the file's engine sprite (`sprites/v1/<file>`). [new]

## 3. ffmpeg slot budget (`engine::slots`)

One budget for every ffmpeg the engine runs: playback (session runs, I-frame and subtitle extracts), background job runs and probe crop samples. [ts:slots]

- Size: `streaming.ffmpegSlots` when set and > 0, else `max(1, cores - 1)`, `cores` = `available_parallelism` (CPU affinity and cgroup v1/v2 quota already applied). Read at every `PUT /v1/config`. [ts:slots:47-54,79-85]
- Playback never waits: it takes a slot at once, even past the size. A throttled (paused) session run gives its slot back while paused. [new] (the TS pool held background work only)
- Background: `allowed = size - playback in use` (floor 0). Holders are granted FIFO within a priority, newest last; the first `allowed` holders run, the ones past it are preempted: paused through the host's pause capability (`signal`: SIGSTOP the group, `stdin`: `p`) and resumed (`u`, SIGCONT) when playback releases its slots, oldest first. Without a pause capability a preempted run keeps going (it runs under `ionice -c3 nice -n19` on Linux), and no new background run starts until `allowed` grows. [ts:slots:93-122] (FIFO, no preemption)
- A shrunk size preempts the same way (the TS lets running work drain). A queued waiter that gave up (cancelled) is skipped.
- Probe crop samples take background slots (they used their own `cores - 1` semaphore). [ts:modules/media-probe/ffprobe.service.ts]

## 4. Background runs

- Spawned through `proc::Ffmpeg` (own process group, kill-on-drop, `PR_SET_PDEATHSIG`) behind `ionice -c3 nice -n19` when both are on PATH (Linux). [ts:common/utils/spawn-priority.ts] Every tool of a job goes through `Ctx::pipeline`: one slot for one process or for a pipeline of several (ffmpeg + fpcalc of a window), paused and stopped together. Binaries: `ffmpeg` (`FFMPEG_PATH`), `ffprobe` (`FFPROBE_PATH`), `fpcalc` (`FPCALC_PATH`, default `fpcalc`).
- Every 200 ms: the slot's pause verdict is applied; `core::session::job_watch` (stderr lines and CPU time are progress, paused time exempt, `jobStallTimeoutMs`) kills a hung run, and a run past its timeout of unpaused wall time (30 s for a sprite frame or tile, the kind's own below) is killed. Either is a run failure (`<tool> stalled` / `<tool> timed out`). Bytes of the source read and lines parsed count as progress for the watchdog (`Ctx::pulse`), as stderr lines and CPU time do. [ts:thumb:672-690,735-745]

## 5. Sprite (`core::jobs::sprite`, `engine::jobs::sprite`)

Input: `media.path`, `info`. Output: `<cacheRoot>/sprites/v1/<mediaFileId>/sprite.jpg` and `sprite.json`; result `{ type: "Sprite", sprite: SpriteMeta }`, `SpriteMeta { interval, columns, thumbWidth, thumbHeight, count }` = the `sprite.json` text. [ts:thumb:35-41]

- Existing: without `force`, a readable `sprite.json` ends the job `done` at once with its contents. [ts:thumb:163-174]
- Duration `d` = `info.durationSeconds`. Interval: `d < 300` -> 2, `d <= 3600` -> 5, `d <= 7200` -> 15, else 30. `count = ceil(d / interval)`, `columns = 10`, `rows = ceil(count / 10)`, `thumbWidth = 240`. [ts:thumb:285-287,780-785]
- Seek offset = `input_seek_seconds(0, source_timeline(info))` (tile times count from the first frame); HDR = `video[0].colorTransfer` is `smpte2084` or `arib-std-b67`; crop = `video[0].crop`. [ts:thumb:139-150,43-47]
- Frame `i` (0-based) at `i * interval + offset`, written to `frames/frame-<i+1, 4 digits>.jpg`, one ffmpeg each, `min(slots size, count)` workers. A failed frame is logged and extraction goes on; missing frames are then filled from the nearest present one (lower first at equal distance) so the tiler's sequence has no gap. The job fails only when no frame at all was extracted (`Sprite failed: no frame could be extracted (<n> attempts)`). [ts:thumb:495-660] (the TS stops at 5 failures and fails above `count * 0.1`)
- Extractor (`pick_extractor`): darwin: VideoToolbox; linux: VAAPI on the detected render node when the host runs `vaapi` or `qsv` and there is no crop, QSV for a crop on a `qsv` host, CUDA on an `nvenc` host, else software; software whenever a playback run holds a slot (the GPU belongs to the transcode). [ts:ext/index.ts:20-75] [ts:thumb:103-113]
- Argv per extractor: [ts:ext/sw.extractor.ts] [ts:ext/vaapi.extractor.ts] [ts:ext/qsv.extractor.ts] [ts:ext/cuda.extractor.ts] [ts:ext/videotoolbox.extractor.ts] [ts:ext/tonemap.ts]; seek printed as JS `String(x)`.
- Tile: `-nostdin -hide_banner -loglevel error -start_number 1 -i <frames>/frame-%04d.jpg -vf tile=10x<rows> -q:v 5 -y <out>`. [ts:thumb:710-735]
- `thumbHeight = round(jpegHeight / rows)` from the tiled JPEG's frame header, 90 when unreadable. Written as `sprite-tmp.jpg` then renamed, `sprite.json` last; frames removed. [ts:thumb:404-420]
- Progress: `pct = 100 * frames done or failed / count`, capped at 99 until the tile lands.
- Serving: `/api/stream/:id/thumbnails/sprite.{json,jpg}` still asks the resolver (`purpose: sprite`, a grant without `path` is accepted for this purpose only), then serves the engine's file when present, else the resolver's `path` (sprites the host generated before M5), else 404.

Golden vectors: `fixtures/jobs/sprite/*.json` (layout, frame argv, tile argv, seek offset, backfill), recorded once from the TS, Rust-owned since (`BLESS=1`).

## 6. Scan (`core::jobs::scan`, `engine::jobs::scan`)

Input: `media.path`, `info` (a video stream). Output: result `{ type: "Scan", scan: SourceScan }` (`schemaVersion` stamped), the decide input's `scan`; no file. Argv, reader rules and timeouts are `docs/spec/jobs-pure.md` section 1. [ts:modules/streaming/services/source-scan.ts, video-packets.ts]

- Reorder lead: known from `info` when it has `reorderFrames` and `avgFrameRate`, else one `ffprobe` read of the stream (30 s), first line.
- Packets: `ffprobe` on the file (stdout read line by line, `PacketReader::feed_line` until the clock breaks: ffprobe is then stopped), timeout `scan_timeout_ms(size)`. Progress: `pct` = source time read / `info.durationSeconds`, capped at 99.
- MPEG-TS with AAC tracks (every OS): a first short `ffprobe` (`stream=index,id`, 30 s) gives each AAC track's PID. Then the engine reads the file once, in 1 MiB chunks, and feeds the same bytes to ffprobe (`pipe:0`) and to a `core::jobs::ts::TsDemuxer`, which walks the ADTS headers of those PIDs with one `AdtsConfigWalker` each (no extra process). ffprobe stopping (clock break) stops the read. `audioConfigChanges` is `true` for a track with no PID in the ffprobe answer, whose PES cannot be parsed or whose PID the PMT says is not ADTS; the other tracks keep their own verdict. The packet pipeline = one slot.
- Failure: a non-zero ffprobe exit not asked for is `ffprobe exited N: <stderr>`; no packet with a time is `no video packet with a time`.

## 7. Fingerprint (`core::jobs::fingerprint`, `engine::jobs::fingerprint`)

Input: `media.path`, `window`, `maxLookupMinutes` (absent or 0: 10). Output: result `{ type: "Fingerprint", fingerprint: Fingerprint }`; no file. [ts:modules/markers/intro-detection.service.ts:815-940]

- `head`: `fpcalc -raw -length <lookup> <path>` (the file's first `lookup` seconds, `baseOffsetSec` 0), 180 s.
- `outro { durationSeconds }`: the window `[max(0, d - lookup), lookup]`, ffmpeg's WAV on stdout piped into `fpcalc -raw -length <lookup> -` (one slot for the pair, 180 s), `baseOffsetSec` = the unfloored start. Progress: WAV bytes piped / the expected size, capped at 99.
- Failure: fpcalc exiting non-zero is `fpcalc exited N: <fpcalc stderr, else ffmpeg's>`; no `FINGERPRINT=` line is `fpcalc returned no FINGERPRINT line`.
- Matching, clustering and markers stay on the host.

## 8. SubtitleExtract (`core::jobs::subtitles`, `engine::jobs::subtitles`)

Input: `media.path`, `info` (its non-image `subtitles` are the tracks). Output: `<cacheRoot>/subtitles/v2/<mediaFileId>/<stream>.vtt` per track; result `{ type: "SubtitleExtract", streams }` = every text track's stream index, served by the subtitle routes (`embedded/{idx}`). [ts:modules/streaming/subtitle-stream.service.ts]

- Tracks already cached are skipped unless `force`; none left to extract: `done` at once. Otherwise the disk check, then one ffmpeg run for every missing track (`extract_args`, raw ASS next to the targets, 15 min), `check_extracted`, `ass_to_vtt`, written atomically (`<idx>.vtt.<rand>` then rename), raw files removed. One bad track (empty file, write error) does not hold back the others: the job then fails with `stream <n>: <error>` for each. Progress: tracks converted / tracks, capped at 99.
- Unified path: `GET .../subtitles/embedded/{idx}` on a cache miss extracts the one track with the same `extract_args` (to `pipe:1`), `check_extracted` and `ass_to_vtt`, and stores the same file, so the served text equals the job's output byte for byte (`runtime_jobs`, `runtime_e2e` compare them). `X-TIMESTAMP-MAP` is added at serve time only. External subtitles still go through `ffmpeg -f webvtt`.
- The cache is `subtitles/v2`: the `-f webvtt` extracts of `subtitles/v1` are purged at start.

## 9. Adding a kind

A variant of `core::api::JobKind` (and `JobResult` when it returns data), its validation in `engine::jobs::submit`, its pure part in `core::jobs`, a branch in `engine::jobs::run` calling its module. That module gets a `Ctx`: `ffmpeg(argv)` for one ffmpeg run and `pipeline(timeout, cmds, work)` for any tools (one background slot, pause, watchdog, stop on cancel; `Cmd` names the binary and whether its stdout is exposed; `work` drives them), `pulse()`, `progress(pct)`, `cancelled()`, `check_disk(dir)` before writing under the engine cache, `media_path()`.

## 10. Deviations

- Playback counts against the budget and preempts background runs; the TS pool held background work only, FIFO, with no pause. A shrunk budget pauses (TS: drains).
- The sprite extractor follows the detected backend and render node (TS: `/dev/dri/renderD129`/`128` and `/dev/nvidiactl` existence, `THUMB_HWACCEL_DEVICE`); QSV crop only on a `qsv` host, nothing but software on Windows (as the TS).
- Sprite failed frames: as the TS backfills, but extraction never stops at 5 failures and there is no `count * 0.1` threshold; only a job with no frame at all fails.
- `thumbHeight` comes from the JPEG header, not `ffprobe`; per-run timeouts count unpaused time only; no global 10-minute sprite timeout (the watchdog covers hung runs).
- Sprites live in the engine cache (`sprites/v1`), not `<cacheDir>/thumbnails`; the host's activity/command rows and `task.progress` become `BgJobProgress`/`BgJobDone`.
- Scan: the engine reads the file itself (tokio) for the piped MPEG-TS case, not `cat` under `ionice`, and a native demuxer reads the ADTS headers in that one pass on every OS (the host runs a second ffmpeg). Progress and the slot, pause and watchdog framework are new.
- Fingerprint: the head read and the window pipeline run under a background slot (the TS: `withFfmpegSlot` too), ffmpeg of a window is stopped once fpcalc has its `-length`.
- SubtitleExtract: a job form of the host's warmup (no Command rows, `subtitlePrewarm` stays the host's call); a track missing from `info` is not extracted.

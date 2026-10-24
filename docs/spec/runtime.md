# Spec: runtime (M3)

The engine's stateful half: ffmpeg jobs, playback sessions, the `cache/v1` directory, the data plane and the event stream. Pure rules come from `core::{decide,session,args,hls,mp4}`; this layer owns processes, disk, clock and HTTP. Tags are relative to `fliks/backend/src/modules/streaming/` (`ctl` = `streaming.controller.ts`, `ts` = `transcoding/transcoding.service.ts`, `live` = `live-session.service.ts`, `router` = `services/session-router.service.ts`, `pkg` = `services/segment-packaging.service.ts`). Plan: sections 4, 5, 6, 7, 16. Remux runs: section 9; the open report: section 10.

## 1. Processes (`engine::proc`)

- Every ffmpeg runs through `proc::spawn` (ETXTBSY retry), stdin piped (pause keys), stdout null, stderr piped, `kill_on_drop`. Unix: its own process group (`process_group(0)`), signals go to the group. Windows: assigned to the engine's kill-on-close Job Object (shared with the supervisor). [new]
- Every run holds a playback slot of the engine's one ffmpeg budget while its process lives (lent back while throttled), and so do I-frame and subtitle extracts; background jobs yield to them (`docs/spec/jobs.md` section 3). [new]
- stderr feeds a bounded ring (last 40 lines, 300 bytes each): `JobStats.stderrTail` and the failure log. [ts:ts:1199-1250] keeps the whole text; bounded is [new].
- Stop: unix SIGCONT (a stopped process ignores TERM) then SIGTERM, SIGKILL after 2 s; windows `q` on stdin, kill after 2 s. The caller awaits the exit before touching the directory again. The TS sends SIGKILL at once [ts:ts:1671-1702]; the plan asks for a trailer-writing stop [new].
- Pause per `Capabilities.pauseCapability` (`core::session::pause_command`): `stdin` writes `p`/`u`, `signal` sends SIGSTOP/SIGCONT to the group, `none` does nothing. [ts:transcoding/ffmpeg-pause.ts:15-40]
- Exit is published once: `{ code?, signal }` (`signal` = killed by a signal, code unset).

## 2. Jobs (`engine::runtime`)

A job is one ffmpeg run of one session key: `<file>-u<owner>|anon-<profileHash>` (`core::session::session_key`, main variant). It writes the rung directory `cache/v1/<u<owner>|anon>/<file>/<hash>/<quality>` (`core::session::cache_path`); the var_stream_map layout (more than one audio track) writes `0/` (video, `init_0.mp4`) and `<i+1>/` (audio track i, `init_<i+1>.mp4`). [ts:ts:816-845] [ts:transcoding/audio-layout.ts:20-25]

- argv: `core::args::vod` with the session's frozen context: profile = the rung (bitrates as plain bps), hwAccel = the detected backend, start segment, tonemap = `decision.transcodeTonemapping`, crop only when `autoCropEnabled`, audio streams and plans of the decision, timeline (`core::mp4::source_timeline`), fps (`parse_source_fps`), colour tags, `trustedStreamInfo` when the probe named a video codec, `encoderPreset` = `qsvPreset`, `seekKeyframeDts` = the last scan keyframe at or before the start for an MPEG-TS source. [ts:ts:1740-1804] [ts:services/session-context-builder.service.ts:31-80]
- Production frontier: a watcher lists the job's directories every 200 ms while ffmpeg runs and once after it exits; each `seg-<n>` file (ffmpeg renames `*.tmp` when complete, `temp_file`) goes into the track's `SegmentSet`. Waiters are woken on every change and on exit. [ts:ts:1082-1135] (fs.watch there) [new]
- Start (`start_job`): kill the key's previous run and await it, `purge_from(start)` on disk and in memory (one timeline forward of the start), mkdir, spawn. [ts:ts:790-845]
- HW fallback: when the run's hwAccel is not `none` and ffmpeg exits non-zero before its first segment landed, the job is respawned with `hwAccel = none` at the same start; `JobStats.actualHw` becomes `none`, a `HwFallback` event carries the stderr tail. [ts:ts:870-940]
- Ownership: a job belongs to the live sessions of its file, owner and profile hash (main, early and remux runs alike). A session ending for any reason (DELETE or `stop`, expiry, owner-cap eviction, revoke) stops, through section 1's stop, every job no other live session owns, before the call returns (the cache stays); invalidate and engine shutdown stop every job of the file or all. Each `liveSessionGcIntervalMs` tick also stops the jobs no live session owns (a run a request registered after its session ended). `core::session::job_idle` and `jobGraceMs`/`jobFallbackTimeoutMs` are no longer read. [new] (the TS keeps the job through `STREAM_JOB_GRACE_MS` [ts:ts:1600-1669])
- Watchdog: every 200 ms the watcher runs `core::session::job_watch` on a running, not paused job. Progress = a new or removed output file or init, a `.tmp` file that grew, a stderr line, or any CPU time burnt by the process since the previous tick (Linux: utime + stime + reaped children from `/proc/<pid>/stat`, threads included; macOS: `proc_pid_rusage` user + system + children; Windows: `GetProcessTimes` user + kernel; a slow encode that buffers a whole segment in memory and prints nothing is not stalled, a hung one with every thread in futex wait is). A resume from a pause restarts the clock. No progress for `jobStallTimeoutMs` (default 30 s, 0 disables) is `Stalled`; an early companion (natural end: `-to`, `core::session::early_run_seconds` of media) still running `jobStallTimeoutMs` after that media time is `Overran`. Either stops the process through section 1's stop (SIGTERM, SIGKILL after 2 s, since a hung ffmpeg ignores TERM); the run is not marked killed, so it ends `Failed` (`JobEnded.reason` = `stalled` or `overran`) and the next segment request restarts it through the usual matrix (crashed run: respawn, its directory purged; a hardware run without a first segment falls back to the CPU). [new]
- Children never outlive the engine: every spawned process asks the kernel for SIGKILL when the engine dies (Linux `PR_SET_PDEATHSIG`), so a SIGKILL or crash leaks nothing; iframe and subtitle extracts are request-bound (`kill_on_drop`, 20 to 300 s timeouts). [new]
- Throttle: every 5 s each running job runs `core::session::throttle_step` (viewers' positions and last requested segments, uniform grid, the job's `SegmentSet`, `throttleEnabled`, `throttleThresholdSeconds`, the pause capability); Pause/Resume apply section 1. [ts:transcoding/ffmpeg-throttle.service.ts:21,94-150]
- `transcodePercent` = `min(100, segment_index_to_seconds(latest + 1) / duration * 100)`. [ts:ts:621-637]

## 3. Sessions

`POST /v1/sessions` (`OpenSessionRequest`): `decide(request, detected hostCaps, config.streaming)` then a session holding the request, the `Decision`, the frozen `StreamingConfig` (config revision), owner, tags, client. 400 before the first `PUT /v1/config`, 503 + `Retry-After: 1` before detection ends.

- kind: DirectPlay `directplay` (no hash), DirectStream `remux`, Transcode `transcode`. [ts:ctl:1105-1124]
- profile hash: `core::session::profile_hash(profile_from_layout(..))` over the decision's layout (mux, audio plan and per-track plans with their encoder ids, variant, timeline, `sourceVersion`, DV, tonemap, curve), segment duration in ms. [ts:ctl:1083-1112] [ts:transcoding/session-profile.ts:56-117]
- A second live session of the same (owner, file, hash) gets `~i<8 alnum of its sid>` appended: its own job. [ts:live:409-432]
- Owner cap: at `maxSessionsPerOwner` the owner's oldest-beaten session is evicted (`session_to_evict`, `SessionEnded{reason: "evicted"}`). [ts:live:434-450]
- `playPath` = `decision.response.playUrl` + `sid=<sid>` (`?` or `&`). [ts:ctl:1236-1245]
- Prewarm: Transcode with a `startQuality` other than `auto`, `remux`, `original` spawns the pinned rung (`hls::pinned_rung_name`) at the segment of `startAtS`. [ts:ctl:612-690]
- Heartbeat `{ ownerId?, positionS?, state?, quality? }`: unknown sid (and not revivable) answers `{ sessionLost: true }`; an owner other than the session's is 403. [ts:live:462-520]
- Touch: every data-plane request with a known `sid` refreshes `lastBeat`; segment requests record the index (throttle playhead). [ts:live:524-547]
- GC: past `liveSessionTtlMs` (`pinnedSessionTtlMs` when pinned) a session becomes a tombstone (`SessionEnded{reason: "expired"}`); a presented sid revives it within `sessionReviveTtlMs` (`tombstone_revivable`). [ts:live:571-590,642-670]
- Command `stop` = DELETE; `pauseJob`/`resumeJob` apply section 1 to the session's job. DELETE is idempotent (`?reason=`, default `stopped`): the session and its tombstone go and its jobs stop (section 2, Ownership). [ts:live:553-558]
- Revoke `{ ownerId?, mediaFileIds? }` (at least one): matching sessions are removed (later requests with their sid get 410), their jobs killed, the resolver cache entries of that owner/files purged. Invalidate `{ mediaFileId }`: every job of the file killed, its cache directories and extracted subtitles deleted, resolver entries purged; sessions stay. [new]
- `GET /v1/sessions[?ownerId&mediaFileId]`, `GET /v1/sessions/{sid}`: `SessionSnapshot` with `JobStats` of the session's job. Times are f64 epoch ms.

## 4. Seek and quality switch

A video segment request runs the TS order [ts:ctl:2245-2545]:
0. A segment past the uniform grid's last one: 404 before anything runs [new] (the TS only guards the remux grid [ts:ctl:551-565]).
1. Job of the session key on this quality and the segment in its set: serve.
2. No job on this quality, but its rung directory holds a non-empty file: serve it without spawning (ABR switch back [ts:ctl:2380-2395]; also a resume after the job was reaped [new]: the TS respawns there).
3. Otherwise, with a job on another rung: kill it (awaited) and start at the request's anchor (the TS inherits the killed run's start for a `seg-0` request too [ts:ts:757-772]; here an init already asks for its resume floor). With a job on this rung: `core::session::resolve_existing` (`exitCode`, `signal`, start segment, set). `Serve`/`Wait` attach; `Respawn { kill, startSegment, purgeCache }` kills (awaited), purges the rung directory when asked (a crashed run), and starts at `startSegment ?? job start`. Without a job: start at the anchor. [ts:ts:498-588,743-860]
4. The wait follows a replacement on the same rung (a CPU fallback registers before the failed run's waiters wake).
- Anchor: a segment anchors at itself; an init at the resume floor `max(job start, segment_of(position))`. [ts:ctl:428-512]
- Early companion (`<hash>-early`, `JobKind::Early`): a VOD run of the requested rung at segment 0 with `early: true` (input `-to` of two segments plus 1 s, `veryfast`), never throttled. Reused while it runs or after a clean exit at the same rung, else killed, its directory removed, and spawned again. [ts:ts:965-1060]
  - Video init with a main of that rung: the early's init first (10 s wait), then the main's. In the slow path (no file on disk), with `probesSegZero` (default true): an init without a same-rung main, or `seg-0`/`seg-1` while the resume floor is past segment 2, is served by the early (spawned when absent or on another rung, 10 s wait), falling through to the main when it does not land. [ts:ctl:2289-2345,2409-2480]
  - Audio rendition: when the main started past 0, its init and `seg-0`/`seg-1` come from the early's `<i>/` directory first. [ts:ctl:2008-2060]
  - Prewarm: a pinned rung resumed past segment 0 starts the early next to the main for `probesSegZero` clients. [ts:ctl:678-693]
- A session's served kind (`SessionSnapshot.kind`, the job snapshots and commands follow) is set by each video request: `remux` or a rung. [ts:ctl:2274-2282]

## 5. Cache (`engine::cache`)

- Root: `FLIKS_TRANSCODE_DIR`, else `FLIKS_CACHE_DIR`, else `<os temp>/fliks-transcode`. Layout `<root>/cache/v1/...` (`core::session::CACHE_NAMESPACE`). [ts:../../common/constants/paths.ts:8-16]
- Boot: every entry of `<root>/cache/` other than `v1` is deleted once (the old layout). [new]
- GC every `cacheGcIntervalMs`: index = one entry per `<owner>/<file>/<hash>` dir (bytes, newest mtime), `live` when a job or session uses it; `core::session::plan_gc` with `CachePolicy::from_config`. [ts:transcoding/transcode-cache.service.ts:268-306]
- `GET /v1/cache` -> `{ usedBytes, entries[] }`; `DELETE /v1/cache[?mediaFileId&ownerId]` removes the matching non-live entries -> `{ removed }`.

## 6. Data plane (`engine::serve`)

Routes are the host's, byte for byte (`/api/stream/...`, plan section 16), `GET` and `HEAD`. Prefix in playlists: `/api/stream/<id>`; token suffix `?token=..&sid=..` from the request (`encodeURIComponent`). [ts:ctl:161-199]

- Auth: `credential` is one string as received, in the host's extractor order: the `Cookie` header, else a `Bearer` `Authorization` value, else `?token=` [ts:../auth/strategies/jwt.strategy.ts:43-47]. A request carrying any cookie sends it, even next to a token: the host must find its access cookie in it. `POST {hostUrl}/engine/resolve` (`Authorization: Bearer {hostToken}`) `{ mediaId, credential, purpose, subtitleId?, streamIndex? }` -> `{ allow, path?, ownerId?, format?, ttlS? }`, cached per (SipHash of the credential, mediaId, purpose, subtitleId, streamIndex) for `ttlS` (60 s default), denials too. `allow: false` = 403. A transport error, timeout (5 s) or non-2xx is never cached: an expired grant keeps serving, a live session of that file (by `sid`) keeps serving, else 503 + `Retry-After: 2`. [new] (plan section 7)
- sid: unknown and not revivable = 410 `{ code: "session_expired", sid, message }` [ts:session-expired.exception.ts]; a session of another owner or file = 403. No sid: the owner's most recent session of the file. HLS routes without any session: 410 (sid `null`). [ts:router:30-120] [ts:ctl:500-508]
- master `[ts:ctl:1402-1585]`: `core::hls::master_playlist` from the session (source size with crop, `remux=1`, audio layout and plans, `startQuality`, picked audio, HDR pass-through and `canEmitHdrLadder` = `resolve_encoder`, SDR variant, frame rate, I-frame grid when `supportsIFrameTrickPlay`); then the prewarm of section 3 for `startQuality`.
- variant / audio / iframe playlists: `hls::variant_playlist`, `audio_playlist`, `iframe_index`; duration = `?duration=` else `info.durationSeconds`, 404 without one. Invalid quality = 400. [ts:ctl:2126-2242,1855-1906,1586-1622]
- segments (`remux/*` and a remux session's audio renditions: section 9): section 4, then the wait (60 s; init: 5 s more to be non-empty), `core::session::segment_response` (503 + `Retry-After: 2` while producing or after an intentional kill, 404 when over). Served bytes: TS verbatim, fMP4 through `mp4::rewrite_cmaf` and `rewrite_segment_tfdt(index, real_segment_seconds, served_origin)` (init tracks parsed once per directory). [ts:pkg:30-130]
- Headers: playlists `application/vnd.apple.mpegurl`; segments `video/mp4` or `video/MP2T`; all of them `Access-Control-Allow-Origin: *` and `Cache-Control: no-store`. [ts:ctl:238-246]
- iframe segment: `core::args::iframe` at `input_seek_seconds(n * real_segment_seconds)`, stdout as `video/mp2t`, `Cache-Control: public, max-age=3600`; failure 503. [ts:ctl:1625-1680]
- Direct play `:id` (`purpose: stream`, `download` with `?download=1` + `Content-Disposition: attachment`): RFC 7233 single range (`bytes=S-E`, `S-`, `-N`), 206/416, `Accept-Ranges: bytes`, `X-Content-Duration` from the session's info. Content type: the resolver's `format` when it is a MIME type, else by extension. [ts:ctl:2555-2641] [ts:byte-range.util.ts:16-50]
- Sprites (`purpose: sprite`, `path` = `sprite.jpg`, `sprite.json` its sibling, neither checked by the host): 404 when missing, `Cache-Control: public, max-age=86400`. [ts:ctl:1363-1395]
- Subtitles (`purpose: subtitle`): external `:subId` (`subtitleId`; `path` = the file, `format` its extension), embedded `embedded/:idx` (`streamIndex`; `format: embedded`, `path` = the media file). WebVTT: `.vtt` verbatim (BOM, CRLF normalised), anything else through ffmpeg `-f webvtt`; an embedded track is extracted as ASS and converted by `jobs::subtitles::ass_to_vtt` (the `SubtitleExtract` job's path, `jobs.md` section 8) and cached under `<root>/subtitles/v2/<file>/<idx>.vtt`. `X-TIMESTAMP-MAP=MPEGTS:<round(max(0,o)*90000)>,LOCAL:<hh:mm:ss.mmm of max(0,-o)>` after the `WEBVTT` line, `o` = `cue_offset_seconds` of the latest session of (file, owner), else 0. `/download` serves the original file (external) or the VTT (embedded) as an attachment; `/index.m3u8` is `hls::subtitle_playlist`. [ts:ctl:119-130,1682-1849]
- CORS: `Access-Control-Allow-Origin: *` on every data-plane answer, as the host does for HLS.
- Media paths go through `map_media_path` (`EngineConfig.pathMappings`: longest `from` prefix that ends on a separator; absent, the path is unchanged). Nothing assumes localhost: `hostUrl` is whatever the host registered.

## 7. Events

`GET /v1/events`: SSE, one `EngineEvent` JSON per `data:` line, event name = `type`. `SessionOpened`, `SessionEnded{reason}` (`stopped`, `expired`, `evicted`, `revoked`), `PipelineChanged` (a session's served rung or hw changed), `SegmentServed{track, rung, index, waitedMs, cacheHit, cmcdRaw}` (`cmcdRaw` = `?CMCD=` else the `CMCD-*` headers joined by `,`), `JobStarted`, `JobEnded{state, exitCode?, reason?}` (`reason` only when the watchdog ended the run), `HwFallback`. A lagging subscriber has its stream closed; on reconnect the host lists `GET /v1/sessions`. [ts:ctl:529-547] (plan section 6)

## 8. Deviations

- Live TV is its own runtime: `docs/spec/live.md`.
- A remux file or a rung file already on disk is served without a run; the early's init gets the 10 s early wait in front of the main's (the TS waits 60 s).
- A crashed run's rung directory is purged by the matrix (`purgeCache`); the session and the cache entry survive and the next request respawns.
- Stop is SIGTERM then SIGKILL (TS: SIGKILL); subtitles other than WebVTT are converted by ffmpeg, not by the host's SRT/ASS converters.
- The production frontier comes from a 200 ms directory poll, not inotify.

## 9. Remux runs (`engine::remux`, `JobKind::Remux`)

DirectStream sessions copy the video. [ts:ts:1406-1580] [ts:ctl:555-600,1885-1900,2215-2240]
- Grid: `core::mp4::remux_segment_grid(scan, origin, segmentDuration, frameRate)` frozen at open for a `remux` session (`SessionPlan.remux_grid`); without a scan, the uniform grid at `segmentDuration`. The variant and audio playlists of a remux carry the grid durations; a segment past the grid is a 404 before any run.
- Key and directory: `<hash>-remux[-a<N>][-u]` (`remux_variant_of`), served files in `<entry>/remux/` (`init.mp4`, `seg-NNNN.m4s`, audio rendition `i` in `<i>/init_<i>.mp4` and `<i>/seg-NNNN.m4s`); each run writes its GOP files into its own `remux/gop-<random>/`, removed when the run ends.
- Run: `remux_run_start(grid, start, segmentDuration, origin, remux_audio_grid(basis, libfdk))` (basis: the group's transcoded plan, else the picked one), `core::session::remux_args` then `core::args::remux`; `remux_assembly_plan` with the multi-audio rendition count.
- Assembly: the watcher pumps the assembler every 200 ms (blocking pool): open the run on its first GOP (served init written once), assemble each segment whose last GOP is complete (retimed moofs, mdat copied through), group every rendition's frames; three failures in a row stop the run and unregister it. On exit: one more pass, the tail of a clean run, GOP directory removed; the job stays producing (`pending`) until then.
- Requests: a served file on disk is returned as is (every run lands on the same timeline). Else `resolve_existing` with the assembler's `RemuxProgress` for the track (an exited run is awaited until its assembly ended): reuse while `can_serve`, else kill and restart at the request (an init: the remux resume floor on the grid). Serve: CMAF rewrite only, never a tfdt re-anchor.
- Throttle: the assembler's span; a pause freezes its throughput clock.
- `startAt` on the remux variant playlist starts the run at that time's grid segment unless one runs. [ts:ctl:2155-2180]

## 10. Session open report

`OpenSessionResponse` adds what the host's playback-info returns beside the decision (`core::session::open_report`, detected host caps, the session's burn-in): `playbackClockOffsetS` = `formatStart - origin` (0 on Direct Play) [ts:edl/edl-sidecar.ts:94-110]; `tonemapAlgo`/`tonemapCurve` = `resolveTonemapReport` of the decision's variant when `response.tonemapping` (the curve only on opencl/vulkan/cpu/cuda and not for a no-base DV HDR10 target) [ts:transcoding/encode-pipeline.ts:306-358]; `qoePipeline` = `describeQoePipeline` (stages from the same `resolve_pipeline` and decoder pick as the argv) [ts:transcoding/qoe-pipeline.ts:36-76] [ts:ctl:1086-1196]. Golden vectors: `fixtures/report/open-report.json`.


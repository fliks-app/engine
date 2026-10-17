# Protocol

Version: `protocolVersion = 1` (`GET /v1/hello`, with `features[]`). Schema: [openapi.json](openapi.json), generated from `crates/core/src/api.rs`; `crates/engine/tests/openapi_wire.rs` keeps every wire key in it.

## Transport

JSON over HTTP, SSE for events. Two planes:

1. Control (`/v1/*`): host to engine, `Authorization: Bearer $FLIKS_ENGINE_TOKEN`, loopback only.
2. Data (player to engine): `GET`/`HEAD` with Range, credential delegated to the host resolver.

The control plane is only on `--listen`. The data plane is served on `--listen` too (for a host behind its own proxy) and, with `--upstream`, on the public gateway (below).

## Rules

- Additive only within v1: never remove or rename a field, endpoint or enum value.
- Readers ignore unknown fields, and unknown variants of event and other tagged unions (new `EngineEvent` types are additive).
- No 64-bit integers in JSON (use f64, u32 or i32).
- A breaking change goes to `/v2`, served next to `/v1` for one release.
- A host declares a minimum protocol and degrades per feature above it.

## Control endpoints

| Endpoint | Status |
|---|---|
| `GET /v1/hello`, `GET`/`PUT /v1/config` (409 on lower revision). `pathMappings`: `{ from, to }[]`, longest prefix on a path boundary; absent leaves the path unchanged | M0 implemented, mappings M9 |
| `GET /v1/capabilities` (`Capabilities` of the detected host plus `hostCaps`, the exact state the decision reads; 503 + `Retry-After: 1` until the first detection run ends; `FFMPEG_PATH` selects the ffmpeg, see `docs/spec/hw.md`) | M2 implemented |
| `POST /v1/estimate-sizes` (`{ info, fileSize? }` -> `[{ key, label, estimatedSize }]`, see `docs/spec/estimate-sizes.md`) | M1 implemented |
| `POST /v1/probe` (`{ media: { mediaFileId, path }, crop, mkvTags, moviehash }` -> `{ ok, info?, moviehash?, error? }`: `ok: false` means ffprobe could not tell, never "no subtitles"; see `docs/spec/probe.md`) | M2 implemented |
| Detection re-runs when `PUT /v1/config` changes `streaming.gpuRenderNode`; the previous snapshot is served meanwhile | M2 implemented |
| `POST /v1/decide` (`{ request, hostCaps? }`, 400 before the first `PUT /v1/config`; without `hostCaps` the detected host decides, 503 + `Retry-After: 1` until detection ends; `hostCaps` is a debug/shadow override) | M1 implemented |
| `POST /v1/sessions` (`OpenSessionRequest` = `{ decide, ownerId?, client, tags, startAtS?, pinned, burnIn?, sourceVersion?, subtitleRenditions? }` -> `{ sessionId, profileHash?, playPath, decision, hints, playbackClockOffsetS?, tonemapAlgo?, tonemapCurve?, qoePipeline? }`, the last four what the host's playback-info adds beside the decision, `docs/spec/runtime.md` section 10; decide + session + prewarm of a pinned rung; 400 before the first config or on a bad `ownerId`/relative path, 503 + `Retry-After: 1` before detection ends) | M3 implemented |
| `POST /v1/sessions/{sid}/heartbeat` (`{ ownerId?, positionS?, state?, quality? }` -> `{ sessionLost }`, 403 for another owner; revives a session within `sessionReviveTtlMs`) | M3 implemented |
| `POST /v1/sessions/{sid}/command` (`{ action: stop\|pauseJob\|resumeJob }`: 204, 404 unknown sid, 409 no running job or no pause capability), `DELETE /v1/sessions/{sid}[?reason=]` (204, idempotent) | M3 implemented |
| `GET /v1/sessions[?ownerId&mediaFileId]`, `GET /v1/sessions/{sid}` (`SessionSnapshot` with `job: JobStats`: `state`, `requestedHw`, `actualHw`, `startSegment`, `frontierSegment`, `transcodePercent`, `throttled`, `restarts`, `stderrTail`; times are epoch ms as f64) | M3 implemented |
| `POST /v1/revoke` (`{ ownerId?, mediaFileIds? }`, every given filter must match: sessions removed so their `sid` answers 410, jobs killed, resolver answers purged -> `{ sessions }`; 400 with neither) | M3 implemented |
| `POST /v1/invalidate` (`{ mediaFileId }`: jobs and background jobs killed, cache entries, subtitle extracts and the engine sprite deleted, resolver answers purged; sessions stay -> `{ jobs, cacheEntries }`) | M3 implemented |
| `GET /v1/cache` (`{ usedBytes, entries: [{ ownerId?, mediaFileId, profileHash, bytes, lastAccessMs, live }] }`), `DELETE /v1/cache[?mediaFileId&ownerId]` (non-live entries -> `{ removed }`) | M3 implemented |
| `GET /v1/events` (SSE, event name = `type`, data = `EngineEvent`: `SessionOpened`, `SessionEnded{reason}`, `PipelineChanged`, `SegmentServed`, `JobStarted`, `JobEnded{reason?}`, `HwFallback`, `BgJobProgress{jobId, pct}` (at most 1/s per job), `BgJobDone{jobId, state, result?, error?}`, `LiveAttempt`, `LiveReleased` (M6; no `SessionOpened` for live viewers); resync with `GET /v1/sessions`, `GET /v1/jobs` and `GET /v1/live`) | M3 implemented |
| Cache root `FLIKS_TRANSCODE_DIR` (else `FLIKS_CACHE_DIR`, else the OS temp dir), layout `cache/v1/`; the old layout is purged once at boot. See `docs/spec/runtime.md` | M3 implemented |
| `POST /v1/jobs` (`JobRequest` = `{ kind, media, priority?: high\|normal\|low, force? }`, `kind` one of `{ type: "Sprite", info }`, `{ type: "Scan", info }`, `{ type: "Fingerprint", window: { type: "head" } \| { type: "outro", durationSeconds }, maxLookupMinutes? }`, `{ type: "SubtitleExtract", info }`, `{ type: "SubtitleOcr", info, streamIndex, language? }`, `{ type: "SubtitleSync", info, subtitle: { format, text }, reference: { type: "auto" } \| { type: "audioStream", index } \| { type: "subtitleStream", index } \| { type: "file", path }, maxOffsetSeconds?, noFixFramerate?, gss? }, { type: "SubtitleToSrt", info, source: { type: "file", path } \| { type: "stream", index } }` -> `{ jobId }`; an identical request still queued or running answers its `jobId`; 400 on a relative path or an unusable request), `GET /v1/jobs`, `GET /v1/jobs/{id}` (`JobSnapshot`: `state` queued/running/paused/done/failed/cancelled, `pct`, `result?`, `error?`; the last 200 ended jobs stay listed), `DELETE /v1/jobs/{id}` (204, 404 unknown). Results are data (`JobResult`: `Sprite { sprite }`, `Scan { scan }`, `Fingerprint { fingerprint }`, `SubtitleExtract { streams }`, `SubtitleOcr { srt }`, `SubtitleSync { subtitle }`, `SubtitleToSrt { srt }`); files (sprites, embedded subtitle VTTs) stay in the engine cache and are served by the data plane. `hello.features` lists `jobs`, `scan`, `fingerprint`, `subtitle-extract`, and `subtitle-ocr` when `pgsrip` or both `subtile-ocr` and `mkvextract` are on PATH, and `subtitle-sync` when `ffsubsync` is on PATH. Tools: `FFMPEG_PATH`, `FFPROBE_PATH`, `FPCALC_PATH`. One ffmpeg budget (`streaming.ffmpegSlots`, default cores - 1): playback first, background runs preempted while it needs the slots. A job writing files fails below `streaming.jobMinFreeBytes` (default 1 GiB). See `docs/spec/jobs.md` | M5 implemented |
| `POST /v1/live` (`OpenLiveRequest` = `{ channelId, upstreams: [{ url, priority?, headers?, probedContainer?, probedVideoCodec?, probedAudioCodec?, atCapacity? }], caps: { directPlay?, maxBitrateBps?, useTs? }, ownerId?, capacityToken, atCapacity?, tags? }` -> `{ sessionId, playPath, mode: direct\|remux\|transcode, upstream, capacityToken, probe?, dvrWindowSeconds, segmentSeconds, qoePipeline }`): opens or joins the one upstream of (channel, mode, container, snapped bitrate); `sessionId` is a viewer; `capacityToken` in the answer is the joined session's when it is not the request's; 409 `{ code: live_at_capacity }` when an upstream must open and `atCapacity` is set or every upstream has `atCapacity`, 503 `{ code: live_unavailable }` when every upstream failed, 400 (no config, no upstream, an URL scheme other than http(s)/rtmp/rtsp/rtp/udp/srt, CR/LF in a header, bad `ownerId`), 503 + `Retry-After: 1` before detection. `GET /v1/live[?ownerId]`, `GET /v1/live/{sid}` (`LiveSnapshot` per viewer); a viewer leaves through `DELETE /v1/sessions/{sid}` and an owner-only `POST /v1/revoke`. Settings: `EngineConfig.livetv` (`segmentSeconds` 2, `timeshiftMinutes` 15, `probeSeconds` 3, `fastZap?`, `channelIdleSeconds` 30). `hello.features` lists `live`. Events: `SessionEnded` per viewer (`stopped`, `expired`, `revoked`, `failed`), `LiveAttempt { capacityToken, upstream, error?, probe? }` per upstream attempt, `LiveReleased { capacityToken, reason: idle\|failed\|shutdown }` once per session that opened an upstream (before a 503 too). See `docs/spec/live.md` | M6 implemented |
| `POST /v1/intro-detect` (`IntroDetectRequest` = `{ kind: intro\|outro, lookupSeconds, minSegmentSeconds?, episodes: [{ mediaFileId, durationSeconds, chapters?, introFingerprint?, outroFingerprint? }] }` -> `{ results: [{ mediaFileId, startSeconds, endSeconds, confidence, source: chapter\|reference\|pairwise }] }`; 400 on bad lookup/duration or a missing fingerprint; no config required). `hello.features` lists `intro-detect`. See `docs/spec/intro.md` | M8 implemented |

## Resolver (implemented by the host)

```
POST {hostUrl}/engine/resolve   Authorization: Bearer {hostToken}   (401 on a wrong token)
  <- { mediaId, credential, purpose: stream|download|sprite|subtitle|live, subtitleId?, streamIndex? }
  -> { allow: bool, path?, ownerId?, format?, ttlS? }
```

- `hostUrl`/`hostToken` come from `PUT /v1/config`. Types: `ResolveRequest`/`ResolveResponse` in `openapi.json`.
- `credential`: the client credential exactly as received, one string, in the host's own extractor order: the `Cookie` header value, else a `Bearer` `Authorization` value (`Bearer <jwt>`), else the `?token=` value. The host parses all three; a request with any cookie sends the cookie header.
- `mediaId`: the media file id (number); for `live` the live viewer's `sessionId` (string).
- `live` (M6): every data-plane request of a live sid; the host checks the channel is still playable for that user (its `assertStillPlayable`) and answers `{ allow, ownerId, ttlS }` without a path. A denial that names the viewer's owner (`{ allow: false, ownerId }`) ends the viewer (`SessionEnded { reason: revoked }`); any other denial (no `ownerId`, another user's) is a 403 and leaves it; an `ownerId` other than the viewer's on a grant is a 403. `ttlS: 0` re-checks every request, as the host did on every playlist poll.
- `stream`/`download`: `path` = the media file, `format` = its MIME type (optional, else the engine guesses from the extension).
- `sprite`: `path` = `sprite.jpg` (its `sprite.json` is the sibling), not checked by the host, and optional since M5: the engine serves its own sprite (`Sprite` job) first, then this path, else 404.
- `subtitle`: `subtitleId` (external file: `path` = the file, `format` = its extension `srt`/`ass`/`vtt`) or `streamIndex` (embedded: `format: "embedded"`, `path` = the media file, the engine extracts the stream).
- Denials are `{ allow: false, ttlS: 60 }` without a path (`ownerId` too on a `live` denial for a known user). A host error is an HTTP 5xx and is never cached as a denial.

The engine knows neither JWT nor ACL. Answers (denials included) are cached per `(hash(credential), mediaId, purpose, subtitleId, streamIndex)` for `ttlS` (60 s default). Host unreachable, timing out (5 s) or failing: an expired grant and the sessions of the requested file keep being served, other requests get 503 + `Retry-After: 2`. Data-plane answers: 403 on a denial; on a `?sid=` the engine no longer knows, 410 `{ code: "session_expired", sid, message }`; on a session of another owner or file, 403. `POST /v1/revoke` kills sessions and purges the cache immediately.

## Data plane routing (host prefix `/api/stream`, M3 implemented)

Engine serves `GET`/`HEAD` of `/api/stream/:mediaFileId/**` except `edl`; everything else is forwarded to the host.

| Engine | Host |
|---|---|
| `:id` (direct play, `?download=1`) | `POST :id/playback-info`, `DELETE sessions/:sid`, other non-GET |
| `:id/master.m3u8`, `:id/:quality/index.m3u8`, `:id/:quality/:segment` | `info/*` (via `/v1/estimate-sizes`, `/v1/capabilities`) |
| `:id/audio/:idx/index.m3u8`, `:id/audio/:idx/:segment` | `:id/edl` |
| `:id/iframe/index.m3u8`, `:id/iframe/:segment` | companions/download, `subtitles/*` and subtitle search |
| `:id/thumbnails/sprite.{json,jpg}` | |
| `:id/subtitles/embedded/:idx[/download\|/index.m3u8]`, `:id/subtitles/:subId[/download\|/index.m3u8]` | |
| `/api/livetv/sessions/:sid/{index.m3u8,direct.ts,:segment}` (M6, `GET`/`HEAD`) | `/api/livetv/*` otherwise (`DELETE sessions/:sid`, `PUT sessions/:sid/state`, `channels/:id/play`) |

DirectStream: `:id/remux/*` and a remux session's audio renditions come from copy-video runs on the keyframe grid frozen at open (`docs/spec/runtime.md` section 9).

## Gateway (`--upstream`, spec: `docs/spec/gateway.md`)

`fliks-engine --upstream http://host:port [--engine-upstream http://engine:port] [--public-listen 0.0.0.0:4848] [--gateway-routes routes.json] [--supervise -- cmd]`

- The public listener serves the engine routes of a configurable route table (ordered `{ target: engine|forward, methods?, path }` rules, `:name`/`*` one segment, trailing `**`; default = the table above) and forwards everything else to the upstream verbatim: method, path+query, headers (plus `X-Forwarded-For/Proto/Host`, minus hop-by-hop), streamed bodies, SSE, WebSocket/`Upgrade`. Upstream unreachable: `502` on forwarded routes, engine routes unaffected. With `--engine-upstream`, the `engine` rows are forwarded to that URL instead of a local data plane; this process does not transcode.
- `/v1/*` of the engine is never on the public listener; it stays on `--listen`. A `/v1/*` path there is just another forwarded path.
- `--supervise --upstream`: the host is started with `PORT=<upstream port>` (plus `FLIKS_ENGINE_URL`/`FLIKS_ENGINE_TOKEN`) so it listens on the internal address. Without `--supervise` the host is any process reachable at the URL (remote host, gateway-only role); without `--upstream` nothing changes.

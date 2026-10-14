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
| `GET /v1/hello`, `GET`/`PUT /v1/config` (409 on lower revision) | M0 implemented |
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
| `GET /v1/events` (SSE, event name = `type`, data = `EngineEvent`: `SessionOpened`, `SessionEnded{reason}`, `PipelineChanged`, `SegmentServed`, `JobStarted`, `JobEnded{reason?}`, `HwFallback`, `BgJobProgress{jobId, pct}` (at most 1/s per job), `BgJobDone{jobId, state, result?, error?}`; resync with `GET /v1/sessions` and `GET /v1/jobs`) | M3 implemented |
| Cache root `FLIKS_TRANSCODE_DIR` (else `FLIKS_CACHE_DIR`, else the OS temp dir), layout `cache/v1/`; the old layout is purged once at boot. See `docs/spec/runtime.md` | M3 implemented |
| `POST /v1/jobs` (`JobRequest` = `{ kind: { type: "Sprite", info }, media, priority?: high\|normal\|low, force? }` -> `{ jobId }`; an identical request still queued or running answers its `jobId`; 400 on a relative path or an unusable request), `GET /v1/jobs`, `GET /v1/jobs/{id}` (`JobSnapshot`: `state` queued/running/paused/done/failed/cancelled, `pct`, `result?`, `error?`; the last 200 ended jobs stay listed), `DELETE /v1/jobs/{id}` (204, 404 unknown). Results are data (`JobResult`); files stay in the engine cache and are served by the data plane. One ffmpeg budget (`streaming.ffmpegSlots`, default cores - 1): playback first, background runs preempted while it needs the slots. A job writing files fails below `streaming.jobMinFreeBytes` (default 1 GiB). See `docs/spec/jobs.md` | M5 implemented |
| `POST /v1/live` | M6 |

## Resolver (implemented by the host)

```
POST {hostUrl}/engine/resolve   Authorization: Bearer {hostToken}   (401 on a wrong token)
  <- { mediaId, credential, purpose: stream|download|sprite|subtitle|live, subtitleId?, streamIndex? }
  -> { allow: bool, path?, ownerId?, format?, ttlS? }
```

- `hostUrl`/`hostToken` come from `PUT /v1/config`. Types: `ResolveRequest`/`ResolveResponse` in `openapi.json`.
- `credential`: the client credential exactly as received, one string, in the host's own extractor order: the `Cookie` header value, else a `Bearer` `Authorization` value (`Bearer <jwt>`), else the `?token=` value. The host parses all three; a request with any cookie sends the cookie header.
- `mediaId`: the media file id (number); for `live` (M6) the live session id (string).
- `stream`/`download`: `path` = the media file, `format` = its MIME type (optional, else the engine guesses from the extension).
- `sprite`: `path` = `sprite.jpg` (its `sprite.json` is the sibling), not checked by the host, and optional since M5: the engine serves its own sprite (`Sprite` job) first, then this path, else 404.
- `subtitle`: `subtitleId` (external file: `path` = the file, `format` = its extension `srt`/`ass`/`vtt`) or `streamIndex` (embedded: `format: "embedded"`, `path` = the media file, the engine extracts the stream).
- Denials are `{ allow: false, ttlS: 60 }` without a path. A host error is an HTTP 5xx and is never cached as a denial.

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
| M6: `livetv/sessions/:sid/{index.m3u8,:segment}` | |

DirectStream: `:id/remux/*` and a remux session's audio renditions come from copy-video runs on the keyframe grid frozen at open (`docs/spec/runtime.md` section 9).

## Gateway (`--upstream`, spec: `docs/spec/gateway.md`)

`fliks-engine --upstream http://host:port [--public-listen 0.0.0.0:4848] [--gateway-routes routes.json] [--supervise -- cmd]`

- The public listener serves the engine routes of a configurable route table (ordered `{ target: engine|forward, methods?, path }` rules, `:name`/`*` one segment, trailing `**`; default = the table above) and forwards everything else to the upstream verbatim: method, path+query, headers (plus `X-Forwarded-For/Proto/Host`, minus hop-by-hop), streamed bodies, SSE, WebSocket/`Upgrade`. Upstream unreachable: `502` on forwarded routes, engine routes unaffected.
- `/v1/*` of the engine is never on the public listener; it stays on `--listen`. A `/v1/*` path there is just another forwarded path.
- `--supervise --upstream`: the host is started with `PORT=<upstream port>` (plus `FLIKS_ENGINE_URL`/`FLIKS_ENGINE_TOKEN`) so it listens on the internal address. Without `--supervise` the host is any process reachable at the URL (remote host, gateway-only role); without `--upstream` nothing changes.

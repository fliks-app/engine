# Protocol

Version: `protocolVersion = 1` (`GET /v1/hello`, with `features[]`). Schema: [openapi.json](openapi.json), generated from `crates/core/src/api.rs`.

## Transport

JSON over HTTP, SSE for events (planned). Two planes:

1. Control (`/v1/*`): host to engine, `Authorization: Bearer $FLIKS_ENGINE_TOKEN`, loopback only.
2. Data (player to engine): `GET`/`HEAD` with Range, credential delegated to the host resolver (M3).

## Rules

- Additive only within v1: never remove or rename a field, endpoint or enum value.
- Readers ignore unknown fields.
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
| `POST /v1/sessions`, heartbeat, command, `DELETE`, `GET /v1/sessions[/{sid}]`, `POST /v1/revoke`, `GET`/`DELETE /v1/cache`, `POST /v1/invalidate`, `GET /v1/events` | M3 |
| `POST /v1/jobs`, `DELETE /v1/jobs/{id}` | M5 |
| `POST /v1/live` | M6 |

## Resolver (implemented by the host)

```
POST {hostUrl}/engine/resolve   Authorization: Bearer {hostToken}
  <- { mediaId, credential /* raw cookie, Bearer or ?token= */, purpose: stream|download|sprite|subtitle|live, subtitleId? }
  -> { allow: bool, path?, ownerId?, format?, ttlS }
```

The engine knows neither JWT nor ACL. Answers are cached per `(hash(credential), mediaId, purpose)` for `ttlS` (60 s default). Host down: cached entries and existing sessions keep being served, new requests get 503. `POST /v1/revoke` kills sessions and purges the cache immediately.

## Data plane routing (host prefix `/api/stream`, M3)

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

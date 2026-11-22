# Spec: gateway (front door)

The engine can own the public port and forward what it does not serve to a host (`--upstream`). A dumb reverse proxy plus a route table: no auth, no host logic. Plan refs: §4, §7, §16 (M9: role separable from transcoding, no hardcoded localhost). All `[new]`.

## CLI `[new]`

| Flag | Meaning |
|---|---|
| `--upstream URL` | host to forward to; `http://host[:port]` only (no path, no TLS: terminate TLS in front) |
| `--engine-upstream URL` | remote engine for route-table `engine` rows; same URL shape as `--upstream`; requires `--upstream` and `FLIKS_ENGINE_TOKEN` in the environment (no local data plane, no transcoding in this process) |
| `--public-listen ADDR` | public listener, default `0.0.0.0:4848` when `--upstream` is set; error without `--upstream` |
| `--gateway-routes FILE` | JSON route table replacing the default |

- `--engine-upstream`: `engine` rows go to that URL, everything else to `--upstream`. No local runtime, no `--listen`, no data plane mount. Off-LAN TLS is terminated in front of this process (same as `--upstream`; no TLS client here). With `--supervise`, the child gets `FLIKS_ENGINE_URL=<engine-upstream URL>` and `FLIKS_ENGINE_TOKEN` from the environment; `PORT` stays the `--upstream` port.
- `--listen` stays the private control plane (`/v1/*`, bearer). The public listener never serves `/v1/*` of the engine: it only has the route table and the forwarder. A `/v1/...` request there matches no engine rule, so it is forwarded to the upstream like any other path (the host's own `/v1`, if any, is its business).
- `--upstream` without `--supervise` works (remote or separately managed host). `--supervise` without `--upstream` is unchanged (no public listener, no `PORT`).
- `--supervise --upstream URL`: the host is spawned with `PORT=<port of URL>` (80 when absent) next to `FLIKS_ENGINE_URL`/`FLIKS_ENGINE_TOKEN`, so a host that reads `PORT` (Fliks Nest) listens on the internal port. The host part of URL is not interpreted: the host must be reachable there.
- Startup prints `FLIKS_ENGINE_PUBLIC_LISTENING <addr>` on stdout.

## Route table

Ordered rules, first match wins, no match = forward. A rule is data, `{ "target": "engine"|"forward", "methods": ["GET"], "path": "/api/stream/:id/**" }`; empty or absent `methods` = any.

- Path pattern: `/`-separated segments; `:name` or `*` = exactly one non-empty segment; `**`, last only, = zero or more segments; anything else is literal. Matched on the raw request path (no decoding, no normalisation, query ignored).
- `engine` rules go to the engine router handed to `gateway::router` (the M3 data plane plugs in there). With no engine router they are forwarded, so the gateway is usable before the data plane exists.
- Default table (Fliks, `src/fliks-routes.json`), the engine rows being the shapes of `serve::routes` (a test asserts every row matches a served route and every served route a row, so a host `GET` the engine does not serve is forwarded):
  1. forward `/api/stream/info/**`
  2. engine `GET`/`HEAD` `/api/stream/:id`, `:id/master.m3u8`, `:id/:a/:b` (variant, segment, parts, item, iframe, thumbnails, subtitles), `:id/audio/:idx/:seg`, `:id/subtitles/embedded/:idx[/download|/index.m3u8]`, `:id/subtitles/:sub/{download,index.m3u8}`
  3. the same shapes under `/cred/:credential` (the data plane with a path credential, `runtime.md` section 6)
  4. engine `GET`/`HEAD` `/api/livetv/sessions/:sid/:file` (live playlist, segments, `direct.ts`)
  Non-GET verbs (`playback-info`, `DELETE sessions/:sid`, live `PUT .../state`) and every other path fall through to the forward.

## Forwarding

- Method, path and query verbatim, to `http://<upstream authority>`; `Host` kept as received. HTTP/1.1 to the upstream.
- `Authorization` (any scheme) and path segments are never touched, so HTTP Basic and path tokens reach the upstream as received; the engine never logs request paths, headers or bodies.
- Hop-by-hop headers (`Connection` and the headers it names, `Keep-Alive`, `Proxy-*`, `TE`, `Trailer`, `Transfer-Encoding`, `Upgrade`) are stripped in both directions, except on an upgrade (below).
- Added: `X-Forwarded-For` (peer IP appended to the existing value), `X-Forwarded-Proto` (`http`) and `X-Forwarded-Host` (`Host`), a client's own kept only when the peer is loopback or on a private network.
- Request and response bodies are streamed frame by frame with backpressure (never collected): large downloads/uploads use bounded memory, SSE events are flushed as they arrive.
- Upgrade (`Connection: upgrade` + `Upgrade`): both headers are forwarded; on `101` the response goes back and the two connections are spliced byte for byte until either side closes (WebSocket, any protocol).
- Upstream refused, reset or unreachable before a response: `502 Bad Gateway`, empty body. Engine routes are unaffected. No retry, no timeout beyond a 5 s connect timeout.
- Ceiling: no TLS to the upstream, no HTTP/2, no health checks or load balancing.

## Tests

`crates/engine/tests/gateway.rs`, see `docs/tests.md`.

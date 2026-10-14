# fliks-engine

Video engine for [Fliks](https://github.com/fliks-app): playback decisions, transcoding sessions and segment serving behind a versioned HTTP protocol. Usable by any host in any language; it has no database, ACL or JWT (the host resolves access).

## Run

```sh
cargo run -p fliks-engine -- --listen 127.0.0.1:0     # prints FLIKS_ENGINE_LISTENING <addr>
cargo run -p fliks-engine -- --supervise -- node dist/main   # engine as parent of the host
```

The control plane (`/v1/*`) needs `Authorization: Bearer $FLIKS_ENGINE_TOKEN` (random when unset; a supervised host receives it with `FLIKS_ENGINE_URL`).

## Docker

`ghcr.io/fliks-app/engine` (linux/amd64 + arm64): the engine plus jellyfin-ffmpeg, mkvmerge, fpcalc and, on amd64, Intel OpenCL. Tags: `main`, `sha-<short>`, `X.Y.Z`, `X.Y`, `X`, `latest` (latest release). The entrypoint is `fliks-engine`, the default command `--listen 0.0.0.0:7400`.

As the front door of a host (the gateway serves the public port and forwards the rest to `--upstream`):

```sh
docker run --rm --device /dev/dri -p 4848:4848 \
  -e FLIKS_ENGINE_TOKEN=... -e FLIKS_TRANSCODE_DIR=/transcode -v transcode:/transcode \
  -v /media:/media:ro \
  ghcr.io/fliks-app/engine --listen 0.0.0.0:7400 --public-listen 0.0.0.0:4848 --upstream http://host:port
```

Mount the media at the same paths as the host sees them (the engine reads the paths the host sends). `/dev/dri` is only for hardware transcoding; with a non-root `--user`, add `--group-add 0` (and the render group) so `/transcode` is writable.

## Protocol

See [PROTOCOL.md](PROTOCOL.md) and the generated [openapi.json](openapi.json). Clients generate their types from `openapi.json` (attached to every release). Repository: https://github.com/fliks-app/engine

AGPL-3.0-or-later.

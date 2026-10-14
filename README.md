# fliks-engine

Video engine for [Fliks](https://github.com/fliks-app): playback decisions, transcoding sessions and segment serving behind a versioned HTTP protocol. Usable by any host in any language; it has no database, ACL or JWT (the host resolves access).

## Run

```sh
cargo run -p fliks-engine -- --listen 127.0.0.1:0     # prints FLIKS_ENGINE_LISTENING <addr>
cargo run -p fliks-engine -- --supervise -- node dist/main   # engine as parent of the host
```

The control plane (`/v1/*`) needs `Authorization: Bearer $FLIKS_ENGINE_TOKEN` (random when unset; a supervised host receives it with `FLIKS_ENGINE_URL`).

## Protocol

See [PROTOCOL.md](PROTOCOL.md) and the generated [openapi.json](openapi.json). Clients generate their types from `openapi.json` (attached to every release). Repository: https://github.com/fliks-app/engine

AGPL-3.0-or-later.

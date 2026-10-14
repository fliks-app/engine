# Tests

- `crates/core/src/api.rs` (unit): MediaInfo/DeviceProfile/SourceScan round-trip equal to the host JSON, unknown fields ignored.
- `crates/core/tests/decide_fixtures.rs`: every `fixtures/decide/*.json` input (`info`, `device`, `scan`) round-trips the wire types, and `decide(input, hostCaps, config)` equals `expected` (numbers compared as f64; mismatches print the fixture name and the differing JSON paths; explicit `SKIPPED` allowlist, empty). Vectors come from the host harness `backend/test/engine-vectors` (`FLIKS_ENGINE_REPO=<this repo> npx jest test/engine-vectors`), see `docs/spec/decide.md`.
- `crates/engine/tests/control_plane.rs`: bearer auth, hello, config revision rule, `POST /v1/decide` (400 before the first config, then the golden `android-tv-h264-sdr-mkv-cpu-720p` decision with the stub caps).
- `crates/engine/tests/supervise.rs` (unix): failing host is restarted with the injected env.
- CI drift: `openapi.json` and `clients/ts/src/schema.d.ts` match the types; `oasdiff breaking` against main.

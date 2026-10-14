# Tests

- `crates/core/src/api.rs` (unit): MediaInfo/DeviceProfile/SourceScan round-trip equal to the host JSON, unknown fields ignored.
- `crates/core/tests/decide_fixtures.rs`: every `fixtures/decide/*.json` input (`info`, `device`, `scan`) round-trips the wire types; the `decide()` snapshot joins in M1. Vectors come from the host harness `backend/test/engine-vectors` (`FLIKS_ENGINE_REPO=<this repo> npx jest test/engine-vectors`), see `docs/spec/decide.md`.
- `crates/engine/tests/control_plane.rs`: bearer auth, hello, config revision rule.
- `crates/engine/tests/supervise.rs` (unix): failing host is restarted with the injected env.
- CI drift: `openapi.json` and `clients/ts/src/schema.d.ts` match the types; `oasdiff breaking` against main.

# Tests

- `crates/core/src/api.rs` (unit): MediaInfo/DeviceProfile/SourceScan round-trip equal to the host JSON, unknown fields ignored.
- `crates/core/tests/decide_fixtures.rs`: golden `fixtures/decide/*.json` (placeholder until M1).
- `crates/engine/tests/control_plane.rs`: bearer auth, hello, config revision rule.
- `crates/engine/tests/supervise.rs` (unix): failing host is restarted with the injected env.
- CI drift: `openapi.json` and `clients/ts/src/schema.d.ts` match the types; `oasdiff breaking` against main.

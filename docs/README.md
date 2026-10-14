# Docs

- [tests.md](tests.md): one line per test set.
- `spec/<domain>.md`: port specs (written before each port).
  - [spec/decide.md](spec/decide.md): the play-method decision (`evaluate()`), ladder, audio plan, HDR/DV, host caps.

## Tags

- `[ts:path:line]`: behaviour taken from the TS source at that location (relative to `fliks/backend/src`).
- `[assumed]`: not verified in the source; check with a fixture.
- `[new]`: new behaviour with no TS counterpart.

## Deviations

- decide: the TS throws `No audio encoder for group codec` when a track must re-encode into a group codec that cannot be encoded. That state is unreachable (only an all-copy group settles on such a codec), so the port falls back to AAC instead of failing.
- decide: no vector is skipped, so the test allowlist `SKIPPED` is empty; the fuzz exclusion list `EXCLUDED` is empty too.
- decide fuzz (35 000 random inputs, 0 TS throws): the TS never threw, so the unreachable `No audio encoder for group codec` throw above stays the only TS-throws-vs-Rust-returns case. `serde_json` runs with `float_roundtrip`: its default parser can be one ulp off, which flipped exact-boundary comparisons (`fixtures/decide/fuzz-float-roundtrip.json`).

## Open divergences

- decide `Decision` keeps fields the host never reads, for round-trip exactness: `videoVariant.bitDepth` (implied by `hdr`), `audioPlans` (index-aligned with `response.audioTracks`), `response.audioPlan` (= `audioPlans[picked]`). Candidates to drop in v2.
- decide `response.hwAccel` means three things: `none` on Direct Play, the detected backend on DirectStream, the effective encoder backend on Transcode.
- decide `HostCaps` from the stub `Capabilities` (no `hostCaps` in the body) has no passed probe and nothing blacklisted; M2 hardware detection must fill `probes`, `vaapiWritesHdrMetadata`, `disabledEncoders`, `disabledDecoders`.
- decide ports read the vectors' host caps only for linux/darwin and `none`/`vaapi`/`nvenc`/`qsv`/`videotoolbox`: the win32, AMF and blacklisted-id branches (encoder ids, `{codec}_qsv[_d3d11]_decode`) follow the TS; no golden vector exercises them `[assumed]`, but the fuzz does (win32, AMF, QSV, blacklisted ids; 35 000 inputs, 0 mismatches).
- decide `maxAudioChannels` and `audioChannelsByCodec` are `f64` on the wire; a fractional cap (`5.1`) makes the TS report fractional `channels`, which `Decision` (`u32`) truncates. The fuzz only samples integer caps.
- decide bitrates, sample rates and ids are `u32` on the wire (`MediaInfo` already is); a host sending a fractional or negative `audioStreamIndex`/`burnInSubtitleId` gets a 422 where the TS coerces.
- `hls-variant-ladder.ts` is not ported: `evaluate()` does not read it (master playlist, M3).

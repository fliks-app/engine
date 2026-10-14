# Docs

- [tests.md](tests.md): one line per test set.
- `spec/<domain>.md`: port specs (written before each port).
  - [spec/decide.md](spec/decide.md): the play-method decision (`evaluate()`), ladder, audio plan, HDR/DV, host caps.
  - [spec/hw.md](spec/hw.md): host capability detection (backend choice, test encodes, probes, GPUs, pause), the source of `Capabilities` and `HostCaps`.
  - [spec/probe.md](spec/probe.md): media probing (`POST /v1/probe`): ffprobe/mkvmerge/cropdetect to `MediaInfo`, moviehash.
  - [spec/session.md](spec/session.md): `core::session`, the pure session runtime: keys, `cache/v1` profile hash, seek/quality-switch matrix, throttle, segment responses, lifetimes, cache eviction.
  - [spec/args.md](spec/args.md): the ffmpeg argv (`core::args`): encode layer (decode, filters, encoders, audio) and outputs (HLS VOD, remux, live, iframe, file).
  - [spec/hls.md](spec/hls.md): playlist text (`core::hls`): master, uniform/variable/I-frame/subtitle media playlists, URI prefix as a parameter.
  - [spec/mp4.md](spec/mp4.md): segment packaging (`core::mp4`): boxes, tfdt timelines, edit lists, CMAF rewrite, keyframe grid, remux plan.
  - [spec/runtime.md](spec/runtime.md): the M3 runtime (`engine::{proc,runtime,cache,resolve,serve,events}`, `core::session::plan`): ffmpeg jobs, sessions, seek/rung switch, CPU fallback, throttle, idle reaping, `cache/v1`, the data plane behind the host resolver, SSE events.

## Tags

- `[ts:path:line]`: behaviour taken from the TS source at that location (relative to `fliks/backend/src`).
- `[assumed]`: not verified in the source; check with a fixture.
- `[new]`: new behaviour with no TS counterpart.

## Deviations

- decide: the TS throws `No audio encoder for group codec` when a track must re-encode into a group codec that cannot be encoded. That state is unreachable (only an all-copy group settles on such a codec), so the port falls back to AAC instead of failing.
- decide: no vector is skipped, so the test allowlist `SKIPPED` is empty; the fuzz exclusion list `EXCLUDED` is empty too.
- decide fuzz (35 000 random inputs, 0 TS throws): the TS never threw, so the unreachable `No audio encoder for group codec` throw above stays the only TS-throws-vs-Rust-returns case. `serde_json` runs with `float_roundtrip`: its default parser can be one ulp off, which flipped exact-boundary comparisons (`fixtures/decide/fuzz-float-roundtrip.json`).
- probe: the host serialises a NaN number (e.g. a stream `start_time` of `"N/A"`) as `null`; the engine leaves the key absent. The fixtures list those paths in `expected.nulls`.
- probe moviehash: the host computes the size term with `bytesize & 0xffffffff`, which turns negative once bit 31 of the low word is set (sizes of 2 to 4 GiB modulo 4 GiB), leaving the high word of the hash one short: those hashes never match OpenSubtitles. The engine computes the specified hash; `fixtures/probe/_moviehash.json` holds both values. Fix the host (or drop its hash lookup) before trusting hashes across the switch.
- probe errors: the text of an unparsable ffprobe output and of a spawn failure is the engine's own (`ok: false` either way); a timeout reads `Command failed: ...` like the host's.
- probe ffprobe fields of an unexpected type (e.g. `level: "4.1"`) are dropped; the host copies them through. A non-numeric `format.duration` or `start_time` is absent; the host would pass `NaN` to `-ss` and `-analyzeduration`. ffprobe never prints either.
- probe adds `mkvTags` and `crop` switches, absolute-path validation, `FFPROBE_PATH`/`FFMPEG_PATH`/`MKVMERGE_PATH`, and stamps `schemaVersion` (the pure parser's output does not carry it). The host's warning logs are not ported.

- hw: every detection run recomputes all flags (the TS keeps the flags of probes a re-pin did not re-run), uses the pinned node for the backend choice too, and runs CPU probes serially; `FFMPEG_PATH` is new (the TS only looks `ffmpeg` up on PATH). Details in `spec/hw.md` section 10.
- hw: `/v1/capabilities` and `/v1/decide` answer 503 until the first detection run ends (the TS treats unprobed encoders as usable and probes as off).
- session: the cache profile hash is new (`cache/v1`, FNV-1a 64 over a documented canonical string, `spec/session.md` section 2), not the host's sha1 of `computeProfileHash`: no parity, snapshot and stability tests only. The old cache layout is purged once.
- session: segment names accept `seg-<n>` with any number of digits (the host's regex allows 3 to 4); the production frontier is the in-memory `SegmentSet`, not `existsSync`.
- session: `EngineConfig.lifetime` (`LifetimeConfig`) carries the durations of the host's `lifetime-constants.ts` (env vars are not read by the engine); zero keeps the default. `MAX_TOMBSTONES` trimming is not ported.
- session fuzz (3 seeds, 168 000 inputs: 30 000 seek-matrix, 30 000 throttle ticks, 30 000 throttle decisions): 0 mismatches, no class promoted to a `fuzz-*` fixture.
- args: inputs are typed (`VodArgs`, `RemuxArgs`, `LiveArgs`, `EncoderInput` with the wire enums); a value the TS would print as `undefined` (an audio transcode to `dts`, a fractional stream index) is rejected when the input is read. `null` reads as absent, where a TS destructuring default only replaces `undefined`; the harness never writes such a `null`.
- args: the TS reads probes, platform, render node and `FLIKS_OPENCL_DEVICE` from process globals; the port takes them from `HostCaps` and `hw::Device`. The run's `hwAccel` wins over `HostCaps.hwAccel`, as in the TS where the option and the detected backend are separate values.
- args: not ported: the builders' log lines, `remuxRunStart`/`remuxAudioGrid` (the session layer's, from the keyframe grid), and the stats labels of `resolveTonemapReport` (`path`, `curve`); `decide` and `args` share one `resolve_pipeline`.
- args fuzz: the host harness first computed `expected` from the generated object, whose key order differs from the stored one, so `No encoder for variant {...}` messages differed in key order only; the harness now runs from the stored input (`fixtures/argv/fuzz-no-encoder-variant-json.json`).
- hls: a collapsed (pinned `remux`/`original`, or `supportsAbr: false`) HDR ladder with no fitting rung emits no rung; the TS throws reading `undefined.maxWidth`. The fuzz skips TS throws (33 of 20 000 inputs, all this one).
- hls: every URI takes a `prefix` (the host's `/api/stream/{id}`) and a `token` suffix: the engine names no host or route root.
- mp4: `trun_samples` refuses a `sample_count` above 2^24 (`OutOfRange`) where the TS would allocate one object per declared sample of a corrupt box.
- mp4: ticks are `i128` and a non-integral tick count fails with the JS `BigInt` message; a value past 2^127 saturates (the TS keeps a bigint), only changing the number in an error message the write then raises anyway.

- runtime: ffmpeg stops with SIGTERM then SIGKILL after 2 s (the TS sends SIGKILL); the run is awaited before its directory is touched again.
- runtime: a segment past the uniform grid's end is a 404 before anything spawns; a rung switch starts at the request's segment (the TS inherits the killed run's start for a `seg-0` request); a rung or resume whose file is already on disk is served without a new run (the TS respawns when no run is registered).
- runtime: a CPU fallback registers its run before the failed run's waiters wake, and a waiter follows it; the TS answers that wait from the dead run.
- runtime: the production frontier is a 200 ms directory listing (the TS uses `fs.watch`); stderr is a bounded ring (40 lines).
- runtime: subtitles other than WebVTT are converted by ffmpeg (`-f webvtt`), not by the host's SRT/ASS converters (alignment tags and the b/i/u filter differ); embedded extracts are cached under `<root>/subtitles/v1`.
- runtime: the resolver credential is the `?token=` value first, then `Authorization`, then `Cookie` (the host's own order is cookie first): the engine cannot tell an access cookie from other cookies.

## Open divergences

- decide `Decision` keeps fields the host never reads, for round-trip exactness: `videoVariant.bitDepth` (implied by `hdr`), `audioPlans` (index-aligned with `response.audioTracks`), `response.audioPlan` (= `audioPlans[picked]`). Candidates to drop in v2.
- decide `response.hwAccel` means three things: `none` on Direct Play, the detected backend on DirectStream, the effective encoder backend on Transcode.
- hw: nvenc, amf/win32 and videotoolbox/darwin detection only ever ran against scripted ffmpeg answers (`fixtures/hw` `source: "synthetic"`); the argv are the TS's, but no real NVIDIA, AMD/Windows or Apple host has been compared. Real hosts: this machine's Intel iGPU + Arc under jellyfin-ffmpeg (QSV, VAAPI, OpenCL, vpp_qsv) and Ubuntu ffmpeg without a VA driver.
- hw: HW decode probes are not deterministic on this machine's GPUs, in the TS as in the port: pinned to the Arc A370M, which decoders pass changed run to run (4 of 4 TS runs, 5 of 5 Rust runs differed); on the iGPU the TS was stable (13 of 13) and the port flaked in 6 of the first 24 runs, then 0 of 50. Encoder, tone-map and backend results were identical in all 9 Arc runs. Both probe once, so a transient failure disables that decoder until the next detection.
- decide ports read the vectors' host caps only for linux/darwin and `none`/`vaapi`/`nvenc`/`qsv`/`videotoolbox`: the win32, AMF and blacklisted-id branches (encoder ids, `{codec}_qsv[_d3d11]_decode`) follow the TS; no golden vector exercises them `[assumed]`, but the fuzz does (win32, AMF, QSV, blacklisted ids; 35 000 inputs, 0 mismatches).
- decide `maxAudioChannels` and `audioChannelsByCodec` are `f64` on the wire; a fractional cap (`5.1`) makes the TS report fractional `channels`, which `Decision` (`u32`) truncates. The fuzz only samples integer caps.
- decide bitrates, sample rates and ids are `u32` on the wire (`MediaInfo` already is); a host sending a fractional or negative `audioStreamIndex`/`burnInSubtitleId` gets a 422 where the TS coerces.
- hls: the master's options come from the session (`core::session::master_options`); `dvSupplementalCodecs`/`dvStandaloneCodecs` are not ported, so a Dolby Vision remux variant has no `SUPPLEMENTAL-CODECS` and keeps the probed CODECS.
- mp4: the stateful `RemuxSegmentAssembler` (watching GOP files, completion, run landing, audio grouping, `canServe`) and its file readers are not ported: the serve runtime will drive `core::mp4` with them. `output-timeline.spec.ts` asserts ffmpeg argv: only its `remuxRunStart` calls are replayed; the args port should reuse `mp4::remux_run_start`.
- probe: `detectEmbeddedSubtitles` and `detectStreams` (embedded-subtitle, subtitle-sync and the media controller) are not ported: they read the same ffprobe JSON but are not part of `MediaInfo`.
- probe crop samples share one process-wide budget of `cores - 1` runs in the engine; the host's cgroup-aware pool (and its admin override) stays in Node until the engine owns every ffmpeg slot (M3).
- probe is verified on Linux with jellyfin-ffmpeg 8.1.2 and mkvtoolnix 82 only; `ionice`/`nice` and process kills on macOS and Windows follow the same code but have no live run. No corpus file carries Dolby Vision or HDR10+ (ffmpeg cannot write them): those fields are covered by synthetic ffprobe output.
- session: the 503/404/410 segment rule and the `Retry-After` value are inline in the host's controller (no function to call): hand-written tests, no golden vectors. The job-idle vectors run with the default durations (the host reads them from env at import).
- args: the file output (`args::file`, MP4/MKV) has no TS counterpart and no real ffmpeg run yet; only unit tests pin its argv. `Profile` keeps the host's bitrate strings (`8M`), the session port may switch it to `ladder::Rung`.
- args: argv parity is against the TS only; like hw, nvenc, amf/win32, qsv/win32 and videotoolbox argv were never run on those hosts here.
- runtime: no remux runs (DirectStream `remux/*` segments and remux audio renditions answer 501) and no early companion (`-early`): the stateful `RemuxSegmentAssembler` is not ported, and a seg-0 probe on a resume goes through the seek matrix (it can restart the run at 0, then at the resume point).
- runtime: e2e runs used the CPU only (jellyfin-ffmpeg 8.1.2 on Linux); the CPU fallback is exercised with a VAAPI host on a missing render node, no real GPU run went through the runtime yet. Throttle runs with the `signal` capability only; Windows Job Objects, the `stdin` pause and the `q` stop have no live run.

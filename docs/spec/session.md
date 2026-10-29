# Spec: `core::session`

Pure session logic of the engine (M3), ported from the host's streaming runtime: keys, variants, the seek and quality-switch matrix, the throttle predicates, segment responses, lifetimes and cache eviction. No tokio, fs, clock or rand: every time is an argument (`i64` milliseconds), every file listing is a [`SegmentSet`]. The `engine` crate (later) owns the processes, the disk and the clock and feeds these functions. Paths below are relative to `fliks/backend/src/modules/streaming`; the vectors are `fixtures/session/` (`docs/tests.md`).

Plan sections: 3 "Exécution", 4 "Cache", 5 "Seek et changement de qualité" and "Segments", 9 `core::session`, 12 item 5.

## 1. Session keys and variants

- `session_key(mediaFileId, ownerId?, cacheKey)` = `<media>-u<owner>-<cacheKey>`, `anon` without an owner. [ts:transcoding/session-key.ts:13-20]
- `SessionVariant` = `Main | Early | Remux { audioIndex?, keyframeGrid }`. [ts:transcoding/variant.ts:11-27]
- `remux_variant`: a multi-audio source drops the audio index (every rendition is published), a single-audio one defaults it to 0. [ts:transcoding/variant.ts:32-42]
- `variant_suffix`: `""`, `-early`, `-remux[-a<N>][-u]` (`-u` when the grid is uniform, not keyframe). `variant_hash` appends it to a base hash. [ts:transcoding/variant.ts:48-69]
- `base_profile_hash` strips one trailing variant suffix (leftmost `-early` or `-remux[-a<digits>][-u]` that reaches the end). [ts:transcoding/variant.ts:73-77]
- Concurrent playbacks of one `(file, owner, profile)` fork onto their own job by appending the live session's instance suffix (`~i<8 alnum>`) to the base hash: `with_instance_suffix`. [ts:transcoding/transcoding.service.ts:~405]

## 2. Profile hash (`cache/v1`)

[new] The cache moves to a new namespace, `cache/v1`, purged once from the old layout (plan section 4): the hash does **not** reproduce `computeProfileHash` (sha1 of a `|` list), it only has to be stable. No TS parity, no fixtures: snapshot and stability tests.

- Inputs: the host's `PlaybackProfile` fields [ts:transcoding/profile-hash.ts:30-77]: video codec, bit depth, hdr, audio codec / channels / mode / encoder id, per-rendition mask (`c`, or `t<channels><encoder>`), mux flavour, audio layout, segment duration (ms), tv platform, timeline origin and format start, source version, `dolbyVision`, `dvTonemap`, `dvTonemapHdr10`, `hdr10PlusStrip`, `doviElStrip`, tone-map curve.
- `canonical()`: `cache/v1|` then `key=value` pairs joined by `|`, fixed order, every field always present (no optional segments, so a new field can never alias an old one), floats as the 16 hex digits of their IEEE bits (`-0` folded to `0`), unknown text `null`/empty as in the TS. The TS rules kept: a `var-stream-map` layout hashes the rendition mask instead of the picked track's channels/mode/encoder; `dvTonemapHdr10` only counts with `dvTonemap`; the curve only counts for an SDR-output tone-map. [ts:transcoding/profile-hash.ts:87-123]
- Hash: FNV-1a 64 over the canonical bytes, 16 lowercase hex digits (core has no hash dependency; the key is a cache directory name, not a security boundary). `SEGMENT_TIMELINE_VERSION` (2) is part of the input: raise it when the segment layout changes.
- `profile_from_layout`: the session fields the hash derives from, frozen at playback-info [ts:transcoding/session-profile.ts:12-113], `buildPlaybackProfileFromContext` [ts:transcoding/profile-hash.ts:140-186]. Encoder ids are inputs (they belong to `core::args`): `LayoutAudio { codec, channels, mode, encoderId }`.
- Layout on disk: `cache/v1/<u<id>|anon>/<mediaFileId>/<cacheKey>[/<quality>]`, `cache_path()`. [ts:transcoding/transcode-cache.service.ts:~380]

## 3. Lifetimes

`LifetimeConfig` (`api::LifetimeConfig`, in `EngineConfig.lifetime`, every field defaults, additive in v1; zero means unset) holds the durations of `lifetime-constants.ts` [ts:lifetime-constants.ts:19-27]; env vars are not read by the engine, the host sends them:

| Field | Default | TS |
|---|---|---|
| `liveSessionTtlMs` | 30 000 | `STREAM_LIVE_SESSION_TTL_MS` |
| `liveSessionGcIntervalMs` | 5 000 | `STREAM_LIVE_SESSION_GC_INTERVAL_MS` |
| `sessionReviveTtlMs` | 900 000 | `STREAM_SESSION_REVIVE_TTL_MS` |
| `maxSessionsPerOwner` | 10 | `STREAM_MAX_SESSIONS_PER_USER` |
| `jobGraceMs` | 60 000 | `STREAM_JOB_GRACE_MS` |
| `jobFallbackTimeoutMs` | 1 800 000 | `STREAM_JOB_FALLBACK_TIMEOUT_MS` |
| `pinnedSessionTtlMs` | 3 600 000 | `pinnedTtlMs` [ts:live-session.service.ts:389] |
| `cacheGcIntervalMs` | 300 000 | `TRANSCODE_CACHE_GC_INTERVAL_MS` |
| `jobStallTimeoutMs` | 30 000 | [new] stall watchdog, `0` disables (the one field where zero is not "unset") |

The cache TTL (4 h) and budget (20 GiB) are `streaming.cacheTtlMs` / `cacheMaxBytes`; absent or non-positive keeps the default (`CachePolicy::from_config`). [ts:transcoding/transcode-cache.service.ts:66-69]

Decisions (all `<` / `<=` as in the TS):
- `live_session_expired(lastBeat, pinned, now)`: `lastBeat < now - (pinned ? pinnedTtl : ttl)`. [ts:live-session.service.ts:642-647]
- `tombstone_revivable(lastBeat, now)`: `!(lastBeat < now - reviveTtl)`; a presented sid revives it. [ts:live-session.service.ts:578-585]
- `session_to_evict(lastBeats, cap)`: at `len >= cap` the oldest beat (first on ties) goes. [ts:live-session.service.ts:439-452]
- `job_idle(...)` = `cleanupStaleSessions` for one run: no base hash, or never paired with a viewer: idle fallback (`now - lastAccess <= jobFallbackTimeout` keeps it; past it the entry goes and the process is killed only if still running). Paired: a viewer clears the zero-live clock; a fetch within `jobGrace` keeps it; else the clock starts, and `jobGrace` later the run is reaped and killed (cache kept). [ts:transcoding/transcoding.service.ts:1553-1620]

## 4. Seek and quality switch (`resolve_existing`)

[ts:transcoding/transcoding.service.ts:451-537] The segment index of the request drives it. Constants: `SEEK_WAIT_THRESHOLD = 15` [ts:transcoding/constants.ts:28], `REMUX_MAX_WAIT_SEGMENTS = 3`, `REMUX_WAIT_BUFFER_SECONDS = 3`. A run is *producing* when its process is alive or its output is pending. [ts:transcoding/transcoding.service.ts:254]

Inputs: the run (`exitCode?`, `signal`, `outputPending`, `startSegment`, `remux` + its assembler progress), `requestedSegment`, `qualityMatch`, the run's `SegmentSet`. Output `ResumeAction`: `Serve` (the segment is produced), `Wait` (attach, the segment is expected), or `Respawn { kill, startSegment?, purgeCache }` (the caller spawns again; the map entry stays until then).

1. Exited 0 and quality matches: remux serves iff the assembler `can_serve(.., exited)`; otherwise `segment_nearby` (the segment or the one before). Reachable: reuse. Not: `Respawn { startSegment: requested }`, no kill.
2. Not producing: killed or crashed (`signal` or non-zero exit) purges the cache dir unless remux; a clean exit for another quality does not. `Respawn { startSegment: none }`, no kill.
3. Producing, quality differs: `Respawn` as is (the caller kills the old rung).
4. Producing remux: reuse iff `can_serve(.., live)`, else `Respawn { kill, startSegment: requested }`.
5. Producing transcode, segment not nearby: `runStarting` (requested in `[start, start + 15]` and nothing at or below `start` yet) or `within_reach(requested, 15)` (a segment in `[requested - 15, requested]`) waits; otherwise `Respawn { kill, startSegment: requested }`.
6. Producing transcode, segment nearby: `first_missing(requested)` (2000 look-ahead) below `startSegment` is an unreachable gap: `Respawn { kill, startSegment: gap }`. Else reuse (`Serve` iff the segment itself is on disk).

`can_serve` (remux assembler): exited: `frontier set && start <= seg < frontier`. Live: `seg < start` is false; window = `3` without a measured rate, else `min(3, max(1, round(rate * 3)))`; `frontier unset ? seg - start <= window : seg <= frontier + window`. [ts:transcoding/remux-assembler.ts:351-362]

## 5. Segment responses

[ts:streaming.controller.ts:2506-2545, 2081-2120, 238-246] after the wait for the file (60 s; 10 s on the early companion; init files then get 5 s to become non-empty):
- file landed, size 0 or vanished: `503` + `Retry-After: 2`.
- file never landed: `503` while the run produces or was killed on purpose (a seek or rung switch is not a missing segment), `404` once it is over. The audio route tests `exitCode === null` only, `producing` carries that.
- stale `?sid=` (registry no longer knows it, tombstone expired): `410` with the `session_expired` body; no sid passes. [ts:services/session-router.service.ts:104-109]
- every segment: `Cache-Control: no-store` (a CDN must not pin a 0-byte answer).
- [new] segment names accept `\d+` (`seg-<n>.m4s|ts`, `init[_<n>].mp4`), the TS allowed 3-4 digits. [ts:streaming.controller.ts:161]
- [new] the production frontier is the in-memory `SegmentSet` (insert when a segment lands, `purge_from` on a restart [ts:transcoding/segment-utils.ts:127-170]), never `existsSync`. `latest`, `first_missing`, `nearby`, `within_reach` mirror the TS disk helpers [ts:transcoding/segment-utils.ts:48-125, 172-187]; the flat and `0/` layouts are one set.

## 6. Throttle and pause

[ts:transcoding/ffmpeg-throttle.service.ts:25-225]
- `decide_throttle(ahead, paused, threshold)`: running and `ahead > threshold` pauses; paused and `ahead <= threshold / 2` resumes; else nothing. [:25-31]
- `run_lead_seconds(span, playhead)`: `0` when the playhead is before the run's start, else `frontier - playhead`. [:46-49]
- `job_playhead_seconds(viewers, grid)`: max over viewers of `position` and the start of the last requested segment (uniform grid via `segment_index_to_seconds`, or the remux assembler's boundaries); `None` without a viewer. [:54-72]
- `is_throttle_eligible(kind, viewers)`: not `early`, no pinned viewer. [:79-84]
- Transcode run span (inside `throttle_step`): from `startSegment` to the first missing one counted from the remembered frontier (never from another run's islands); `None` before the first segment lands or when nothing is past the start. The remux span comes from the assembler. [:166-190]
- `throttle_step`: one tick for one run: pause capability `none` skips; throttle off or an ineligible run resumes a paused one; no viewer or no span skips; else `decide_throttle`. A dead process skips. [:128-150, 108-126]
- Pause by capability [ts:transcoding/ffmpeg-pause.ts:15-40] (capability itself is `hw::pause_capability`): `stdin` writes `p`/`u`, `signal` sends `SIGSTOP`/`SIGCONT`, `none` does nothing: `pause_command`. The segment-rate clock freezes while paused (engine side).

## 7. Cache eviction

[ts:transcoding/transcode-cache.service.ts:268-306] `plan_gc(entries, now, policy)` over an index of `{ key, totalBytes, lastAccess, live }`: (1) every non-live entry with `now - lastAccess > ttl` goes, in index order; (2) if the remaining total exceeds `maxBytes`, non-live entries go oldest `lastAccess` first (stable on ties) until the total fits. A live entry (its dir is, or holds, a session's dir) is never evicted. The index is rebuilt from disk before each pass (engine side); an entry's `lastAccess` is its newest file mtime.

## 8. Deviations and divergences

- Profile hash differs from the TS by design (section 2).
- Segment names: `\d+` (section 5).
- `MAX_TOMBSTONES` (500) trimming of the live-session registry is not ported (registry state stays in the engine's session map).
- The 503/404 segment rule has no TS function to call: it is inline in the controller, so it has hand-written tests, no golden vectors.

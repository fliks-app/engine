# Spec: estimate-sizes

`estimate_sizes(req) -> [QualityEstimate]`, a pure port of the host's download-quality estimate. Golden vectors: `fixtures/estimate-sizes/*.json` (`input`, `expected`, produced by calling the real controller method). Tags are relative to the `fliks` repo root.

## Contract

- Input `{ info: MediaInfo, fileSize? }`: `fileSize` is the source size in bytes (host `resolved.size`), 0 when unknown. No `StreamingConfig`: the host reads no setting here. `[ts:backend/src/modules/streaming/streaming.controller.ts:738-800]`
- Output: bare array `[{ key, label, estimatedSize }]`, `key == label == rung name`, `estimatedSize` in bytes (f64, may exceed 2^32). `[ts:backend/src/modules/streaming/streaming.controller.ts:795-799]`

## Algorithm

1. Source facts: `video[0]` width/height default 1920x1080, `durationSeconds` default 0, `formatBitRate` default 0 (raw, kept as the fallback divisor). `[ts:backend/src/modules/streaming/streaming.controller.ts:745-751]`
2. Rungs: the SDR ladder without `eco-*` (the host's `PROFILES` is the desktop ladder, never the HDR or eco ones), in ladder order, kept when `profile_fits_source` (bucketed heights). `[ts:backend/src/modules/streaming/transcoding/profiles.ts:95,299]` `[ts:backend/src/modules/streaming/streaming.controller.ts:762-763]`
3. Video bps = `capped_rung_video_bitrate_bps`: output codec = source codec (raw, so only exactly `hevc` gets the Main-tier clamp), source video bitrate = `resolve_source_video_bitrate_bps(video[0].bitRate, formatBitRate, sum(audio.bitRate))`. `[ts:backend/src/modules/streaming/transcoding/quality-ladder.ts:30-56]` `[ts:backend/src/modules/streaming/transcoding/profiles.ts:47-66]`
4. Frame rate is `Number(video.frameRate) || 0`: a fraction such as `24000/1001` is NaN, so 0. `[ts:backend/src/modules/streaming/streaming.controller.ts:773]`
5. Audio: every `info.audio` track adds its bitrate on the rung's stereo budget: copied (own bitrate, else the encode rate) when its codec is aac/ac3/eac3/opus/flac, else eac3 at `min(ch, 6)` for 6+ channels, else aac. Same `output_bitrate_bps` as `decide`. `[ts:backend/src/modules/streaming/transcoding/audio-encode.ts:177-193]`
6. Size: `duration > 0 ? floor(total * duration / 8) : floor(fileSize * total / max(formatBitRate, total))`, then `min(size, fileSize)` when `fileSize > 0`. `[ts:backend/src/modules/streaming/streaming.controller.ts:786-796]`

## Notes

- Rung tables, codec factors and the audio rate come from `ladder` and `decide::audio`: nothing is duplicated.
- `[assumed]` the JS `Number()` of a frame rate string is replaced by Rust `parse::<f64>` on the trimmed string, finite only; hex or `Infinity` spellings are not covered.

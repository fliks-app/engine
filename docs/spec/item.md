# Spec: items (audio parts as one timeline)

An item is an ordered list of parts, each a file (or a slice of one), played as one continuous timeline: an audiobook of N files. The engine knows nothing of what an item is for; the host decides which file stands for each position. Code: `core::item` (timeline, plan, argv), `core::decide::item` (decision), `engine::runtime::item` and `engine::serve::item` (runs and routes). [new]

## 1. Wire

- Resolver: `ResolveResponse.parts` (`{ path, durationS?, startS?, endS?, format? }[]`, in playing order) answers a `stream` or `download` request in place of `path`. A single `path` stays valid. The engine reads `parts` for `GET /api/stream/{id}/parts/{n}`; an answer with neither `path` nor `parts` is a host failure, as before.
- Open and decide: `DecideMedia.parts` (`{ path, info, startS?, endS? }[]`). `media.path`, `media.ext` and the request's `info` stay required and are the first part's; a request without `parts` whose `info` has audio and no video is an item of one part. `startS`/`endS` slice the file (file seconds); a part's duration is `endS or info.durationSeconds` less `startS`.
- Decision: `PlaybackInfo.parts` (`{ index, playUrl, offsetS, durationS, contentType }[]`), present when the request has `media.parts`. `mediaFileId` is the id the data-plane URLs and the resolver use for the whole item.

## 2. Decision (`core::decide::item`)

Per part the picked audio track is `selection.audioStreamIndex` when the part has it, else its first.

1. Direct Play: every part is a whole file whose container (extension or an ffprobe format name) and codec one profile lists, within the channel cap, and the client neither requires HLS nor rejects copy. `playUrl` is the first part's byte-range URL (`/api/stream/{id}/parts/0`, or `/api/stream/{id}` for a request without `parts`); the client plays `parts[]` in order.
2. DirectStream: otherwise, when `decide_audio` copies every part, they share codec, channels, sample rate and profile, and the client does not reject copy: one HLS with the audio copied.
3. Transcode: otherwise every part is encoded to the one group codec (AAC unless a surround source takes a Dolby group), at the widest layout any part keeps. Reasons add `ItemFormatsDiffer` (copyable parts of different formats), `ItemPartTrimmed` (a slice cannot be a byte range), `ClientRequiresHls`, `ClientRejectedCopy`, `ContainerNotSupported` and the per-track `Audio*` flags.

HLS is `playUrl = /api/stream/{id}/item/index.m3u8`, fMP4 unless the device asks for MPEG-TS (`useTs`). `decide_audio` is the video path's own: a codec it cannot copy into fMP4 (mp3, vorbis) is encoded. `Decision.videoVariant` is a placeholder (H.264, 8 bit, SDR) for the required field.

## 3. Timeline (`core::item::ItemTimeline`)

- Every part is cut in segments of `SEGMENT_SECONDS` (6 s); its last one is what remains (a part has `ceil(duration / 6)` segments, none when it has no duration). A segment never holds samples of two parts, so no decode crosses a file boundary and no part's tail needs the next one.
- The playlist numbers segments over the whole item (`seg-0000.m4s` ...), one `EXTINF` each, `EXT-X-ENDLIST`, one `EXT-X-MAP` (`init.mp4`) for fMP4. No discontinuity: every part is written in the same format, and a segment's timestamps are the item's.
- `locate(position)` maps a position to (part, offset in the part, global segment, local segment); a boundary belongs to the part that starts there, parts with no duration are skipped, a position past the end is the end of the last part.
- Timestamps: fMP4 segments get `tfdt = part offset + local grid time` (`rewrite_segment_tfdt` with the part's offset as origin), MPEG-TS runs stamp it with `-output_ts_offset` (1 s headroom + offset). The timestamps follow the declared durations: parts shorter or longer than declared leave a gap or overlap of the difference, so the host sends probed durations.

## 4. Runs (`engine::runtime::item`)

- One run per part and session, key `<file>-u<owner>-<hash>-p<part>`, directory `<hash>/p<part>/`. `core::item::run_args`: `-ss` at the segment's file time (plus `startS`), `-t` the part's remainder capped at `RUN_SEGMENTS` (30) segments, audio only (`-vn -sn -dn`), the audio plan of the decision (copy, or the existing AAC encode with its alignment filter), HLS output numbered from the local segment.
- A request for a segment of part `p`: the file when it is on disk; else the part's run when it will reach the segment (`resolve_existing`, which also restarts a run too far behind); else a new run at the segment (the old one is killed when it still runs). A run exits cleanly after its budget, so a request past `start + RUN_SEGMENTS`, or past an exited run's last segment, starts the next run instead of waiting.
- `init.mp4`: any part's, when one is on disk, else the run of the playhead's part (`startAt` of the playlist, heartbeats, `startAtS`). Every part writes the same format, so the inits are interchangeable.
- Opening or fetching the playlist warms the run of the playhead's segment. Runs of an item are never throttled (they are bounded) and never use a GPU. A part's run is not stopped when another part is requested: it ends by itself, and an ended session stops them all.
- Segment events are `SegmentServed { track: audio, rung: "item", index }` with the global index. `GET /v1/sessions/{sid}` reports the run of the playhead's part (`job.quality = "p<part>"`, `startSegment` local to the part).
- The cache hash (`cache/v1|item=1|...`) covers the output format, `sourceVersion` and each part's path and slice: reordering or replacing a part forks the directory.

## 5. Routes

`/api/stream/{id}/item/index.m3u8`, `/api/stream/{id}/item/{init.mp4|seg-N.m4s|seg-N.ts}` (global `N`; 404 past the last), `/api/stream/{id}/parts/{n}` (`Range`, `purpose: stream`, or `download` with `?download=1`; the part comes from the resolver's `parts`, 404 past the last; a slice is not applied). All behind the resolver like the video routes, `sid` and `token` propagated into every URI.

## 6. Not done

- Part versions (parts sharing a `position`): the host sends one file per position.
- Chapters inside a part, per-part audio-track choice other than one index for all, gapless encoder priming across parts (a seam can be one AAC frame, 21 ms, off).
- MPEG-TS item output is covered by fake-ffmpeg session tests only; the real-ffmpeg test runs fMP4.

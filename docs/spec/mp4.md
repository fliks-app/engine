# Spec: mp4

`core::mp4`: the ISO-BMFF and timeline work of segment packaging, bytes in, bytes out (no fs, no clock). Golden vectors: `fixtures/mp4/*.json`, bytes as base64, 64-bit ticks as decimal strings. Tags are relative to `fliks/backend/src/modules/streaming/transcoding/`: `tl` = `timeline.ts`, `cmaf` = `cmaf-rewrite.ts`, `ra` = `remux-assembler.ts`, `sb` = `segment-boundaries.ts`, `st` = `source-timeline.ts`, `fa` = `ffmpeg-args.ts`.

## Errors

Where the TS throws, the port returns `Err(Mp4Error)`: `OutOfRange` for a Node buffer read or write out of bounds (`ERR_OUT_OF_RANGE`, `ERR_BUFFER_OUT_OF_BOUNDS`), `Msg(text)` with the TS message otherwise. Reads use the whole buffer, as Node does: a field read past its box but inside the buffer reads the next bytes. `buf[i]` past the end is "not 1". Latin-1 slices clamp. Ticks are `i128` (the TS uses `BigInt`). `[ts:tl:46-63]`

## Boxes `[ts:tl:44-84,288-303]`

- `read_box_header(buf, off, start, end)`: size = u32 at `off` (0 with under 8 bytes left); 1 with 16 bytes left reads the u64 largesize (payload at `start + 16`); 0 means `end - start`; `None` when size < 8 or `start + size > end`.
- Box iteration over `[start, end)` stops at the first malformed header. `find_box` = first of a type.
- `make_box(type, children)` = 32-bit size + type + concatenation.

## Init and fragments `[ts:tl:86-529]`

- `parse_init_tracks(init)`: `moov`'s `trak`s with a `tkhd` and `mdia/mdhd`: track id (tkhd version 1 at +20, else +12), timescale (mdhd idem), skipped when 0; `isVideo` = hdlr handler `vide`. Ordered by first insertion, a repeated id overwrites the value in place (JS `Map`).
- Fragment tfdts: every top-level `moof`, each `traf` with a `tfhd` (track id at +4) and a `tfdt` (version 1 = signed i64 at +4, else u32); the tfdt is read before the tfhd is checked.
- `rewrite_segment_tfdt(seg, tracks, index, duration, startPts)`: unchanged without tracks, fragments or a reference timescale; reference = the first video fragment, else the first; `runStart = index*duration + startPts - tfdt/ts`, snapped to `round((runStart - startPts)/duration)*duration + startPts` without video; each fragment of a known track moves by `round(runStart*ts)` ticks. `[ts:tl:170-205]`
- Writing a tfdt: negative fails (`tfdt V of track T would be negative at offset O`), version 1 writes u64 (`OutOfRange` past it), version 0 writes u32 or fails (`tfdt V exceeds the 32-bit version-0 box at offset O`). A non-integral tick count fails like JS `BigInt` (`The number NaN cannot be converted to a BigInt because it is not an integer`). `[ts:tl:221-238]`
- `read_init_edits(init)`: per track (init order), the first `trak` with that id, its `edts/elst` (version 1: u64 duration + i64 media time, else u32 + i32): add the first media time other than -1; each empty edit before it subtracts `duration * timescale / mvhd timescale` (integer division); no mvhd timescale fails (`track T has an empty edit and no movie timescale`). 0 without an edit. `[ts:tl:242-275]`
- `with_init_edits(init, secondsOf)`: top-level boxes copied (iteration stops at a malformed one); in `moov`, each `trak` of a parsed track loses its `edts` and gets one edit after each `tkhd`: `round(seconds * timescale)` ticks, version 1 above `0x7fffffff` (i64), else i32, rate 1.0, duration 0. `[ts:tl:305-352]`
- `retime_fragments(seg, deltas)`: every fragment moved by its track's delta; a track without one fails (`fragment of track T, absent from the init`). `first_tfdt(seg, id)`: the first fragment's tfdt of that track. `[ts:tl:355-371]`
- `trun_samples(buf, traf, trexDefault)`: tfhd defaults (flags 0x8 duration, 0x10 size, 0x20 flags; skipping 0x1 base offset and 0x2 description index), then trun fields (0x1 data offset skipped, 0x4 first-sample flags, 0x100/0x200/0x400/0x800 per sample; cts signed). `[ts:tl:421-472]`
- `video_decode_extent(moofs, id, trexDefault)`: first matching traf's tfdt plus every matching sample duration; `None` without one or when that traf has no tfdt. `parse_trex_defaults(init)`: `moov/mvex/trex` id at +4, default duration at +12. `[ts:tl:485-529]`

## CMAF rewrite `[ts:cmaf:51-173]`

- `is_cmaf_rewritten(buf)`: false under 12 bytes or on a leading `styp`; true on any first box but `ftyp`; for `ftyp`, whether `hlsf` sits at a 4-byte step from 16 within `min(size, len)`.
- `cmaf_rewrite(buf)`: walk top-level boxes; size 0, size 1 or a box past the end copies the rest verbatim; `sidx`/`styp` dropped; `ftyp` gets `hlsf` appended (size + 4) unless present; the rest copied. Bytes after the last 8-byte header are dropped.
- `rewrite_cmaf(bytes)` (the host's `readAndRewriteCmaf` without the file read): `None` for empty, the bytes when already rewritten, else `cmaf_rewrite`.

## Grid and source timeline `[ts:sb:29-89]` `[ts:st:33-93]`

- `compute_segment_grid(keyframes, origin, end, seg, frame = 1/24)`: keyframes from `origin - frame/2`; `None` when none or `seg <= 0`; start `max(origin, first)`; a cut at each keyframe at or past the advancing target unless it is within half a frame of `max(end, last)`; returns durations, boundaries, kept keyframes, `firstKeyframe` (then their count).
- `grid_segment_index(boundaries, seconds, origin)`: 0 for `seconds <= 0` or under 2 boundaries; the window holding `seconds + origin`, else the last.
- `remux_segment_grid(scan, origin, seg, frameRate)` = the grid at `frame_seconds_of(parse_source_fps(frameRate))`.
- `source_timeline(info)`: origin = video[0] first frame, else its start, else 0; formatStart = container start, else video start, else 0; clockBreak = the break; end = break, else video end, else `formatStart + duration`. `input_seek_seconds = content + origin - formatStart`, `served_shift = max(0, -origin)`, `served_origin`, `cue_offset_seconds = formatStart + shift`. The host's once-per-file warning is not ported.

## Remux plan `[ts:ra:96-153,806-865]` `[ts:fa:1412-1462]`

- `remux_run_start(grid, startSegment, seg, origin, audioGrid)`: no grid: start `origin + startSegment*seg`, seek only past segment 0; past the grid fails (`remux segment S is past the last one (N)`); segment 0 seeks to its keyframe's dts less 0.01 s; later ones seek to the dts, audio on the packet `ceil((dts - first + padding)/frame - 1e-9)` of the encoded grid (`dts` when copied).
- `remux_edits(start, firstDecode)` = `{ video: start - firstDecode, shift: max(0, -start) }`; `remux_assembly_plan` takes the grid's `firstKeyframe`, keyframe pts and dts, `start` (first boundary, else origin), first dts, boundaries, `segmentDuration` (default 3), `audioRenditions` (default 0); `dir` and `gopDir` stay with the caller.
- `plan_boundary_seconds(plan, i)` = boundary `min(i, last)`, else `start + i*seg`; `gops_of(plan, s)` = `[s]` without a grid, `None` past it, else its GOP numbers.
- `parse_frame(moof, mdat)`: one-sample audio fragment (traf after the moof header, tfdt, trun), mdat payload after its header; errors `audio fragment missing traf/tfhd/tfdt/trun`, `expected 1 sample per audio fragment (frag_every_frame), got N`.
- `build_fragment(track, seq, tfdt, samples, data)`: `moof(mfhd, traf(tfhd base-is-moof, tfdt v1, trun))` + `mdat`; trun flags data offset, duration, size, flags (+ cts when any sample has one; version 1 when one is negative), data offset = moof size + 8.

- `remux_audio_grid(plan, libfdk)`: the packet grid of an encoded track at 48 kHz (`aac` 1024/1024, `libfdk_aac` up to 6 channels 1024/2048, `opus` 960/312, `ac3`/`eac3` 1536/256 samples), `None` for a copy; no plan is AAC stereo. `[ts:fa:1465-1470]` `[ts:audio-encode.ts:48-93]`

## Assembler `[ts:ra:192-745]`

The decisions of `RemuxSegmentAssembler` are pure (`assemble.rs`); `engine::remux` owns the files (`docs/spec/runtime.md` section 9).
- `open_run(plan, init, firstGopMoofs)`: with a grid, the first GOP's video tfdt plus the plan's `-ss` must hit a keyframe pts within 2 ms (`first GOP starts at Xs, on no source keyframe`; no video fragment: `first GOP has no video fragment`); landing on another keyframe sets `gop_offset` and moves `next` to the first segment whose first GOP is not before it. `correction`/`dts_correction` = that keyframe's pts/dts less the tfdt time (the plan's `-ss` without a grid). Edits: `remux_edits(start, firstDecode ?? start - runVideoEdit)`; audio edits are the run's own encoder delay (`max(0, edit) / ts`). Per track delta `round((edit + shift + correction) * ts) - runEdit`, clamped so the first fragment never starts before 0. The served init is `with_init_edits` of those edits.
- `gop_complete`: without a grid or video, a renamed file is whole; else the video decode extent plus half its last sample reaches the next keyframe's dts (the last GOP: the grid end), both through the run's corrections.
- `check_gop`: the GOP's first video fragment must sit nearer its planned keyframe than either neighbour (`GOP file N starts at Xs, on keyframe K ...`); a file without a video fragment fails the append (`GOP g has no video fragment`).
- `tail_boxes(buf, from)`: complete top-level boxes of a growing file; a size-0 box or one past the end stops.
- `AudioGroup` (`open_audio` from the growing file's ftyp+moov): the first frame clamps the delta so it never starts before 0; frames group by `tfdt + round(correction * ts)` against the planned boundaries in ticks; a seeked run drops frames before its first segment; `flush` coalesces the pending frames into one `build_fragment` (sequence = segment + 1); past the grid's last segment nothing is grouped.

## Deviations

- `trun_samples` refuses a `sample_count` above 2^24 (`OutOfRange`): the TS would allocate one object per declared sample of a corrupt box.
- `output-timeline.spec.ts` asserts ffmpeg argv (args domain): only its `remuxRunStart` calls are replayed here.

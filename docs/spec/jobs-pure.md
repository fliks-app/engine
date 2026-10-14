# Background jobs, pure half (`core::jobs`)

Argv, output parsers and result assembly of three background jobs, so the engine can run them without
re-deriving anything. No spawn, no file read, no clock: the caller runs the tools (priority, slot, timeout,
cancellation are its own) and feeds the output back. Sources are the host's job code, paths relative to
`fliks/backend/src`. Fixtures: `fixtures/jobs/{scan,fingerprint,subtitles}` (see `docs/tests.md`).

## 1. Keyframe scan (`jobs::scan`)

Source: `modules/streaming/services/{source-scan,video-packets}.ts`. The result is `api::SourceScan`
(already the wire type of the `decide` input; `schemaVersion` is left for the caller to stamp, like probe).

- `ScanPlan::new(info, path)`: first video stream, `mpeg_ts` (`decide::source::is_mpeg_ts`: format name, else
  extension), the AAC tracks (MPEG-TS only). No video stream: `no video stream to scan`.
- Reorder lead: `known_lead` when `reorderFrames` and `avgFrameRate` are known, else `reorder_args` (ffprobe
  `stream=has_b_frames,avg_frame_rate`, 30 s) and `lead_from_probe(first line)`. `lead_seconds` truncates to
  whole microseconds as ffmpeg does; a frame rate that is not `n/d` with both positive gives 0.
- Packets: `packet_args` (ffprobe `packet=pts_time,dts_time,duration_time,flags`, compact output, the file or
  `pipe:0` when `plan.piped()`), one `PacketReader::feed_line` per non-empty stdout line until it returns
  false (the clock broke: kill ffprobe). `scan_timeout_ms(size)` is `max(300 s, size / 10 MiB/s)`.
- Reader rules: a packet with no finite pts is skipped; an unknown dts is the previous packet's dts + duration,
  else `pts - lead`; keyframes are lines whose flags contain `K`; `end` is the largest `pts + duration`; a
  missing or `N/A` duration is 0. On MPEG-TS only, a decode time more than 0.1 s behind the last one or more
  than 300 s past its end breaks the clock: `breakSeconds` is the `end` so far, the rest is ignored.
- ADTS (MPEG-TS with AAC): the caller reads the file once and feeds the same chunks to ffprobe (stdin) and
  to a `mpegts::Demuxer`. `pid_args(file)` is ffprobe `stream=index,id` (compact output, any `id=0x...` line,
  repeated lines harmless) and `aac_pids(plan, lines)` maps each AAC track to its PID by stream index (`None`
  when ffprobe gave none, no guessing by order). `TsDemuxer::new(&pids)` / `push(chunk)` / `finish()` hands
  back one `Option<AdtsConfigWalker>` per track; the walker notes any header whose profile, sampling index or
  channel configuration (private bit masked) differs from the first, across PES and chunk boundaries.
  `audio_config_changes(aac, walkers)` reports a track as changing when its walker is `None`.
- `mpegts::Demuxer` (no dependency: `core` stays serde-only, and the PAT/PMT/PES subset a scan needs is about 200
  lines):
  - Framing: the first run of 4 sync bytes (0x47) at a 188, 192 (M2TS, 4-byte timecode prefix) or 204
    (Reed-Solomon suffix) spacing fixes the stride (`mpegts` in ffprobe covers all three); a missing sync
    byte afterwards drops the lock, resets every track and PSI buffer and searches again. A last packet cut
    by the end of the data is read at `finish`.
  - Packet: transport-error packets are dropped; a duplicate packet (same continuity counter) is dropped; a
    continuity gap or a discontinuity indicator drops the rest of the PES in flight (the partial ADTS frame
    is forgotten) until the next payload_unit_start; adaptation-field-only packets carry no payload and keep
    the counter; scrambled payload makes the track unreadable.
  - PES: at payload_unit_start the 9-byte header and its `PES_header_data_length` are skipped, even when the
    header spans packets; no `00 00 01` start code makes the track unreadable. The ES bytes go straight to the
    track's walker (a frame may span PES).
  - PSI: PAT (PID 0) names the PMT PIDs; sections are reassembled across packets (pointer_field, 1 KiB cap,
    CRC not checked). A PMT (any version, so an update too) listing a tracked PID with a stream type other
    than 0x0f makes that track unreadable. A tracked PID is fed from the first packet, PMT or not.
  - `finish` gives `None` for an unreadable track and for one whose walker never saw a frame: both report
    "may change", the other tracks are unaffected.
- `PacketReader::finish(audioConfigChanges)`: the `SourceScan`; a stream with no timed packet is an error
  (`no video packet with a time`).

## 2. Intro fingerprint (`jobs::fingerprint`)

Source: `modules/markers/intro-detection.service.ts` (`fingerprint`, `fingerprintWindow`, `detectSeason*`). The
result is `api::Fingerprint { hashes, durationSeconds, baseOffsetSec }`. Matching, clustering and the marker
rows stay out (M8).

- Window choice: `lookup_seconds(setting)` is `introMaxLookupMinutes` (0, NaN or unset: 10) times 60. The intro
  reads the head: `head_args(path, lookup)` is `fpcalc -raw -length N path`. The outro reads the tail:
  `outro_window(duration, lookup)` is `[max(0, duration - lookup), lookup]`, none without a duration.
- Window pipeline: `window_ffmpeg_args` (mono 22.05 kHz WAV of `[start, start + length]` on stdout, `-ss` is the
  floored second, never negative) into `window_fpcalc_args` (`fpcalc -raw -length N -`). `baseOffsetSec` is the
  unfloored start (the TS quirk is kept). 180 s for either shape.
- `parse_fpcalc(stdout, fallback, offset)`: the first `FINGERPRINT=[0-9,-]+` and `DURATION=[0-9.]+` anywhere in
  the text; each hash is `parseInt(token, 10) | 0` (empty or odd tokens 0, 64-bit values wrap). No fingerprint
  line: `fpcalc returned no FINGERPRINT line`. A file read reports the file's length as `DURATION`, stdin 0.
- `window_failure(code, fpcalcStderr, ffmpegStderr)`: `fpcalc exited N: <fpcalc stderr, else ffmpeg's>`.

## 3. Subtitles (`jobs::subtitles`)

Sources: `modules/streaming/{subtitle-stream.service,subtitle-vtt.util,subtitle-burn-in.service}.ts`.

- Extraction: every text track (`text_stream_indices`: not image based) of a file in one ffmpeg run,
  `extract_args(path, [(stream, raw)])`: `-analyzeduration 0 -probesize 200000 -vn -an -i path` then
  `-map 0:N -c:s ass -f ass -y raw` per track (any input format, srt, ass, ssa, mov_text, webvtt, goes through
  ffmpeg's ASS encoder). `check_extracted(0, raw)` rejects an empty file (a track with no cue still has its
  header). The caller writes `ass_to_vtt(raw text)` atomically, 15 min cap, stream index at most 999.
- `ass_to_vtt`: field positions come from the `Format:` lines (styles and events); times `H:MM:SS.CC` become
  `HH:MM:SS.CC0`, other shapes drop the cue; `\N`, `\n` line breaks, `\h` a space; only `\b`/`\i`/`\u` overrides
  survive as tags (closed at the end in opening order), the other override blocks are dropped; a cue with no
  visible text is dropped. Alignment is the last inline `\anN`/`\aN` (SSA numbers mapped to the numpad) else
  the style's (`[V4 Styles]` legacy numbers mapped) and becomes `line:5%`/`line:45%`, `position:10% align:left`
  or `position:90% align:right`. No `X-TIMESTAMP-MAP` here: the host adds it at serve time and the engine's
  `serve::with_timestamp_map` already does.
- Burn-in: `burn_in_extract_args(video, stream, out)` is `-y -i video -map 0:N -c:s ass out`, 30 s, the file is
  `burn_in_file_name(mediaFileId, stream)` (`3-2.ass`) in a temp directory of the caller's. `burn_in_filter`
  builds the `-vf` step of a text track: `ass='...'` for `.ass`/`.ssa`, else `subtitles='...'` (path escaped for
  `\`, `:`, `'`), wrapped by `at_cue_time` (`setpts` shifts of the cue offset, microsecond precision) when the
  offset is not 0; a stream of the video itself is `subtitles='video':si=N`; an image track has no filter.
  The host normally sends `BurnInSpec.filter` already built; this is for an engine-extracted file.

## 4. Not ported

The services' queues, slots, `inflight` joins, event and Command rows, cache paths and settings (engine
framework); `scanSource`'s `cat` reader and stream plumbing (the caller reads); the hash matching.

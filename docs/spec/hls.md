# Spec: hls

`core::hls`: the playlist text the host serves, as pure functions. Master: `master_playlist(&MasterOptions)`. Media playlists: `vod_playlist`, `variable_vod_playlist`, `iframe_playlist`, `subtitle_playlist`, and the route-level `variant_playlist`, `audio_playlist`, `iframe_index`. Golden vectors: `fixtures/hls/*.json`. Tags are relative to `fliks/backend/src/modules/streaming/`: `mp` = `transcoding/master-playlist.ts`, `hvl` = `transcoding/hls-variant-ladder.ts`, `cs` = `transcoding/codec/codec-strings.ts`, `ctl` = `streaming.controller.ts`.

## Contract

- Every URI is `{prefix}/<path>{token}`. `prefix` replaces the host's `/api/stream/{mediaFileId}`, so the engine never names a host or a route root; `token` is the query suffix the host appends (`?token=..&sid=..`, empty when none). `[new]` `[ts:ctl:175-199]`
- Lines are joined with `\n`, no trailing newline. `[ts:mp:338,447]`
- Numbers print as JS `String(n)`; `EXTINF`, a fractional `FRAME-RATE` and the subtitle `EXTINF` as JS `toFixed(3)` (exact decimal value, ties away from zero). `[ts:mp:138-140]` `[ts:ctl:283]`
- Options come as serde structs (`camelCase`) but are not wire types yet: no endpoint serves them. `[new]`

## Master `[ts:mp:297-561]`

1. Header `#EXTM3U`, `#EXT-X-VERSION:7`, `#EXT-X-INDEPENDENT-SEGMENTS`. `[ts:mp:338]`
2. Audio layout: `audioStreams` non-empty = renditions (`multiAudio`); present and empty = `noAudio`; absent = muxed. `plans` = `audioPlans` when non-empty, else AAC stereo transcode; `noAudio` remux bandwidth uses no plan. `[ts:mp:331-343]`
3. Group CODECS audio entry: `plans[0].codec` lowercased; non-AAC maps `ac3 eac3 opus flac` to `ac-3 ec-3 Opus fLaC` (else none); AAC is `mp4a.40.N` with N the least object type any plan needs (`he-aac` 5, `he-aacv2` 29, else 2; 2 beats 5 beats 29). `codecsTail` = `,<entry>` unless `noAudio` or none. `[ts:cs:153-191]` `[ts:mp:345-347]`
4. A rung's audio bps (BANDWIDTH is a peak): 0 for `noAudio`, else the max over `plans` of `output_bitrate_bps(plan, rung.audio_bps)` (`decide::audio`). `[ts:mp:349-355]`
5. `FRAME-RATE`: `sourceFrameRate` (default 24), integer as is, else `toFixed(3)`. `[ts:mp:357]`
6. Audio renditions (`multiAudio`): picked = `defaultAudioIndex` when in range, else 0; NAME = title, else language, else `und`, a repeat suffixed ` #2`, ` #3`...; LANGUAGE = `languageTag`, else `language`, else `und`; DEFAULT and AUTOSELECT `YES` on the picked one; CHANNELS = `16/JOC` for a plan `eac3` with an `atmos` profile (any case), else the plan's channels, else (no plan at that index or no channels) 2 for an AAC group, else the source count when positive, else 2; URI `{prefix}/audio/{i}/index.m3u8{token}`. `dedupesAudioByLanguage` keeps one per language (`language`, else `und`): the picked track's own, else the first. `[ts:hvl:45-117]` `[ts:cs:195-213]`
7. Muxed rendition: when not `multiAudio`, not `noAudio`, `muxedAudio` set and it has a title, a `languageTag` or a language other than `und`: one URI-less `#EXT-X-MEDIA` (DEFAULT/AUTOSELECT YES, CHANNELS from `plans[0]` as above). `AUDIO="audio"` goes on every variant when either kind of rendition is out. `[ts:mp:408-418]` `[ts:hvl:120-135]`
8. Subtitles: group `subs`, NAME unique as in 6 over `{title: name, language}`, LANGUAGE as in 6, `DEFAULT=NO,AUTOSELECT=NO,FORCED=YES|NO`, URI `{prefix}/subtitles/embedded/{key}/index.m3u8{token}` (embedded) or `{prefix}/subtitles/{key}/index.m3u8{token}`; `SUBTITLES="subs"` on every variant. `[ts:mp:362-383]`
9. Remux variant (`includeRemux` and no `onlyQuality`, in both branches, alone): BANDWIDTH = AVERAGE-BANDWIDTH = `round(sourceVideoBitrateBps) + max(0, max over plans of output_bitrate_bps(plan, 192 kbps))` when the video bitrate is non-zero, else `round(formatBitRate)` when non-zero, else 8 000 000; RESOLUTION `remuxWidth x remuxHeight` (default the source size); `VIDEO-RANGE` from `sourceHdrFormat` (`HLG`, else `PQ` when set); `NAME="remux"`; `CODECS` only with a non-empty `remuxCodecs` (+ `codecsTail`), `SUPPLEMENTAL-CODECS` only with both; URI `{prefix}/remux/index.m3u8{token}`. Then the I-frame line, then the end. `[ts:mp:53-132,427-448,506-525]`
10. HDR branch (`hdrPassThrough`): range `HLG` for an HLG source, else `PQ`; after 9, only with `canEmitHdrLadder`: the HDR ladder (`ladder::HDR_LADDER`) kept by `profile_fits_source`, pinned (11) with the `-hdr` suffix, emitted by 12 with `hdrVariant`. Never the SDR ladder. `[ts:mp:424-490]`
11. Pin: no `onlyQuality` keeps the ladder, or with `supportsAbr: false` the first rung that fits (else the first); `remux`/`original` take that same rung; any other name (`-hdr` appended on the HDR ladder unless present) is `ladder::resolve_rung`, the whole ladder when it resolves to nothing. `[ts:mp:146-194]`
12. Ladder lines, per rung: `(w, h)` = rung width capped at the source, height `max(2, ceil(w*srcH/srcW/2)*2)`; video = `capped_rung_video_bitrate_bps` (output codec = variant codec); AVERAGE-BANDWIDTH = video + audio (4); BANDWIDTH = `round(avg * 1.5)`; CODECS per variant: AV1 `av01.0.LLM.DD` (level 04/06/08/09/12/13 by luma rate `w*h*fps` above 83.5M/156M/312M/531M/1.07G), HEVC Main10 `hvc1.2.4.L<idc>.B0` for HDR, Main `hvc1.1.6.L<idc>.B0` (idc 93/120/123/150/153/156 up to 33.2M/66.8M/133.7M/267.4M/534.8M), H.264 `avc1.6400LL` (first level whose MaxFS and MaxMBPS cover `ceil(w/16)*ceil(h/16)` and its rate, else `34`); URI `{prefix}/{rung}/index.m3u8{token}`. `[ts:hvl:27-37,164-218]` `[ts:cs:27-145]`
13. SDR branch: the SDR ladder kept by `profile_fits_source` (the last rung, `eco-720p`, when none fits), pinned (11), emitted by 12 with `sdrVariant` (default H.264 8-bit SDR). `[ts:mp:527-558]`
14. I-frame line (when `iFrameTrickPlaySegmentSeconds` is non-zero), last: BANDWIDTH `round(960000 / max(1, s))`, RESOLUTION = the source size even-rounded (`max(2, round(n/2)*2)`), capped to height 720 keeping the aspect, `CODECS="avc1.4d401f"`, URI `{prefix}/iframe/index.m3u8{token}`. `[ts:mp:385-392]` `[ts:transcoding/iframe-trick-play.ts:17-38]`

`remux_bandwidth_bps` (9's formula, shared with playback-info) and `pinned_rung_name(q, w, h, hdr)` (the rung a `startQuality` pin lands on: the name, `-hdr` appended on the HDR ladder, resolved by `resolve_rung` on the fitting ladder, else the name itself) are public too. `[ts:mp:68-77,211-225]`

Dolby Vision `[ts:transcoding/codec/dolby-vision.ts:69-91]` (`decide::dolby_vision`): `dv_supplemental_codecs` = `dvh1.08.LL` / `dav1.10.LL` + `/db1p` (compat 1, HDR10) or `/db4h` (compat 4, HLG), only for a probed level, `tv` range, `bt2020nc`, `yuv420p10[le]`; `dv_standalone_codecs` = `dvh1.05.LL` (P5) or `dav1.10.LL` (P10, compat 0/unknown). The master of a DV remux (`include_remux` and `dolbyVision`) takes the standalone string as `remuxCodecs` (with `VIDEO-RANGE=PQ`), else the probed one plus `SUPPLEMENTAL-CODECS`. `[ts:ctl:1506-1551]`

`copy_source_codec_string(video)` (the host's `remuxCodecs` input) is ported too: H.264 `avc1.PP00LL` from the probed profile and level_idc (<= 255), HEVC `hvc1.1.6.L{level}.B0` / `hvc1.2.4.L{level}.B0` (level >= 30, `main` / `main 10`), AV1 `av01.P.LLM.DD` (level <= 31, profile `main high professional`, bit depth from `bitDepth` 8/10/12 else the pixel format `p12`/`p10`, else 8 when one is named); `None` otherwise. `[ts:cs:216-305]`

## Media playlists `[ts:ctl:261-347]`

- Uniform (`vod_playlist`): `#EXTM3U`, `#EXT-X-VERSION:7`, `#EXT-X-TARGETDURATION:ceil(seg)`, `#EXT-X-MEDIA-SEQUENCE:0`, `#EXT-X-PLAYLIST-TYPE:VOD`, `#EXT-X-INDEPENDENT-SEGMENTS`, `#EXT-X-MAP:URI="<init>"` when an init is given, then for `i < uniform_segment_count(duration, seg, frame)` the entry `#EXTINF:<min(seg, duration - i*seg)>,` + URL of `NNNN` (4 digits), stopping at a non-positive length, then `#EXT-X-ENDLIST`.
- I-frames (`iframe_playlist`): the uniform one without init, `#EXT-X-I-FRAMES-ONLY` after `#EXT-X-INDEPENDENT-SEGMENTS`.
- Variable (`variable_vod_playlist`): TARGETDURATION `ceil(max(0, durations...))`, one entry per duration.
- Subtitle (`subtitle_playlist`) `[ts:ctl:1820-1849]`: TYPE before TARGETDURATION `max(1, ceil(d or 1))`, one entry of `d or target` pointing at `{prefix}/{vttPath}{token}`.

## Routes (URL composition) `[ts:ctl:1586-1622,1855-1906,2126-2242]`

- Segment URL `{base}/seg-NNNN.{ts|m4s}{token}` (`ts` when `useTs`). Variant `base = {prefix}/{quality}`, init `init.mp4`, `init_0.mp4` in the multi-audio layout except for `remux`, none with `useTs`. Audio `base = {prefix}/audio/{i}`, init `init_{i+1}.mp4`.
- A remux rendition (variant `remux`, audio with a remux session) with frozen grid durations is the variable playlist (an empty list included); otherwise the uniform one at `segmentDuration` for remux, `real_segment_seconds(segmentDuration, fps)` otherwise, frame `frame_seconds_of(fps)`.
- I-frame index: the I-frame playlist at `real_segment_seconds(segmentDuration, fps)`, URLs `{prefix}/iframe/seg-NNNN.ts{token}`.
- The host's duration, fps and session lookups (`?duration=`, `live.sourceFps`, the 404 without a duration) stay with the caller: the functions take the resolved values.

## Deviations

- A collapsed (pinned `remux`/`original`, or `supportsAbr: false`) HDR ladder with no fitting rung: the TS throws (`profileResolution(undefined)`), the port emits no rung. The fuzz skips TS throws.
- `deviceType` and `hdrPassThrough.videoBitRateBps` are read by nothing in the TS (`getLadderForDevice` ignores the device): not ported.

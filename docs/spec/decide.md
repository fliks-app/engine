# Spec: decide

`decide(input, hostCaps, config) -> Decision`, a pure port of `StreamBuilderService.evaluate()`.
Golden vectors: `fixtures/decide/*.json` (`input`, `hostCaps`, `config`, `expected`). `expected` is the exact TS result, so field names and ordering below are the contract. Tags are relative to the repo root of `fliks` on the refactor branch (`refactor/engine-m0-prep`).

## 0. Contract

| Part | Content |
|---|---|
| `input.media` | `mediaFileId`, `path`, `ext` (with dot, e.g. `.mkv`), `contentType` (host MIME of the file). `[ts:backend/src/modules/streaming/stream-builder.service.ts:305,713]` |
| `input.info` | `MediaInfo` (wire type). Only `video[0]`, `audio[]`, container fields are read. `[ts:backend/src/modules/media-probe/ffprobe.service.ts:225-243]` |
| `input.scan` | `SourceScan` or null. Only read for AAC-in-TS and the remux grid. `[ts:backend/src/modules/streaming/transcoding/source-scan.ts:14-24]` |
| `input.device` | `DeviceProfile`. `[ts:backend/src/modules/streaming/dto/device-profile.dto.ts:1-200]` |
| `input.selection` | `tokenParam` (host URL suffix, copied verbatim into `playUrl`), `burnInSubtitleId`, `burnInIsText`, `requestedQuality`, `audioStreamIndex`. All optional. `[ts:backend/src/modules/streaming/stream-builder.service.ts:216-235]` |
| `config` | The four `StreamingConfig` fields read: `autoCropEnabled`, `tonemapAlgo`, `autoQualityMode`, `segmentDuration`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:211-214]` |
| `hostCaps` | See section 11. |
| output | `{ response, useHdrLadder, videoVariant, muxFlavour, audioPlans, transcodeTonemapping }`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:237-251]` |

- `remuxGrid` is not an input: the host derives it from `scan` as `computeSegmentGrid(scan.keyframes, origin, scan.end, segmentDuration, frameSeconds)` where `origin = videoPresentationStart(video[0])`. `[ts:backend/src/modules/streaming/streaming.controller.ts:958]` `[ts:backend/src/modules/streaming/transcoding/segment-boundaries.ts:75,29]` `[ts:backend/src/modules/streaming/transcoding/source-timeline.ts:41-69]`. Null when no scan or no keyframe at/after `origin - frame/2`.
- `selection.burnInSubtitleId` is only tested for truthiness (an id of 0 means no burn-in). `[ts:backend/src/modules/streaming/stream-builder.service.ts:425]`
- `info` gets `schemaVersion: 1` stamped in fixtures; the TS blob has none. `[assumed]`
- Log lines (`audioDecision[...]`, `hdrDecision[...]`) are not part of the contract.
- Vectors replay the controller's inputs, not the specs' hand-built ones: missing `streamIndex`/`language` were filled, the legacy crop string `"w:h:x:y"` became an object, and the grid is always derived.

## 1. Step order

1. Pick the audio track and read the source facts (2).
2. HDR/DV classification, codec variant, ladder choice, tone-map flags (3, 4).
3. Build the wrapper `wrap()` (output side channels). `[ts:backend/src/modules/streaming/stream-builder.service.ts:414-423]`
4. `tryDirectPlay` (5), then DV/HDR gate, burn-in, crop, ladder gate, other guards (6, 7). Order matters: it fixes the order of `transcodeReasons`.
5. If `canDirectPlay`: return DirectPlay (9.1).
6. Copy gates reasons (7.4), audio decision (8), then DirectStream if `canCopyVideo` (9.2), else Transcode (9.3). `[ts:backend/src/modules/streaming/stream-builder.service.ts:709,732,822,889]`

## 2. Source facts

| Fact | Rule |
|---|---|
| `v` | `info.video[0]` (absent allowed: all video facts undefined, codec `""`). `[ts:backend/src/modules/streaming/stream-builder.service.ts:285]` |
| `pickedAudio` | `audioStreamIndex` if `0 <= i < audio.len` (an array position, not `streamIndex`), else 0. `a = audio[pickedAudio]`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:288-296]` |
| `hasAudio` | `audio.len > 0`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:287]` |
| `formatBitRate` | `info.formatBitRate` if `> 0`, else undefined. `[ts:backend/src/modules/streaming/transcoding/profiles.ts:47-58]` |
| `videoBitRate` | `video[0].bitRate` if `> 0`; else `formatBitRate - sum(audio[].bitRate ?? 0)` if that is `> 10_000`; else undefined. Sum is over ALL tracks. `[ts:backend/src/modules/streaming/transcoding/profiles.ts:30-43,59]` |
| `audioBitRate` | `a.bitRate`; if absent and both format and video bitrates known: `formatBitRate - videoBitRate` when `> 1000`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:299-303]` |
| `sourceContainer` | `ext` without the dot, lowercase. `[ts:backend/src/modules/streaming/stream-builder.service.ts:305]` |
| `sourceVideoCodec`, `sourceAudioCodec` | `codec` lowercased, `""` if absent. `[ts:backend/src/modules/streaming/stream-builder.service.ts:307-308]` |
| `sourceMpegTs` | `formatName` contains `mpegts` (comma list); with no `formatName`, extension in `.ts .m2ts .mts .m2t .tp .trp`. `[ts:backend/src/modules/streaming/transcoding/video-packets.ts:30-45]` |
| `source.crop` | `video[0].crop` only when `config.autoCropEnabled`, else undefined. `[ts:backend/src/modules/streaming/stream-builder.service.ts:313,337]` |
| `source` echo | `container, videoCodec, videoProfile (lowercased), videoLevel, videoBitRate, formatBitRate, videoBitDepth, width, height, frameRate, audioCodec, audioChannels, audioChannelLayout, audioBitRate, audioSampleRate, audioLanguage, durationSeconds, hdrFormat, colorSpace, colorTransfer, colorPrimaries, crop`. Undefined fields are omitted from JSON. `[ts:backend/src/modules/streaming/stream-builder.service.ts:315-338]` |
| `isSourceHdr` | `video[0].hdrFormat` is set (`HDR10` or `HLG`). DV alone with no HDR VUI is not HDR here. `[ts:backend/src/modules/streaming/stream-builder.service.ts:341]` |

## 3. Dolby Vision and HDR classification

- `dv = {profile: dvProfile, compatId: dvBlSignalCompatId, singleLayer}`; `singleLayer = profile in {5,8,10} && dvElPresent != true`. `[ts:backend/src/modules/streaming/transcoding/codec/dolby-vision.ts:33-39]`
- `noBase` (no HDR10/HLG fallback): `profile == 5 || (profile == 10 && (compat == 0 || compat is absent))`. `[ts:backend/src/modules/streaming/transcoding/codec/dolby-vision.ts:43-48]`
- `dvLabel` = `"Dolby Vision P{profile}"` if noBase else `hdrFormat`. Used in reason messages only. `[ts:backend/src/modules/streaming/stream-builder.service.ts:348]`
- `clientSupportsHdr = device.supportsHdr == true`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:349]`
- `dvProfiles` = `device.dolbyVisionProfiles` if present, else `[5,8]` if `supportsDolbyVision == true`, else `[]`. `[ts:backend/src/modules/streaming/transcoding/codec/dolby-vision.ts:27-31]`
- `dvNoBaseHdr10Eligible = noBase && clientSupportsHdr && dvNoBaseHdr10PathSupported(hwAccel, config.tonemapAlgo, {hasCrop: !!source.crop, hasBurnIn: burnInIsText})`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:356-364]` Path support (host probes, platform): `[ts:backend/src/modules/streaming/transcoding/tonemap-path.ts:68-93]`
  - videotoolbox: false. nvenc: `cuda ? cudaHdr10 : opencl ? openclHdr10 : true`. amf: `opencl ? openclHdr10 : true`.
  - Others: `path = resolveTonemapPath(algo, {hasCrop, dvNoBase: true})`; if `opencl`: win32 `qsvOpenclHdr10`, else `tonemapOpenclHdr10`; else `!(path == vulkan && hwAccel == vaapi && !hasBurnIn)`.
- `resolveTonemapPath(algo, {hasCrop, dvNoBase, dvApplyRpu})` `[ts:backend/src/modules/streaming/transcoding/tonemap-path.ts:41-66]`, `openclBridgeOk` `[ts:backend/src/modules/streaming/transcoding/tonemap-path.ts:33-40]`:
  1. `dvNoBase`: `opencl` if bridge ok; else `vulkan` if platform is linux and `vulkan` probe; (else continue).
  2. `(algo == auto || dvApplyRpu)` and bridge ok: `opencl`.
  3. `algo == auto`: `qsv` if not linux and `vppQsv` probe; else `vaapi`.
  4. else `algo` as configured.
  - Bridge: win32 `qsvOpencl`; else `hasCrop ? tonemapOpenclCrop : tonemapOpencl`.
- `dvApplyRpu` = `noBase || (profile in {8,10} && compat == 4)`. `[ts:backend/src/modules/streaming/transcoding/codec/dolby-vision.ts:52-65]`

## 4. Output codec variant, HDR ladder, tone-map flags

- `selectedVariant = pickPrimaryVariant({width, height, hdr, codec}, device, hwAccel)`; `hdr = noBase ? (dvNoBaseHdr10Eligible ? "HDR10" : null) : hdrFormat ?? null`; `codec = normalise(sourceVideoCodec)` (`h264/avc/avc1`, `hevc/h265/hvc1/hev1`, `av1/av01`, else none); width/height default 0. `[ts:backend/src/modules/streaming/stream-builder.service.ts:375-388]` `[ts:backend/src/modules/streaming/transcoding/codec/normalise.ts:7-26]`
- Selector `[ts:backend/src/modules/streaming/transcoding/codec/selector.ts:40-138]`:
  - `clientCodecs` = union of all `directPlayProfiles[].videoCodecs`, lowercased (container ignored). A codec is supported if it or its alias is listed (`h264`/`avc1`, `hevc`/`h265`/`hvc1`/`hev1`, `av1`/`av01`). `[ts:backend/src/modules/streaming/transcoding/codec/selector.ts:161-172]`
  - `codecOrder`: `rejectCopy` -> `[av1,hevc,h264]` minus source codec, source codec last; else source codec first when client supports it, then the rest of `[av1,hevc,h264]`.
  - Decode-resolution gate: codec dropped when its `codecConditions` entry has `maxWidth/maxHeight` and `resolutionFitsCap(srcW, srcH, ...)` fails (long edge vs long edge, short vs short, 0/absent = no cap). `[ts:backend/src/modules/streaming/transcoding/codec/selector.ts:179-188]` `[ts:backend/src/common/utils/resolution.util.ts:51-64]`
  - HDR candidates only when `hdr != null && (supportsHdr || tonemapsHdrLocally)`: variant `{codec, bitDepth: 10, hdr}`, h264 skipped. Pass 1: first codec with a HW encoder (`encoder.hwAccel == hostCaps.hwAccel`, or any CPU one when hwAccel is `none`) wins and stops. Pass 2 (only if pass 1 found none): first codec with any usable encoder (CPU fallback included).
  - SDR candidates always follow: `{codec, bitDepth: 8, hdr: null}` for every supported codec with a HW encoder, in `codecOrder`; if none, every codec with any usable encoder.
  - Primary = first candidate, else `{h264, 8, null}`. Output is the whole `{codec, bitDepth, hdr}`.
- Encoder usability: descriptor matches the variant exactly (codec, bitDepth, hdr); `supports()` (AMF: platform win32 only; all others true); if `hdr != null` then `supportsHdrMetadata()` (see 11); and the encoder is not in `disabledEncoders`. Registry: pass 1 exact `hwAccel` match in table order, pass 2 CPU descriptor. `[ts:backend/src/modules/streaming/transcoding/codec/encoders/index.ts:30-106]`
- `useHdrLadder = selectedVariant.hdr != null`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:397]`
- `dvNoBaseHdr10 = noBase && selectedVariant.hdr == "HDR10"`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:400]`
- `transcodeTonemaps = (isSourceHdr || noBase) && !useHdrLadder`; `runsTonemapFilter = transcodeTonemaps || dvNoBaseHdr10` (this is `transcodeTonemapping` in the output and `tonemapping` on Transcode). `[ts:backend/src/modules/streaming/stream-builder.service.ts:406-409]`
- `muxFlavour = useTs || (useTsOnSingleAudio && audio.len <= 1) ? ts : fmp4`. `[ts:backend/src/modules/streaming/transcoding/audio-layout.ts:1-10]`
- [new] With `muxFlavour == ts` and at most one audio track the copy gate below stays open, so a source whose only problem is the audio is a DirectStream of MPEG-TS GOP files (runtime.md section 9). `rejectCopy` still closes it.

## 5. tryDirectPlay (reasons in this exact order) `[ts:backend/src/modules/streaming/stream-builder.service.ts:1289-1429]`

Inputs: `source`, `device`, `hasAudio`. `containerSupported/videoSupported/audioSupported` = any `directPlayProfiles` entry lists the container / video codec / audio codec (exact, case-sensitive string match against the lowercase source value). `!hasAudio` forces `audioSupported`.

- `boundMatch` = one single entry lists container AND video codec AND (audio codec or `!hasAudio`). `[ts:backend/src/modules/streaming/stream-builder.service.ts:1321-1326]`
- `videoConditionsMet` starts true; only evaluated when `videoSupported` and `codecConditions` non-empty and an entry has `codec == sourceVideoCodec` (first match). Each failing check pushes its reason and clears the flag:

| Flag | Trigger |
|---|---|
| `VideoLevelNotSupported` | `cond.maxLevel` truthy, `videoLevel` truthy, `videoLevel > maxLevel`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:1335-1345]` |
| `VideoProfileNotSupported` | `cond.profiles` non-empty, `videoProfile` (lowercased) set, not in `profiles` (exact, case-sensitive). `[ts:backend/src/modules/streaming/stream-builder.service.ts:1346-1356]` |
| `VideoBitDepthNotSupported` | `maxBitDepth` and `videoBitDepth` truthy, `videoBitDepth > maxBitDepth`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:1357-1367]` |
| `VideoResolutionNotSupported` | `!resolutionFitsCap(w, h, maxWidth, maxHeight)`. Message uses `?` for an absent cap. `[ts:backend/src/modules/streaming/stream-builder.service.ts:1368-1381]` |

- `VideoBitrateNotSupported`: `maxStreamingBitrate > 0` and `total > maxStreamingBitrate`, `total = formatBitRate ?? (videoBitRate ?? 0) + (audioBitRate ?? 0)`; also clears `videoConditionsMet`. Message `Bitrate too high (N Mbps)` (rounded). `[ts:backend/src/modules/streaming/stream-builder.service.ts:1386-1397]`
- Channels: `audioChannels > audioChannelCap(device, audioCodec)` clears `audioSupported` (no reason here). Cap = `audioChannelsByCodec[codec]` ?? `maxAudioChannels` ?? 2. `[ts:backend/src/modules/streaming/stream-builder.service.ts:1401-1404,202-208]`
- `VideoCodecNotSupported` if `!videoSupported`; then `ContainerNotSupported` if `!containerSupported`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:1406-1417]`
- `canDirectPlay = boundMatch && audioSupported && videoConditionsMet`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:1421]`

## 6. HDR / DV / burn-in / crop gates

Facts after tryDirectPlay `[ts:backend/src/modules/streaming/stream-builder.service.ts:472-517]`:
- `clientCanPresentHdr = isSourceHdr && !noBase && (supportsHdr || tonemapsHdrLocally) && videoSupported && videoConditionsMet`.
- `clientCanPresentDv = dv.singleLayer && dv.profile != null && dvProfiles has dv.profile && videoSupported && videoConditionsMet`.
- `clientCanPresentDynamicRange = clientCanPresentHdr || clientCanPresentDv`.
- `dvWithBase = clientCanPresentDv && dvSupplementalCodecs(v) != null`; `dvStandaloneCopy = clientCanPresentDv && dvStandaloneCodecs(v) != null`. `[ts:backend/src/modules/streaming/transcoding/codec/dolby-vision.ts:69-91]`
  - `dvSupplementalCodecs`: profile 8 or 10, `dvLevel` truthy, `colorRange == tv`, `colorSpace == bt2020nc`, `pixelFormat` in `{yuv420p10, yuv420p10le}`, and (`compat == 1 && hdrFormat == HDR10`) or (`compat == 4 && hdrFormat == HLG`).
  - `dvStandaloneCodecs`: `dvLevel` truthy and (profile 5, or profile 10 with compat 0/absent).
- `dvCopyAllowed = !noBase || clientCanPresentDv`; `dvRemuxCopyAllowed = !noBase || dvStandaloneCopy`.
- `dvRemuxEligible = (dvWithBase && copySourceCodecString(v) != null) || dvStandaloneCopy`. `[ts:backend/src/modules/streaming/transcoding/codec/codec-strings.ts:262-304]`: needs codec+profile+level>0; h264 profile in a fixed map (`baseline`, `constrained baseline`, `main`, `extended`, `high`, `high 10`, `high 4:2:2`, `high 4:4:4 predictive`), level <= 255; hevc level >= 30 and profile `main` / `main 10`; av1 level <= 31, profile `main/high/professional`, known bit depth.
- `dvDirectPlayLabel = (clientCanPresentDv && (noBase || compat in {1,4})) || (profile == 7 && dvProfiles has 7)`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:505-508]`
- `clientTonemap = (clientCanPresentHdr || clientCanPresentDv || useHdrLadder) && !clientSupportsHdr`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:512]`

Gates, in order (each also clears `canDirectPlay`):

| Order | Flag | Trigger |
|---|---|---|
| 1 | `VideoHdrNotSupported` | `(isSourceHdr && !clientCanPresentDynamicRange) || !dvCopyAllowed`, and `runsTonemapFilter`, only once per session. Message `HDR → SDR (tone mapping {dvLabel})`, or `{dvLabel} → HDR10 (tone mapping)` when `dvNoBaseHdr10`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:436-447,519-527]` |
| 2 | `SubtitleBurnIn` | `burnInSubtitleId` truthy. `[ts:backend/src/modules/streaming/stream-builder.service.ts:528-536]` |
| 3 | `VideoCrop` | `source.crop` set (needs `autoCropEnabled`) and `!cropsBlackBarsLocally`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:538-546]` |
| 4 | `VideoDolbyVisionP7NotSupported` | `dv.profile == 7 && 7 not in dvProfiles && copyableIgnoringGates`; always clears DirectPlay even when the reason is skipped. `[ts:backend/src/modules/streaming/stream-builder.service.ts:633-642]` |
| 5 | `ClientRequiresHls` | `supportsDirectPlay == false` and DirectPlay still possible. `[ts:backend/src/modules/streaming/stream-builder.service.ts:645-651]` |
| 6 | `ClientNeedsHlsSubtitles` | `needsHlsSubtitles == true` and DirectPlay still possible. `[ts:backend/src/modules/streaming/stream-builder.service.ts:653-661]` |
| 7 | `ClientCannotSwitchAudio` | DirectPlay still possible and `((pickedAudio != 0 && unreachable) || remuxForSwitching)`; `unreachable = switchesDirectPlayAudio == false || (dedupesAudioByLanguage && first track with the picked language != pickedAudio)`; `remuxForSwitching = switchesDirectPlayAudio == false && audio.len > 1 && canCopyVideo`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:663-678]` |
| 8 | (none) | `rejectCopy == true` clears DirectPlay silently. `[ts:backend/src/modules/streaming/stream-builder.service.ts:679]` |
| 9 | `VideoAbr` | `forceLadder && autoOnLadder`; clears DirectPlay. `[ts:backend/src/modules/streaming/stream-builder.service.ts:680-691]` |
| 10 | `VideoQualityReduced` | `forceLadder && !autoOnLadder` and the requested name matches a ladder rung exactly. `[ts:backend/src/modules/streaming/stream-builder.service.ts:699-707]` |

Gates 4 to 10 read `forceLadder`, `copyableIgnoringGates` and `canCopyVideo` (section 7), computed just before gate 4. Gate 1 to 3 come before them in the code.

## 7. Quality routing and copy eligibility

### 7.1 Ladders `[ts:backend/src/modules/streaming/transcoding/profiles.ts:95-278]`

| Ladder | Rungs (name: maxW x maxH, video, audio) |
|---|---|
| SDR full | `2160p` 3840x2160 20M 192k; `1080p` 1920x1080 8M 192k; `720p` 1280x720 4M 128k; `480p` 854x480 2M 96k; `360p` 640x360 1M 64k; `240p` 426x240 500k 64k; `144p` 256x144 200k 48k |
| SDR eco | `eco-2160p` 8M 192k; `eco-1080p` 3M 192k; `eco-720p` 1500k 128k |
| HDR full | `2160p-hdr` 28M 192k; `1080p-hdr` 5500k 192k; `720p-hdr` 2800k 128k; `480p-hdr` 1400k 96k |
| HDR eco | `eco-2160p-hdr` 12M 192k; `eco-1080p-hdr` 2200k 192k |

- Device type is ignored: ladder = full then eco. `qualityLadder = useHdrLadder ? HDR : SDR`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:555-565]` `[ts:backend/src/modules/streaming/transcoding/profiles.ts:264-278]`
- Rate strings: `<n>[kK|mM]` -> bps (`k` x1e3, `m` x1e6, rounded); unparsable = 0. `[ts:backend/src/modules/streaming/transcoding/profiles.ts:334-344]`
- `bucketResolutionHeight(w,h)`: both 0 -> 480; `w<=256&&h<=192` 144; `<=426,320` 240; `<=640,432` 360; `<=854,576` 480; `<=1280,962` 720; `<=1920,1440` 1080; else 2160. `profileFitsSource(rung, w, h)`: `bucket(rung) <= bucket(source)`. `[ts:backend/src/common/utils/resolution.util.ts:26-40]` `[ts:backend/src/modules/streaming/transcoding/profiles.ts:299-308]`
- `isEco(name)` = starts with `eco-`.

### 7.2 Requested quality `[ts:backend/src/modules/streaming/stream-builder.service.ts:555-596]`

- `wantsSourceRung` = `requestedQuality` absent, `auto` or `original`.
- `autoOnLadder` = (absent or `auto`) and `config.autoQualityMode == abr`.
- `explicitRung` (only when not `wantsSourceRung`) = `resolveLadderRung(name, qualityLadder)`: exact name; else tier `N` from `(\d+)p` (no digit: undefined); among rungs of the same class (eco vs full) sorted by `maxHeight` desc, the first with `maxHeight <= N`, else the last. `[ts:backend/src/modules/streaming/transcoding/profiles.ts:241-257]`
- `negotiatedQuality = explicitRung?.name ?? "auto"` (the output `quality` on Transcode).
- `negotiatedAudioRung = explicitRung ?? first non-eco rung of qualityLadder (else first)`; `negotiatedStereoBps` = its audio rate. `[ts:backend/src/modules/streaming/stream-builder.service.ts:150-152,589]`
- `explicitDownscale = explicitRung && (bucket(rung) < bucket(source w,h) || isEco(rung))`.
- `forceLadder = autoOnLadder || explicitDownscale`.

### 7.3 Copy eligibility

- `sourceCopyable = videoSupported && videoConditionsMet && dvCopyAllowed && (!isSourceHdr || clientCanPresentDynamicRange) && !needsBurnIn && (!needsCrop || cropsBlackBarsLocally)`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:602-608]`
- `copyableIgnoringGates = sourceCopyable && !forceLadder`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:613]`
- `canCopyVideo = copyableIgnoringGates && dvRemuxCopyAllowed && no active copy gate`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:627-628]`

### 7.4 Copy gates (pushed only when `copyableIgnoringGates`, after DirectPlay was ruled out, in this order) `[ts:backend/src/modules/streaming/stream-builder.service.ts:609-626,722-728]`

| Flag | Active when |
|---|---|
| `MuxNotSupported` | `muxFlavour == ts` unless there is at most one audio track. Message `MPEG-TS packaging can't carry a copied video stream`. |
| `ClientRejectedCopy` | `rejectCopy == true`. |

## 8. Audio plan `[ts:backend/src/modules/streaming/stream-builder.service.ts:1158-1287]`

Runs once over all tracks (a track is decided as part of its group), before the DirectStream/Transcode split `[ts:backend/src/modules/streaming/stream-builder.service.ts:773-820]`.

- `profileAudioCodecs` = union of `directPlayProfiles[].audioCodecs`, lowercased.
- `FMP4_COMPATIBLE_AUDIO = {aac, ac3, eac3, opus, flac}` (mp3, dts, truehd... are not). `[ts:backend/src/modules/streaming/transcoding/audio-encode.ts:165-171]`
- Encodable: `aac, ac3, eac3, opus`; encoder channel max: aac 8, opus 8, ac3 6, eac3 6. `[ts:backend/src/modules/streaming/transcoding/audio-encode.ts:40-46,112-114,125-127]`
- `encodeChannelCap(device, codec) = min(audioChannelCap(device, codec), encoderMax(codec))`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:144-146]`

### 8.1 Copy blockers per track (only when `muxFlavour == fmp4`) `[ts:backend/src/modules/streaming/stream-builder.service.ts:90-126]`

| Blocker | Trigger |
|---|---|
| `AudioFormatMayChange` | codec `aac` and source is MPEG-TS and `aacConfigMayChange(scan, streamIndex) != false` (no scan -> undefined -> blocks; scan -> `scan.audioConfigChanges[streamIndex] ?? false`). `[ts:backend/src/modules/streaming/transcoding/source-scan.ts:28-30]` |
| `AudioStartOffset` | track `startTimeSeconds` and `videoPresentationStart` both known and `abs(diff) > 0.05`. `videoPresentationStart = firstFrameSeconds ?? startTimeSeconds`. `[ts:backend/src/modules/streaming/transcoding/source-timeline.ts:33-38]` |
| `AudioEndsEarly` | track `endSeconds` known, `lastSegmentStart` defined, `endSeconds <= lastSegmentStart`. |

`lastSegmentStart` is computed only for multi-audio sources (`audio.len > 1`, var-stream-map layout), else undefined. `[ts:backend/src/modules/streaming/stream-builder.service.ts:128-142,773-790]`
- Timeline `{origin, end}` from `sourceTimeline` (end = `timestampBreakSeconds ?? video.endSeconds ?? formatStart + durationSeconds`); `end` undefined -> undefined. `[ts:backend/src/modules/streaming/transcoding/source-timeline.ts:41-69]`
- `lastVideoSegmentStart(grid)` with `grid = canCopyVideo ? (remuxGrid ?? null) : undefined`; undefined when the timeline `end` is unknown. `[ts:backend/src/modules/streaming/stream-builder.service.ts:128-142]`
  - a grid object: `boundaries[len - 2]`.
  - `null` (video copied, no scan): `seg = segmentDuration`.
  - `undefined` (video re-encoded): `seg = realSegmentSeconds(segmentDuration, fps)`.
  - for `null`/`undefined`: `origin + (uniformSegmentCount(end - origin, seg, frameSeconds) - 1) * seg`. `[ts:backend/src/modules/streaming/transcoding/constants.ts:66-110]`
  - `fps = parseSourceFps(frameRate)`: `"a/b"` (both > 0) -> a/b, else `parseFloat`, 0/NaN -> undefined. `frameSeconds = 1/(fps or 24)`.
  - `realSegmentSeconds(d, fps) = round_ms(max(1, round(d*fps)) / fps)`; `d` if fps unknown.
  - `uniformSegmentCount(dur, seg, frame) = max(1, floor((dur - frame)/seg + 1e-9) + 1)`.

### 8.2 Group codec (one output codec for all renditions) `[ts:backend/src/modules/streaming/stream-builder.service.ts:1251-1287]`

1. All tracks share one source codec `c`, `c` in `profileAudioCodecs` and fMP4-safe: if `(every track channels <= cap(c) && no blocker) || isEncodable(c)` the group codec is `c`.
2. Else if any track has `channels >= 6`: `surround = [eac3, ac3]` filtered to those in `profileAudioCodecs` with `encodeChannelCap >= 6`; pick the first one some track already is (`codec == c`, no blocker, channels within cap), else `surround[0]`. If none -> `aac`.
3. Else `aac`.

### 8.3 Track decision `[ts:backend/src/modules/streaming/stream-builder.service.ts:1180-1240]`

- `copyable = codec == groupCodec && codecSupported && fmp4Safe && !(channels > cap(codec))`; copy when also no blocker: plan `{mode: copy, codec, channels, bitrateBps: bitRate, profile}`, no flags.
- Else transcode to `groupCodec` (must be encodable, else an internal error) with `outputChannels = min(channels ?? 2, encodeChannelCap(groupCodec))`.
  - `AudioChannelsNotSupported` if `channels != null && outputChannels < channels`.
  - `AudioCodecNotSupported` if `!(codecSupported && fmp4Safe) && !(groupCodec != aac && outputChannels >= 6)`.
  - then the blocker flag, if any. Plan `{mode: transcode, codec: groupCodec, channels: outputChannels}`.
- Picked track: `audioPlan = decisions[pickedAudio].plan` (no audio: `{mode: copy, codec: ""}`; no decision: AAC stereo). `canCopyAudio = audioPlan.mode == copy`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:809-814]`
- The picked track's flags are appended to `transcodeReasons` (after the video reasons) with messages: `AudioChannelsNotSupported` `"{channels} channels > max {outputChannels}"`, `AudioCodecNotSupported` `Audio codec "{codec}" is not playable here`, `AudioStartOffset`, `AudioFormatMayChange`, `AudioEndsEarly` (fixed texts at `[ts:backend/src/modules/streaming/stream-builder.service.ts:185-199]`). `[ts:backend/src/modules/streaming/stream-builder.service.ts:816-819]`

### 8.4 Audio bitrates `[ts:backend/src/modules/streaming/transcoding/audio-encode.ts:129-159]`

- `audioEncodeBitrateBps(codec, channels, stereoBps)`: `budget = round(stereoBps * max(channels,2) / 2)`; aac/opus -> `budget`; ac3/eac3 -> `min(640000, max(floor, budget))` where floor = `192000 + 48000*(max(ch,2)-2)` for ac3, `round(2/3 * that)` for eac3; ac3 then rounds UP to the next AC-3 rate `{32,40,48,56,64,80,96,112,128,160,192,224,256,320,384,448,512,576,640} kbps`.
- `audioOutputBitrateBps(plan, stereoBps)`: copy with `bitrateBps` truthy -> that; else `audioEncodeBitrateBps(encodable(codec) ? codec : aac, plan.channels ?? 2, stereoBps)`.
- `audioTracks[i]`: `index` (array position), `language`, `codec` (lowercase), `channels`, `copy`, `outputCodec`, `outputChannels`, `reasonFlags`, `bitrateBps = audioOutputBitrateBps(plan, ref)`, `sampleRate` = source rate when copied, else 48000. `[ts:backend/src/modules/streaming/stream-builder.service.ts:164-183]`
- `ref` (stereo reference): DirectPlay and Transcode `negotiatedStereoBps`; DirectStream the remux budget = audio rate of `DESKTOP_PROFILES[0]` = 192k. `[ts:backend/src/modules/streaming/stream-builder.service.ts:798-800]` `[ts:backend/src/modules/streaming/transcoding/ffmpeg-args.ts:1473]`
- `rungAudioBps(p) = hasAudio ? audioOutputBitrateBps(audioPlan, rate(p.audioBitrate)) : 0`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:812-813]`

## 9. Outputs by play method

Common: `mediaFileId`, `playMethod`, `playUrl`, `contentType`, `transcodeReasons`, `videoCopyStream`, `audioCopyStream`, `outputVideoCodec`, `outputAudioCodec`, `audioPlan`, `outputContainer`, `quality`, `hwAccel`, `tonemapping`, `clientTonemap`, `dolbyVision`, `qualities`, `audioTracks`, `source`. `useHdrLadder`, `videoVariant` (= selectedVariant), `muxFlavour`, `audioPlans`, `transcodeTonemapping` wrap every result. `[ts:backend/src/modules/streaming/stream-builder.service.ts:414-423]`

### 9.1 DirectPlay (`canDirectPlay` still true after section 6) `[ts:backend/src/modules/streaming/stream-builder.service.ts:709-765]`

| Field | Value |
|---|---|
| `playUrl` | `/api/stream/{id}{tokenParam}` |
| `contentType` | `input.media.contentType` |
| `transcodeReasons` | `[]` (even if tryDirectPlay recorded some) |
| `videoCopyStream`, `audioCopyStream` | true, true |
| `outputVideoCodec`, `outputAudioCodec` | source codecs (lowercase) |
| `audioPlan`, `audioPlans`, `audioTracks` | all tracks `copy` with `reasonFlags: []`; `audioPlan` = picked copy plan (fallback `{copy, sourceAudioCodec}`) |
| `outputContainer` | source container (not `hls`) |
| `quality`, `hwAccel`, `tonemapping` | `original`, `none`, false |
| `clientTonemap`, `dolbyVision` | `clientTonemap`; `dvDirectPlayLabel` |
| `qualities` | `buildQualityList(source, DirectPlay, sourceCopyable, qualityLadder, selectedVariant.codec, hasAudio)` |

No `transcodeBitrateByQuality`, no `remuxMasterBandwidthBps`.

### 9.2 DirectStream (`canCopyVideo`) `[ts:backend/src/modules/streaming/stream-builder.service.ts:822-887]`

- `ContainerNotSupported` is added if tryDirectPlay did not already record it and the container is unsupported (after the audio reasons).
- `playUrl = /api/stream/{id}/master.m3u8{tokenParam}{(tokenParam ? "&" : "?")}remux=1`; `contentType = application/vnd.apple.mpegurl`; `videoCopyStream = true`; `audioCopyStream = canCopyAudio`; `outputVideoCodec = sourceVideoCodec`; `outputAudioCodec = audioPlan.codec`; `outputContainer = hls`; `quality = original`; `hwAccel = hostCaps.hwAccel` (detected, not effective); `tonemapping = false`; `clientTonemap`; `dolbyVision = dvRemuxEligible`.
- `remuxMasterBandwidthBps` = `remuxBandwidthBps(videoBitRate, formatBitRate, audioPlans)`, omitted when `<= 0`: `videoBitRate` set -> `round(videoBitRate) + max over plans of audioOutputBitrateBps(plan, 192000)`; else `round(formatBitRate || 0)` or 8_000_000 when 0. `[ts:backend/src/modules/streaming/transcoding/master-playlist.ts:45-77]`
- `transcodeBitrateByQuality` per 9.4; `qualities = buildQualityList(source, DirectStream, sourceCopyable, ...)`.

### 9.3 Transcode `[ts:backend/src/modules/streaming/stream-builder.service.ts:889-955]`

- Before building: `VideoHdrNotSupported` pushed if `runsTonemapFilter` and not yet recorded (so it follows the audio reasons when no earlier gate pushed it). `[ts:backend/src/modules/streaming/stream-builder.service.ts:891]`
- `playUrl = /api/stream/{id}/master.m3u8{tokenParam}`; `videoCopyStream = false`; `audioCopyStream = canCopyAudio`; `outputVideoCodec = selectedVariant.codec`; `outputAudioCodec = audioPlan.codec`; `outputContainer = hls`; `quality = negotiatedQuality`; `tonemapping = runsTonemapFilter`; `clientTonemap`; `dolbyVision = false`.
- `hwAccel = resolveEncodePipeline(selectedVariant, ctx).effectiveHwAccel` (section 10).
- `qualities = buildQualityList(source, Transcode, canCopyVideo, ...)`: since `canCopyVideo` is false here, the top rung is listed as a normal rung, never `original`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:944-951]`
- `transcodeBitrateByQuality` per 9.4.

### 9.4 Bitrate hints and `qualities`

- Rung video bitrate `cappedRungVideoBitrateBps(rung, ctx)`, ctx = `{outputCodec: selectedVariant.codec, sourceWidth, sourceHeight (0 if absent), sourceFrameRate: fps ?? 24, sourceVideoBitrateBps: videoBitRate, sourceVideoCodec}`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:982-994]` `[ts:backend/src/modules/streaming/transcoding/quality-ladder.ts:30-58]`
  1. `capped = min(rungBps, round(videoBitRate * max(1, f[target]/f[source])))` when `videoBitRate > 0`, else `rungBps`. Factors: h264/avc/avc1 1; hevc/h265/hvc1/hev1 0.6; vp9 0.7; av1/av01 0.5; unknown 1. `[ts:backend/src/modules/streaming/transcoding/profiles.ts:7-18,80-93]`
  2. If output codec is `hevc`: `min(capped, mainTierCap)`, `mainTierCap` from the luma rate `profileResolution(rung, srcW, srcH).w*h*fps`: `<= 33_177_600` 10M, `<= 66_846_720` 12M, `<= 133_693_440` 20M, `<= 267_386_880` 25M, `<= 534_773_760` 40M, else 60M. `profileResolution`: `w = min(rung.maxWidth, srcW)`, `h = max(2, ceil(w*srcH/srcW / 2) * 2)`. `[ts:backend/src/modules/streaming/transcoding/codec/codec-strings.ts:98-132]` `[ts:backend/src/modules/streaming/transcoding/profiles.ts:319-328]`
- `transcodeBitrateByQuality[rung.name] = {videoBitrateBps, audioBitrateBps: rungAudioBps(rung), totalBitrateBps: sum}` for every rung of `qualityLadder` (not filtered by source size). Not emitted on DirectPlay. `[ts:backend/src/modules/streaming/stream-builder.service.ts:959-977]`
- `buildQualityList` `[ts:backend/src/modules/streaming/stream-builder.service.ts:1014-1139]`:
  - `sourceTotal = formatBitRate ?? (videoBitRate ?? 0) + (audioBitRate ?? 0)`. `available = ladder.filter(profileFitsSource)`; if empty, `[last rung of ladder]`.
  - `topProfile` = first non-eco of `available` (else first). `displayLabel`: strip `eco-` and `-hdr`; `2160p` -> `4K`.
  - `totalOf(p) = cappedRungVideoBitrateBps(p, ctx) + (hasAudio ? rate(p.audioBitrate) : 0)`.
  - Non-eco rungs in order. The top rung, when `videoCopyStream` argument is true, becomes `{id: "original", label, height: source h or rung maxHeight, width: source w or rung maxWidth, totalBitrateBps: sourceTotal > 0 ? sourceTotal : totalOf, isRemux: playMethod == DirectStream}`; every other rung `{id: name, label, height: maxHeight, width: maxWidth, totalBitrateBps: totalOf, isRemux: false}`.
  - Eco rungs of `available`, each added as `{..., lowBandwidth: true}` only when it reduces video: `sourceVideoBps = resolveSourceVideoBitrateBps(videoBitRate, formatBitRate, audioBitRate ?? 0)`; `cappedRung < sourceVideoBps` if that is truthy, else `totalOf(p) < (sourceTotal > 0 ? sourceTotal : totalOf(top))`.
  - Sort by tier (the rung's `maxHeight`) desc, then non-eco before eco, then `totalBitrateBps` desc. Output order = sorted order.

## 10. Effective hwAccel on Transcode `[ts:backend/src/modules/streaming/transcoding/encode-pipeline.ts:147-259,263-293]`

Context: `hwAccel = hostCaps.hwAccel`, `crop = needsCrop` (the raw `source.crop` presence, even when the client crops locally), `burnIn = burnInIsText` (image burn-in does not count), `tonemap = runsTonemapFilter`, `tonemapAlgo = config.tonemapAlgo`, `dvNoBase`, `dvApplyRpu`, `sourceBitDepth = (isSourceHdr || noBase) ? 10 : 8`.

1. `tonemapPath = resolveTonemapPath(algo, {hasCrop: crop, dvNoBase, dvApplyRpu: tonemap && dvApplyRpu})` (section 3).
2. `qsvNativeDecoder` = native QSV decoder for the normalised source codec (platform-specific table, `[assumed]` from decoder registry); `hasUsableQsvNativeDecoder = hwAccel == qsv && !burnIn && decoder exists && decoder not in disabledDecoders`.
3. `vulkanUsable = path == vulkan && tonemap && !burnIn && hwAccel == vaapi`; `dvNoBaseNeedsCpu = dvNoBase && tonemap && path != opencl && !vulkanUsable`.
4. `qsvNativeAvailable = !dvNoBaseNeedsCpu && hasUsableQsvNativeDecoder && (platform != linux || crop || path in {qsv, opencl}) && (!tonemap || (path == qsv && vppQsv) || (path == opencl && bridgeOk(crop)))`; `qsvCanCrop = qsvNativeAvailable || (hwAccel == qsv && crop && tonemap && !burnIn)`.
5. `requested = hwAccel`, except `qsv` becomes `vaapi` when `crop && !qsvCanCrop && platform == linux`. `[ts:backend/src/modules/streaming/transcoding/hw-detect.ts:148-161]`
6. `requested = none` when: (platform != linux && hwAccel == qsv && !qsvNativeAvailable) || (dvNoBaseNeedsCpu && hwAccel in {qsv, vaapi}) || (tonemap && path == vulkan && burnIn).
7. `effectiveHwAccel = encoderRegistry.resolve(selectedVariant, requested)?.hwAccel ?? none` (registry falls back to the CPU descriptor of the same variant).
- AMF-only inputs (`amfOpenclAvailable`) do not change `effectiveHwAccel`. `[assumed]`

## 11. HostCaps (as in fixtures)

`{ hwAccel: vaapi|nvenc|qsv|amf|videotoolbox|none, platform: linux|darwin|win32, libfdkAac, vaapiWritesHdrMetadata, probes: [names that passed], disabledEncoders: [ids], disabledDecoders: [ids] }`. Unlisted probes read false (fail-closed). The engine's detected snapshot supplies it (`hw.md`).

| Probe name | TS function |
|---|---|
| `tonemapOpencl`, `tonemapOpenclCrop`, `tonemapOpenclHdr10` | `isTonemapOpenclEnabled`, `...WithCrop`, `isTonemapOpenclHdr10Enabled` `[ts:backend/src/modules/streaming/transcoding/codec/tonemap-opencl-probe.ts:23-39]` |
| `qsvOpencl`, `qsvOpenclHdr10` | `isQsvOpenclTonemapEnabled`, `...Hdr10Enabled` (win32 bridge) |
| `vppQsv`, `vulkan` | `isVppQsvTonemapEnabled`, `isVulkanTonemapEnabled` |
| `cuda`, `cudaHdr10`, `opencl`, `openclHdr10`, `amfOpencl` | `isCuda*`, `isOpencl*`, `isAmfOpenclEnabled` |
| `deinterlaceVaapi`, `deinterlaceQsv`, `bwdifCuda`, `bwdifVideotoolbox` | `[new]` hardware deinterlacers (`hw.md` section 7); `deinterlaceQsv` also gates `qsvNativeAvailable` for an interlaced source |

- Encoder descriptors `[ts:backend/src/modules/streaming/transcoding/codec/encoders/index.ts:30-72]`; ids are `{codec}_{backend}[_hdr10|_hlg]` `[assumed]` (read `id:` in each descriptor file). Variants per backend:
  - `h264` (8, SDR): qsv, vaapi, nvenc, amf, videotoolbox, cpu.
  - `hevc` (8 SDR; 10 HDR10; 10 HLG): qsv, vaapi, nvenc, amf, videotoolbox, cpu. HLG shares its HDR10 sibling's `supportsHdrMetadata`.
  - `av1` (8 SDR; 10 HDR10): qsv, vaapi, nvenc, amf, cpu. (10 HLG): nvenc, amf, cpu only. No videotoolbox AV1.
  - `supportsHdrMetadata`: false on every SDR descriptor; true on HDR descriptors of cpu, nvenc, qsv, videotoolbox, amf; vaapi HDR (hevc, av1) = `vaapiWritesHdrMetadata`.
- `libfdkAac` is not read by `decide` (it only selects the AAC encoder binary and packet grid in the argv/timeline code). `disabledDecoders` only matters in section 10 step 2.

## 12. Reason order and message summary

Order in `transcodeReasons` (every flag at most once, except where noted): tryDirectPlay flags (5) -> `VideoHdrNotSupported` -> `SubtitleBurnIn` -> `VideoCrop` -> `VideoDolbyVisionP7NotSupported` -> `ClientRequiresHls` -> `ClientNeedsHlsSubtitles` -> `ClientCannotSwitchAudio` -> `VideoAbr` -> `VideoQualityReduced` -> (non-DirectPlay) `MuxNotSupported` -> `ClientRejectedCopy` -> picked-track audio flags -> (DirectStream) `ContainerNotSupported` if absent / (Transcode) `VideoHdrNotSupported` if absent. `[ts:backend/src/modules/streaming/stream-builder.service.ts:436-447,519-707,722-728,816-819,832,891]`
- Messages are free text for display; fixtures pin them, so port them verbatim from the cited lines (all in `stream-builder.service.ts`). Substitutions: `Level {l} > max {m}`, `Video profile "{p}" not supported`, `{d} bit > max {m} bit`, `Resolution {w}x{h} > max {mw ?? ?}x{mh ?? ?}`, `Codec "{c}" not supported`, `Container "{c}" not supported`, `Reduced-quality rung selected ({requestedQuality})`.

## 13. Known TS quirks the port must reproduce

- `Direct Play` never reports reasons, even when tryDirectPlay found some (`transcodeReasons: []`). `[ts:backend/src/modules/streaming/stream-builder.service.ts:725-731]`
- A copy gate blames nothing when another cause already made the source uncopyable (`copyableIgnoringGates`). `[ts:backend/src/modules/streaming/stream-builder.service.ts:613-625]`
- `needsCrop` uses the config-gated `source.crop`; crop in the response `source` is hidden when auto-crop is off. `[ts:backend/src/modules/streaming/stream-builder.service.ts:313,428]`
- `qualities` for DirectStream/DirectPlay use `sourceCopyable` (ignoring mux/reject gates and the quality choice); for Transcode `canCopyVideo`. `[ts:backend/src/modules/streaming/stream-builder.service.ts:728-737,944-951]`
- Empty `codec` strings stay `""` (no unknown-codec error): the source then fails every list match and transcodes. `[ts:backend/src/modules/streaming/stream-builder.service.ts:307]`
- Numbers: every bitrate is an integer through `Math.round` (half up, positives only); keep f64 math and round half up. `[assumed]`

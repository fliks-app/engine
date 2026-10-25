# Spec: args

`core::args`: the ffmpeg argv of every engine run, pure. Golden vectors: `fixtures/argv/*.json` (`kind`, `hostCaps`, `env`, `input`, `expected`), `expected` = `{argv}` or `{error}` from the TS builders. Tags are relative to `fliks/backend/src` (`ff` = `modules/streaming/transcoding/ffmpeg-args.ts`, `tc` = `modules/streaming/transcoding`).

## 0. Contract and layers

| Kind | TS entry | Rust |
|---|---|---|
| `vod` | `buildFfmpegArgs(opts)` `[ts:ff:945-1409]` | `args::vod` |
| `remux` | `buildRemuxArgs(opts)` `[ts:ff:1568-1733]` | `args::remux` |
| `encoder` | `EncoderDescriptor.buildArgs(input)` `[ts:tc/codec/encoders/index.ts:39-77]` | `args::encoder_args` |
| `live` | `buildLiveFfmpegArgs(opts)` `[ts:modules/livetv/services/live-ffmpeg-args.ts:99-208]` | `args::live` |
| `iframe` | `buildIFrameSegmentArgs(opts)` `[ts:tc/iframe-trick-play.ts:44-89]` | `args::iframe` |
| file | none | `args::file` `[new]` |

- Inputs are the TS option bags, field for field (camelCase JSON), typed with the wire types (`CodecVariant`, `AudioPlan`, `HostCaps`, `TonemapAlgo`, `TonemapCurve`, `CropInfo`, `HdrMetadata`). Global state the TS reads becomes explicit: probes, platform, libfdk, disabled ids from `HostCaps`; render node and `FLIKS_OPENCL_DEVICE` from `hw::Device` (`env.renderNode`, `env.openclDevice`). `[ts:tc/hw-device.ts:13-110]`
- Paths (`inputPath`, `outputDir`, `inputUrl`, the burn-in filter) are opaque strings the caller gives. Nothing is derived from a local directory or host name. `[new]` (M9: engine and host may not share a file system.)
- Layer 1 (`args::encode`): input open, seek, HW decode, filter graph (crop, scale, tone-map, burn-in), video encoder, audio stream args. It returns the argv up to and including the video encode plus the video map spec; the audio args of one stream are `encode::audio_stream_args`.
- Layer 2 (`args::output`): what the run writes. HLS VOD (single muxed, `var_stream_map`), remux GOP files, live sliding window, iframe pipe, plain file (MP4/MKV). VOD, live and file compose layer 1; live uses the layer-1 device init and audio helpers.
- `VodArgs.hwAccel` is the run's backend and replaces `HostCaps.hwAccel` for the pipeline (the TS passes the detected one as the option; tests pass others). `[ts:ff:1132-1142]`
- Errors are `Err(String)` with the TS message.

## 1. Numbers

- `formatSeconds(x)` = `String(Number(x.toFixed(6)))`. `[ts:ff:110-112]` JS `toFixed` (exact expansion, ties away from zero) and JS `Number#toString` (shortest round trip; exponent form below 1e-6 and from 1e21).
- `String(n)` of every number in an argv uses JS `Number#toString`. `${Math.round(bps / 1000)}k` for audio. `[ts:tc/audio-encode.ts:206]`
- `parseBitrateToBps`: `^(\d+(\.\d+)?)\s*([kKmM])?$` after trim, `M` x1e6, `k` x1e3, `Math.round`; no match is 0. `[ts:tc/profiles.ts:334-344]`

## 2. Segment grid and timeline `[ts:ff:292-336]`

- `fps = sourceFps > 0 ? sourceFps : 24`; `gopSize = max(1, round(segmentDuration * fps))`; `realSeg = realSegmentSeconds(segmentDuration, sourceFps)` (`decide::timeline`); `seekSeconds = startSegment > 0 ? startSegment * realSeg : 0`; `forceKeyframesExpr = expr:gte(n,n_forced*{gop})`. `[ts:tc/constants.ts:66-73,124-131]`
- `alignStartSeconds = seekSeconds + sourceStartPts` (default 0); `sourceFormatStart` defaults to `sourceStartPts`. `[ts:ff:1001-1009]`
- Output seek `runOutputSeek = startSegment > 0 || alignStart < 0 ? alignStart : 0`. `[ts:ff:182-184]`
- Read end `-to`: `min(clockBreak - formatStart, early ? 2 * segmentDuration + 1 : inf)` when finite. `[ts:ff:168-178,1225-1232]`

## 3. VOD argv order `[ts:ff:1030-1408]`

1. `-hide_banner -loglevel warning`, then `-analyzeduration 0 -probesize 5000000` when `trustedStreamInfo`, else `-analyzeduration 1000000 -probesize 1000000`.
2. `startSegment > 0`: `-ss formatSeconds(seekKeyframeDts != null ? seekKeyframeDts - formatStart : seekSeconds + origin - formatStart)`. `[ts:tc/source-timeline.ts:71-76]`
3. Output size, bitrate (section 4), pipeline (section 5); `videoVariant` absent is a type error here (the TS throws). No encoder: `No encoder for variant {JSON} on {requested}` (JSON keys sorted as the vectors store them).
4. Decode stage (section 6), then the input colour tags (section 8) when `!isHdrOutput && !vtMetal && !tonemap`, then `-to`, `-threads N`, `-i inputPath`, `-copyts -muxdelay 0 -muxpreload 0`, `-ss formatSeconds(alignStart)` when the output seek is not 0. `N` is `encodeThreads` (absent: 4). The engine sets it to the CPU quota (`available_parallelism`, which follows a cgroup). libx264 and libx265 also take `-threads:v N`. `[new]`
5. Encoder args (section 7). SDR tag step: `setparams=color_primaries=P:color_trc=T:colorspace=S:range=R` appended to the first `-vf` when `!isHdrOutput && !vtMetal && !vulkan && effective != nvenc` and no image burn-in. `[ts:ff:1313-1325]`
6. Image burn-in (`burnIn.type == image && streamIndex != null`): the first `-vf` pair is removed and its value goes into `-filter_complex` (section 9), with the tag step spliced before `[vout]`; video map `[vout]`. `[ts:ff:1327-1363]`
7. Output colour tags when `!isHdrOutput && !vtMetal && !vulkan && !(useVaapiTonemap && effective == vaapi)`. `[ts:ff:1379-1386]`
8. Audio and muxer (section 10).

## 4. Size and bitrate `[ts:ff:1067-1100]`

- Framed size = crop size, else source size; when both > 0: `width = min(maxWidth, fw)`, `height = max(2, ceil(width * fh / fw / 2) * 2)`, else the profile max. `[ts:ff:355-366]` `[ts:tc/profiles.ts:319-328]`
- `bitrate = cappedTranscodeVideoBitrateBps(parse(videoBitrate), sourceVideoBitrateBps, sourceVideoCodec, variant.codec)`: `min(rung, round(src * max(1, factor(tgt) / factor(src))))` for a positive source rate (factors: h264 1, hevc 0.6, vp9 0.7, av1 0.5, aliases included, else 1). `[ts:tc/profiles.ts:80-93]`
- HEVC output: `min(bitrate, mainTierCap(w * h * fps))` (10/12/20/25/40/60 Mbps). `[ts:tc/codec/codec-strings.ts:98-131]`
- Tuning: `preset = early ? veryfast : encoderPreset (faster)`, `nvencPreset = p4`, `qsv.rcInitOccupancy = max(1, early ? round(b / 2) : b)`, `qsv.bufsize = b`, `libx264BufsizeMb = max(1, ceil(b * (early ? 1 : 2) / 1e6)) + "M"`, `qsv.extra = -forced_idr 1 -adaptive_i 0 -bf 0 -b_strategy 0`. `[ts:ff:889-943,1020-1029]`

## 5. Encode pipeline `[ts:tc/encode-pipeline.ts:147-259]`

- `dvNoBase`, `dvApplyRpu` as in decide; `dvNoBaseHdr10 = dvNoBase && variant.hdr == HDR10`. `[ts:ff:1117-1121]`
- `tonemapPath = resolveTonemapPath(algo, {crop, dvNoBase, dvApplyRpu: tonemap && dvApplyRpu})` (`decide::tonemap`).
- `qsvNativeDecoder`: `{codec}_qsv_d3d11_decode` on win32 else `{codec}_qsv_decode`, for a normalised source codec; usable when hw = qsv, no text burn-in, not disabled. `amfOpenclAvailable = hw == amf && probe amfOpencl && amf native decoder exists && sourceBitDepth <= its max (h264 8, else 10) && not disabled`.
- `vulkanUsable`, `dvNoBaseNeedsCpu`, `qsvNativeAvailable`, `qsvCanCrop`, the requested hwAccel (QSV crop splice to VAAPI, CPU drops) and `effective = resolved encoder's hw` exactly as `decide::tonemap::effective_hw_accel`; the args port computes the same struct and decide reads `effective` from it. `qsvNative = qsvNativeAvailable && effective == qsv`.
- `useVaapiTonemap = !dvNoBaseNeedsCpu && tonemap && effective != amf && (path == vaapi || (tonemap && path == qsv && !qsvNativeAvailable && hasVaapi))`.
- Tone-map report `[ts:tc/encode-pipeline.ts:306-358]`: `useVulkan = path == vulkan && effective == vaapi`; `cuda = tonemap && effective == nvenc && probe cuda`; `opencl = tonemap && !cuda && probe opencl && (effective in {nvenc, amf} || dvNoBase || (dvApplyRpu && effective == none))`.
- Burn-in flags differ by call: the pipeline gets `burnIn = !!burnIn.filter` (text), the decode stage and report get `!!burnIn` (any). `[ts:ff:1132-1168,1203]`

## 6. Decode stage `[ts:ff:660-882]`

- `pickDecoder`: `qsvNative && codec` -> QSV native decoder of the platform, else registry; `amfOpenclAvailable && effective == amf && codec` -> D3D11 native decoder; else registry for `codec ?? h264` on `effective`, but `none` when a reported codec does not normalise.
- Registry `[ts:tc/codec/decoders/index.ts:40-103]`: pass 1 same hwAccel, pass 2 any other HW (only if preferred != none), else CPU; usable = platform supports, codec matches (h264, hevc, av1, vp9, mpeg2, vc1), `bitDepth <= max` (8 for h264, mpeg2 and vc1), not disabled. Descriptor hwAccel/surface: qsv (vaapi surface, not win32), vaapi, cuda (nvenc), d3d11va (amf, cpu surface, win32), videotoolbox (cpu surface, darwin), cpu. Input args are `hw::decoders` (`hw::Device`).
- Then: CPU decoder + qsv encoder adds the QSV device init and `-filter_hw_device qs`; vulkan path on a VAAPI decoder + vaapi encoder replaces everything up to the decoder's `-filter_hw_device va` with the Vulkan chain; VT surface paths (`isVtTonemapPath`: tonemap, VT, no burn-in, a VT 10-bit decoder for the codec; metal = no crop, hwTonemap = crop; HDR passthrough = HDR out, VT, no burn-in, no crop) add `-hwaccel_output_format videotoolbox_vld`; OpenCL `-init_hw_device opencl=ocl:0.0` for a VAAPI-surface tonemap that is not vaapi/qsv-native/opencl-report/vulkan/nvenc; `openclTonemapInitArgs` when the report says opencl and not AMF native decode; QSV D3D11 + opencl path repoints the last `-filter_hw_device` to `ocl` after inserting `-init_hw_device opencl=ocl@dx`. `[ts:tc/encode-pipeline.ts:57-98]`
- `inputSurface` for the encoder = `videotoolbox` on any VT surface path, else the decoder surface.

## 7. Filters and encoders

- `buildVideoFilters` `[ts:tc/ffmpeg-filter-graph.ts:88-176]`: `cropStr`, `cpuCropPrefix`, `hwCropPrefix` (p010le for 10-bit source), `burnInFilter` (`,{filter}`), and one tone-map string per path (opencl, vaapi, vulkan with libplacebo crop, cuda, cpu: opencl bounce, `tonemapx` for no-base DV, else the zscale chain plus side-data deletes). `tonemapOpenclOpts` and `apply_dovi` `[ts:tc/ffmpeg-filter-graph.ts:17-32]`.
- Encoder descriptors in registry order with their `buildArgs` `[ts:tc/codec/encoders/*.ts]`: the argv tables of each file, `hlgFromHdr10` = HDR10 args with the first `-color_trc` value set to `arib-std-b67` `[ts:tc/codec/encoders/helpers/hdr-variants.ts:27-42]`. Helpers: `qsvScaleFilter8bit/10bit` `[ts:tc/codec/encoders/helpers/qsv-filters.ts:30-145]`, `vaapiScaleFilter` `[ts:tc/codec/encoders/helpers/vaapi-filters.ts:5-26]`, `nvencScaleFilter` + `nvencVfEndsOnGpu` `[ts:tc/codec/encoders/helpers/nvenc-filters.ts:7-55]`, `amfOpenclFilter`/`amfScaleFilter` + `amfVfEndsOnGpu` `[ts:tc/codec/encoders/helpers/amf-filters.ts:11-91]`, `vtTonemapFilter` `[ts:tc/codec/encoders/helpers/vt-filters.ts:8-22]`, `scaleEvenHeight` `[ts:tc/codec/encoders/helpers/scale-filter.ts:13-15]`, HDR metadata strings (generic 1000-nit fallback, SVT decimal form with `toFixed(5)`) `[ts:tc/codec/encoders/helpers/hdr-metadata.ts:19-56]`, SVT preset map and `-bufsize round(2 * max(1, b))` `[ts:tc/codec/encoders/av1-cpu.ts:12-31]`.
- Errors kept: `qsvScaleFilter10bit: tonemap requested with no opencl chain on a qsv-native surface` / `... on a vaapi-derived surface`, `vaapiScaleFilter10bit: tonemap requested with no opencl chain`, `hevc_videotoolbox_main10: tonemap requested but not supported` (also from the HLG variant).
- Registry `[ts:tc/codec/encoders/index.ts:76-111]`: same order and usability as `decide::codec`, plus the variant's `bitDepth` must match the descriptor (8 SDR, 10 HDR).

## 8. Colour tags `[ts:ff:264-290,369-386]`

`tag(v)` = v unless empty, `unknown` or `reserved`. Tone-map: all `bt709`, range `tv`; else each axis `tag(source) ?? bt709`, range `pc` for `pc|full|jpeg` (lowercased) else `tv`. Flags `-color_primaries P -color_trc T -colorspace S -color_range R`.

## 9. Image burn-in `[ts:tc/subtitle-overlay-filter.ts:19-106]`

`framesOnGpu` = `nvencVfEndsOnGpu` on nvenc, `amfVfEndsOnGpu` on amf, else true; `openclFilterDevice = effective == amf || qsvOpenclTonemap`; bit depth = `variant.bitDepth`. Input `[0:{videoStreamIndex ?? v}]`, video `null` when the chain is empty; subtitle `[0:s]` cropped like the video, scaled, `format=rgba`, whitened under 10 bit. CPU paths (`none`, `videotoolbox`, CPU frames): direct overlay (RGB round trip under 10 bit). HW paths: hwdownload, overlay, re-upload (`hwupload_cuda`, `derive_device=vaapi`, `derive_device=d3d11va|qsv` with an OpenCL filter device, else bare `hwupload`).

## 10. Audio and HLS muxer

- One audio stream `[ts:ff:148-164]`: copy -> `-c:a{s} copy` (+ `-bsf:a{s} aac_adtstoasc` for AAC into fMP4); encode -> `-c:a{s} {encoder} -b:a{s} {round(bps/1000)}k -ac[:a{s}] N -ar[:a{s}] 48000 -filter:a{s} {align}`. Encoder: `aac` -> `libfdk_aac` when `hostCaps.libfdkAac` and `channels <= 6`; opus -> `libopus`. Bitrate as `decide::audio` (`audioEncodeBitrateBps` of the stereo budget). `[ts:tc/audio-encode.ts:62-66,132-144,196-225]`
- Align filter: `asetpts=PTS{-+}S/TB,aresample=async=1:first_pts=0[,apad=whole_dur=D],asetpts=PTS{+-}S/TB`, `S = formatSeconds(|start|)`, pad when `end > start`. `[ts:ff:122-133]`
- Missing `audioPlan`: AAC stereo transcode. Map spec `0:{streamIndex}` else `0:a:{i}?`. `[ts:ff:501-507]`
- `var_stream_map` when `audioStreams` given and `videoOnly && len > 1` `[ts:tc/audio-layout.ts:20-25]`: maps video and every track, per-track args (`perStreamAudioArgs`, error `{n} audio plans for {m} audio streams`), else the single audio args for all; map `v:0,agroup:audio a:i,agroup:audio,language:{lang || und}`; `hls_time = formatSeconds(realSeg - 0.0001)`, init `init_%v.mp4`, paths `{dir}/%v/seg-%04d.{ext}`, `{dir}/%v/index.m3u8`. `[ts:ff:575-616]`
- Single: `audioStreams` explicitly empty -> `-map V -an`; else `-map V -map {audio(audioStreamIndex ?? 0)}` plus the audio args; `hls_time = String(segmentDuration)`, `init.mp4`, `{dir}/seg-%04d.{ext}`. `[ts:ff:628-655]`
- Muxer tail `[ts:ff:191-255]`: fMP4 `-movflags +cmaf -avoid_negative_ts disabled`; remux `-hls_segment_options movflags=+frag_discont[:strict=unofficial]`; fMP4 not remux with origin != 0 and no output seek: `-output_ts_offset formatSeconds(-origin)`; TS with `seek + max(0, 1 - origin) > 0`: `-output_ts_offset` of that; `-f hls -hls_time T -hls_list_size N -start_number S -hls_segment_type fmp4|mpegts [-hls_fmp4_init_filename I] -hls_flags independent_segments+temp_file [-var_stream_map M] -hls_segment_filename P INDEX`. Output paths join with `/` and turn `\` into `/`. `[ts:ff:93-95]`

## 11. Remux `[ts:ff:1568-1733]`

Probe flags as VOD; `seek = run.seekSeconds` -> `-noaccurate_seek -seek_timestamp 1 -ss seek` before `-i` and `-ss seek` after `-copyts...`; clock break -> output `-to`. Audio budget: 192k, align `run.audioStartSeconds`. HEVC: `-tag:v dvh1|hvc1 -bsf:v hevc_mp4toannexb[,hevc_metadata=remove_dovi=1:remove_hdr10plus=1] -max_muxing_queue_size 2048` (dvh1 when DV and no base; remove_dovi for P7 without DV; remove_hdr10plus for DV + HDR10+). AV1: `-tag:v dav1` (DV no base), `-bsf:v av1_metadata=remove_hdr10plus=1`. Multi-audio (`varStreamMapLayout`) or no audio: `-map V -c:v copy` (+ `-an` when no audio); else video + one audio. Muxer: `hls_time = grid ? 0 : segmentDuration`, `start_number = run.startNumber`, output seek `run.seekSeconds ?? 0`, `gop-%d.m4s`, list size 1. Multi-audio then appends per track `[-ss seek] [-to break] -map A {args} -f mp4 -movflags +frag_every_frame+delay_moov+default_base_moof+frag_discont -map_chapters -1 {dir}/a{i}.mp4`. `remuxRunStart` is the session layer's (needs the keyframe grid), not ported here.

## 12. Live `[ts:modules/livetv/services/live-ffmpeg-args.ts:99-208]`

`-reconnect 1 -reconnect_streamed 1 -reconnect_on_network_error 1 -reconnect_delay_max 5 -rw_timeout 15000000 -user_agent {ua || VLC/3.0.20 LibVLC/3.0.20} [-headers "Referer: {r}\r\n"]` `[ts:modules/livetv/livetv-http.ts:36,64-68]`, `probe = max(3, probeSeconds ?? 3)`, `-analyzeduration probe*1e6 -probesize round(5e6 * probe / 3)`. Transcode: input HW flags (qsv: QSV device init + `-hwaccel qsv -hwaccel_output_format qsv -hwaccel_device qs`; vaapi: VAAPI init + `-hwaccel vaapi -hwaccel_output_format vaapi -hwaccel_device va`; nvenc: `-hwaccel cuda -hwaccel_output_format cuda`; videotoolbox: `-hwaccel videotoolbox`), then `-threads N` before `-i` (`N` = `encodeThreads`, absent 4; the engine passes the CPU quota). `-i url -map 0:v:0 -map 0:a:0? -sn -dn`; remux `-c:v copy`, else `-c:v {h264 encoder}` (`-threads:v N -preset veryfast -tune zerolatency` on CPU), `-b:v B -maxrate B -bufsize 2B -force_key_frames expr:gte(t,n_forced*{seg})` with `B = videoBitrateBps ?? 4000000`. Audio: AAC copy into TS, else AAC stereo 128k encode. Muxer: `-f hls -hls_time seg -hls_list_size max(1, ceil(window*60/seg)) -hls_delete_threshold 3 -hls_segment_type ... [-hls_fmp4_init_filename init.mp4] -hls_flags delete_segments+omit_endlist+independent_segments+temp_file+program_date_time[+append_list+discont_start] -hls_segment_filename {dir}/seg-%05d.{ext} {dir}/index.m3u8`. `extraHeaders` (the upstream's other headers, `[new]`) follow `Referer` in `-headers`; `http_header_args` is shared with the live probe.

## 13. Iframe and file output

- Iframe `[ts:tc/iframe-trick-play.ts:44-89]`: `-hide_banner -loglevel error -ss {seek.toFixed(3)} -copyts -i P -frames:v 1 -an -sn -dn -vf [crop,]scale=W:H -c:v libx264 -preset veryfast -pix_fmt yuv420p [-output_ts_offset String(offset) when truthy] -f mpegts -muxdelay 0 -muxpreload 0 pipe:1`.
- File `[new]`: layer 1 of a VOD request (no start segment, no early) + `-map V`, the picked or every audio track (`-c:a:i`), `-f mp4 -movflags +faststart` or `-f matroska`, then the path.

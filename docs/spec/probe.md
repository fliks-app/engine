# Spec: probe

`POST /v1/probe` replaces the host's `FfprobeService.detectMediaFileInfo` + `detectCrop`, `readMkvLanguageTags` and `computeMovieHash`. Pure parsing lives in `core::probe` (`Tools` trait, ffprobe/mkvmerge/cropdetect output to `MediaInfo`), processes in `engine::probe`. Golden vectors: `fixtures/probe/*.json` (recorded raw tool output + the host's result). Tags are relative to the `fliks` repo root (`ffprobe.service.ts` = `backend/src/modules/media-probe/ffprobe.service.ts`).

## Contract `[new]`

- Request `{ media: { mediaFileId, path }, crop, mkvTags, moviehash }`. `path` must be absolute (400 otherwise: a relative path could read as a tool option). The host does the ACL.
- Response `{ ok, info?, moviehash?, error? }`. `ok: false`: ffprobe could not read the file (spawn error, timeout, exit != 0, output over 1 MiB, unparsable JSON): `error` is its stderr, else the failure message, and `info` is the empty blob `{video:[],audio:[],subtitles:[],error}` the host persisted before. A readable file without video and audio is `ok: true` with `info.error = "No streams detected"`. `[ts:backend/src/modules/media-probe/ffprobe.service.ts:583-587,724-762]`
- `info` is the host's `MediaFileInfo` plus `schemaVersion` (`MEDIA_INFO_SCHEMA_VERSION`, stamped by the endpoint when ok; the pure parser leaves it out like the host). `moviehash` is `{ hash, bytesize }`, absent under 128 KiB or unreadable.
- `crop` runs the cropdetect samples on `video[0]` and sets `video[0].crop`; `mkvTags` runs `mkvmerge -J` on Matroska/WebM. The host has no such switches: it crops where the caller asks (`probeAndResolve` `detectCrop`) and always reads tags. `[ts:backend/src/modules/media/media-service/media-rescan.service.ts:1038-1066]`
- Binaries: `FFPROBE_PATH`, `FFMPEG_PATH`, `MKVMERGE_PATH`, default the bare names on `PATH` (the host only uses `PATH`). `[new]`

## Tool runs (`core::probe::args`: arguments, order and timeouts as the host)

| Run | Arguments | Timeout |
|---|---|---|
| main | `ffprobe -v error -print_format json -show_streams -show_format -show_chapters PATH` | 30 s, 1 MiB output `[ts:backend/src/modules/media-probe/ffprobe.service.ts:595-600]` |
| frame rate | `-select_streams N -show_entries packet=pts_time -read_intervals FROM%+60 -of csv=p=0`, only when `duration >= 90`, `FROM = max(0, floor(duration/2 - 30))` | 30 s `[ts:backend/src/modules/media-probe/ffprobe.service.ts:940-962]` |
| first frame | `-select_streams N -show_entries frame=pts,best_effort_timestamp,side_data_list:frame_side_data=...` `-of json`, read line by line, killed at the first complete frame | 30 s `[ts:backend/src/modules/media-probe/ffprobe.service.ts:781-823]` |
| audio starts | `-show_entries packet=stream_index,pts_time -of csv=p=0`, killed once every audio stream showed or a packet is 120 s past the first | 60 s `[ts:backend/src/modules/media-probe/ffprobe.service.ts:886-912]` |
| late audio | `-analyzeduration ceil(reach*1e6) -probesize FILESIZE -select_streams a -show_entries stream=index,channels,channel_layout,sample_rate -of json`, only for audio missing channels or sample rate | 120 s `[ts:backend/src/modules/media-probe/ffprobe.service.ts:825-880]` |
| tags | `mkvmerge -J PATH`, 16 MiB output, missing binary remembered per process; exit 1 with output still counts `[ts:backend/src/modules/media-probe/mkv-language-tags.ts:74-110]` | 15 s |
| crop | `ffmpeg [-threads 2 \| hw decode args] -ss S -i PATH -t 5 -vf format=nv12,cropdetect=limit=24:round=16:reset=0:skip=24 -an -f null -`, `ionice -c3 nice -n19` in front on Linux `[ts:backend/src/modules/media-probe/ffprobe.service.ts:1108-1150]` | 30 s |

## Algorithm

1. Streams: programme video = video streams without `attached_pic`; a still (`mjpeg png bmp gif webp`) beside a moving track is dropped. Order is the container's. `[ts:backend/src/modules/media-probe/ffprobe.service.ts:319-328]`
2. Video fields are ffprobe's, numbers via JS `Number()` (a non-numeric string is `NaN`). Size swaps for a quarter-turn rotation (`rotation` side data, else the `rotate` tag; `abs % 180 == 90`). `dvProfile`, `dvBlSignalCompatId`, `dvLevel` come from the first side data with a numeric `dv_profile`; `dvRpuPresent`, `dvBlPresent`, `dvElPresent` are `flag === 1`, so `false` without a record. `hdrFormat`: `smpte2084` + `bt2020` = HDR10, `arib-std-b67` + `bt2020` = HLG, nothing else. `[ts:backend/src/modules/media-probe/ffprobe.service.ts:332-346,764-777,583-660]`
3. Frame rate: `r_frame_rate`, else `avg_frame_rate`; `0/0` and non-positive rates are skipped, a value without `/`, or with a zero part, is kept verbatim; otherwise decimal with `toFixed(3)` (ties up) and trailing zeros dropped. The first programme stream is measured (middle 60 s window, at least 100 packets, 1..480 fps, sorted `(n-1)/span`); the measured rate wins when it differs from the declared one by more than 0.1%. `[ts:backend/src/modules/media-probe/ffprobe.service.ts:87-90,940-1021]`
4. `end_seconds`: the `DURATION` tag when `h:mm:ss[.f]` (Matroska), else `start_time + duration`. `[ts:backend/src/modules/media-probe/ffprobe.service.ts:294-304]`
5. `video[0]` first frame: `firstFrameSeconds = pts ?? best_effort_timestamp` in the stream time base (`0` when unusable), absent when no frame decodes. HDR10 only: `hdrMetadata` from the mastering display and content light side data (`G()B()R()WP()L()` in 0.00002 / 0.0001 units, JS `Math.round`), dropped when any field is unparsable. `hdr10Plus` is always set (`true` for the SMPTE 2094-40 side data). `[ts:backend/src/modules/media-probe/ffprobe.service.ts:357-385,442-510,688-700]`
6. Audio: language = `language` tag unless empty or `und`, else inferred from the title (28 languages, ASCII `\b` word matching), else `und`; tag keys match case-insensitively, first key in file order wins. `commentary`, `audioDescription` only when true. A first packet later than the header start replaces it (`startTimeSeconds`). Streams without channels or sample rate get them from the late-audio run (`reach = max(0, latest start - format start) + 1`). `[ts:backend/src/modules/media-probe/ffprobe.service.ts:389-440,306-317,825-880]`
7. Subtitles: `forced` / `hearingImpaired` = disposition or title regex (`forced`, `foreign parts` not preceded by `non-`/`not `; `sdh`, `cc`, `hearing impaired`, `closed captions`); image based = `hdmv_pgs_subtitle dvd_subtitle dvb_subtitle xsub`. `[ts:backend/src/common/constants/subtitle-flags.ts:34-50]`
8. Chapters: `start_time`/`end_time` default 0, kept when `end > start`, title from the `title` tag. Container fields: `formatBitRate` only when positive, `formatName`, `formatStartSeconds`, `durationSeconds`. `[ts:backend/src/modules/media-probe/ffprobe.service.ts:719-722]`
9. Tags: Matroska/WebM (`/matroska/` on `format_name`): `mkvmerge -J` track `id` = stream index; `language_ietf` becomes `languageTag` when it has a subtag (`^[a-z]{2,3}(-[a-z0-9]{2,8})+$`, not `und-*`) and the track type matches. `[ts:backend/src/modules/media-probe/mkv-language-tags.ts:19-72]`
10. Crop: 6 samples at `floor(duration * {.05,.15,.3,.45,.6,.8})` (600 s when unknown); the last `crop=W:H:X:Y` of each log; a refused first sample is retried once, in software; the largest area wins (first on a tie); kept when `height - h >= 40` or `width - w >= 40` (the stream's displayed size, `y*2`/`x*2` when unknown). `[ts:backend/src/modules/media-probe/ffprobe.service.ts:1024-1106]`
    Decode: hardware first, `core::probe::args::crop_hw_decode(platform, render node, node exists, /dev/nvidiactl exists)` from the hw snapshot (host platform and default node before the first detection): linux `-hwaccel vaapi -hwaccel_device NODE -hwaccel_output_format vaapi` (+ `hwdownload,` before the filter) when the node exists, else `-hwaccel cuda` when `/dev/nvidiactl` exists, else software; win32 `-hwaccel d3d11va`; darwin `-hwaccel videotoolbox`. The first sample decides: if it fails, all samples (and its retry) run in software, which gives the same nv12 pixels. The decoder used is logged (`decode=vaapi|cuda|...|sw`). `[ts:backend/src/modules/media-probe/ffprobe.service.ts:37-60,1030-1130]`
11. Moviehash: size + sum of the little-endian u64 words of the first and last 64 KiB, modulo 2^64, 16 hex digits; `null` under 128 KiB. `[ts:backend/src/modules/subtitles/moviehash.ts:13-60]`

## Deviations

See `docs/README.md` "Deviations" (probe): NaN as absent, moviehash for sizes with bit 31 set, JSON error text, untyped fields. `[new]`

## Notes

- `[assumed]` The recorded outputs come from jellyfin-ffmpeg 8.1.2 and mkvtoolnix 82 (`tools/probe-tools.sh`); other versions print other fields, which the parser ignores.
- The `Tools` trait keeps the flow (which run follows which, early stops, retries) in `core`: the tests replay recorded output through the very `probe()` the engine runs, and check the arguments of every run.

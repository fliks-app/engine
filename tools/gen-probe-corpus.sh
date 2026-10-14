#!/bin/bash
# Generates the small probe corpus (gitignored) with the ffmpeg/mkvmerge of the engine image.
# Usage: tools/gen-probe-corpus.sh [outdir]   (default: probe-corpus/); env: PROBE_TOOLS (bin dir).
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
out=${1:-$here/probe-corpus}
tools=${PROBE_TOOLS:-$("$here/tools/probe-tools.sh")}
FF="$tools/ffmpeg -hide_banner -loglevel error -y -fflags +bitexact -flags:v +bitexact -flags:a +bitexact"
MKV="$tools/mkvmerge --no-date --disable-track-statistics-tags"
rm -rf "$out"; mkdir -p "$out"; cd "$out"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT

# Lavfi sources: v SIZE RATE DUR, a FREQ DUR.
v() { echo "-f lavfi -i testsrc2=size=${1:-160x90}:rate=${2:-24}:duration=${3:-3}"; }
a() { echo "-f lavfi -i sine=frequency=${1:-440}:sample_rate=48000:duration=${2:-3}"; }
X264="-c:v libx264 -preset veryfast -threads 1 -pix_fmt yuv420p"
X264_10="-c:v libx264 -preset veryfast -threads 1 -pix_fmt yuv420p10le"
X265_10="-c:v libx265 -preset ultrafast -pix_fmt yuv420p10le"
HDR_PQ="colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc"
MASTER="master-display=G(8500,39850)B(6550,2300)R(35400,14600)WP(15635,16450)L(10000000,1):max-cll=1000,400"

# Containers and codecs.
$FF $(v) $(a) $X264 -c:a aac -b:a 64k -metadata:s:a:0 language=eng h264-aac.mkv
$FF $(v) $(a) $X264 -c:a aac -b:a 64k -metadata:s:a:0 language=eng -movflags +faststart h264-aac.mp4
$FF $(v) $(a) $X264 -c:a aac -b:a 64k -metadata:s:a:0 language=eng -f mpegts h264-aac.ts
$FF $(v) $(a) $X264_10 -c:a aac h264-10bit.mkv
$FF $(v) $(a) -c:v libx265 -preset ultrafast -pix_fmt yuv420p -tag:v hvc1 -c:a aac hevc-8bit.mp4
$FF $(v) $(a) $X265_10 -c:a ac3 hevc-10bit.mkv
$FF $(v) $(a) -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -threads 1 -c:a libopus -b:a 48k vp9-opus.webm

# HDR: HDR10 with static metadata (mkv and mp4), without, HLG, an 8-bit mis-tagged PQ.
$FF $(v) $(a) $X265_10 -x265-params "$HDR_PQ:$MASTER:hdr10-opt=1:repeat-headers=1:range=limited" -c:a aac hdr10-meta.mkv
$FF $(v) $(a) $X265_10 -x265-params "$HDR_PQ:$MASTER:hdr10-opt=1:repeat-headers=1:range=limited" -tag:v hvc1 -c:a aac hdr10-meta.mp4
$FF $(v) $(a) $X265_10 -x265-params "$HDR_PQ:range=limited" -c:a aac hdr10-nometa.mkv
$FF $(v) $(a) $X265_10 -x265-params "colorprim=bt2020:transfer=arib-std-b67:colormatrix=bt2020nc:range=limited" -c:a aac hlg.mkv
$FF $(v) $(a) $X264 -x264-params "$HDR_PQ" -c:a aac hdr-pq-8bit.mkv

# Audio layouts, codecs, languages, roles. Stream order matters: the index is the identity.
$FF $(v) $(a 220) $(a 330) $(a 440) $(a 550) $(a 660) $X264 \
  -filter_complex "[1:a]aformat=channel_layouts=mono[a1];[2:a]aformat=channel_layouts=stereo[a2];[3:a]aformat=channel_layouts=5.1[a3];[4:a]aformat=channel_layouts=7.1[a4];[5:a]aformat=channel_layouts=5.1[a5]" \
  -map 0:v -map "[a1]" -map "[a2]" -map "[a3]" -map "[a4]" -map "[a5]" \
  -c:a:0 aac -c:a:1 ac3 -c:a:2 eac3 -c:a:3 flac -c:a:4 libopus -b:a:4 256k \
  -metadata:s:a:0 language=eng -metadata:s:a:0 title="Main" -disposition:a:0 default \
  -metadata:s:a:1 language=fre -metadata:s:a:1 title="Director Commentary" -disposition:a:1 comment \
  -metadata:s:a:2 language=ger -metadata:s:a:2 title="Audio Description" -disposition:a:2 visual_impaired \
  -metadata:s:a:3 language=jpn \
  -metadata:s:a:4 language=und -metadata:s:a:4 title="French Opus 5.1" \
  multi-audio.mkv
$FF $(v) $(a 220) $(a 330) $(a 440) $X264 -map 0:v -map 1:a -map 2:a -map 3:a \
  -c:a:0 aac -c:a:1 ac3 -c:a:2 eac3 -ac:a:2 6 \
  -metadata:s:a:0 language=eng -metadata:s:a:1 language=spa -metadata:s:a:2 language=fra -disposition:a:2 default \
  -movflags +faststart multi-audio.mp4

# Subtitles: text (srt, ass, webvtt) with flags and titles, bitmap (PGS), mov_text in mp4.
printf '1\n00:00:00,500 --> 00:00:02,000\nHello\n\n2\n00:00:02,200 --> 00:00:02,900\nWorld\n' > "$tmp/a.srt"
printf 'WEBVTT\n\n00:00.500 --> 00:02.000\nHello\n' > "$tmp/a.vtt"
$FF -i "$tmp/a.srt" "$tmp/a.ass"
python3 - "$tmp/a.sup" <<'P'
import struct, sys
def seg(t, p, pts): return b'PG' + struct.pack('>II', pts, 0) + bytes([t]) + struct.pack('>H', len(p)) + p
pcs = lambda n, s: struct.pack('>HHBHBBBB', 160, 90, 0x10, n, s, 0, 0, 0)
open(sys.argv[1], 'wb').write(seg(0x16, pcs(0, 0x80), 90000) + seg(0x80, b'', 90000) + seg(0x16, pcs(1, 0), 180000) + seg(0x80, b'', 180000))
P
$FF $(v) $(a) -i "$tmp/a.srt" -i "$tmp/a.ass" -i "$tmp/a.vtt" -i "$tmp/a.srt" -i "$tmp/a.srt" -i "$tmp/a.sup" $X264 -c:a aac \
  -map 0:v -map 1:a -map 2 -map 3 -map 4 -map 5 -map 6 -map 7 \
  -c:s:0 srt -c:s:1 ass -c:s:2 webvtt -c:s:3 srt -c:s:4 srt -c:s:5 copy \
  -metadata:s:s:0 language=eng \
  -metadata:s:s:1 language=fre -metadata:s:s:1 title="Forced" \
  -metadata:s:s:2 language=eng -metadata:s:s:2 title="English SDH" \
  -metadata:s:s:3 language=spa -disposition:s:3 forced \
  -metadata:s:s:4 language=und -metadata:s:s:4 title="Deutsch" -disposition:s:4 hearing_impaired \
  -metadata:s:s:5 language=jpn \
  subs-text.mkv
$FF $(v) $(a) -i "$tmp/a.srt" $X264 -c:a aac -c:s mov_text -metadata:s:s:0 language=eng subs-text.mp4
$FF -i "$tmp/a.srt" -c:s srt -metadata:s:s:0 language=eng subs-only.mkv   # "No streams detected"

# Chapters (one empty, dropped by the probe), start offsets, cover art, audio only, rotation.
cat > "$tmp/chap.txt" <<'C'
;FFMETADATA1
[CHAPTER]
TIMEBASE=1/1000
START=0
END=1000
title=Intro
[CHAPTER]
TIMEBASE=1/1000
START=1000
END=1000
title=Empty
[CHAPTER]
TIMEBASE=1/1000
START=1000
END=3000
title=Main
C
$FF $(v) $(a) -i "$tmp/chap.txt" $X264 -c:a aac -map 0:v -map 1:a -map_metadata 2 chapters.mkv
$FF $(v) $(a) -i "$tmp/chap.txt" $X264 -c:a aac -map 0:v -map 1:a -map_metadata 2 chapters.mp4
$FF $(v) -itsoffset 2.5 $(a) $X264 -c:a aac start-offset-mkv-audio-late.mkv
$FF $(v) $(a) -output_ts_offset 5 $X264 -c:a aac start-offset.mkv
$FF -f lavfi -i "color=c=red:size=64x64:duration=1:rate=1" -frames:v 1 "$tmp/cover.jpg"
$FF $(v) $(a) -i "$tmp/cover.jpg" $X264 -c:a aac -map 0:v -map 1:a -map 2:v -c:v:1 mjpeg -disposition:v:1 attached_pic cover-art.mp4
$FF $(a) -c:a flac audio-only.flac
$FF $(a) -c:a aac -metadata:s:a:0 language=eng audio-only.m4a
$FF -display_rotation 90 -i h264-aac.mp4 -c copy rotated.mp4

# Letterbox: 480x190 content in 480x270 (80px total bars, cropped), 480x254 (16px, not cropped), pillarbox.
bars() { $FF -f lavfi -i "testsrc2=size=$2:rate=24:duration=3" -vf "pad=$3:$4:$5:$6:black" $X264 "$1"; }
bars letterbox.mkv 480x190 480 270 0 40
bars letterbox-small.mkv 480x238 480 254 0 8
bars pillarbox.mkv 320x270 480 270 80 0
$FF -f lavfi -i "testsrc2=size=480x190:rate=24:duration=3" -vf "pad=480:270:0:40:black" $X265_10 -x265-params "$HDR_PQ:$MASTER" letterbox-hdr10.mkv

# Long files (>90 s) exercise the packet-measured frame rate; the second lies in its header.
$FF -f lavfi -i testsrc2=size=96x54:rate=24000/1001:duration=100 $(a 440 100) $X264 -x264-params keyint=240 -c:a aac -b:a 24k long.mkv
$FF -r 500/21 -i long.mkv -c copy long-wrong-default-duration.mkv

# Audio starting past the probe window: a TS whose audio begins 8 s in.
$FF $(v 160x90 24 14) -itsoffset 8 $(a 440 6) $X264 -c:a aac -f mpegts audio-late.ts

# Matroska IETF language tags (mkvmerge), including a bare language that must stay untagged.
$MKV -o mkv-ietf.mkv --language 2:es-419 --language 3:pt-BR --language 4:eng --language 5:zh-Hant --language 6:und \
  subs-text.mkv >/dev/null 2>&1 || true
$MKV -o mkv-ietf-audio.mkv --language 1:es-419 --language 2:fr-CA --language 3:de-DE-1996 multi-audio.mkv >/dev/null || true

# Not media.
head -c 4096 /dev/urandom | od > garbage.bin
cd - >/dev/null; echo "corpus: $(ls "$out" | wc -l) files, $(du -sh "$out" | cut -f1)"

#!/usr/bin/env bash
# Builds the tools pack of one platform into <out>/tools: ffmpeg, ffprobe, fpcalc, mkvmerge and
# mkvpropedit with their licences, from the pinned downloads of pins.env.
# Usage: build.sh darwin-arm64|windows-x64 <out>
set -euo pipefail
name="${1:?platform}"; out="${2:?out dir}"
here="$(cd "$(dirname "$0")" && pwd)"
. "$here/pins.env"
key="$(echo "$name" | tr 'a-z-' 'A-Z_')"
pin() { local v="${1}_${key}"; echo "${!v}"; }
dest="$out/tools"; work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$dest/licenses"

fetch() { # url sha256 file
  curl --proto '=https' --tlsv1.2 -fsSL -o "$work/$3" "$1"
  local got; got="$( (sha256sum "$work/$3" 2>/dev/null || shasum -a 256 "$work/$3") | cut -d' ' -f1)"
  [ "$got" = "$2" ] || { echo "sha256 of $1 is $got, pinned $2" >&2; exit 1; }
}
unzip_py() { python3 -I -c 'import sys,zipfile; zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])' "$1" "$2"; }
# Copies the first directory holding `$2` under `$1` into `$3`.
copy_dir_of() { local f; f="$(find "$1" -name "$2" -type f | head -1)"; [ -n "$f" ] || { echo "$2 not found" >&2; exit 1; }; cp -R "$(dirname "$f")/." "$3/"; }

jf="https://github.com/jellyfin/jellyfin-ffmpeg/releases/download/v${JELLYFIN_FFMPEG_VERSION}/jellyfin-ffmpeg_${JELLYFIN_FFMPEG_VERSION}_portable_"
cp_url="https://github.com/acoustid/chromaprint/releases/download/v${CHROMAPRINT_VERSION}/chromaprint-fpcalc-${CHROMAPRINT_VERSION}-"
mkv="https://mkvtoolnix.download"
case "$name" in
  darwin-arm64)
    fetch "${jf}macarm64-gpl.tar.xz" "$(pin JELLYFIN_FFMPEG_SHA256)" ff.tar.xz
    mkdir "$work/ff" && tar -xJf "$work/ff.tar.xz" -C "$work/ff"
    copy_dir_of "$work/ff" ffmpeg "$dest"
    fetch "${cp_url}macos-arm64.tar.gz" "$(pin CHROMAPRINT_SHA256)" cp.tar.gz
    mkdir "$work/cp" && tar -xzf "$work/cp.tar.gz" -C "$work/cp"
    cp "$(find "$work/cp" -name fpcalc -type f | head -1)" "$dest/"
    fetch "$mkv/macos/releases/${MKVTOOLNIX_VERSION}/MKVToolNix-${MKVTOOLNIX_VERSION}-1-arm64.dmg" "$(pin MKVTOOLNIX_SHA256)" mkv.dmg
    mnt="$work/mnt"; mkdir "$mnt"
    hdiutil attach -nobrowse -readonly -mountpoint "$mnt" "$work/mkv.dmg" >/dev/null
    for t in mkvmerge mkvpropedit; do cp "$(find "$mnt" -path '*Contents/MacOS/*' -name "$t" -type f | head -1)" "$dest/"; done
    hdiutil detach "$mnt" >/dev/null
    ;;
  windows-x64)
    fetch "${jf}win64-clang-gpl.zip" "$(pin JELLYFIN_FFMPEG_SHA256)" ff.zip
    unzip_py "$work/ff.zip" "$work/ff"
    copy_dir_of "$work/ff" ffmpeg.exe "$dest"
    fetch "${cp_url}windows-x86_64.zip" "$(pin CHROMAPRINT_SHA256)" cp.zip
    unzip_py "$work/cp.zip" "$work/cp"
    cp "$(find "$work/cp" -name fpcalc.exe -type f | head -1)" "$dest/"
    fetch "$mkv/windows/releases/${MKVTOOLNIX_VERSION}/mkvtoolnix-64-bit-${MKVTOOLNIX_VERSION}.zip" "$(pin MKVTOOLNIX_SHA256)" mkv.zip
    unzip_py "$work/mkv.zip" "$work/mkv"
    for t in mkvmerge mkvpropedit; do cp "$(find "$work/mkv" -name "$t.exe" -type f | head -1)" "$dest/"; done
    ;;
  *) echo "unknown platform $name" >&2; exit 2 ;;
esac

# The licences ship next to the binaries; the sources are the pinned releases above.
find "$work" -maxdepth 4 \( -iname 'COPYING*' -o -iname 'LICENSE*' \) -type f -exec cp -n {} "$dest/licenses/" \; || true
cat > "$dest/SOURCE" <<TXT
ffmpeg, ffprobe: jellyfin-ffmpeg ${JELLYFIN_FFMPEG_VERSION} (GPL), https://github.com/jellyfin/jellyfin-ffmpeg/releases/tag/v${JELLYFIN_FFMPEG_VERSION}
fpcalc: chromaprint ${CHROMAPRINT_VERSION} (LGPL-2.1+ / MIT), https://github.com/acoustid/chromaprint/releases/tag/v${CHROMAPRINT_VERSION}
mkvmerge, mkvpropedit: MKVToolNix ${MKVTOOLNIX_VERSION} (GPL-2.0), https://mkvtoolnix.download/source.html
TXT

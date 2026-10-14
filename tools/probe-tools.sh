#!/bin/bash
# Prints a directory holding ffprobe, ffmpeg and mkvmerge wrappers that run the binaries of the
# engine image (jellyfin-ffmpeg + mkvtoolnix) on the host, so fixtures use the pinned tool versions.
# Usage: PROBE_TOOLS=$(tools/probe-tools.sh); env: PROBE_IMAGE (default fliks:engine-m0), PROBE_TOOLS_DIR.
set -euo pipefail
image=${PROBE_IMAGE:-fliks:engine-m0}
dir=${PROBE_TOOLS_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/fliks-engine/probe-tools}
if [ ! -x "$dir/bin/mkvmerge" ]; then
  mkdir -p "$dir/sysroot" "$dir/bin"
  docker run --rm --entrypoint sh -v "$dir/sysroot:/out" "$image" -c '
    set -e
    mkdir -p /out/lib
    for b in /usr/lib/jellyfin-ffmpeg/ffprobe /usr/lib/jellyfin-ffmpeg/ffmpeg /usr/bin/mkvmerge; do
      ldd "$b" | awk "/=>/ {print \$3} /ld-linux/ {print \$1}"
    done | sort -u | grep "^/" | while read -r f; do cp -L "$f" /out/lib/; done
    cp /usr/lib/jellyfin-ffmpeg/ffprobe /usr/lib/jellyfin-ffmpeg/ffmpeg /usr/bin/mkvmerge /out/' >&2
  for b in ffprobe ffmpeg mkvmerge; do
    # The image's own loader and libs: independent of the host libc.
    printf '#!/bin/sh\nexec %s/lib/ld-linux-x86-64.so.2 --library-path %s/lib %s/%s "$@"\n' \
      "$dir/sysroot" "$dir/sysroot" "$dir/sysroot" "$b" > "$dir/bin/$b"
    chmod +x "$dir/bin/$b"
  done
fi
echo "$dir/bin"

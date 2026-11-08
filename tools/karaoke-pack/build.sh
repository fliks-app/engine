#!/usr/bin/env bash
# Usage: tools/karaoke-pack/build.sh <darwin-arm64|windows-x64> <out dir>
# Stages out/karaoke/: ONNX Runtime, whisper-cli, ffmpeg-pitch and their licences. The engine
# finds that directory next to its executable (docs/spec/karaoke.md section 1).
set -euo pipefail
platform="${1:?platform}"; out="$(mkdir -p "${2:?out dir}" && cd "$2" && pwd)"
here="$(cd "$(dirname "$0")" && pwd)"
. "$here/pins.env"
work="$(mktemp -d)"; trap 'rm -rf "$work"' EXIT
dest="$out/karaoke"; lic="$dest/licenses"
mkdir -p "$dest" "$lic"/{onnxruntime,whisper.cpp,ffmpeg-pitch}
jobs="$(nproc 2>/dev/null || sysctl -n hw.ncpu)"

fetch() { # url sha256 file
    curl --proto '=https' -fsSL -o "$work/$3" "$1"
    echo "$2  $work/$3" | (sha256sum -c - 2>/dev/null || shasum -a 256 -c -)
}

case "$platform" in
darwin-arm64) ort="onnxruntime-osx-arm64-$ONNXRUNTIME_VERSION.tgz"; ort_sha=$ORT_SHA256_DARWIN_ARM64; exe=""
    whisper_flags="-DGGML_METAL=ON -DGGML_METAL_EMBED_LIBRARY=ON"; ffmpeg_flags="--extra-libs=-lc++" ;;
windows-x64) ort="onnxruntime-win-x64-$ONNXRUNTIME_VERSION.zip"; ort_sha=$ORT_SHA256_WINDOWS_X64; exe=".exe"
    whisper_flags="-G Ninja -DCMAKE_EXE_LINKER_FLAGS=-static"; ffmpeg_flags="--extra-ldflags=-static --pkg-config-flags=--static --extra-libs=-lstdc++" ;;
*) echo "unknown platform $platform" >&2; exit 2 ;;
esac

# ONNX Runtime: the official release, CoreML compiled in on macOS (DirectML is not in the Windows one).
fetch "https://github.com/microsoft/onnxruntime/releases/download/v$ONNXRUNTIME_VERSION/$ort" "$ort_sha" "$ort"
mkdir "$work/ort"
if [ "$platform" = windows-x64 ]; then unzip -q "$work/$ort" -d "$work/ort"; else tar -xzf "$work/$ort" -C "$work/ort"; fi
o="$(echo "$work"/ort/*)" # the one extracted directory
if [ "$platform" = windows-x64 ]; then
    cp "$o/lib/onnxruntime.dll" "$o/lib/onnxruntime_providers_shared.dll" "$dest/"
else
    cp "$o/lib/libonnxruntime.$ONNXRUNTIME_VERSION.dylib" "$dest/libonnxruntime.dylib"
fi
cp "$o/LICENSE" "$o/ThirdPartyNotices.txt" "$lic/onnxruntime/"

# whisper-cli from the same release as the image.
fetch "https://github.com/ggml-org/whisper.cpp/archive/refs/tags/$WHISPER_CPP_VERSION.tar.gz" "$WHISPER_CPP_SHA256" whisper.tgz
mkdir "$work/whisper" && tar -xzf "$work/whisper.tgz" -C "$work/whisper" --strip-components=1
(cd "$work/whisper" && cmake -B build -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF -DGGML_NATIVE=OFF \
    -DGGML_OPENMP=OFF -DWHISPER_BUILD_TESTS=OFF -DWHISPER_BUILD_EXAMPLES=ON $whisper_flags \
    && cmake --build build -j"$jobs" --target whisper-cli)
cp "$work/whisper/build/bin/whisper-cli$exe" "$dest/"
strip "$dest/whisper-cli$exe" 2>/dev/null || true
cp "$work/whisper/LICENSE" "$lic/whisper.cpp/copyright"

# ffmpeg with the rubberband filter, rubberband linked statically (GPL, so the build is GPLv3).
fetch "https://breakfastquay.com/files/releases/rubberband-$RUBBERBAND_VERSION.tar.bz2" "$RUBBERBAND_SHA256" rb.tar.bz2
mkdir "$work/rb" && tar -xjf "$work/rb.tar.bz2" -C "$work/rb" --strip-components=1
deps="$work/deps"
(cd "$work/rb" && meson setup build --prefix "$deps" --libdir lib --buildtype release -Ddefault_library=static \
    -Dresampler=builtin -Dcmdline=disabled -Dtests=disabled -Djni=disabled -Dladspa=disabled \
    -Dlv2=disabled -Dvamp=disabled && meson install -C build)
fetch "https://github.com/FFmpeg/FFmpeg/archive/refs/tags/n$FFMPEG_VERSION.tar.gz" "$FFMPEG_SHA256" ffmpeg.tgz
mkdir "$work/ffmpeg" && tar -xzf "$work/ffmpeg.tgz" -C "$work/ffmpeg" --strip-components=1
(cd "$work/ffmpeg" && PKG_CONFIG_PATH="$deps/lib/pkgconfig" ./configure --disable-autodetect --disable-doc \
    --disable-debug --disable-ffplay --disable-ffprobe --disable-network --enable-gpl --enable-librubberband \
    $ffmpeg_flags && make -j"$jobs" "ffmpeg$exe")
cp "$work/ffmpeg/ffmpeg$exe" "$dest/ffmpeg-pitch$exe"
strip "$dest/ffmpeg-pitch$exe" 2>/dev/null || true
cp "$work/ffmpeg/COPYING.GPLv3" "$lic/ffmpeg-pitch/"
cp "$work/rb/COPYING" "$lic/ffmpeg-pitch/COPYING.rubberband"
cat > "$lic/ffmpeg-pitch/SOURCE" <<EOT
ffmpeg-pitch is FFmpeg $FFMPEG_VERSION built with --enable-gpl --enable-librubberband, under the GNU GPL v3 (COPYING.GPLv3).
FFmpeg source: https://github.com/FFmpeg/FFmpeg/archive/refs/tags/n$FFMPEG_VERSION.tar.gz (sha256 $FFMPEG_SHA256).
Rubber Band Library $RUBBERBAND_VERSION (GPL-2.0-or-later, COPYING.rubberband), linked statically with its built-in resampler: https://breakfastquay.com/files/releases/rubberband-$RUBBERBAND_VERSION.tar.bz2 (sha256 $RUBBERBAND_SHA256).
Build: tools/karaoke-pack/build.sh of https://github.com/fliks-app/engine, which holds every configure option.
EOT
find "$dest" -type f | sort

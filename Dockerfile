FROM ubuntu:24.04@sha256:9c7f200c07e3d4a03e7249d2c16bf66a51120fb876aa2242cf31451412543289 AS vobsub-ocr-build
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl build-essential pkg-config clang libclang-dev \
    libtesseract-dev libleptonica-dev \
  && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain 1.99.0 \
  && . "$HOME/.cargo/env" \
  && cargo install subtile-ocr --version 0.2.6 --locked --root /opt/subtile-ocr \
  && rm -rf /var/lib/apt/lists/* "$HOME/.cargo/registry" "$HOME/.rustup"

# Speech recogniser of the `Align` job (MIT), with the Vulkan backend (a GPU when the host passes one,
# else the CPU). A generic build for the architecture: the image runs on whatever CPU the host has,
# so no `-march=native`; amd64 takes x86-64-v3 (AVX2, FMA, F16C).
FROM ubuntu:24.04@sha256:9c7f200c07e3d4a03e7249d2c16bf66a51120fb876aa2242cf31451412543289 AS whisper-build
ARG TARGETARCH
ARG WHISPER_CPP_VERSION=v1.9.5
ARG WHISPER_CPP_SHA256=ff1a9053feb509ff9d7729703355541ae9690073a6b1c40eb692c962e0dc1720
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl build-essential cmake libvulkan-dev glslc spirv-headers \
  && curl -fsSL -o /tmp/whisper.tar.gz "https://github.com/ggml-org/whisper.cpp/archive/refs/tags/${WHISPER_CPP_VERSION}.tar.gz" \
  && echo "${WHISPER_CPP_SHA256}  /tmp/whisper.tar.gz" | sha256sum -c - \
  && mkdir /src && tar -xzf /tmp/whisper.tar.gz -C /src --strip-components=1 \
  && cd /src \
  && ISA=""; if [ "$TARGETARCH" = "amd64" ]; then ISA="-DGGML_AVX=ON -DGGML_AVX2=ON -DGGML_FMA=ON -DGGML_F16C=ON"; fi \
  && cmake -B build -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF -DGGML_NATIVE=OFF -DGGML_OPENMP=OFF -DGGML_VULKAN=ON \
       -DWHISPER_BUILD_TESTS=OFF -DWHISPER_BUILD_EXAMPLES=ON $ISA \
  && cmake --build build -j"$(nproc)" --target whisper-cli \
  && strip build/bin/whisper-cli \
  && install -D -m 644 LICENSE /licenses/whisper.cpp/copyright

# An ffmpeg with the `rubberband` filter (key changes): jellyfin-ffmpeg is built without it. Same
# release as the main one; only the audio side matters, so nothing but opus is linked in.
FROM ubuntu:24.04@sha256:9c7f200c07e3d4a03e7249d2c16bf66a51120fb876aa2242cf31451412543289 AS pitch-ffmpeg-build
ARG FFMPEG_VERSION=8.1.2
ARG FFMPEG_SHA256=9fd092511605bbebafe095ea6d38d9e40f34d12f7386e1258372df8be0576eb7
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl build-essential pkg-config nasm librubberband-dev libopus-dev \
  && curl -fsSL -o /tmp/ffmpeg.tar.gz "https://github.com/FFmpeg/FFmpeg/archive/refs/tags/n${FFMPEG_VERSION}.tar.gz" \
  && echo "${FFMPEG_SHA256}  /tmp/ffmpeg.tar.gz" | sha256sum -c - \
  && mkdir /src && tar -xzf /tmp/ffmpeg.tar.gz -C /src --strip-components=1 \
  && cd /src \
  && ./configure --disable-autodetect --disable-doc --disable-debug --disable-ffplay --disable-ffprobe \
       --disable-network --enable-gpl --enable-librubberband --enable-libopus \
  && make -j"$(nproc)" ffmpeg \
  && strip ffmpeg \
  && install -D -m 644 COPYING.GPLv3 /licenses/ffmpeg-pitch/COPYING.GPLv3 \
  && RB="$(dpkg-query -W -f='${Version}' librubberband-dev)" \
  && printf '%s\n' \
     "ffmpeg-pitch is FFmpeg ${FFMPEG_VERSION} built with --enable-gpl --enable-librubberband --enable-libopus, under the GNU GPL v3 (COPYING.GPLv3)." \
     "FFmpeg source: https://github.com/FFmpeg/FFmpeg/archive/refs/tags/n${FFMPEG_VERSION}.tar.gz (sha256 ${FFMPEG_SHA256})." \
     "Build options: see the configure line of the pitch-ffmpeg-build stage of https://github.com/fliks-app/engine/blob/main/Dockerfile." \
     "librubberband (GPL-2.0-or-later) ${RB}, Ubuntu 24.04: source with 'apt source librubberband' or https://packages.ubuntu.com/source/noble/rubberband." \
     > /licenses/ffmpeg-pitch/SOURCE

FROM rust:1.99.0-bookworm@sha256:114c7a4425406451c2866b6aafe69fe29b1b298832db1277d411ac73c82d04d6 AS chef
RUN cargo install cargo-chef --version 0.1.78 --locked
WORKDIR /src

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
ARG CARGO_BUILD_JOBS
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json -p fliks-engine --bin fliks-engine --features karaoke
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN cargo build --release --locked -p fliks-engine --bin fliks-engine --features karaoke

FROM ubuntu:24.04@sha256:9c7f200c07e3d4a03e7249d2c16bf66a51120fb876aa2242cf31451412543289

# Per-arch deb; Intel OpenCL (tonemap_opencl) is amd64-only, arm64 is CPU-only.
ARG TARGETARCH
ARG JELLYFIN_FFMPEG_VERSION=8.1.2-1
ARG JELLYFIN_FFMPEG_SHA256_AMD64=0683c730f0cfa7d2edd321d5c4921ef17d4e6a33a9626c04bef9092c29f94e1e
ARG JELLYFIN_FFMPEG_SHA256_ARM64=e8fc2af5e4b9644aab5d41bbeb7b4443ada4236993031ce0010de936685f55c4
# ONNX Runtime (CPU) for the model jobs, loaded at run time by the engine. A CUDA or OpenVINO build
# of the same library can be mounted over it (`ORT_DYLIB_PATH`).
ARG ONNXRUNTIME_VERSION=1.31.1
# The NuGet package (every platform in one archive), as tools/karaoke-pack/pins.env.
ARG ONNXRUNTIME_SHA256=6871ccbc97f3d5bdb4095a1dcec33aa78d56f6ed8de5980ef5fa1d9f2e7c679a

COPY tools/ocr-pip/requirements.txt /tmp/requirements.txt
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates wget libchromaprint-tools mkvtoolnix librubberband2 libvulkan1 mesa-vulkan-drivers \
    python3 python3-pip tesseract-ocr \
    tesseract-ocr-eng tesseract-ocr-fra tesseract-ocr-spa tesseract-ocr-deu tesseract-ocr-ita \
    tesseract-ocr-por tesseract-ocr-nld tesseract-ocr-swe tesseract-ocr-dan tesseract-ocr-nor \
    tesseract-ocr-fin tesseract-ocr-pol tesseract-ocr-ces tesseract-ocr-slk tesseract-ocr-hun \
    tesseract-ocr-ron tesseract-ocr-ell tesseract-ocr-rus tesseract-ocr-ukr tesseract-ocr-bul \
    tesseract-ocr-srp tesseract-ocr-hrv tesseract-ocr-tur tesseract-ocr-ara tesseract-ocr-heb \
    tesseract-ocr-fas tesseract-ocr-hin tesseract-ocr-tha tesseract-ocr-vie tesseract-ocr-ind \
    tesseract-ocr-jpn tesseract-ocr-kor tesseract-ocr-chi-sim \
  && python3 -m pip install --no-cache-dir --break-system-packages --no-deps --require-hashes -r /tmp/requirements.txt \
  && rm /tmp/requirements.txt \
  && if [ "$TARGETARCH" = "amd64" ]; then apt-get install -y --no-install-recommends intel-opencl-icd; fi \
  && wget -q -O /tmp/jellyfin-ffmpeg.deb "https://github.com/jellyfin/jellyfin-ffmpeg/releases/download/v${JELLYFIN_FFMPEG_VERSION}/jellyfin-ffmpeg8_${JELLYFIN_FFMPEG_VERSION}-noble_${TARGETARCH}.deb" \
  && if [ "$TARGETARCH" = "amd64" ]; then FF_SHA256="$JELLYFIN_FFMPEG_SHA256_AMD64"; else FF_SHA256="$JELLYFIN_FFMPEG_SHA256_ARM64"; fi \
  && echo "${FF_SHA256}  /tmp/jellyfin-ffmpeg.deb" | sha256sum -c - \
  && apt-get install -y --no-install-recommends /tmp/jellyfin-ffmpeg.deb \
  && ln -sf /usr/lib/jellyfin-ffmpeg/ffmpeg /usr/local/bin/ffmpeg \
  && ln -sf /usr/lib/jellyfin-ffmpeg/ffprobe /usr/local/bin/ffprobe \
  && rm -f /tmp/jellyfin-ffmpeg.deb \
  && if [ "$TARGETARCH" = "amd64" ]; then ORT_ARCH=x64; else ORT_ARCH=arm64; fi \
  && wget -q -O /tmp/ort.nupkg "https://api.nuget.org/v3-flatcontainer/microsoft.ml.onnxruntime/${ONNXRUNTIME_VERSION}/microsoft.ml.onnxruntime.${ONNXRUNTIME_VERSION}.nupkg" \
  && echo "${ONNXRUNTIME_SHA256}  /tmp/ort.nupkg" | sha256sum -c - \
  && python3 -m zipfile -e /tmp/ort.nupkg /tmp/ort \
  && cp /tmp/ort/runtimes/linux-${ORT_ARCH}/native/libonnxruntime*.so /usr/local/lib/ \
  && install -D -m 644 /tmp/ort/LICENSE /usr/share/doc/onnxruntime/LICENSE \
  && install -D -m 644 /tmp/ort/ThirdPartyNotices.txt /usr/share/doc/onnxruntime/ThirdPartyNotices.txt \
  && rm -rf /tmp/ort.nupkg /tmp/ort \
  && rm -f /usr/share/vulkan/icd.d/lvp_icd*.json \
  && apt-get purge -y wget python3-pip && apt-get autoremove -y \
  && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/target/release/fliks-engine /usr/local/bin/fliks-engine
COPY --from=vobsub-ocr-build /opt/subtile-ocr/bin/subtile-ocr /usr/local/bin/subtile-ocr
COPY --from=whisper-build /src/build/bin/whisper-cli /usr/local/bin/whisper-cli
COPY --from=whisper-build /licenses/whisper.cpp /usr/share/doc/whisper.cpp
COPY --from=pitch-ffmpeg-build /src/ffmpeg /usr/local/bin/ffmpeg-pitch
COPY --from=pitch-ffmpeg-build /licenses/ffmpeg-pitch /usr/share/doc/ffmpeg-pitch
ENV ORT_DYLIB_PATH=/usr/local/lib/libonnxruntime.so

# The container toolkit mounts libnvidia-opencl.so on NVENC hosts but not its ICD entry (harmless elsewhere).
RUN mkdir -p /etc/OpenCL/vendors && echo 'libnvidia-opencl.so.1' > /etc/OpenCL/vendors/nvidia.icd

# group=root + g=u: writable by an arbitrary uid with gid 0 (`--user <uid> --group-add 0`); mount a volume to keep it.
ENV FLIKS_TRANSCODE_DIR=/transcode
# Downloaded models live in `/data/models` (`/v1/models`); a volume on `/data` keeps them across container replacements.
ENV FLIKS_DATA_DIR=/data
RUN mkdir -p /transcode /data && chgrp 0 /transcode /data && chmod g=u /transcode /data

EXPOSE 7400 4848
ENTRYPOINT ["fliks-engine"]
CMD ["--listen", "0.0.0.0:7400"]

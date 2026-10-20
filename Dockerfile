FROM ubuntu:24.04 AS vobsub-ocr-build
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl build-essential pkg-config clang libclang-dev \
    libtesseract-dev libleptonica-dev \
  && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain 1.99.0 \
  && . "$HOME/.cargo/env" \
  && cargo install subtile-ocr --version 0.2.6 --locked --root /opt/subtile-ocr \
  && rm -rf /var/lib/apt/lists/* "$HOME/.cargo/registry" "$HOME/.rustup"

FROM rust:1.99.0-bookworm AS chef
RUN cargo install cargo-chef --version 0.1.78 --locked
WORKDIR /src

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
ARG CARGO_BUILD_JOBS
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json -p fliks-engine --bin fliks-engine
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN cargo build --release --locked -p fliks-engine --bin fliks-engine

FROM ubuntu:24.04

# Per-arch deb; Intel OpenCL (tonemap_opencl) is amd64-only, arm64 is CPU-only.
ARG TARGETARCH
ARG JELLYFIN_FFMPEG_VERSION=8.1.2-1

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates wget libchromaprint-tools mkvtoolnix \
    python3 python3-pip tesseract-ocr \
    tesseract-ocr-eng tesseract-ocr-fra tesseract-ocr-spa tesseract-ocr-deu tesseract-ocr-ita \
    tesseract-ocr-por tesseract-ocr-nld tesseract-ocr-swe tesseract-ocr-dan tesseract-ocr-nor \
    tesseract-ocr-fin tesseract-ocr-pol tesseract-ocr-ces tesseract-ocr-slk tesseract-ocr-hun \
    tesseract-ocr-ron tesseract-ocr-ell tesseract-ocr-rus tesseract-ocr-ukr tesseract-ocr-bul \
    tesseract-ocr-srp tesseract-ocr-hrv tesseract-ocr-tur tesseract-ocr-ara tesseract-ocr-heb \
    tesseract-ocr-fas tesseract-ocr-hin tesseract-ocr-tha tesseract-ocr-vie tesseract-ocr-ind \
    tesseract-ocr-jpn tesseract-ocr-kor tesseract-ocr-chi-sim \
  && python3 -m pip install --no-cache-dir --break-system-packages opencv-python-headless \
     appdirs babelfish cleanit click numpy pysrt pytesseract pyyaml setuptools trakit ffsubsync==0.5.1 \
  && python3 -m pip install --no-cache-dir --break-system-packages --no-deps pgsrip \
  && if [ "$TARGETARCH" = "amd64" ]; then apt-get install -y --no-install-recommends intel-opencl-icd; fi \
  && wget -q -O /tmp/jellyfin-ffmpeg.deb "https://github.com/jellyfin/jellyfin-ffmpeg/releases/download/v${JELLYFIN_FFMPEG_VERSION}/jellyfin-ffmpeg8_${JELLYFIN_FFMPEG_VERSION}-noble_${TARGETARCH}.deb" \
  && apt-get install -y --no-install-recommends /tmp/jellyfin-ffmpeg.deb \
  && ln -sf /usr/lib/jellyfin-ffmpeg/ffmpeg /usr/local/bin/ffmpeg \
  && ln -sf /usr/lib/jellyfin-ffmpeg/ffprobe /usr/local/bin/ffprobe \
  && rm -f /tmp/jellyfin-ffmpeg.deb \
  && apt-get purge -y wget python3-pip && apt-get autoremove -y \
  && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/target/release/fliks-engine /usr/local/bin/fliks-engine
COPY --from=vobsub-ocr-build /opt/subtile-ocr/bin/subtile-ocr /usr/local/bin/subtile-ocr

# The container toolkit mounts libnvidia-opencl.so on NVENC hosts but not its ICD entry (harmless elsewhere).
RUN mkdir -p /etc/OpenCL/vendors && echo 'libnvidia-opencl.so.1' > /etc/OpenCL/vendors/nvidia.icd

# group=root + g=u: writable by an arbitrary uid with gid 0 (`--user <uid> --group-add 0`); mount a volume to keep it.
ENV FLIKS_TRANSCODE_DIR=/transcode
RUN mkdir -p /transcode && chgrp 0 /transcode && chmod g=u /transcode

EXPOSE 7400 4848
ENTRYPOINT ["fliks-engine"]
CMD ["--listen", "0.0.0.0:7400"]

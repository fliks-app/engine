FROM rust:1-bookworm AS build
ARG CARGO_BUILD_JOBS
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN cargo build --release --locked -p fliks-engine --bin fliks-engine

FROM ubuntu:24.04

# Per-arch deb; Intel OpenCL (tonemap_opencl) is amd64-only, arm64 is CPU-only.
ARG TARGETARCH
ARG JELLYFIN_FFMPEG_VERSION=8.1.2-1

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates wget libchromaprint-tools mkvtoolnix \
  && if [ "$TARGETARCH" = "amd64" ]; then apt-get install -y --no-install-recommends intel-opencl-icd; fi \
  && wget -q -O /tmp/jellyfin-ffmpeg.deb "https://github.com/jellyfin/jellyfin-ffmpeg/releases/download/v${JELLYFIN_FFMPEG_VERSION}/jellyfin-ffmpeg8_${JELLYFIN_FFMPEG_VERSION}-noble_${TARGETARCH}.deb" \
  && apt-get install -y --no-install-recommends /tmp/jellyfin-ffmpeg.deb \
  && ln -sf /usr/lib/jellyfin-ffmpeg/ffmpeg /usr/local/bin/ffmpeg \
  && ln -sf /usr/lib/jellyfin-ffmpeg/ffprobe /usr/local/bin/ffprobe \
  && rm -f /tmp/jellyfin-ffmpeg.deb \
  && apt-get purge -y wget && apt-get autoremove -y \
  && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/target/release/fliks-engine /usr/local/bin/fliks-engine

# The container toolkit mounts libnvidia-opencl.so on NVENC hosts but not its ICD entry (harmless elsewhere).
RUN mkdir -p /etc/OpenCL/vendors && echo 'libnvidia-opencl.so.1' > /etc/OpenCL/vendors/nvidia.icd

# group=root + g=u: writable by an arbitrary uid with gid 0 (`--user <uid> --group-add 0`); mount a volume to keep it.
ENV FLIKS_TRANSCODE_DIR=/transcode
RUN mkdir -p /transcode && chgrp 0 /transcode && chmod g=u /transcode

EXPOSE 7400 4848
ENTRYPOINT ["fliks-engine"]
CMD ["--listen", "0.0.0.0:7400"]

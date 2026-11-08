FROM ubuntu:24.04 AS build
ARG WHISPER_CPP_VERSION=v1.9.5
ARG WHISPER_CPP_SHA256=ff1a9053feb509ff9d7729703355541ae9690073a6b1c40eb692c962e0dc1720
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl build-essential cmake libvulkan-dev glslc spirv-headers
RUN curl -fsSL -o /tmp/w.tgz "https://github.com/ggml-org/whisper.cpp/archive/refs/tags/${WHISPER_CPP_VERSION}.tar.gz" \
 && echo "${WHISPER_CPP_SHA256}  /tmp/w.tgz" | sha256sum -c - && mkdir /src && tar -xzf /tmp/w.tgz -C /src --strip-components=1
WORKDIR /src
ARG ISA="-DGGML_AVX=ON -DGGML_AVX2=ON -DGGML_FMA=ON -DGGML_F16C=ON"
RUN cmake -B bcpu -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF -DGGML_NATIVE=OFF -DGGML_OPENMP=OFF -DWHISPER_BUILD_TESTS=OFF -DWHISPER_BUILD_EXAMPLES=ON $ISA && cmake --build bcpu -j"$(nproc)" --target whisper-cli
RUN cmake -B bvk -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF -DGGML_NATIVE=OFF -DGGML_OPENMP=OFF -DGGML_VULKAN=ON -DWHISPER_BUILD_TESTS=OFF -DWHISPER_BUILD_EXAMPLES=ON $ISA && cmake --build bvk -j"$(nproc)" --target whisper-cli
FROM ubuntu:24.04
RUN apt-get update && apt-get install -y --no-install-recommends libvulkan1 mesa-vulkan-drivers vulkan-tools && rm -rf /var/lib/apt/lists/* && rm -f /usr/share/vulkan/icd.d/lvp_icd*.json
COPY --from=build /src/bcpu/bin/whisper-cli /opt/whisper-cpu
COPY --from=build /src/bvk/bin/whisper-cli /opt/whisper-vk

# align-check

Measures what the engine's `Align` produces against word times known to the millisecond, for
any `whisper-cli` build (CPU, Vulkan, a new release or model).

1. `gen.py` builds a clip from generated sentences: one TTS call per word (espeak-ng or piper),
   trimmed and placed on a timeline with fixed gaps, so every word's start and end are exact.
   Kinds: `clean`, `music` (a synthetic pad and drums 8 dB under the voice), `fast` (quicker
   speech, 30 ms gaps). It writes `<name>.wav` (16 kHz mono) and `<name>.json` (lyric lines and truth).
2. `crates/engine/examples/align-check.rs` runs `whisper-cli` with the engine's own arguments
   (`karaoke::whisper_argv`, the lyrics prompt included), feeds what it heard to the engine's
   `align` (order-preserving matching) and prints one JSON line: error of every word start against
   the truth, line start error, unmatched and misplaced words, wall time. Lines carry no times.
3. `run.sh` runs every clip x model x backend x run into a JSONL file. A backend is an executable
   taking `whisper-cli`'s arguments (a wrapper that picks the build, the GPU, or adds `-nfa`).
4. `report.py` prints the tables: by clip kind, language, voice, and each GPU backend against the
   CPU's own spread (flash attention on and off, run to run).

Clips of the recorded run: 7 voices (piper en_US lessac and ryan, fr_FR siwis and tom; espeak-ng
en-us+m3, en-us+f4, fr+f2) x `clean`, `music`, `fast` = 21 clips of 24 to 43 s, 49 to 72 words each.

`whisper.Dockerfile` builds the pinned whisper.cpp twice (`/opt/whisper-cpu`, `/opt/whisper-vk`) in an
Ubuntu 24.04 image with Mesa's Vulkan drivers (the llvmpipe device removed). Backends used for
the recorded run, one wrapper each (`-nfa` and the GPU device are the only differences):

```sh
#!/bin/sh   # vk-arc; vk-igpu is renderD128, cpu-fa runs /opt/whisper-cpu, cpu-nfa adds -nfa
exec docker run --rm -u "$(id -u):$(id -g)" -v /work:/work --device /dev/dri/renderD129 \
  --group-add "$(stat -c %g /dev/dri/renderD129)" -e MESA_SHADER_CACHE_DIR=/work/shadercache -e HOME=/tmp \
  align-check-whisper /opt/whisper-vk "$@"
```

```sh
cargo build --release -p fliks-engine --example align-check
RUNS=2 ALIGN_CHECK=target/release/examples/align-check tools/align-check/run.sh clips out.jsonl backends \
  base=/models/base.bin small=/models/small.bin
python3 tools/align-check/report.py out.jsonl
```

#!/bin/sh
# Every clip of <clips-dir> x model x backend x run, one JSON line each into <out.jsonl>.
#   run.sh <clips-dir> <out.jsonl> <backends-dir> <model> [<model>...]   (model = name=path/to/model.bin)
# A backend is an executable in <backends-dir> that takes whisper-cli's arguments (a wrapper that
# picks the build, the GPU or `-nfa`); the file name is its label. ALIGN_CHECK=<example binary>, RUNS=2.
set -eu
clips=$1 out=$2 backends=$3; shift 3
: > "$out"
for json in "$clips"/*.json; do
  clip=$(basename "$json" .json)
  lang=$(sed -n 's/.*"lang": "\(..\)".*/\1/p' "$json" | head -n 1)
  for m in "$@"; do
    for b in "$backends"/*; do
      for run in $(seq "${RUNS:-2}"); do
        res=$(WHISPER_CLI_PATH=$b "$ALIGN_CHECK" "${m#*=}" "$clips/$clip.wav" "$json" "$lang") || continue
        echo "$res" | sed "s/^{/{\"clip\":\"$clip\",\"model\":\"${m%%=*}\",\"backend\":\"$(basename "$b")\",\"run\":$run,/" >> "$out"
      done
    done
  done
done

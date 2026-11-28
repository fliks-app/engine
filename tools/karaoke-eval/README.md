# karaoke-eval

Measures the engine's own `Align`, `Melody` and `Separate` jobs against ground truth that exists offline. `eval.py` submits real jobs to a running engine over `/v1` and scores the results; it re-implements none of the algorithms. Other tools: `tools/align-check` (synthetic speech, no engine) and `tools/ctc-export/eval.py` (a Python copy of the aligner, proxy metrics).

Python 3 and numpy. Optional: `mir_eval` (melody), `museval` and `scipy` (separation); without them the same melody definitions run in numpy and separation reports a plain SDR. `ffmpeg` is needed by `separate`.

```sh
uv run --with numpy python tools/karaoke-eval/eval.py align ~/songs --engine http://127.0.0.1:3000 --token-file ~/.engine-token
uv run --with numpy --with mir_eval --with museval --with scipy python tools/karaoke-eval/eval.py melody ~/songs
python3 tools/karaoke-eval/test_ultrastar.py      # parser self-check (also `python3 -m pytest`)
```

## Running an engine for it

```sh
FLIKS_ENGINE_TOKEN=$(openssl rand -hex 16) FLIKS_DATA_DIR=/data ORT_DYLIB_PATH=... WHISPER_CLI_PATH=... \
  fliks-engine --listen 127.0.0.1:3000
```

The control plane (`/v1/*`) takes `Authorization: Bearer $FLIKS_ENGINE_TOKEN` (PROTOCOL.md "Transport"); without the variable the engine writes a token to `<FLIKS_CONF_DIR>/.engine-token` (default `/app/conf`). The tool reads `--token`, `--token-file` or `FLIKS_ENGINE_TOKEN`, and the URL from `--engine` or `FLIKS_ENGINE_URL`. The engine needs the ONNX runtime and the models: install what is measured with `POST /v1/models/{id}/download` (`GET /v1/models` lists them): `swiftf0`, `spleeter-2stems`, `whisper-small-q5_1` and the CTC models (`wav2vec2-ctc-fr`, `wav2vec2-ctc-en`, `omniasr-ctc-300m`, and `w2v-bert-music-ctc` which only runs when named with `--model`).

Every path in a job request is absolute and read (or written, for `separate`) by the engine, so the folders given to the tool must have the same absolute path on the engine's host. Behind a container or a path mapping (`pathMappings` of `PUT /v1/config`), mount the data at the same path, or run the tool where the paths are valid. A job is submitted with `force`, so a repeated run measures the compute again.

## Data

- UltraStar songs: a folder of `*.txt` UltraStar files of songs you own, each with its audio (`#MP3` or `#AUDIO`) and ideally a vocal stem (`#VOCALS`). No stem: `align` runs on the mix alone and `melody` skips the song. A stem from `separate` or the song's own multitrack works.
- JamendoLyrics MultiLang (word-level lyric alignment for 79 songs in 4 languages, CC licences): <https://github.com/f90/jamendolyrics>. Not UltraStar: convert its word annotations to an UltraStar `.txt` (beat = ms, `#BPM:15000`, `#GAP:0`) to feed `align`.
- MUSDB18-HQ (150 tracks, `mixture.wav`, `vocals.wav`, `drums.wav`, `bass.wav`, `other.wav` per track folder): <https://zenodo.org/records/3338373>, for non-commercial research.

## Commands

`eval.py <command> <folder> [options]`. One JSON line per song, then a markdown summary table; `--out results.json` keeps `{summary, rows}`; `--limit N` takes the first N.

### align

Per song, `Align` on the vocal stem (`--inputs stem`) and on the mix with the stem as `voicing` (`mix`), untimed and with the UltraStar line start times (`--timing untimed,timed`), once per `--model` (repeatable; absent: the engine's default pick). `language` comes from `--language` or the `#LANGUAGE` header.

- Truth: words are the syllables joined until a syllable text begins with a space, ends with a space, or a line starts; `~` only extends the word. A word starts at its first syllable, a line at its first word. Beat time is `GAP + beat * 60000 / (BPM * 4)` ms; `#RELATIVE:YES` offsets and the first singer of a duet are handled; freestyle and rap syllables count as words.
- Request lines are the words of each line joined by a space, so engine words match truth words one to one (a line where the counts differ is not scored).
- Word onset error = engine word `startMs` minus truth word start, over the first copy of each line. `medianAbsMs`, `meanAbsMs`, `medianSignedMs` (the model's onset bias, which a CTC export's `onsetMs` should cancel), `within100` and `within300` (share of words with absolute error at most 100 and 300 ms).
- Line start: `lineMeanAbsMs` is the mean absolute difference of the engine line `startMs` and the truth line start (MIREX lyrics alignment style), `lineUnder300` the share under 300 ms.
- `matched`: the result's `matched`; the summary gives its mean and 5th percentile.
- The summary pools the words of all songs per input, timing and model.

### negative

The same audio (stem, else mix) aligned with the lyrics of another song (the next one in the folder, cyclic, untimed), per model. `matched` of a wrong lyric is what the host's rejection threshold has to stay above: compare its 95th percentile and maximum with the 5th percentile of `matched` in the `align` table, per model. Needs at least 2 songs.

### melody

Request spans are the pitched syllables (`:` and `*`) of each song, on the `#VOCALS` stem.

- `within1Semitone`: share of syllables whose octave-folded distance (`d = |truth - midi| mod 12`, `min(d, 12 - d)`) between the truth pitch (`pitch + 60`) and the engine note covering the syllable midpoint is at most 1 semitone (the client's score). A syllable with no covering note counts as a miss; `covered` is the share with a note.
- On a 10 ms grid over the truth syllables: `rawPitch` and `rawChroma` (estimate within 50 cents of the truth, chroma octave-folded, over the truth voiced frames), `voicingRecall` (truth voiced frames the engine calls voiced), as in `mir_eval.melody`. `impl` says `mir_eval` or `numpy`.
- `voicingFalseAlarm` (engine voiced in truth unvoiced frames) needs frames where the truth is silent: `--gaps` also sends the silences of at least 50 ms between syllables as spans. Without it the field is empty. The extra spans enter the engine's register estimate, so `--gaps` runs are not comparable with runs without.
- `analysis` is the result's `{tuningCents, lowMidi, highMidi, key, tempo}`, shown beside `truthRange`, the 5th and 95th percentile of truth pitches weighted by duration.

### separate

For each `mixture.wav` under the folder: `Separate` (`--model`, `--bitrate`) writing vocals and instrumental Opus files under `--work` (an absolute directory the engine can write; removed after scoring unless `--keep`), decoded with ffmpeg at 44.1 kHz stereo. Truth: `vocals.wav` and accompaniment = mixture minus vocals.

- With `museval`: `museval.evaluate` on 1 s windows, SDR, SIR and SAR per target, the median over windows (NaN ignored) per track, then the median over tracks in the summary, as SiSEC does.
- Without it: SDR only, 10 log10(energy of truth / energy of error) per 1 s window, median over windows.
- `computeSPerMin`: the job's run time (`endedMs - startedMs`) per minute of audio. `provider` is the result's.

## Keeping the numbers

Record a result next to the manifest entry or constant it justifies (the model's comment in the manifest, the threshold's doc comment), with the corpus, the engine version and the command. A change of model, of an `onsetMs`, of a matching threshold or of an aligner or melody constant is gated on this tool: run the same corpus before and after, and land the change only when the table does not regress. `results.json` files are not committed.

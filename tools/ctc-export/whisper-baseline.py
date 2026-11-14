#!/usr/bin/env python3
"""Today's aligner as a baseline for eval.py: whisper-cli with the engine's arguments on a vocal stem,
then each lyric word is paired with the next heard word of similar spelling inside its line window
(order-preserving, like the engine's `align`, but a plainer match). Writes <track>.whisper.json.

  whisper-baseline.py <whisper-cli> <model.bin> <tracks-dir> [track ...]
"""
import difflib, json, re, subprocess, sys, tempfile, unicodedata
from pathlib import Path

PROMPT_CHARS, MARGIN = 400, 1500


def norm(w):
    return re.sub(r"[^\w']", "", unicodedata.normalize("NFC", w).lower())


def main(cli, model, tracks, *only):
    tracks = Path(tracks)
    for wav in sorted(tracks.glob("*.wav")):
        if only and wav.stem not in only or wav.stem.startswith("tts-"):
            continue
        spec = json.load(open(wav.with_suffix(".json")))
        lines = [l for l in spec["lines"] if l["text"].strip()]
        prompt = ""
        for w in " ".join(l["text"] for l in lines).split():
            if len(prompt) + len(w) + 1 > PROMPT_CHARS:
                break
            prompt += (" " if prompt else "") + w
        with tempfile.TemporaryDirectory() as d:
            subprocess.run([cli, "-m", model, "-f", str(wav), "-l", spec["lang"], "-ml", "1", "-sow", "-oj", "-of", f"{d}/o",
                            "-np", "-t", "8", "-ng", "--prompt", prompt], check=True, capture_output=True)
            heard = [(norm(t["text"]), t["offsets"]["from"], t["offsets"]["to"]) for t in json.load(open(f"{d}/o.json"))["transcription"]]
        heard = [h for h in heard if h[0]]
        cur, out = 0, []
        for k, ln in enumerate(lines):
            nxt = lines[k + 1]["start"] if k + 1 < len(lines) else ln["start"] + 8000
            cues = []
            while cur < len(heard) and heard[cur][1] < ln["start"] - MARGIN:
                cur += 1
            for w in map(norm, ln["text"].split()):
                for i in range(cur, min(cur + 5, len(heard))):
                    h = heard[i]
                    if ln["start"] - MARGIN <= h[1] <= nxt + MARGIN and w and difflib.SequenceMatcher(None, w, h[0]).ratio() >= 0.6:
                        cues.append({"start": h[1], "end": h[2]})
                        cur = i + 1
                        break
            out.append({"text": ln["text"], "cues": cues})
        json.dump({"lines": out}, open(tracks / f"{wav.stem}.whisper.json", "w"), ensure_ascii=False)
        print(wav.stem, len(heard), "heard,", sum(len(l["cues"]) for l in out), "paired", flush=True)


if __name__ == "__main__":
    main(*sys.argv[1:]) if len(sys.argv) >= 4 else sys.exit(__doc__)

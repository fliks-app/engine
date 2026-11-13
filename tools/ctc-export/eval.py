#!/usr/bin/env python3
"""Aligns known lyrics with every exported CTC model and prints proxy metrics (no hand-synced truth).

  eval.py <models-dir> <tracks-dir> [fp32,fp16,int8] [out.jsonl]

<models-dir>/<lang>-<name>/ holds export.py's output; a track's lyrics language picks the models.
<tracks-dir>/<track>.wav is the 16 kHz mono vocal stem (prep.py) and <track>.json holds
{"lang", "lines": [{"start": ms, "text"}]} (provider line times). An optional <track>.whisper.json
(a lyrics document with word cues) is scored the same way as the baseline.

Per model and variant: starts in silence (mean stem energy over the 50 ms after the word start is
under -35 dB of the stem's 95th percentile), words under 60 ms, lines whose first word sits on the window start (the aligner gave up), line starts against the provider
times (median, 90th percentile, share off by more than 1 s), word-start drift from the fp32 variant
(median, 95th percentile), wall time as a multiple of the audio length.
Windows: [line start - 1 s, next start + 0.5 s], clamped after the previous line's last word.
"""
import json, re, statistics as st, sys, time, unicodedata
from pathlib import Path

import numpy as np, onnxruntime as ort, soundfile as sf

SR, FRAME = 16000, 320  # samples per 20 ms frame


def tokens(text, vocab, delim):
    """Per word, its vocabulary ids; a word with none left is dropped."""
    lower = any(c.islower() for c in vocab.values() if len(c) == 1)
    ids = {c: i for i, c in vocab.items()}
    words = []
    for w in unicodedata.normalize("NFC", text).split():
        w = re.sub("[‘’`´]", "'", w)
        w = w.lower() if lower else w.upper()
        t = [ids[c] for c in w if c in ids and c != delim]
        if t:
            words.append(t)
    return words


def viterbi(lp, labels):
    """Frames each label state holds on the best CTC path; blank is label 0 of `ext`."""
    ext = [0]
    for t in labels:
        ext += [t, 0]
    S, T = len(ext), lp.shape[0]
    ext = np.array(ext)
    skip = np.zeros(S, bool)
    skip[3:] = (ext[3:] != 0) & (ext[3:] != ext[1:-2])
    em = lp[:, ext]
    score = np.full(S, -np.inf)
    score[:2] = em[0, :2]
    back = np.zeros((T, S), np.uint8)
    for t in range(1, T):
        cand = np.stack([score, np.r_[-np.inf, score[:-1]], np.where(skip, np.r_[-np.inf, -np.inf, score[:-2]], -np.inf)])
        back[t] = cand.argmax(0)
        score = cand.max(0) + em[t]
    s = S - 1 if score[S - 1] >= score[S - 2] else S - 2
    path = np.zeros(T, int)
    for t in range(T - 1, -1, -1):
        path[t] = s
        s -= back[t, s]
    return path, em


def align_track(sess, meta, audio, lines):
    """[(line index, [(start ms, end ms, score)], window start ms)] for the lines that kept a word."""
    vocab = {int(i): c for i, c in meta["vocab"].items()}
    delim = vocab.get(meta["delimiter"])
    out, floor = [], 0
    for k, ln in enumerate(lines):
        nxt = lines[k + 1]["start"] if k + 1 < len(lines) else ln["start"] + 8000
        a = max(ln["start"] - 1000, floor)
        b = min(nxt + 500, ln["start"] + (nxt - ln["start"]) + 3000)
        words = tokens(ln["text"], vocab, delim)
        if not words or b <= a:
            continue
        x = audio[a * SR // 1000:b * SR // 1000].astype(np.float32)
        if len(x) < 3200:
            continue
        if meta["doNormalize"]:
            x = (x - x.mean()) / np.sqrt(x.var() + 1e-7)
        lg = sess.run(None, {"input_values": x[None]})[0][0]
        lp = lg - np.logaddexp.reduce(lg, axis=1, keepdims=True)
        labels = [t for w in words for t in w]
        if len(labels) > lp.shape[0]:
            continue
        path, em = viterbi(lp, labels)
        res = []
        for i, w in enumerate(words):
            first = sum(len(v) for v in words[:i])
            frames = [f for f in range(len(path)) if path[f] % 2 == 1 and first <= path[f] // 2 < first + len(w)]
            if not frames:
                continue
            res.append((a + frames[0] * 20, a + (frames[-1] + 1) * 20, float(np.mean([em[f, path[f]] for f in frames]))))
        if res:
            out.append((k, res, a))
            floor = res[-1][1]
    return out


def energy(audio):
    n = len(audio) // 320
    rms = np.sqrt((audio[:n * 320].reshape(n, 320) ** 2).mean(1))
    return rms, np.percentile(rms, 95)


def metrics(lines_words, lines, rms, p95, ref=None):
    """Raw counts and deltas of one track; `summary` pools them."""
    ws = [w for _, ws, _ in lines_words for w in ws]
    sil = sum(rms[w[0] // 20:w[0] // 20 + 3].mean() < p95 * 10 ** (-35 / 20) for w in ws)
    m = dict(words=len(ws), silent=int(sil), short=sum(w[1] - w[0] < 60 for w in ws), lines=len(lines_words),
             stuck=sum(ws_[0][0] - a < 60 for _, ws_, a in lines_words if a is not None),
             line_d=[abs(ws_[0][0] - lines[k]["start"]) for k, ws_, _ in lines_words])
    if ref is not None:
        r = dict(((k, i), w[0]) for k, ws_, _ in ref for i, w in enumerate(ws_))
        m["drift"] = [abs(w[0] - r[(k, i)]) for k, ws_, _ in lines_words for i, w in enumerate(ws_) if (k, i) in r]
    return m


def summary(rows):
    g = {}
    for r in rows:
        g.setdefault((r["lang"], r["model"], r["variant"]), []).append(r)
    print("| lang | model | variant | tracks | words | in silence | <60 ms | line starts at window start | line start med / p90 ms | >1 s | drift fp32 med / p95 ms | wall x audio |")
    print("|---|---|---|---|---|---|---|---|---|---|---|---|")
    for (lang, model, v), rs in g.items():
        w = sum(r["words"] for r in rs)
        d = [x for r in rs for x in r["line_d"]]
        dr = [x for r in rs for x in r.get("drift", [])]
        drift = f"{st.median(dr):.0f} / {np.percentile(dr, 95):.0f}" if dr else "-"
        wall = f"{st.mean(r['wall_x'] for r in rs):.2f}" if "wall_x" in rs[0] else "-"
        print(f"| {lang} | {model} | {v} | {len(rs)} | {w} | {100 * sum(r['silent'] for r in rs) / w:.1f} % | "
              f"{sum(r['short'] for r in rs)} | {100 * sum(r['stuck'] for r in rs) / sum(r['lines'] for r in rs):.0f} % | {st.median(d):.0f} / {np.percentile(d, 90):.0f} | "
              f"{100 * sum(x > 1000 for x in d) / len(d):.0f} % | {drift} | {wall} |")


def whisper_words(doc, lines):
    """Baseline in align_track's shape: each document line is matched to the next provider line of the same text."""
    out, k = [], 0
    for ln in doc["lines"]:
        j = next((j for j in range(k, len(lines)) if lines[j]["text"] == ln["text"]), None)
        if j is not None and ln.get("cues"):
            out.append((j, [(c["start"], c["end"], 0.0) for c in ln["cues"]], None))
            k = j + 1
    return out


def main(models, tracks, variants="fp32,fp16,int8", out=None):
    models, tracks = Path(models), Path(tracks)
    rows = []
    for wav in sorted(tracks.glob("*.wav")):
        name = wav.stem
        spec = json.load(open(tracks / f"{name}.json"))
        lines = [l for l in spec["lines"] if l["text"].strip()]
        audio, sr = sf.read(wav, dtype="float32")
        assert sr == SR
        rms, p95 = energy(audio)
        if (tracks / f"{name}.whisper.json").exists():
            doc = json.load(open(tracks / f"{name}.whisper.json"))
            ww = whisper_words(doc, lines)
            rows.append(dict(track=name, lang=spec["lang"], model="whisper", variant="today",
                             **metrics(ww, lines, rms, p95)))
            print(json.dumps(rows[-1]), flush=True)
        for mdir in sorted(models.glob(f"{spec['lang']}-*")):
            meta = json.load(open(mdir / "vocab.json"))
            ref = None
            for v in variants.split(","):
                sess = ort.InferenceSession(str(mdir / f"model.{v}.onnx"), providers=["CPUExecutionProvider"],
                                            sess_options=_opts())
                t0 = time.time()
                res = align_track(sess, meta, audio, lines)
                wall = (time.time() - t0) / (len(audio) / SR)
                ref = res if v == "fp32" else ref
                rows.append(dict(track=name, lang=spec["lang"], model=mdir.name, variant=v, wall_x=wall,
                                 **metrics(res, lines, rms, p95, None if v == "fp32" else ref)))
                print(json.dumps(rows[-1]), flush=True)
    summary(rows)
    if out:
        Path(out).write_text("".join(json.dumps(r) + "\n" for r in rows))


def _opts():
    o = ort.SessionOptions()
    o.intra_op_num_threads = 4
    return o


if __name__ == "__main__":
    main(*sys.argv[1:5]) if len(sys.argv) >= 3 else sys.exit(__doc__)

#!/usr/bin/env python3
"""Aligns known lyrics with every exported CTC model and prints proxy metrics (no hand-synced truth).

  eval.py <models-dir> <tracks-dir> [fp32,fp16,int8] [out.jsonl] [ctx ms list] [star list]
  e.g.    eval.py out .truth fp16 res.jsonl 1000,2000 none,0,-1,-2.3

<models-dir>/<lang>-<name>/ holds export.py's output (`xx-<name>` serves every language); a track's
lyrics language picks the models. <tracks-dir>/<track>.wav is the 16 kHz mono vocal stem (prep.py)
and <track>.json holds {"lang", "lines": [{"start": ms, "text"}]} (provider line times). An optional
<track>.whisper.json (a lyrics document with word cues) is scored the same way as the baseline.

Windows: [line start - ctx, next start + ctx / 2], clamped after the previous line's last word and to
the line length + 3 s. `star` is the per-frame log-probability of an extra "anything" state allowed
before and after the line (none = no such state; 0 absorbs freely, -2.3 is about p = 0.1).

Per model, variant and config: starts in silence (mean stem energy over the 50 ms after the word
start is under -35 dB of the stem's 95th percentile), words under 60 ms, lines whose first word sits
on the window start when the previous line did not clamp it (the aligner gave up), line starts against the provider times (median, 90th
percentile, share off by more than 1 s), word-start drift from the fp32 variant (median, 95th
percentile), wall time as a multiple of the audio length (network + search of that config).
"""
import json, re, statistics as st, sys, time, unicodedata
from pathlib import Path

import numpy as np, onnxruntime as ort, soundfile as sf

SR = 16000
MAX_WINDOW_MS = 35000  # the omnilingual model takes under 40 s


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


def viterbi(lp, labels, star):
    """Token index of every frame on the best CTC path (-1 on blank or star); blank is id 0."""
    ext = [0]
    for t in labels:
        ext += [t, 0]
    if star is not None:
        ext = [-1] + ext + [-1]
    e = np.array(ext)
    S, T = len(e), lp.shape[0]
    em = lp[:, np.maximum(e, 0)]
    if star is not None:
        em[:, e < 0] = star
    skip = np.zeros(S, bool)
    skip[2:] = (e[2:] > 0) & (e[2:] != e[:-2])
    if star is not None:
        skip[S - 1] = True
    score = np.full(S, -np.inf)
    score[:3 if star is not None else 2] = em[0, :3 if star is not None else 2]
    back = np.zeros((T, S), np.uint8)
    for t in range(1, T):
        cand = np.stack([score, np.r_[-np.inf, score[:-1]], np.where(skip, np.r_[-np.inf, -np.inf, score[:-2]], -np.inf)])
        back[t] = cand.argmax(0)
        score = cand.max(0) + em[t]
    ends = [S - 1, S - 2] + ([S - 3] if star is not None else [])
    s = max(ends, key=lambda i: score[i])
    path = np.zeros(T, int)
    for t in range(T - 1, -1, -1):
        path[t] = s
        s -= back[t, s]
    off = 1 if star is not None else 0
    j = path - off
    tok = np.where((e[path] > 0), (j - 1) // 2, -1)
    return tok, np.take_along_axis(em, path[:, None], 1)[:, 0]


class Net:
    """One session; log-probabilities of a window, cached for the current line only."""

    def __init__(self, path, meta):
        o = ort.SessionOptions()
        o.intra_op_num_threads = 4
        self.sess, self.meta, self.audio, self.cache = ort.InferenceSession(str(path), providers=["CPUExecutionProvider"], sess_options=o), meta, None, {}

    def lp(self, a, b):
        if (a, b) not in self.cache:
            t0 = time.time()
            x = self.audio[a * SR // 1000:b * SR // 1000].astype(np.float32)
            if len(x) < 3200:
                return None, 0.0
            if self.meta["doNormalize"]:
                x = (x - x.mean()) / np.sqrt(x.var() + 1e-7)
            lg = self.sess.run(None, {"input_values": x[None]})[0][0]
            self.cache[(a, b)] = (lg - np.logaddexp.reduce(lg, axis=1, keepdims=True), time.time() - t0)
        return self.cache[(a, b)]


def align_track(net, lines, ctx, stars):
    """{star: ([(line index, [(start ms, end ms, score)], window start ms)], seconds)} for the lines that kept a word."""
    vocab = {int(i): c for i, c in net.meta["vocab"].items()}
    delim = vocab.get(net.meta["delimiter"])
    assert net.meta["blank"] == 0
    out = {s: ([], 0.0) for s in stars}
    floor = {s: 0 for s in stars}
    for k, ln in enumerate(lines):
        net.cache = {}
        nxt = lines[k + 1]["start"] if k + 1 < len(lines) else ln["start"] + 8000
        words = tokens(ln["text"], vocab, delim)
        labels = [t for w in words for t in w]
        for s in stars:
            a = max(ln["start"] - ctx, floor[s])
            b = min(nxt + ctx // 2, nxt + 3000)
            if not words or b - a < 200 or b - a > MAX_WINDOW_MS:
                continue
            lp, secs = net.lp(a, b)
            if lp is None or len(labels) > lp.shape[0]:
                continue
            t0 = time.time()
            tok, em = viterbi(lp, labels, s)
            res, first = [], 0
            for w in words:
                f = np.nonzero((tok >= first) & (tok < first + len(w)))[0]
                first += len(w)
                if len(f):
                    res.append((a + int(f[0]) * 20, a + (int(f[-1]) + 1) * 20, float(em[f].mean())))
            if res:
                out[s][0].append((k, res, a if a == ln["start"] - ctx else None))
                floor[s] = res[-1][1]
            out[s] = (out[s][0], out[s][1] + secs + time.time() - t0)
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
             free=sum(a is not None for _, _, a in lines_words),
             stuck=sum(ws_[0][0] - a < 60 for _, ws_, a in lines_words if a is not None),
             line_d=[abs(ws_[0][0] - lines[k]["start"]) for k, ws_, _ in lines_words])
    if ref is not None:
        r = dict(((k, i), w[0]) for k, ws_, _ in ref for i, w in enumerate(ws_))
        m["drift"] = [abs(w[0] - r[(k, i)]) for k, ws_, _ in lines_words for i, w in enumerate(ws_) if (k, i) in r]
    return m


def stuck(rs):
    free = sum(r["free"] for r in rs)
    return f"{100 * sum(r['stuck'] for r in rs) / free:.0f} %" if free else "-"


def summary(rows):
    g = {}
    for r in rows:
        g.setdefault((r["lang"], r["model"], r["variant"], r["ctx"], r["star"]), []).append(r)
    print("| lang | model | variant | ctx ms | star | tracks | words | in silence | <60 ms | on window start (free windows) | line start med / p90 ms | >1 s | drift fp32 med / p95 ms | wall x audio |")
    print("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|")
    for (lang, model, v, ctx, star), rs in g.items():
        w = sum(r["words"] for r in rs)
        d = [x for r in rs for x in r["line_d"]]
        dr = [x for r in rs for x in r.get("drift", [])]
        drift = f"{st.median(dr):.0f} / {np.percentile(dr, 95):.0f}" if dr else "-"
        wall = f"{st.mean(r['wall_x'] for r in rs):.2f}" if "wall_x" in rs[0] else "-"
        print(f"| {lang} | {model} | {v} | {ctx} | {star} | {len(rs)} | {w} | {100 * sum(r['silent'] for r in rs) / w:.1f} % | "
              f"{sum(r['short'] for r in rs)} | {stuck(rs)} | "
              f"{st.median(d):.0f} / {np.percentile(d, 90):.0f} | {100 * sum(x > 1000 for x in d) / len(d):.0f} % | {drift} | {wall} |")


def whisper_words(doc, lines):
    """Baseline in align_track's shape: each document line is matched to the next provider line of the same text."""
    out, k = [], 0
    for ln in doc["lines"]:
        j = next((j for j in range(k, len(lines)) if lines[j]["text"] == ln["text"]), None)
        if j is not None and ln.get("cues"):
            out.append((j, [(c["start"], c["end"], 0.0) for c in ln["cues"]], None))
            k = j + 1
    return out


def main(models, tracks, variants="fp32,fp16,int8", out=None, ctxs="1000", stars="none"):
    models, tracks = Path(models), Path(tracks)
    ctxs = [int(c) for c in ctxs.split(",")]
    stars = [None if s == "none" else float(s) for s in stars.split(",")]
    data = {}
    for wav in sorted(tracks.glob("*.wav")):
        spec = json.load(open(wav.with_suffix(".json")))
        audio, sr = sf.read(wav, dtype="float32")
        assert sr == SR
        data[wav.stem] = (spec["lang"], [l for l in spec["lines"] if l["text"].strip()], audio, *energy(audio))
    rows = []

    def add(name, model, variant, ctx, star, res, secs, ref):
        lang, lines, audio, rms, p95 = data[name]
        rows.append(dict(track=name, lang=lang, model=model, variant=variant, ctx=ctx, star=star, wall_x=secs / (len(audio) / SR),
                         **metrics(res, lines, rms, p95, ref)))
        print(json.dumps(rows[-1]), flush=True)

    for name, (lang, lines, audio, rms, p95) in data.items():
        w = tracks / f"{name}.whisper.json"
        if w.exists():
            add(name, "whisper", "today", "-", "-", whisper_words(json.load(open(w)), lines), 0, None)
            rows[-1].pop("wall_x")
    for mdir in sorted(models.iterdir()):
        meta = json.load(open(mdir / "vocab.json"))
        mlang = mdir.name.split("-")[0]
        mine = [n for n, d in data.items() if mlang in (d[0], "xx")]
        refs = {}
        for v in variants.split(","):
            if not mine or not (mdir / f"model.{v}.onnx").exists():
                continue
            net = Net(mdir / f"model.{v}.onnx", meta)
            for name in mine:
                net.audio = data[name][2]
                for ctx in ctxs:
                    for s, (r, secs) in align_track(net, data[name][1], ctx, stars).items():
                        if v == "fp32":
                            refs[(name, ctx, s)] = r
                        add(name, mdir.name, v, ctx, s, r, secs, None if v == "fp32" else refs.get((name, ctx, s)))
    summary(rows)
    if out:
        Path(out).write_text("".join(json.dumps(r) + "\n" for r in rows))


if __name__ == "__main__":
    main(*sys.argv[1:7]) if len(sys.argv) >= 3 else sys.exit(__doc__)

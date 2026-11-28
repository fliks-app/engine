#!/usr/bin/env python3
"""Ground-truth evaluation of the engine's karaoke jobs, submitted to a running engine over /v1.

  eval.py align    <ultrastar-dir> [--model ID]... [--inputs stem,mix] [--timing untimed,timed]
  eval.py negative <ultrastar-dir> [--model ID]...
  eval.py melody   <ultrastar-dir> [--model ID] [--gaps]
  eval.py separate <musdb18hq-dir> --work DIR
  common: --engine URL (FLIKS_ENGINE_URL) --token T | --token-file F (FLIKS_ENGINE_TOKEN) --out results.json --limit N

One JSON line per song on stdout, then a markdown summary. Paths sent to the engine are absolute and
must be valid on the engine's host. See README.md.
"""
import argparse, json, math, os, statistics as st, subprocess, sys, time, urllib.request, wave, zlib
from collections import defaultdict
from pathlib import Path

import numpy as np

import ultrastar

LANGS = {"english": "en", "french": "fr", "german": "de", "spanish": "es", "italian": "it",
         "dutch": "nl", "portuguese": "pt"}


class Engine:
    def __init__(self, url, token, timeout):
        self.url, self.token, self.timeout = url.rstrip("/"), token, timeout

    def _call(self, method, path, body=None):
        req = urllib.request.Request(self.url + path, method=method, headers={"Authorization": f"Bearer {self.token}"})
        if body is not None:
            req.data = json.dumps(body).encode()
            req.add_header("Content-Type", "application/json")
        with urllib.request.urlopen(req, timeout=60) as r:
            return json.load(r)

    def job(self, kind, path):
        """Runs one job to its end; returns the snapshot (result, startedMs, endedMs)."""
        media = {"mediaFileId": zlib.crc32(str(path).encode()), "path": str(path)}
        jid = self._call("POST", "/v1/jobs", {"kind": kind, "media": media, "force": True})["jobId"]
        t0 = time.time()
        while time.time() - t0 < self.timeout:
            snap = self._call("GET", f"/v1/jobs/{jid}")
            if snap["state"] == "done":
                return snap
            if snap["state"] in ("failed", "cancelled"):
                raise RuntimeError(f"{kind} {snap['state']}: {snap.get('error')}")
            time.sleep(1)
        raise TimeoutError(f"{kind} job {jid} still running after {self.timeout}s")


def pct(xs, q):
    return float(np.percentile(xs, q)) if len(xs) else None


def r(x, n=1):
    return None if x is None else round(float(x), n)


def share(xs, lim):
    return r(100 * np.mean([x <= lim for x in xs]), 1) if len(xs) else None


def lang_of(song, args):
    return args.language or LANGS.get(song.headers.get("LANGUAGE", "").lower())


# ---------- align ----------

def align_job(eng, song, args, audio, voicing, timing, model, lines):
    req = {"type": "Align", "lines": [{"text": t, **({"startMs": max(0, round(s))} if timing == "timed" else {})}
                                      for _, t, s in lines]}
    if lang_of(song, args):
        req["language"] = lang_of(song, args)
    if model:
        req["model"] = model
    if voicing:
        req["voicing"] = str(voicing)
    snap = eng.job(req, audio)
    a = snap["result"]["align"]
    return a, (snap["endedMs"] - snap["startedMs"]) / 1000


def score_align(song, a):
    """Signed word onset errors and line start errors, engine minus truth, over the first copy of each line."""
    truth = defaultdict(list)
    for w in song.words:
        truth[w.line].append(w)
    lines = song.lines()
    errs, lerrs = [], []
    for ln in a["lines"]:
        if ln["repeat"] or ln["line"] >= len(lines):
            continue
        idx, _, start = lines[ln["line"]]
        tw = truth[idx]
        if len(tw) != len(ln["words"]):
            continue
        errs += [ew["startMs"] - t.start_ms for ew, t in zip(ln["words"], tw)]
        lerrs.append(ln["startMs"] - start)
    return errs, lerrs


def cmd_align(eng, args, songs):
    rows, pool = [], {}
    for p in songs:
        song = ultrastar.parse(p)
        lines = song.lines()
        if not lines:
            continue
        runs = []
        if song.vocals and "stem" in args.inputs:
            runs.append(("stem", song.vocals, None))
        if song.audio and "mix" in args.inputs:
            runs.append(("mix", song.audio, song.vocals))
        for input_, audio, voicing in runs:
            for timing in args.timing:
                for model in args.model or [""]:
                    row = {"cmd": "align", "song": song.title, "input": input_, "timing": timing, "model": model or "default"}
                    try:
                        a, wall = align_job(eng, song, args, audio, voicing, timing, model, lines)
                        errs, lerrs = score_align(song, a)
                        ab = [abs(e) for e in errs]
                        row |= {"actualModel": a["model"], "backend": a["backend"], "matched": r(a["matched"], 3), "words": len(errs),
                                "medianAbsMs": r(st.median(ab)) if ab else None, "meanAbsMs": r(np.mean(ab)) if ab else None,
                                "medianSignedMs": r(st.median(errs)) if errs else None, "within100": share(ab, 100),
                                "within300": share(ab, 300), "lineMeanAbsMs": r(np.mean(np.abs(lerrs))) if lerrs else None,
                                "lineUnder300": share(np.abs(lerrs), 300), "wallS": r(wall)}
                        pool.setdefault((input_, timing, row["model"]), []).append((errs, lerrs, a["matched"]))
                    except Exception as e:
                        row["error"] = str(e)
                    emit(row, rows)
    table = []
    for (input_, timing, model), runs in sorted(pool.items()):
        errs = [e for x in runs for e in x[0]]
        ab = np.abs(errs) if errs else np.array([])
        lab = np.abs([e for x in runs for e in x[1]])
        m = [x[2] for x in runs]
        table.append({"input": input_, "timing": timing, "model": model, "songs": len(runs), "words": len(errs),
                      "medianAbsMs": r(pct(ab, 50)), "meanAbsMs": r(ab.mean() if len(ab) else None),
                      "medianSignedMs": r(pct(errs, 50)), "within100": share(ab, 100), "within300": share(ab, 300),
                      "lineMeanAbsMs": r(lab.mean() if len(lab) else None), "lineUnder300": share(lab, 300),
                      "matchedMean": r(np.mean(m), 3), "matchedP5": r(pct(m, 5), 3)})
    cols = ["input", "timing", "model", "songs", "words", "medianAbsMs", "meanAbsMs", "medianSignedMs", "within100",
            "within300", "lineMeanAbsMs", "lineUnder300", "matchedMean", "matchedP5"]
    return rows, table, cols


# ---------- negative ----------

def cmd_negative(eng, args, songs):
    parsed = [ultrastar.parse(p) for p in songs]
    parsed = [s for s in parsed if s.lines() and (s.vocals or s.audio)]
    if len(parsed) < 2:
        sys.exit("negative needs at least 2 songs")
    rows, pool = [], defaultdict(list)
    for i, song in enumerate(parsed):
        other = parsed[(i + 1) % len(parsed)]
        audio = song.vocals or song.audio
        for model in args.model or [""]:
            row = {"cmd": "negative", "song": song.title, "lyricsOf": other.title, "model": model or "default"}
            try:
                a, wall = align_job(eng, other, args, audio, None, "untimed", model, other.lines())
                row |= {"actualModel": a["model"], "backend": a["backend"], "matched": r(a["matched"], 3), "wallS": r(wall)}
                pool[row["model"]].append(a["matched"])
            except Exception as e:
                row["error"] = str(e)
            emit(row, rows)
    table = [{"model": m, "songs": len(v), "matchedMean": r(np.mean(v), 3), "matchedMedian": r(pct(v, 50), 3),
              "matchedP95": r(pct(v, 95), 3), "matchedMax": r(max(v), 3)} for m, v in sorted(pool.items())]
    return rows, table, ["model", "songs", "matchedMean", "matchedMedian", "matchedP95", "matchedMax"]


# ---------- melody ----------

def hz(m):
    return 440 * 2 ** ((np.asarray(m, float) - 69) / 12)


def melody_metrics(ref, est):
    """Voicing recall and false alarm, raw pitch and chroma accuracy (50 cents), on frames where NaN = unvoiced.
    mir_eval when installed, else the same definitions in numpy."""
    try:
        import mir_eval.melody as me
        t = np.arange(len(ref)) * 0.01
        f = lambda x: np.where(np.isnan(x), 0.0, hz(np.nan_to_num(x, nan=69)))
        e = me.evaluate(t, f(ref), t, f(est))
        return {"voicingRecall": e["Voicing Recall"], "voicingFalseAlarm": e["Voicing False Alarm"],
                "rawPitch": e["Raw Pitch Accuracy"], "rawChroma": e["Raw Chroma Accuracy"], "impl": "mir_eval"}
    except ImportError:
        pass
    rv, ev = ~np.isnan(ref), ~np.isnan(est)
    cents = 100 * (est - ref)
    fold = ((cents + 600) % 1200) - 600
    both = rv & ev
    nv = ~rv
    return {"voicingRecall": (both.sum() / rv.sum()) if rv.any() else None,
            "voicingFalseAlarm": ((nv & ev).sum() / nv.sum()) if nv.any() else None,
            "rawPitch": ((both & (np.abs(cents) < 50)).sum() / rv.sum()) if rv.any() else None,
            "rawChroma": ((both & (np.abs(fold) < 50)).sum() / rv.sum()) if rv.any() else None, "impl": "numpy"}


def wpercentile(vals, weights, q):
    o = np.argsort(vals)
    c = np.cumsum(np.asarray(weights)[o])
    return float(np.asarray(vals)[o][np.searchsorted(c, q / 100 * c[-1])])


def cmd_melody(eng, args, songs):
    rows, pool = [], []
    for p in songs:
        song = ultrastar.parse(p)
        syl = [s for s in song.syllables if s.kind in ultrastar.PITCHED and s.end_ms - s.start_ms >= 1]
        row = {"cmd": "melody", "song": song.title, "model": args.model or "default"}
        try:
            if not song.vocals or not syl:
                raise RuntimeError("no #VOCALS stem or no pitched syllable")
            spans = [{"startMs": round(s.start_ms), "endMs": max(round(s.end_ms), round(s.start_ms) + 1)} for s in syl]
            gaps = []
            if args.gaps:
                gaps = [(a.end_ms, b.start_ms) for a, b in zip(syl, syl[1:]) if b.start_ms - a.end_ms >= 50]
                spans += [{"startMs": round(a), "endMs": round(b)} for a, b in gaps]
            req = {"type": "Melody", "spans": spans, **({"model": args.model} if args.model else {})}
            snap = eng.job(req, song.vocals)
            m = snap["result"]["melody"]
            notes = m["spans"]
            near, covered = [], 0
            for s, ns in zip(syl, notes):
                mid = (s.start_ms + s.end_ms) / 2
                n = next((n for n in ns if n["startMs"] <= mid < n["endMs"]), None)
                if n:
                    covered += 1
                    d = abs(s.midi - n["midi"]) % 12
                    near.append(min(d, 12 - d))
                else:
                    near.append(math.inf)
            ref_f, est_f = [], []
            for s, ns in zip(syl, notes):
                for t in np.arange(math.ceil(s.start_ms / 10) * 10, s.end_ms, 10):
                    ref_f.append(s.midi)
                    est_f.append(next((n["midi"] for n in ns if n["startMs"] <= t < n["endMs"]), np.nan))
            for (a, b), ns in zip(gaps, notes[len(syl):]):
                for t in np.arange(math.ceil(a / 10) * 10, b, 10):
                    ref_f.append(np.nan)
                    est_f.append(next((n["midi"] for n in ns if n["startMs"] <= t < n["endMs"]), np.nan))
            mm = melody_metrics(np.array(ref_f), np.array(est_f))
            if not gaps:
                mm["voicingFalseAlarm"] = None
            w = [s.end_ms - s.start_ms for s in syl]
            truth_range = [wpercentile([s.midi for s in syl], w, 5), wpercentile([s.midi for s in syl], w, 95)]
            row |= {"syllables": len(syl), "covered": r(100 * covered / len(syl)),
                    "within1Semitone": share(near, 1), "gaps": bool(gaps), "analysis": m.get("analysis"),
                    "truthRange": [r(x, 1) for x in truth_range],
                    **{k: (r(v, 3) if isinstance(v, float) else v) for k, v in mm.items()},
                    "wallS": r((snap["endedMs"] - snap["startedMs"]) / 1000)}
            pool.append((near, ref_f, est_f, gaps))
        except Exception as e:
            row["error"] = str(e)
        emit(row, rows)
    ok = [x for x in rows if "error" not in x]
    n = lambda k: [x[k] for x in ok if x.get(k) is not None]
    table = [{"songs": len(ok), "within1Semitone": r(np.mean(n("within1Semitone")) if ok else None),
              "covered": r(np.mean(n("covered")) if ok else None), "rawPitch": r(np.mean(n("rawPitch")) if ok else None, 3),
              "rawChroma": r(np.mean(n("rawChroma")) if ok else None, 3),
              "voicingRecall": r(np.mean(n("voicingRecall")) if ok else None, 3),
              "voicingFalseAlarm": r(np.mean(n("voicingFalseAlarm")) if n("voicingFalseAlarm") else None, 3)}]
    return rows, table, list(table[0])


# ---------- separate ----------

def decode(path, rate=44100):
    out = subprocess.run(["ffmpeg", "-v", "error", "-i", str(path), "-f", "f32le", "-ac", "2", "-ar", str(rate), "-"],
                         capture_output=True, check=True).stdout
    return np.frombuffer(out, np.float32).reshape(-1, 2).astype(np.float64)


def separation_scores(refs, ests):
    """refs, ests: [2 targets (vocals, accompaniment), n, 2 channels]. Median over 1 s frames, per target."""
    try:
        import museval
        sdr, _, sir, sar = museval.evaluate(refs, ests, win=44100, hop=44100)
        return {k: [float(np.nanmedian(v[i])) for i in range(2)] for k, v in (("sdr", sdr), ("sir", sir), ("sar", sar))}, "museval"
    except ImportError:
        pass
    n = refs.shape[1] // 44100 * 44100
    sdr = []
    for i in range(2):
        rf, es = refs[i, :n].reshape(-1, 44100 * 2), ests[i, :n].reshape(-1, 44100 * 2)
        num, den = (rf ** 2).sum(1), ((rf - es) ** 2).sum(1)
        ok = (num > 0) & (den > 0)
        sdr.append(float(np.median(10 * np.log10(num[ok] / den[ok]))))
    return {"sdr": sdr, "sir": [None, None], "sar": [None, None]}, "numpy-sdr"


def cmd_separate(eng, args, tracks):
    work = Path(args.work).resolve()
    work.mkdir(parents=True, exist_ok=True)
    rows = []
    for mix in tracks:
        d = mix.parent
        row = {"cmd": "separate", "track": d.name, "model": args.model or "default"}
        outs = {k: work / f"{d.name}-{os.getpid()}-{int(time.time())}.{k}.opus" for k in ("vocals", "instrumental")}
        try:
            with wave.open(str(mix)) as w:
                dur = w.getnframes() / w.getframerate()
            info = {"video": [], "audio": [{"streamIndex": 0, "codec": "pcm_s16le", "language": "und"}],
                    "subtitles": [], "durationSeconds": dur}
            req = {"type": "Separate", "info": info, "vocals": str(outs["vocals"]), "instrumental": str(outs["instrumental"]),
                   **({"model": args.model} if args.model else {})}
            if args.bitrate:
                req["bitrateKbps"] = args.bitrate
            t0 = time.time()
            snap = eng.job(req, mix)
            wall = time.time() - t0
            s = snap["result"]["separate"]
            m, v = decode(mix), decode(d / "vocals.wav")
            ev, ei = decode(outs["vocals"]), decode(outs["instrumental"])
            n = min(len(m), len(v), len(ev), len(ei))
            refs = np.stack([v[:n], m[:n] - v[:n]])
            sc, impl = separation_scores(refs, np.stack([ev[:n], ei[:n]]))
            compute = (snap["endedMs"] - snap["startedMs"]) / 1000
            row |= {"provider": s["provider"], "durationS": r(dur), "impl": impl,
                    **{f"{k}{t}": r(sc[k][i], 2) for k in sc for i, t in enumerate(("Vocals", "Accompaniment"))},
                    "computeSPerMin": r(compute / (dur / 60), 2), "wallS": r(wall)}
        except Exception as e:
            row["error"] = str(e)
        finally:
            if not args.keep:
                for o in outs.values():
                    o.unlink(missing_ok=True)
        emit(row, rows)
    ok = [x for x in rows if "error" not in x]
    med = lambda k: r(st.median([x[k] for x in ok if x[k] is not None]), 2) if any(x[k] is not None for x in ok) else None
    keys = ["sdrVocals", "sirVocals", "sarVocals", "sdrAccompaniment", "sirAccompaniment", "sarAccompaniment", "computeSPerMin"]
    table = [{"tracks": len(ok), "provider": ",".join(sorted({x["provider"] for x in ok})), **{k: med(k) for k in keys}}]
    return rows, table, list(table[0])


# ---------- shared ----------

def emit(row, rows):
    rows.append(row)
    print(json.dumps(row, ensure_ascii=False), flush=True)


def markdown(table, cols):
    out = ["", "| " + " | ".join(cols) + " |", "|" + "---|" * len(cols)]
    out += ["| " + " | ".join("" if t[c] is None else str(t[c]) for c in cols) + " |" for t in table]
    return "\n".join(out)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("cmd", choices=["align", "negative", "melody", "separate"])
    ap.add_argument("root", help="UltraStar songs folder (align, negative, melody) or MUSDB18-HQ folder (separate)")
    ap.add_argument("--engine", default=os.environ.get("FLIKS_ENGINE_URL", "http://127.0.0.1:3000"))
    ap.add_argument("--token", default=os.environ.get("FLIKS_ENGINE_TOKEN"))
    ap.add_argument("--token-file")
    ap.add_argument("--timeout", type=int, default=3600, help="seconds per job")
    ap.add_argument("--out")
    ap.add_argument("--limit", type=int, help="first N songs or tracks")
    ap.add_argument("--model", action="append", help="model id, repeatable for align and negative")
    ap.add_argument("--language", help="two-letter code sent with every Align (default: #LANGUAGE header)")
    ap.add_argument("--inputs", default="stem,mix", help="align: stem and/or mix")
    ap.add_argument("--timing", default="untimed,timed", help="align: untimed and/or timed")
    ap.add_argument("--gaps", action="store_true", help="melody: also send the silences between syllables (voicing false alarm)")
    ap.add_argument("--work", help="separate: absolute dir the engine can write the stems to")
    ap.add_argument("--bitrate", type=int)
    ap.add_argument("--keep", action="store_true", help="separate: keep the stems")
    args = ap.parse_args()
    args.inputs, args.timing = args.inputs.split(","), args.timing.split(",")
    if args.token_file:
        args.token = Path(args.token_file).read_text().strip()
    if not args.token:
        sys.exit("no token: --token, --token-file or FLIKS_ENGINE_TOKEN")
    if args.cmd in ("melody", "separate"):
        args.model = args.model[0] if args.model else None
    if args.cmd == "separate":
        if not args.work:
            sys.exit("separate needs --work")
        items = sorted(Path(args.root).rglob("mixture.wav"))
    else:
        items = ultrastar.find_songs(args.root)
    if not items:
        sys.exit("nothing found under " + args.root)
    items = items[: args.limit]
    fn = {"align": cmd_align, "negative": cmd_negative, "melody": cmd_melody, "separate": cmd_separate}[args.cmd]
    rows, table, cols = fn(Engine(args.engine, args.token, args.timeout), args, items)
    print(markdown(table, cols))
    if args.out:
        Path(args.out).write_text(json.dumps({"cmd": args.cmd, "engine": args.engine, "summary": table, "rows": rows}, indent=1))


if __name__ == "__main__":
    main()

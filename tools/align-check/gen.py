#!/usr/bin/env python3
"""Builds a test clip whose word times are known to the sample: one TTS call per word, trimmed and
laid on a timeline with fixed gaps. Words are drawn from a small generated vocabulary, so no
real lyrics are involved.

  gen.py <out-dir> <name> <lang en|fr> <voice> <clean|music|fast> <seed> [--lines N]

voice: `espeak:<espeak-ng voice, e.g. en-us+m3>` or `piper:<path to the .onnx>`.
Writes <name>.wav (16 kHz mono, 16-bit) and <name>.json (lyrics lines and the truth of every word).
Needs numpy and, for piper voices, piper-tts; espeak-ng and ffmpeg on PATH.
"""
import json, random, subprocess, sys, tempfile, wave
import numpy as np

VOCAB = {
    "en": dict(
        det=["the", "a", "my", "your", "this"],
        adj=["old", "bright", "quiet", "golden", "heavy", "little", "silver", "narrow"],
        noun=["river", "mountain", "window", "garden", "engine", "lantern", "harbor", "meadow", "mirror", "orchard"],
        verb=["carries", "follows", "remembers", "watches", "paints", "borrows", "circles", "answers"],
    ),
    "fr": dict(
        det=["le", "la", "mon", "ton", "ce"],
        adj=["vieux", "calme", "dore", "lourd", "petit", "lointain", "clair", "sombre"],
        noun=["fleuve", "jardin", "moteur", "miroir", "verger", "sentier", "village", "bateau", "nuage", "pont"],
        verb=["porte", "suit", "garde", "regarde", "peint", "emprunte", "contourne", "ecoute"],
    ),
}
TEMPLATES = ["det adj noun verb det noun", "det noun verb det adj noun", "det adj noun verb det adj noun"]
SPEED = {"clean": dict(gap=0.12, line_gap=0.8, espeak=150, piper=1.0),
         "music": dict(gap=0.12, line_gap=0.8, espeak=150, piper=1.0),
         "fast": dict(gap=0.03, line_gap=0.3, espeak=215, piper=0.72)}


def make_lines(lang, n, rng):
    v = VOCAB[lang]
    return [" ".join(rng.choice(v[t]) for t in rng.choice(TEMPLATES).split()) for _ in range(n)]


def tts(voice, word, speed):
    kind, arg = voice.split(":", 1)
    if kind == "espeak":
        with tempfile.NamedTemporaryFile(suffix=".wav") as f:
            subprocess.run(["espeak-ng", "-v", arg, "-s", str(speed["espeak"]), "-w", f.name, word], check=True)
            with wave.open(f.name) as w:
                return np.frombuffer(w.readframes(w.getnframes()), np.int16).astype(np.float32) / 32768, w.getframerate()
    from piper import PiperVoice, SynthesisConfig
    if not hasattr(tts, "p"):
        tts.p = PiperVoice.load(arg)
    chunks = list(tts.p.synthesize(word, SynthesisConfig(length_scale=speed["piper"])))
    return np.concatenate([c.audio_float_array for c in chunks]), chunks[0].sample_rate


def trim(x, sr):
    """Samples from the first to the last 5 ms frame above 3 % of the peak."""
    hop = int(sr * 0.005)
    env = np.array([np.abs(x[i:i + hop]).max() for i in range(0, len(x) - hop, hop)])
    on = np.nonzero(env > 0.03 * env.max())[0]
    return x[on[0] * hop:(on[-1] + 1) * hop]


def music(n, sr, rng):
    t = np.arange(n) / sr
    chords = [[220, 261.6, 329.6], [174.6, 220, 261.6], [261.6, 329.6, 392], [196, 246.9, 293.7]]
    pad = np.zeros(n)
    for i, ch in enumerate(chords * (n // (8 * sr) + 1)):
        a, b = i * 2 * sr, min(n, (i + 1) * 2 * sr)
        if a >= n:
            break
        seg = t[a:b]
        for f in ch:
            pad[a:b] += sum(np.sin(2 * np.pi * f * k * seg) / k for k in (1, 2, 3))
        pad[a:b] *= np.minimum(1, np.minimum(seg - seg[0], seg[-1] - seg) * 4)
    beat = 60 / 100
    drums = np.zeros(n)
    for k in range(int(n / sr / beat) + 1):
        a = int(k * beat * sr)
        m = min(int(0.2 * sr), n - a)
        if m <= 0:
            break
        s = np.arange(m) / sr
        if k % 2 == 0:
            drums[a:a + m] += np.sin(2 * np.pi * (50 * s + 70 * (1 - np.exp(-30 * s)) / 30 * 1)) * np.exp(-18 * s) * 1.5
        else:
            drums[a:a + m] += np.array([rng.uniform(-1, 1) for _ in range(m)]) * np.exp(-25 * s) * 0.8
    return pad / np.abs(pad).max() + drums


def main():
    out, name, lang, voice, kind, seed = sys.argv[1:7]
    n_lines = int(sys.argv[sys.argv.index("--lines") + 1]) if "--lines" in sys.argv else (8 if kind != "fast" else 11)
    rng, sp = random.Random(int(seed)), SPEED[kind]
    lines = make_lines(lang, n_lines, rng)
    cache, pieces, truth, pos, sr = {}, [], [], 0.4, None
    for li, text in enumerate(lines):
        words = []
        for w in text.split():
            if w not in cache:
                x, sr = tts(voice, w, sp)
                cache[w] = trim(x, sr)
            x = cache[w]
            words.append({"text": w, "startMs": round(pos * 1000), "endMs": round((pos + len(x) / sr) * 1000)})
            pieces.append((pos, x))
            pos += len(x) / sr + sp["gap"]
        truth.append({"text": text, "words": words})
        pos += sp["line_gap"] - sp["gap"]
    total = int((pos + 0.5) * sr)
    y = np.zeros(total, np.float32)
    for p, x in pieces:
        a = int(p * sr)
        y[a:a + len(x)] += x
    y *= 0.5 / np.abs(y).max()
    if kind == "music":
        active = np.abs(y) > 0.02
        voice_rms = np.sqrt(np.mean(y[active] ** 2))
        m = music(total, sr, rng)
        m *= voice_rms * 10 ** (-8 / 20) / np.sqrt(np.mean(m ** 2))
        y = y + m
        y *= 0.9 / np.abs(y).max()
    raw = f"{out}/{name}.raw.wav"
    with wave.open(raw, "wb") as w:
        w.setnchannels(1), w.setsampwidth(2), w.setframerate(sr)
        w.writeframes((y * 32767).astype(np.int16).tobytes())
    subprocess.run(["ffmpeg", "-v", "error", "-y", "-i", raw, "-ac", "1", "-ar", "16000", "-c:a", "pcm_s16le", f"{out}/{name}.wav"], check=True)
    import os
    os.remove(raw)
    json.dump({"lang": lang, "voice": voice, "kind": kind, "durationMs": round(total / sr * 1000), "lines": truth},
              open(f"{out}/{name}.json", "w"), indent=1)


main()

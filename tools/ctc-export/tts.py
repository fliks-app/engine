#!/usr/bin/env python3
"""Spoken sample per language with known line starts, for the languages that have no sung track:
the first three public-domain UDHR sentences, one espeak-ng call per line, 1.2 s apart.

  tts.py <out-dir>          writes tts-<lang>.wav (16 kHz mono) and tts-<lang>.json for eval.py
Needs numpy, espeak-ng and ffmpeg on PATH.
"""
import json, subprocess, sys
from pathlib import Path
import numpy as np

TEXT = {
    "de": ["Alle Menschen sind frei und gleich an Würde und Rechten geboren.",
           "Sie sind mit Vernunft und Gewissen begabt und sollen einander im Geist der Brüderlichkeit begegnen.",
           "Jeder hat das Recht auf Leben, Freiheit und Sicherheit der Person."],
    "es": ["Todos los seres humanos nacen libres e iguales en dignidad y derechos.",
           "Dotados como están de razón y conciencia, deben comportarse fraternalmente los unos con los otros.",
           "Todo individuo tiene derecho a la vida, a la libertad y a la seguridad de su persona."],
    "it": ["Tutti gli esseri umani nascono liberi ed eguali in dignità e diritti.",
           "Essi sono dotati di ragione e di coscienza e devono agire gli uni verso gli altri in spirito di fratellanza.",
           "Ogni individuo ha diritto alla vita, alla libertà e alla sicurezza della propria persona."],
    "nl": ["Alle mensen worden vrij en gelijk in waardigheid en rechten geboren.",
           "Zij zijn begiftigd met verstand en geweten, en behoren zich jegens elkander in een geest van broederschap te gedragen.",
           "Een ieder heeft recht op leven, vrijheid en onschendbaarheid van zijn persoon."],
    "pt": ["Todos os seres humanos nascem livres e iguais em dignidade e em direitos.",
           "Dotados de razão e de consciência, devem agir uns para com os outros em espírito de fraternidade.",
           "Todo o indivíduo tem direito à vida, à liberdade e à segurança pessoal."],
}


def speak(lang, text):
    wav = subprocess.run(["espeak-ng", "-v", lang, "-s", "150", "--stdout", text], check=True, capture_output=True).stdout
    raw = subprocess.run(["ffmpeg", "-v", "error", "-i", "-", "-f", "f32le", "-ac", "1", "-ar", "16000", "-"],
                         input=wav, check=True, capture_output=True).stdout
    x = np.frombuffer(raw, np.float32)
    on = np.nonzero(np.abs(x) > 0.03 * np.abs(x).max())[0]
    return x[on[0]:on[-1] + 1]


def main(out):
    for lang, lines in TEXT.items():
        t, audio, spec = 1.0, [np.zeros(16000, np.float32)], []
        for text in lines:
            x = speak(lang, text)
            spec.append({"start": round(t * 1000), "text": text})
            audio += [x, np.zeros(19200, np.float32)]
            t += len(x) / 16000 + 1.2
        pcm = np.concatenate(audio)
        subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "f32le", "-ac", "1", "-ar", "16000", "-i", "-",
                        str(Path(out) / f"tts-{lang}.wav")], input=pcm.tobytes(), check=True)
        json.dump({"lang": lang, "lines": spec}, open(Path(out) / f"tts-{lang}.json", "w"), ensure_ascii=False)


if __name__ == "__main__":
    main(*sys.argv[1:2]) if len(sys.argv) == 2 else sys.exit(__doc__)

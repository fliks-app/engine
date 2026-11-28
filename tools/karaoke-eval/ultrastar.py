#!/usr/bin/env python3
"""UltraStar `.txt` parser: ground truth (syllable and word start, end, pitch) for eval.py.

Beat time is GAP + beat * 60000 / (BPM * 4) ms; pitch is semitones from C4 (MIDI 60).
Run `python3 ultrastar.py` for the self-check (the cases are in test_ultrastar.py).
"""
import re
from dataclasses import dataclass, field
from pathlib import Path

PITCHED = ":*"  # F (freestyle), R and G (rap) carry no usable pitch


@dataclass
class Syllable:
    start_ms: float
    end_ms: float
    midi: int
    text: str
    kind: str
    line: int


@dataclass
class Word:
    text: str
    start_ms: float
    end_ms: float
    line: int


@dataclass
class Song:
    path: Path
    headers: dict
    syllables: list = field(default_factory=list)
    words: list = field(default_factory=list)

    @property
    def title(self):
        return f"{self.headers.get('ARTIST', '?')} - {self.headers.get('TITLE', self.path.stem)}"

    def _file(self, *keys):
        for k in keys:
            if self.headers.get(k):
                return (self.path.parent / self.headers[k]).resolve()

    @property
    def audio(self):
        return self._file("AUDIO", "MP3")

    @property
    def vocals(self):
        return self._file("VOCALS")

    def lines(self):
        """[(line index, text, start ms)] for lines with words; the text is the words joined by a space."""
        out = {}
        for w in self.words:
            out.setdefault(w.line, []).append(w)
        return [(i, " ".join(w.text for w in ws), ws[0].start_ms) for i, ws in sorted(out.items())]


def read_text(path):
    raw = Path(path).read_bytes()
    for enc in ("utf-8-sig", "cp1252"):
        try:
            return raw.decode(enc)
        except UnicodeDecodeError:
            pass
    return raw.decode("latin-1")


def group_words(syllables):
    """A word ends at a line break, or before a syllable starting with a space, or after one ending with
    a space. A `~` syllable only extends the current word."""
    words, cur, prev_space, line = [], None, True, None
    for s in syllables:
        if s.line != line:
            line, cur, prev_space = s.line, None, True
        piece = s.text.strip()
        if not piece:
            prev_space = True
            continue
        if piece == "~":
            if cur:
                cur.end_ms = s.end_ms
        elif cur is None or s.text[0].isspace() or prev_space:
            cur = Word(piece, s.start_ms, s.end_ms, s.line)
            words.append(cur)
        else:
            cur.text += piece
            cur.end_ms = s.end_ms
        prev_space = s.text[-1].isspace() or (piece == "~" and prev_space)
    return words


def parse(path):
    headers, notes = {}, []
    rel = line = 0
    singer = 1
    song = Song(Path(path), headers)
    for raw in read_text(path).splitlines():
        raw = raw.rstrip("\r\n")
        if raw.startswith("#"):
            k, _, v = raw[1:].partition(":")
            headers[k.strip().upper()] = v.strip()
        elif raw[:1] in (":", "*", "F", "R", "G") and singer == 1:
            kind, start, length, pitch, *text = raw.split(" ", 4)
            notes.append((kind, rel + int(start), int(length), int(pitch), text[0] if text else "", line))
        elif raw[:1] == "-" and singer == 1:
            parts = raw.split()
            line += 1
            if headers.get("RELATIVE", "").upper() == "YES" and len(parts) > 2:
                rel += int(parts[2])
        elif raw[:2] in ("P1", "P2"):
            singer = int(raw[1])
        elif raw[:1] == "E":
            break
    bpm = float(headers["BPM"].replace(",", "."))
    gap = float(headers.get("GAP", "0").replace(",", "."))
    ms = lambda beat: gap + beat * 60000 / (bpm * 4)  # ponytail: `B` BPM-change lines are ignored
    song.syllables = [Syllable(ms(b), ms(b + n), p + 60, t, k, ln) for k, b, n, p, t, ln in notes]
    song.words = group_words(song.syllables)
    return song


def find_songs(root):
    """Every `.txt` under root that has a #BPM header, sorted."""
    out = []
    for p in sorted(Path(root).rglob("*.txt")):
        try:
            if re.search(r"^#BPM:", read_text(p), re.M | re.I):
                out.append(p)
        except OSError:
            pass
    return out


if __name__ == "__main__":
    import test_ultrastar

    test_ultrastar.run()
    print("ultrastar self-check ok")

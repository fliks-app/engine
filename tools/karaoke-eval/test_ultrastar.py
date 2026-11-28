#!/usr/bin/env python3
"""Parser checks: `python3 test_ultrastar.py` or `python3 -m pytest`."""
import tempfile
from pathlib import Path

import ultrastar


def song(body, headers="#BPM:120\n#GAP:1000\n#MP3:a.mp3\n#VOCALS:v.ogg\n"):
    d = tempfile.mkdtemp()
    p = Path(d) / "s.txt"
    p.write_text(headers + body + "E\n")
    return ultrastar.parse(p)


def test_timing_and_pitch():
    s = song(": 0 4 0 Hel\n: 4 4 2 lo\n")
    # 120 BPM -> 125 ms per beat
    assert (s.syllables[0].start_ms, s.syllables[0].end_ms) == (1000, 1500)
    assert s.syllables[1].start_ms == 1500 and s.syllables[1].midi == 62
    assert s.audio.name == "a.mp3" and s.vocals.name == "v.ogg"


def test_comma_bpm_and_gap():
    s = song(": 4 4 0 a\n", "#BPM:60,0\n#GAP:500,5\n")
    assert s.syllables[0].start_ms == 500.5 + 4 * 250


def test_words_space_at_start_or_end():
    lead = song(": 0 2 0 Hel\n: 2 2 0 lo\n: 4 2 0  world\n- 8\n: 8 2 0 Next\n")
    trail = song(": 0 2 0 Hel\n: 2 2 0 lo \n: 4 2 0 world\n- 8\n: 8 2 0 Next\n")
    for s in (lead, trail):
        assert [w.text for w in s.words] == ["Hello", "world", "Next"]
        assert s.words[0].start_ms == 1000 and s.words[0].end_ms == 1500
        assert [w.line for w in s.words] == [0, 0, 1]
        assert s.lines()[0][1] == "Hello world"


def test_tilde_and_unpitched():
    s = song(": 0 2 0 Oh\n: 2 2 3 ~\nF 4 2 0  yeah\nR 6 2 0  rap\n")
    assert [(w.text, w.end_ms) for w in s.words] == [("Oh", 1500), ("yeah", 1750), ("rap", 2000)]
    assert [x.kind for x in s.syllables if x.kind in ultrastar.PITCHED] == [":", ":"]


def test_relative():
    s = song(": 0 2 0 a\n- 4 4\n: 0 2 0 b\n- 8 8\n: 1 2 0 c\n", "#BPM:120\n#RELATIVE:YES\n")
    assert [x.start_ms for x in s.syllables] == [0, 500, 1625]


def test_duet_keeps_first_singer():
    s = song("P1\n: 0 2 0 a\nP2\n: 0 2 0 b\nP1\n: 4 2 0  c\n")
    assert [w.text for w in s.words] == ["a", "c"]


def run():
    for name, f in list(globals().items()):
        if name.startswith("test_"):
            f()


if __name__ == "__main__":
    run()
    print("ok")

#!/usr/bin/env python3
"""Writes crates/core/tests/fbank-{even,odd}.f32: what the SeamlessM4T feature extractor makes of the
deterministic signal that `fbank.rs` regenerates (an even and an odd fbank frame count).

  fbank-golden.py <w2v-bert checkpoint dir> <crates/core/tests>
"""
import sys
import numpy as np
from transformers import AutoFeatureExtractor


def signal(n):
    state, out = 12345, np.empty(n, np.float32)
    for i in range(n):
        state = (state * 1664525 + 1013904223) & 0xFFFFFFFF
        noise = np.float32(state >> 8) / np.float32(16777216.0) - np.float32(0.5)
        amp = np.float32(1 + (i // 8000) % 4) / np.float32(8.0)
        tone = np.float32(0.25 if (i // 40) % 2 == 0 else -0.25)
        out[i] = noise * amp + tone * amp
    return out


fe = AutoFeatureExtractor.from_pretrained(sys.argv[1])
for name, n in (("even", 32000), ("odd", 32160)):
    f = fe(signal(n), sampling_rate=16000, return_tensors="np")["input_features"][0]
    f.astype("<f4").tofile(f"{sys.argv[2]}/fbank-{name}.f32")
    print(name, f.shape)

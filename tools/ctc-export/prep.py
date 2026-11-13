#!/usr/bin/env python3
"""Vocal stem of a track as 16 kHz mono wav, with the engine's Spleeter 2-stem recipe
(`crates/core/src/jobs/separate.rs`): 4096-point Hann STFT, hop 1024, 1024 masked bins, 512-frame chunks.

  prep.py <audio> <out.wav> <spleeter-dir holding vocals.onnx and accompaniment.onnx>
"""
import subprocess, sys
import numpy as np, onnxruntime as ort

N_FFT, HOP, BINS, CHUNK = 4096, 1024, 1024, 512
EPS = 1e-10


def main(audio, out, models):
    pcm = np.frombuffer(subprocess.run(
        ["ffmpeg", "-v", "error", "-i", audio, "-f", "f32le", "-ac", "2", "-ar", "44100", "-"],
        check=True, capture_output=True).stdout, np.float32).reshape(-1, 2).T
    n = pcm.shape[1]
    lead = N_FFT - HOP
    frames = -(-(n + lead) // HOP)
    frames = -(-frames // CHUNK) * CHUNK
    pad = np.zeros((2, frames * HOP + N_FFT), np.float32)
    pad[:, lead:lead + n] = pcm
    win = (0.5 - 0.5 * np.cos(2 * np.pi * np.arange(N_FFT) / N_FFT)).astype(np.float32)
    idx = np.arange(frames)[:, None] * HOP + np.arange(N_FFT)
    spec = np.fft.rfft(pad[:, idx] * win, axis=-1)  # [2, frames, 2049]
    mags = np.abs(spec[..., :BINS]).astype(np.float32)
    v_net, a_net = (ort.InferenceSession(f"{models}/{m}.onnx", providers=["CPUExecutionProvider"])
                    for m in ("vocals", "accompaniment"))
    mask = np.zeros(mags.shape, np.float32)
    for c in range(0, frames, CHUNK):
        x = mags[:, None, c:c + CHUNK]
        pv, pa = (net.run(None, {"x": x})[0][:, 0] ** 2 for net in (v_net, a_net))
        mask[:, c:c + CHUNK] = (pv + EPS / 2) / (pv + pa + EPS)
    spec[..., :BINS] *= mask
    spec[..., BINS:] = 0
    time = np.fft.irfft(spec, N_FFT, axis=-1) * win
    voc, gain = np.zeros_like(pad), np.zeros(pad.shape[1], np.float32)
    for f in range(frames):
        voc[:, f * HOP:f * HOP + N_FFT] += time[:, f]
        gain[f * HOP:f * HOP + N_FFT] += win ** 2
    voc = np.where(gain > 1e-3, voc / np.maximum(gain, 1e-3), 0)[:, lead:lead + n].mean(0)
    subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "f32le", "-ac", "1", "-ar", "44100", "-i", "-",
                    "-ar", "16000", out], input=voc.astype(np.float32).tobytes(), check=True)


if __name__ == "__main__":
    main(*sys.argv[1:4]) if len(sys.argv) == 4 else sys.exit(__doc__)

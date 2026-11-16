#!/usr/bin/env python3
"""Exports a Wav2Vec2ForCTC checkpoint for the engine's CTC aligner.

  export.py <hf-id> <revision> <out-dir>

Writes <out-dir>/{model.fp32,model.fp16,model.int8}.onnx (`input_values [1, n]` -> `logits [1, T, V]`,
n dynamic, 20 ms per frame), vocab.json and card.json (licence and revision, read from the hub).
A Wav2Vec2BertForCTC checkpoint (w2v-BERT) takes `input_features [1, T, 160]` (the SeamlessM4T fbank, made by
the engine) instead, and gets only model.fp32.onnx (weights beside it, over 2 GB) and a single-file
model.int8.onnx; its vocab.json adds `input`, `chunkMs`, `contextMs`, `onsetMs` and `retryBlank`.
A repo that already ships `model.onnx` and `tokens.txt` (onnx-asr layout, input `x` (renamed), blank id 0, the
real space as word delimiter) is converted from those files; any other goes through torch.
Needs onnx, onnxruntime, huggingface_hub, plus torch, transformers and onnxscript
for a checkpoint.
"""
import json, shutil, sys
from pathlib import Path

import onnx
from onnxruntime.transformers.float16 import convert_float_to_float16
from huggingface_hub import hf_hub_download, model_info
from onnxruntime.quantization import QuantType, quantize_dynamic


def hub_json(repo, rev, name):
    try:
        return json.load(open(hf_hub_download(repo, name, revision=rev)))
    except Exception:
        return {}


def variants(fp32, out, half=True):
    if half:
        onnx.save(convert_float_to_float16(onnx.load(fp32), keep_io_types=True), out / "model.fp16.onnx")
    quantize_dynamic(fp32, out / "model.int8.onnx", weight_type=QuantType.QInt8, op_types_to_quantize=["MatMul", "Gemm"])


BERT = dict(input="fbank", chunkMs=15000, contextMs=2000, onsetMs=-30, retryBlank=0.99)


def checkpoint(repo, rev, out):
    import numpy as np
    import onnxruntime as ort
    import torch
    from transformers import AutoModelForCTC

    class Net(torch.nn.Module):
        def __init__(self, m):
            super().__init__()
            self.m = m

        def forward(self, input_values):
            return self.m(input_values).logits

    class BertNet(Net):
        def forward(self, input_features):
            return self.m(input_features=input_features).logits

    model = AutoModelForCTC.from_pretrained(repo, revision=rev).eval()
    bert = type(model).__name__ == "Wav2Vec2BertForCTC"
    cfg, pre, tok = model.config, hub_json(repo, rev, "preprocessor_config.json"), hub_json(repo, rev, "tokenizer_config.json")
    vocab = json.load(open(hf_hub_download(repo, "vocab.json", revision=rev)))
    assert cfg.vocab_size == len(vocab) or cfg.vocab_size > max(vocab.values()), "vocab does not match the head"
    meta = dict(sampleRate=pre.get("sampling_rate", 16000), doNormalize=pre.get("do_normalize", True),
                blank=vocab[tok.get("pad_token", "<pad>")], delimiter=vocab.get(tok.get("word_delimiter_token", "|")),
                unk=vocab.get(tok.get("unk_token", "<unk>")),
                vocab={str(i): c for c, i in sorted(vocab.items(), key=lambda kv: kv[1])})
    fp32 = out / "model.fp32.onnx"
    if bert:
        meta.update(BERT, doNormalize=False)
        from transformers import AutoFeatureExtractor

        fe = AutoFeatureExtractor.from_pretrained(repo, revision=rev)

        def feats(seconds):
            t = np.arange(16000 * seconds) / 16000
            wave = 0.1 * np.random.RandomState(seconds).randn(t.size) + 0.3 * np.sin(2 * np.pi * (200 + 80 * t) * t)
            return torch.from_numpy(fe(wave.astype("float32"), sampling_rate=16000, return_tensors="np")["input_features"])

        x, y = feats(6), feats(9)
        with torch.no_grad():
            ref = BertNet(model)(y).numpy()
        scatter = out / ".scatter"
        scatter.mkdir(exist_ok=True)
        torch.onnx.export(BertNet(model), (x,), str(scatter / "m.onnx"), input_names=["input_features"],
                          output_names=["logits"], dynamic_axes={"input_features": {1: "t"}, "logits": {1: "f"}},
                          opset_version=17, dynamo=False)
        # the exporter writes one file per weight: gather them into a single data file
        onnx.save(onnx.load(str(scatter / "m.onnx")), str(fp32), save_as_external_data=True,
                  all_tensors_to_one_file=True, location="model.fp32.onnx.data")
        shutil.rmtree(scatter)
        got = ort.InferenceSession(str(fp32), providers=["CPUExecutionProvider"]).run(None, {"input_features": y.numpy()})[0]
        dev = float(np.abs(ref - got).max())
        print(f"fp32 onnx against torch (another length): max abs {dev:.2e} on logits up to {np.abs(ref).max():.1f}")
        assert dev < 1e-2, "the ONNX export does not match torch"
        return fp32, meta
    n = torch.export.Dim("n", min=1600, max=16000 * 60)
    torch.onnx.export(Net(model), (torch.randn(1, 16000 * 5),), str(fp32), input_names=["input_values"],
                      output_names=["logits"], dynamic_shapes={"input_values": {1: n}}, external_data=False)
    return fp32, meta


def onnx_asr(repo, rev, out):
    toks = [l.rstrip("\n").rsplit(" ", 1) for l in open(hf_hub_download(repo, "tokens.txt", revision=rev), encoding="utf-8")]
    vocab = {int(i): c for c, i in toks}
    fp32 = out / "model.fp32.onnx"
    m = onnx.load(hf_hub_download(repo, "model.onnx", revision=rev))
    old = m.graph.input[0].name
    m.graph.input[0].name = "input_values"
    for node in m.graph.node:
        node.input[:] = ["input_values" if i == old else i for i in node.input]
    onnx.save(m, fp32)
    meta = dict(sampleRate=16000, doNormalize=True, blank=0, delimiter=next(i for i, c in vocab.items() if c == " "),
                unk=next(i for i, c in vocab.items() if c == "<unk>"), vocab={str(i): c for i, c in sorted(vocab.items())})
    return fp32, meta


def main(repo, rev, out):
    out = Path(out)
    out.mkdir(parents=True, exist_ok=True)
    info = model_info(repo, revision=rev)
    licence = (info.card_data.license if info.card_data else None) or ""
    if licence not in ("apache-2.0", "mit"):
        sys.exit(f"{repo}: licence {licence!r}, only apache-2.0 and mit are allowed")
    names = {s.rfilename for s in info.siblings}
    fp32, meta = (onnx_asr if {"model.onnx", "tokens.txt"} <= names else checkpoint)(repo, rev, out)
    json.dump({"id": repo, "revision": info.sha, "frameMs": 20, **meta}, open(out / "vocab.json", "w"), ensure_ascii=False, indent=1)
    json.dump({"id": repo, "revision": info.sha, "license": licence,
               "url": f"https://huggingface.co/{repo}/tree/{info.sha}"}, open(out / "card.json", "w"), indent=1)
    variants(fp32, out, half="input" not in meta)


if __name__ == "__main__":
    main(*sys.argv[1:4]) if len(sys.argv) == 4 else sys.exit(__doc__)

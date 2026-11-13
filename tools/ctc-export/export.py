#!/usr/bin/env python3
"""Exports a Wav2Vec2ForCTC checkpoint for the engine's CTC aligner.

  export.py <hf-id> <revision> <out-dir>

Writes <out-dir>/{model.fp32,model.fp16,model.int8}.onnx (`input_values [1, n]` -> `logits [1, T, V]`,
n dynamic, 20 ms per frame), vocab.json and card.json (licence and revision, read from the hub).
Needs torch, transformers, onnx, onnxruntime, onnxscript, onnxconverter-common, huggingface_hub.
"""
import json, sys
from pathlib import Path

import onnx, torch
from huggingface_hub import hf_hub_download, model_info
from onnxconverter_common import float16
from onnxruntime.quantization import QuantType, quantize_dynamic
from transformers import Wav2Vec2ForCTC


class Net(torch.nn.Module):
    def __init__(self, m):
        super().__init__()
        self.m = m

    def forward(self, input_values):
        return self.m(input_values).logits


def hub_json(repo, rev, name):
    try:
        return json.load(open(hf_hub_download(repo, name, revision=rev)))
    except Exception:
        return {}


def main(repo, rev, out):
    out = Path(out)
    out.mkdir(parents=True, exist_ok=True)
    info = model_info(repo, revision=rev)
    licence = (info.card_data.license if info.card_data else None) or ""
    if licence != "apache-2.0":
        sys.exit(f"{repo}: licence {licence!r}, only apache-2.0 is allowed")
    model = Wav2Vec2ForCTC.from_pretrained(repo, revision=rev).eval()
    cfg, pre, tok = model.config, hub_json(repo, rev, "preprocessor_config.json"), hub_json(repo, rev, "tokenizer_config.json")
    vocab = json.load(open(hf_hub_download(repo, "vocab.json", revision=rev)))
    assert cfg.vocab_size == len(vocab) or cfg.vocab_size > max(vocab.values()), "vocab does not match the head"
    pad = vocab[tok.get("pad_token", "<pad>")]
    delim = tok.get("word_delimiter_token", "|")
    json.dump({
        "id": repo, "revision": info.sha, "sampleRate": pre.get("sampling_rate", 16000),
        "doNormalize": pre.get("do_normalize", True), "frameMs": 20,
        "blank": pad, "delimiter": vocab.get(delim), "unk": vocab.get(tok.get("unk_token", "<unk>")),
        "vocab": {str(i): c for c, i in sorted(vocab.items(), key=lambda kv: kv[1])},
    }, open(out / "vocab.json", "w"), ensure_ascii=False, indent=1)
    json.dump({"id": repo, "revision": info.sha, "license": licence,
               "url": f"https://huggingface.co/{repo}/tree/{info.sha}"}, open(out / "card.json", "w"), indent=1)

    fp32 = out / "model.fp32.onnx"
    x = torch.randn(1, 16000 * 5)
    n = torch.export.Dim("n", min=1600, max=16000 * 60)
    torch.onnx.export(Net(model), (x,), str(fp32), input_names=["input_values"], output_names=["logits"],
                      dynamic_shapes={"input_values": {1: n}}, external_data=False)
    m = onnx.load(fp32)
    onnx.save(float16.convert_float_to_float16(m, keep_io_types=True), out / "model.fp16.onnx")
    quantize_dynamic(fp32, out / "model.int8.onnx", weight_type=QuantType.QInt8, op_types_to_quantize=["MatMul", "Gemm"])


if __name__ == "__main__":
    main(*sys.argv[1:4]) if len(sys.argv) == 4 else sys.exit(__doc__)

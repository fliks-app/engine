#!/usr/bin/env python3
"""Markdown tables from run.sh's JSON lines: python3 report.py <out.jsonl>.
Backends whose label starts with `cpu` are the reference spread (flash attention on/off, every run)."""
import json, statistics as st, sys
from collections import defaultdict

rows = [json.loads(l) for l in open(sys.argv[1])]
for r in rows:
    r["lang"], r["voice"], r["kind"] = r["clip"].split("-", 2)  # clips are named <lang>-<voice>-<kind>


def table(title, key, rows, cols=("model", "backend")):
    g = defaultdict(list)
    for r in rows:
        g[(*(r[c] for c in cols), key(r))].append(r)
    print(f"\n### {title}\n")
    print("| " + " | ".join([*cols, "group", "runs", "words", "mean ms", "median ms", "<=100", "<=250", "<=500", "line start ms", "missing", ">1 s", "wall s"]) + " |")
    print("|" + "---|" * (len(cols) + 11))
    for k in sorted(g):
        rs = g[k]
        errs = [abs(e) for r in rs for e in r["errs"]]
        n = len(errs)
        f = lambda t: f"{100 * sum(e <= t for e in errs) / n:.0f}%"
        print("| " + " | ".join([*k, str(len(rs)), str(n), f"{st.mean(errs):.0f}", f"{st.median(errs):.0f}", f(100), f(250), f(500),
              f"{st.mean(r['lineMeanAbsMs'] for r in rs):.0f}", f"{st.mean(r['missing'] for r in rs):.1f}",
              f"{st.mean(r['misplaced'] for r in rs):.1f}", f"{st.mean(r['wallMs'] for r in rs) / 1000:.1f}"]) + " |")


table("Overall", lambda r: "all", rows)
table("By clip kind", lambda r: r["kind"], rows)
table("By language", lambda r: r["lang"], rows)
table("By voice", lambda r: f"{r['lang']}-{r['voice']}", rows)

print("\n### Per clip: mean abs error ms (words heard by whisper / lyric words)\n")
pc = defaultdict(dict)
for r in rows:
    pc[(r["model"], r["clip"])][r["backend"]] = f"{r['meanAbsMs']:.0f} ({r['heard']}/{r['words']})"
bs = sorted({r["backend"] for r in rows})
print("| model | clip | " + " | ".join(bs) + " |\n|" + "---|" * (len(bs) + 2))
for (m, c), d in sorted(pc.items()):
    print(f"| {m} | {c} | " + " | ".join(d.get(b, "") for b in bs) + " |")

print("\n### Vulkan against the CPU's own spread, per clip and model\n")
print("Spread = the mean abs error of each CPU run (flash attention on and off, every run); a Vulkan run is inside when its mean abs error is within [min - 10 ms, max + 10 ms].\n")
by = defaultdict(lambda: defaultdict(list))
for r in rows:
    by[(r["model"], r["clip"])][r["backend"]].append(r["meanAbsMs"])
print("| model | backend | clips | inside CPU spread | below it | above it | mean (backend - CPU mean) ms | worst excess over CPU max ms |")
print("|---|---|---|---|---|---|---|---|")
for model in sorted({m for m, _ in by}):
    for b in sorted({b for d in by.values() for b in d if not b.startswith("cpu")}):
        ins = lo = hi = 0
        diffs, excess = [], []
        for (m, c), d in by.items():
            if m != model or b not in d:
                continue
            cpu = [v for k, vs in d.items() if k.startswith("cpu") for v in vs]
            for v in d[b]:
                lo += v < min(cpu) - 10
                hi += v > max(cpu) + 10
                ins += min(cpu) - 10 <= v <= max(cpu) + 10
            diffs.append(st.mean(d[b]) - st.mean(cpu))
            excess.append(max(d[b]) - max(cpu))
        print(f"| {model} | {b} | {len(diffs)} | {ins} | {lo} | {hi} | {st.mean(diffs):+.0f} | {max(excess):+.0f} |")

print("\n### Run-to-run: clips whose two runs gave identical word errors\n")
same = defaultdict(lambda: [0, 0])
d = defaultdict(list)
for r in rows:
    d[(r["model"], r["backend"], r["clip"])].append(r["errs"])
for (m, b, c), v in d.items():
    if len(v) > 1:
        same[(m, b)][0] += v[0] == v[1]
        same[(m, b)][1] += 1
print("| model | backend | identical | clips |\n|---|---|---|---|")
for (m, b), (s, n) in sorted(same.items()):
    print(f"| {m} | {b} | {s} | {n} |")

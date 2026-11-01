#!/usr/bin/env python3
"""Records the tool runs of an audio-only corpus file as a probe fixture (`expected` is blessed after).
Usage: tools/record-probe-fixture.py FILE NAME   (env: PROBE_TOOLS, the bin dir of tools/probe-tools.sh)
"""
import json, os, subprocess, sys

tools = os.environ.get("PROBE_TOOLS") or subprocess.check_output(
    [os.path.join(os.path.dirname(__file__), "probe-tools.sh")], text=True).strip()
file, name = sys.argv[1], sys.argv[2]
out_path = os.path.join(os.path.dirname(__file__), "..", "fixtures", "probe", name + ".json")


def run(cmd, args):
    r = subprocess.run([os.path.join(tools, cmd)] + args, capture_output=True, text=True)
    return r.stdout.replace(file, "{path}"), r.stderr


def call(cmd, args, stdout, stderr="", **extra):
    return {"args": [a.replace(file, "{path}") for a in args], "cmd": cmd, "stderr": stderr, "stdout": stdout, **extra}


main_args = ["-v", "error", "-print_format", "json", "-show_streams", "-show_format", "-show_chapters", file]
main_out, main_err = run("ffprobe", main_args)
main = json.loads(main_out)
calls = [call("ffprobe", main_args, main_out, main_err)]

# The audio start run stops once every audio stream showed a packet.
audio = {s["index"] for s in main["streams"] if s["codec_type"] == "audio"}
packet_args = ["-v", "error", "-show_entries", "packet=stream_index,pts_time", "-of", "csv=p=0", file]
lines, seen = [], set()
for line in run("ffprobe", packet_args)[0].splitlines():
    lines.append(line)
    seen.add(int(line.split(",")[0]))
    if audio <= seen:
        break
calls.append(call("ffprobe", packet_args, "\n".join(lines) + "\n", stopped=True))

if "matroska" in main["format"]["format_name"]:
    mkv_out, mkv_err = run("mkvmerge", ["-J", file])
    calls.append(call("mkvmerge", ["-J", file], mkv_out, mkv_err))

fixture = {
    "calls": calls, "expected": {"nulls": []}, "file": os.path.basename(file), "fileSize": os.path.getsize(file),
    "moviehash": None, "name": name, "opts": {"crop": True, "mkvTags": True}, "source": "corpus",
}
with open(out_path, "w") as f:
    json.dump(fixture, f, indent=2, sort_keys=True, ensure_ascii=False)
    f.write("\n")

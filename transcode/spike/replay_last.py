#!/usr/bin/env python3
"""Replay the NVENC agent's last translated ffmpeg command for 15 s into a temp dir and report
the per-stream bitrate of the segments, to see where the bytes go."""
import glob
import os
import json
import subprocess

LAB = os.environ.get("TC_LAB", os.path.expanduser("~/.cache/tcmesh-lab"))
TMP = "/tmp/replay"

line = [l for l in open(os.path.join(LAB, "tc-agent-nv.log")) if " start: " in l][-1]
cmd = json.loads(line.split(" start: ", 1)[1])
os.makedirs(TMP, exist_ok=True)
for f in glob.glob(TMP + "/*"):
    os.remove(f)
out = []
for a in cmd:
    if a.endswith(".m3u8"):
        a = os.path.join(TMP, "p.m3u8")
    elif a.endswith("%d.ts"):
        a = os.path.join(TMP, "p%d.ts")
    out.append(a)
i = out.index("-i")
out = out[:i] + ["-t", "15"] + out[i:]  # limit input read to 15 s
if os.environ.get("EXTRA"):
    j = out.index("-preset")
    out[j:j] = os.environ["EXTRA"].split()
r = subprocess.run(out, capture_output=True, text=True)
print("exit", r.returncode, r.stderr.strip().splitlines()[-1][:150] if r.stderr.strip() else "")
for seg in sorted(glob.glob(TMP + "/p*.ts"), key=os.path.getmtime)[:4]:
    info = subprocess.run(["ffprobe", "-v", "error", "-show_entries", "stream=codec_type,codec_name,width,height,bit_rate:format=duration,bit_rate",
                           "-of", "compact", seg], capture_output=True, text=True).stdout.strip().replace("\n", " | ")
    print(os.path.basename(seg), os.path.getsize(seg) // 1024, "KiB |", info)
print("video opts:", " ".join(a for a in out if a.startswith(("-codec", "-c:", "-b:", "-maxrate", "-bufsize", "-cq", "-rc", "-preset", "-vf", "-map"))))

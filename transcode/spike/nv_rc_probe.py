#!/usr/bin/env python3
"""Which NVENC rate-control flags honour Jellyfin's cap (-maxrate 2.5M -bufsize 5M)?

Encodes 20 s of the lab's HEVC test movie to 720p H.264 on the host GPU with each variant and
prints the resulting video bitrate. Used to pick the agent's -crf translation.
"""
import glob
import os
import subprocess

LAB = os.environ.get("TC_LAB", os.path.expanduser("~/.cache/tcmesh-lab"))
SRC = glob.glob(os.path.join(LAB, "media", "movies", "*", "*.mkv"))[0]
OUT = "/tmp/rc-probe.ts"
CAP = ["-maxrate", "2500000", "-bufsize", "5000000"]
VARIANTS = {
    "cq 23 (first translation)": ["-cq", "23"] + CAP,
    "vbr + b:v 2.5M + cq 23 (current)": ["-rc", "vbr", "-b:v", "2500000", "-cq", "23"] + CAP,
    "vbr + b:v 2.5M, no cq": ["-rc", "vbr", "-b:v", "2500000"] + CAP,
    "cbr 2.5M": ["-rc", "cbr", "-b:v", "2500000"] + CAP,
    "vbr + b:v 2.2M + cq 23 + multipass": ["-rc", "vbr", "-b:v", "2200000", "-cq", "23", "-multipass", "qres"] + CAP,
}

for name, rc in VARIANTS.items():
    cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-hwaccel", "cuda", "-t", "20", "-i", SRC,
           "-vf", "scale=-2:720,format=yuv420p", "-c:v", "h264_nvenc", "-preset", "p2"] + rc + ["-an", "-f", "mpegts", OUT]
    r = subprocess.run(cmd, capture_output=True, text=True)
    kbps = os.path.getsize(OUT) * 8 / 20 / 1000 if r.returncode == 0 else float("nan")
    print(f"{name:40} {kbps:7.0f} kbps {'' if r.returncode == 0 else r.stderr.strip()[:80]}")

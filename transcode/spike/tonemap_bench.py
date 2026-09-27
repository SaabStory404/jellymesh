#!/usr/bin/env python3
"""Run inside a worker pod: GPU vs CPU filter chain on an HDR title, via the agent's translate().

  python3 /code/tonemap_bench.py [seconds] [start]

Encodes the same excerpt twice (Jellyfin's real HDR tonemap command line, GPU chain then CPU
chain) into /transcodes/tm-<kind>-<gpu|cpu>.mkv and reports fps, output colour tags and the
mean luma / saturation of a fixed frame (signalstats) so a broken tonemap shows up as numbers.
"""
import glob
import os
import re
import subprocess
import sys
import time

sys.path.insert(0, "/code")
os.environ.setdefault("TC_LOG", "/dev/null")
import agent  # noqa: E402

SECS = sys.argv[1] if len(sys.argv) > 1 else "20"
START = sys.argv[2] if len(sys.argv) > 2 else "1200"
SRC = glob.glob("/media/movies/sample-c/*.mkv")[0]
FF = agent.FFMPEG
FP = os.path.join(os.path.dirname(FF), "ffprobe")
VF = (r"setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc,"
      r"scale=trunc(min(max(iw\,ih*a)\,1920)/2)*2:trunc(ow/a/2)*2,"
      r"tonemapx=tonemap=bt2390:desat=0:peak=100:t=bt709:m=bt709:p=bt709:format=yuv420p")


def jellyfin_args(out):
    return ["-ss", START, "-t", SECS, "-i", f"file:{SRC}", "-map", "0:0", "-an", "-codec:v:0", "libx264",
            "-preset", "veryfast", "-crf", "23", "-maxrate", "8000000", "-bufsize", "16000000",
            "-vf", VF, "-y", out]


for mode in ("gpu", "cpu"):
    out = f"/transcodes/tm-{agent.KIND}-{mode}.mkv"
    args, used = agent.translate(jellyfin_args(out), gpu_filters=(mode == "gpu"))
    t = time.time()
    r = subprocess.run([FF, "-hide_banner", "-v", "error", "-stats"] + args, capture_output=True, text=True)
    dt = time.time() - t
    fps = re.findall(r"fps=\s*([\d.]+)", r.stderr)
    if r.returncode != 0:
        print(f"{agent.KIND} {mode}: FAILED exit {r.returncode}: {r.stderr.strip().splitlines()[-1:]}")
        continue
    tags = subprocess.run([FP, "-v", "error", "-select_streams", "v:0", "-show_entries",
                           "stream=width,height,pix_fmt,color_transfer,color_primaries", "-of", "csv=p=0", out],
                          capture_output=True, text=True).stdout.strip()
    stats = subprocess.run([FF, "-v", "error", "-ss", "5", "-i", out, "-frames:v", "1", "-vf",
                            "signalstats,metadata=print:file=-", "-f", "null", "-"], capture_output=True, text=True).stdout
    vals = dict(re.findall(r"lavfi\.signalstats\.(YAVG|SATAVG|YMAX)=([\d.]+)", stats))
    print(f"{agent.KIND} {mode}: gpu_chain={used} {float(SECS) * 23.976 / dt:.0f} fps avg "
          f"(last {fps[-1] if fps else '?'}) in {dt:.1f}s | {tags} | "
          f"Yavg={float(vals.get('YAVG', 0)):.1f} Ymax={vals.get('YMAX')} SATavg={float(vals.get('SATAVG', 0)):.1f}")

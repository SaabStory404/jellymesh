#!/usr/bin/env python3
"""Run inside a worker pod: how much GPU memory each kind of transcode work uses.

  python3 /code/vram_bench.py [parallel]

Runs each activity for ~10 s on the 4K DV/HDR10 title and samples memory every 0.2 s:
  nvenc: nvidia-smi (per-process used_memory + card total)
  qsv:   i915 DRM fdinfo of the ffmpeg process (drm-total-* / drm-resident-* per memory region)
With `parallel` N > 1, starts N GPU-chain jobs at once and reports which ones failed.
"""
import glob
import os
import re
import subprocess
import sys
import threading
import time

sys.path.insert(0, "/code")
os.environ.setdefault("TC_LOG", "/dev/null")
import agent  # noqa: E402

# TC_BENCH_TITLE picks the source (default: the 4K DV/HDR10 worst case); SDR titles use the SDR chain
TITLE = os.environ.get("TC_BENCH_TITLE", "Sample C")
SRC = glob.glob(f"/media/movies/{TITLE}/*.mkv")[0]
FF = agent.FFMPEG
HDR_VF = agent.HDR_VF if "Sample C" in TITLE else (
    r"setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709,"
    r"scale=trunc(min(max(iw\,ih*a)\,1920)/2)*2:trunc(ow/a/2)*2,format=yuv420p")


def job(mode):
    """Command line for one activity."""
    base = ["-ss", "1200", "-t", os.environ.get("TC_BENCH_SECS", "10"), "-i", f"file:{SRC}", "-map", "0:0", "-an"]
    if mode == "decode-only (GPU decode, frames to RAM)":
        args, _ = agent.translate(base + ["-codec:v:0", "libx264", "-vf", HDR_VF, "-f", "null", "-"],
                                  gpu_filters=False)
        i = args.index("-codec:v:0")
        return args[:i] + ["-f", "null", "-"]  # decode + download only
    if mode == "encode-only (CPU decode+filters, GPU encode)":
        enc = agent.ENCODERS[agent.KIND]["libx264"]
        init = agent.HWACCEL[agent.KIND]
        init = init[:init.index("-hwaccel")]
        return init + base + ["-vf", HDR_VF, "-c:v", enc, "-f", "null", "-"]
    gpu = mode.startswith("full GPU")
    args, _ = agent.translate(base + ["-codec:v:0", "libx264", "-preset", "veryfast", "-vf", HDR_VF,
                                      "-f", "null", "-"], gpu_filters=gpu)
    return args


def nv_sample(pid):
    apps = subprocess.run(["nvidia-smi", "--query-compute-apps=pid,used_memory", "--format=csv,noheader,nounits"],
                          capture_output=True, text=True).stdout
    total = subprocess.run(["nvidia-smi", "--query-gpu=memory.used", "--format=csv,noheader,nounits"],
                           capture_output=True, text=True).stdout.strip()
    mine = sum(int(m) for p, m in re.findall(r"(\d+),\s*(\d+)", apps) if int(p) == pid)
    return mine, int(total or 0)


def i915_sample(pid):
    """Sum drm memory stats over the process's DRM fds (KiB), per key."""
    out = {}
    for fd in glob.glob(f"/proc/{pid}/fdinfo/*"):
        try:
            txt = open(fd).read()
        except OSError:
            continue
        if "drm-driver" not in txt:
            continue
        for k, v, unit in re.findall(r"^(drm-(?:total|resident)-[\w-]+):\s+(\d+)\s*(\w*)", txt, re.M):
            kib = int(v) * {"": 1 / 1024, "KiB": 1, "MiB": 1024, "GiB": 1024 * 1024}.get(unit, 1)
            out[k] = out.get(k, 0) + kib
    return out


def run(mode, results):
    t0 = time.time()
    p = subprocess.Popen([FF, "-hide_banner", "-v", "error"] + job(mode), stdout=subprocess.DEVNULL,
                         stderr=subprocess.PIPE, text=True)
    peak_mine, peak_total, peak_i915 = 0, 0, {}
    while p.poll() is None:
        if agent.KIND == "nvenc":
            mine, total = nv_sample(p.pid)
            peak_mine, peak_total = max(peak_mine, mine), max(peak_total, total)
        else:
            for k, v in i915_sample(p.pid).items():
                peak_i915[k] = max(peak_i915.get(k, 0), v)
        time.sleep(0.2)
    err = (p.stderr.read() if p.stderr else "").strip().splitlines()
    speed = float(os.environ.get("TC_BENCH_SECS", "10")) / max(time.time() - t0, 0.001)
    results.append((mode, p.returncode, peak_mine, peak_total, peak_i915, err[-1:] if p.returncode else [], speed))


def report(results):
    for mode, code, mine, total, i915, err, speed in results:
        if agent.KIND == "nvenc":
            print(f"{mode:48} exit={code} {speed:4.1f}x realtime card total peak={total} MiB {err}")
        else:
            regions = {k: f"{v / 1024:.0f} MiB" for k, v in sorted(i915.items()) if v}
            print(f"{mode:48} exit={code} {speed:4.1f}x realtime {regions} {err}")


if __name__ == "__main__":
    par = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    if agent.KIND == "nvenc":
        print("idle card total:", nv_sample(-1)[1], "MiB")
    if par == 1:
        res = []
        for mode in ("decode-only (GPU decode, frames to RAM)", "encode-only (CPU decode+filters, GPU encode)",
                     "CPU filters (GPU decode + GPU encode)", "full GPU (decode+scale+tonemap+encode)"):
            run(mode, res)
        report(res)
    else:
        res = []
        ts = [threading.Thread(target=run, args=("full GPU (decode+scale+tonemap+encode)", res)) for _ in range(par)]
        for t in ts:
            t.start()
        for t in ts:
            t.join()
        report(res)

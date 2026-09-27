#!/usr/bin/env python3
"""Transcode worker agent: runs ffmpeg for the Jellyfin shim, adapted to this node's hardware.

Jellyfin emits a software command line (hwaccel "none"). The agent only swaps what differs per
hardware — decoder and encoder — so heterogeneous GPUs all look like the same capability set
(the lowest common denominator: H.264 / HEVC 8-bit). Filters (scale, tonemap) stay on the CPU.

Env:
  TC_NAME     worker name (logs)
  TC_KIND     cpu | nvenc | qsv
  TC_FFMPEG   ffmpeg binary on this worker
  TC_PATHMAP  "/config=/host/config,/media=/host/media"  (Jellyfin path prefix = local path)
  TC_PORT     listen port (default 9901)
  TC_MAX_JOBS concurrent transcodes this card may run (default unlimited); further jobs get "busy"
  TC_CAPACITY resolution-weighted capacity in units (overrides TC_MAX_JOBS): a job costs 1 unit
              (<=1080p source), TC_WEIGHT_1440 (default 2) or TC_WEIGHT_4K (default 3)
  TC_OUTPUTS  optional comma list; restricts the probed outputs (operator knob, never adds)
  TC_FENCE_AFTER  kill ffmpeg after this many seconds without a frame from the shim (default 3;
                  must stay below the shim's TC_DEAD_AFTER)
"""
import json
import os
import socket
import socketserver
import struct
import subprocess
import threading
import time

KIND = os.environ.get("TC_KIND", "cpu")
NAME = os.environ.get("TC_NAME", KIND)
FFMPEG = os.environ.get("TC_FFMPEG", "ffmpeg")
LOG = os.environ.get("TC_LOG", f"/tmp/tc-agent-{NAME}.log")
FENCE_AFTER = float(os.environ.get("TC_FENCE_AFTER", "3"))
HW_FILTERS = os.environ.get("TC_HW_FILTERS", "1") != "0"  # scale/tonemap on the GPU
MAX_JOBS = int(os.environ.get("TC_MAX_JOBS", "0")) or 1000  # concurrent transcodes this card may take
# Resolution-weighted capacity (2026-09-26). A 4K HDR job costs the Arc ~3x a 1080p one
# (MEASURED: 6x 4K = 1.3x realtime vs 12x 1080p = 1.8x realtime), so a plain job count either
# wastes 1080p headroom or overloads on 4K. Each job costs units by source height; the card takes
# jobs while the units in use + the new job's units <= TC_CAPACITY. Unset = TC_MAX_JOBS, weight 1.
CAPACITY = float(os.environ.get("TC_CAPACITY", "0")) or None
WEIGHTS = [(1100, 1.0),                                          # <= 1080p (incl. 1920x1080 with bars)
           (1700, float(os.environ.get("TC_WEIGHT_1440", "2"))),   # 1440p / ultrawide
           (10 ** 6, float(os.environ.get("TC_WEIGHT_4K", "3")))]  # 2160p and up
CAP_LOCK = threading.Lock()
USAGE = {"units": 0.0, "jobs": 0}
# CUDA JIT cache: the worker's HOME (/config) does not exist in the pod, so without this every
# ffmpeg recompiled its CUDA kernels (~12 s on the P4, MEASURED). Warmed at startup by the probe.
os.environ.setdefault("CUDA_CACHE_PATH", "/tmp/cuda-cache")
PATHMAP = [tuple(p.split("=", 1)) for p in os.environ.get("TC_PATHMAP", "").split(",") if "=" in p]

# Jellyfin's software encoder -> this kind's encoder
ENCODERS = {
    "nvenc": {"libx264": "h264_nvenc", "libx265": "hevc_nvenc", "libsvtav1": "av1_nvenc"},
    "qsv": {"libx264": "h264_qsv", "libx265": "hevc_qsv", "libsvtav1": "av1_qsv"},
    "cpu": {"libx264": "libx264", "libx265": "libx265", "libsvtav1": "libsvtav1"},
}
# Output formats a viewer can be handed. Only these need a lowest common denominator across
# workers: a restarted job keeps its arguments, so the replacement must produce the same codec.
# Decode, scaling and tonemapping fall back per job and never restrict what is offered.
# token -> (Jellyfin software encoder, 10-bit?)
OUTPUTS = {"h264": ("libx264", False), "hevc": ("libx265", False), "hevc10": ("libx265", True),
           "av1": ("libsvtav1", False), "av1-10": ("libsvtav1", True)}
# qsv device init copied from prod Jellyfin's own QSV command line (FFmpeg.Transcode log, Arc A380);
# decode through VA-API, frames land in system memory for the CPU filter chain, h264_qsv encodes.
HWACCEL = {"nvenc": ["-hwaccel", "cuda"],
           "qsv": ["-init_hw_device", "vaapi=va:/dev/dri/renderD128,driver=iHD",
                   "-init_hw_device", "qsv=qs@va", "-filter_hw_device", "qs", "-hwaccel", "vaapi"]}
# x264/x265 presets -> NVENC p1 (fastest) .. p7 (slowest)
NV_PRESET = {"ultrafast": "p1", "superfast": "p1", "veryfast": "p2", "faster": "p3", "fast": "p4",
             "medium": "p4", "slow": "p5", "slower": "p6", "veryslow": "p7"}


def log(msg):
    with open(LOG, "a") as f:
        f.write(f"{time.strftime('%H:%M:%S')} {msg}\n")


def map_path_args(args):
    return [map_path(a) for a in args]


def map_path(a):
    for src, dst in PATHMAP:
        a = a.replace(src, dst)  # also inside filter strings (subtitles=..., fonts)
    return a


def split_filters(vf):
    """Split a filter chain on commas that are not escaped (scale expressions use '\\,')."""
    parts, cur, i = [], "", 0
    while i < len(vf):
        if vf[i] == "\\" and i + 1 < len(vf):
            cur += vf[i:i + 2]
            i += 2
            continue
        if vf[i] == ",":
            parts.append(cur)
            cur = ""
        else:
            cur += vf[i]
        i += 1
    return parts + [cur]


def opts(filt):
    """'tonemapx=tonemap=bt2390:peak=100' -> ('tonemapx', {'tonemap': 'bt2390', 'peak': '100'})."""
    name, _, rest = filt.partition("=")
    return name, dict(kv.split("=", 1) for kv in rest.split(":") if "=" in kv) if rest else {}


def hw_filters(vf):
    """Jellyfin's software chain -> this kind's GPU chain, or None to keep the CPU chain.

    Only the shapes Jellyfin emits for plain transcodes are translated (setparams, scale,
    tonemapx, format); anything else (subtitle burn-in, deinterlace, overlays) stays on the CPU.
    Arc: prod Jellyfin's own VA-API chain (procamp b=16 + tonemap_vaapi, then hwmap to QSV for
    the encoder). P4: scale_cuda + tonemap_cuda with Jellyfin's tonemap parameters.
    """
    parts = split_filters(vf)
    names = [opts(p)[0] for p in parts]
    if not set(names) <= {"setparams", "scale", "tonemapx", "format"}:
        return None
    setparams = [p for p in parts if p.startswith("setparams=")]
    scale = next((p[len("scale="):] for p in parts if p.startswith("scale=")), None)
    tone = next((opts(p)[1] for p in parts if p.startswith("tonemapx=")), None)
    out_fmt = next((opts(p)[1].get("format") for p in parts if p.startswith("tonemapx=")), None) or \
        next((p[len("format="):] for p in parts if p.startswith("format=")), "yuv420p")
    ten = "10" in out_fmt
    w = h = ""
    if scale is not None:
        w, _, h = scale.partition(":")  # expressions pass through; hw scalers evaluate iw/ih/a/ow
    if KIND == "qsv":
        chain = list(setparams)
        if scale is not None:
            chain.append(f"scale_vaapi=w={w}:h={h}" + ("" if tone else f":format={'p010' if ten else 'nv12'}")
                         + ":extra_hw_frames=24")
        if tone:
            chain += ["procamp_vaapi=b=16",
                      f"tonemap_vaapi=format={'p010' if ten else 'nv12'}:p={tone.get('p', 'bt709')}"
                      f":t={tone.get('t', 'bt709')}:m={tone.get('m', 'bt709')}:extra_hw_frames=32"]
        if scale is None and not tone:
            chain.append(f"scale_vaapi=format={'p010' if ten else 'nv12'}")
        return ",".join(chain + ["hwmap=derive_device=qsv", "format=qsv"])
    if KIND == "nvenc":
        chain = list(setparams)
        if tone:
            if scale is not None:
                chain.append(f"scale_cuda=w={w}:h={h}")  # keeps p010 for the tonemapper
            keep = ("tonemap", "desat", "peak", "t", "m", "p")
            chain.append("tonemap_cuda=format=" + ("p010" if ten else "yuv420p") + "".join(
                f":{k}={tone[k]}" for k in keep if k in tone))
        else:
            chain.append(f"scale_cuda=w={w}:h={h}:format={'p010' if ten else 'yuv420p'}" if scale is not None
                         else f"scale_cuda=format={'p010' if ten else 'yuv420p'}")
        return ",".join(chain)
    return None


def translate(args, gpu_filters=True):
    """Adapt Jellyfin's software command line to this worker. Returns (args, used GPU filters)."""
    args = [map_path(a) for a in args]
    if KIND == "cpu":
        return args, False
    enc = {k: v for k, v in ENCODERS[KIND].items()}
    hw_vf = None
    if gpu_filters and HW_FILTERS and "-vf" in args:
        hw_vf = hw_filters(args[args.index("-vf") + 1])
    out, i = [], 0
    while i < len(args):
        a = args[i]
        nxt = args[i + 1] if i + 1 < len(args) else None
        if a.startswith("-codec:v") or a.startswith("-c:v") or a == "-vcodec":
            out += [a, enc.get(nxt or "", nxt)]
            i += 2
            continue
        if KIND == "nvenc" and a.startswith("-preset") and nxt in NV_PRESET:
            out += [a, NV_PRESET[nxt]]
            i += 2
            continue
        if a.startswith("-crf"):
            out += (["-cq" + a[4:], nxt] if KIND == "nvenc" else ["-global_quality" + a[4:], nxt])
            i += 2
            continue
        if a.startswith(("-x264opts", "-x264-params", "-x265-params", "-tune")):
            i += 2  # encoder-private options the hardware encoders do not accept
            continue
        if a == "-vf" and hw_vf:
            out += [a, hw_vf]
            i += 2
            continue
        out.append(a)
        i += 1
    # Jellyfin forces a keyframe every segment (-force_key_frames expr:gte(t,n_forced*3)) and its
    # playlist + restart logic assume segment N starts at N*3 s. Hardware encoders only honour that
    # if forced frames are IDR; without it NVENC cut ~10 s segments (MEASURED: 10.46 s vs 3.0 s).
    enc_at = next(j for j, a in enumerate(out) if a.startswith(("-codec:v", "-c:v", "-vcodec")))
    out[enc_at + 2:enc_at + 2] = ["-forced-idr", "1"] if KIND == "nvenc" else ["-forced_idr", "1"]
    # hardware decode; with GPU filters the frames stay on the GPU, otherwise they are downloaded
    # to system memory so the CPU filter chain works
    first_input = out.index("-i")
    dec = HWACCEL[KIND] + (["-hwaccel_output_format", "vaapi" if KIND == "qsv" else "cuda"] if hw_vf else [])
    return out[:first_input] + dec + out[first_input:], bool(hw_vf)


def source_height(args):
    """Height of the job's video input via ffprobe (fast; seconds-bounded). None if unknown."""
    try:
        src = args[args.index("-i") + 1]
    except (ValueError, IndexError):
        return None
    probe = os.path.join(os.path.dirname(FFMPEG), "ffprobe") if os.sep in FFMPEG else "ffprobe"
    try:
        r = subprocess.run([probe, "-v", "error", "-select_streams", "v:0", "-show_entries", "stream=height",
                            "-of", "csv=p=0", src.removeprefix("file:")], capture_output=True, text=True, timeout=10)
        return int(r.stdout.strip().splitlines()[0])
    except (OSError, subprocess.TimeoutExpired, ValueError, IndexError):
        return None


def job_weight(args):
    """Capacity units this job costs; the most expensive weight when the source can't be probed."""
    if CAPACITY is None:
        return 1.0
    h = source_height(args)
    if h is None:
        return WEIGHTS[-1][1]
    return next(w for limit, w in WEIGHTS if h <= limit)


def reserve(weight):
    with CAP_LOCK:
        limit = CAPACITY if CAPACITY is not None else float(MAX_JOBS)
        if USAGE["units"] + weight > limit + 1e-9:
            return False
        USAGE["units"] += weight
        USAGE["jobs"] += 1
        CAPS["active"], CAPS["units_used"] = USAGE["jobs"], USAGE["units"]
        return True


def release(weight):
    with CAP_LOCK:
        USAGE["units"] -= weight
        USAGE["jobs"] -= 1
        CAPS["active"], CAPS["units_used"] = USAGE["jobs"], USAGE["units"]


def first_segment(args):
    """The file Jellyfin waits for: -hls_segment_filename % -start_number."""
    if "-hls_segment_filename" not in args:
        return ""
    pattern = args[args.index("-hls_segment_filename") + 1]
    start = int(args[args.index("-start_number") + 1]) if "-start_number" in args else 0
    return pattern % start


def probe_caps():
    """Test-encode 0.5 s with every output this kind might support; keep what actually works.

    Measured, not declared: a card is only offered for what it encoded here (e.g. the Tesla P4
    lists av1_nvenc in `ffmpeg -encoders` but cannot encode AV1).
    """
    init = HWACCEL.get(KIND, [])
    init = init[:init.index("-hwaccel")] if "-hwaccel" in init else init  # device init only
    works = []
    for token, (sw, ten) in OUTPUTS.items():
        enc = ENCODERS[KIND][sw]
        if KIND == "cpu":
            fmt, prof = ("yuv420p10le" if ten else "yuv420p"), []
        else:
            fmt = "p010le" if ten else "nv12"
            prof = ["-profile:v", "main10"] if ten and token.startswith("hevc") else []
        cmd = [FFMPEG, "-hide_banner", "-v", "error"] + init + [
            "-f", "lavfi", "-i", "testsrc2=s=320x240:d=0.5:r=24", "-vf", f"format={fmt}",
            "-c:v", enc] + prof + ["-f", "null", "-"]
        try:
            r = subprocess.run(cmd, capture_output=True, timeout=30)
            ok = r.returncode == 0
            why = r.stderr.decode(errors="replace").strip().splitlines()[-1:] if not ok else []
        except (OSError, subprocess.TimeoutExpired) as e:
            ok, why = False, [str(e)]
        log(f"[{NAME}/{KIND}] probe {token} ({enc}): {'ok' if ok else 'NO ' + ' '.join(why)[:150]}")
        if ok:
            works.append(token)
    # operator restriction (never an addition): e.g. TC_OUTPUTS=h264 keeps a card out of HEVC
    if os.environ.get("TC_OUTPUTS"):
        allowed = set(os.environ["TC_OUTPUTS"].split(","))
        works = [t for t in works if t in allowed]
    return {"name": NAME, "kind": KIND, "outputs": works, "gpu_tonemap": probe_gpu_tonemap(init),
            "max_jobs": MAX_JOBS, "active": 0, "capacity": CAPACITY or float(MAX_JOBS), "units_used": 0.0,
            "probed": int(time.time())}


HDR_VF = (r"setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc,"
          r"scale=trunc(min(max(iw\,ih*a)\,1920)/2)*2:trunc(ow/a/2)*2,"
          r"tonemapx=tonemap=bt2390:desat=0:peak=100:t=bt709:m=bt709:p=bt709:format=yuv420p")


def probe_gpu_tonemap(init):
    """Run the GPU decode -> scale -> tonemap -> encode chain once on a 1 s HDR10-tagged clip.

    Verifies GPU tonemapping works on this card/driver, and warms the CUDA JIT cache: without a
    warm cache the Tesla P4 spent ~12 s compiling tonemap/scale kernels before the first frame of
    EVERY job (MEASURED), which would land on the first segment after a failover.
    """
    if KIND == "cpu" or not HW_FILTERS:
        return False
    clip = f"/tmp/tc-probe-hdr-{NAME}.mkv"
    enc = ENCODERS[KIND]["libx265"]
    make = [FFMPEG, "-hide_banner", "-v", "error", "-y"] + init + [
        "-f", "lavfi", "-i", "testsrc2=s=1280x720:d=1:r=24", "-vf", "format=p010le",
        "-c:v", enc, "-profile:v", "main10", "-color_primaries", "bt2020", "-color_trc", "smpte2084",
        "-colorspace", "bt2020nc", clip]
    args, gpu = translate(["-i", clip, "-codec:v:0", "libx264", "-preset", "veryfast", "-vf", HDR_VF,
                           "-f", "null", "-"])
    t = time.time()
    try:
        ok = subprocess.run(make, capture_output=True, timeout=60).returncode == 0
        r = subprocess.run([FFMPEG, "-hide_banner", "-v", "error"] + args, capture_output=True, timeout=120) \
            if ok else None
        ok = bool(gpu and r is not None and r.returncode == 0)
        why = "" if ok else (r.stderr.decode(errors="replace").strip().splitlines()[-1:] if r else ["clip"])
    except (OSError, subprocess.TimeoutExpired) as e:
        ok, why = False, [str(e)]
    log(f"[{NAME}/{KIND}] probe gpu tonemap: {'ok' if ok else 'NO ' + str(why)[:150]} ({time.time() - t:.1f}s)")
    return ok


CAPS = {"name": NAME, "kind": KIND, "outputs": [], "probed": 0}


def frame(kind, payload):
    return kind + struct.pack(">I", len(payload)) + payload


def recv_exact(f, n):
    buf = f.read(n)
    return buf if buf is not None and len(buf) == n else None


class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        s = self.request
        f = s.makefile("rb")
        # hello: proves this process is alive (not just its listen socket) and carries what this
        # card can output, for the shim's routing and mesh-sync's lowest common denominator
        s.sendall(frame(b"R", json.dumps(CAPS).encode()))
        try:
            head = recv_exact(f, 5)
        except OSError:
            return  # a probe or mesh-sync read the hello and hung up
        if head is None or head[:1] != b"J":
            return
        body = recv_exact(f, struct.unpack(">I", head[1:])[0])
        if body is None:
            return
        req = json.loads(body)
        # capacity: past a card's measured limit every session on it drops below realtime (Arc 8x
        # 4K = 0.7x) or fails outright (two full-GPU jobs next to a pinned LLM on the P4), so the
        # card only takes a job that fits; B = busy, the shim tries the next worker at once
        weight = job_weight(map_path_args(req["args"]))
        if not reserve(weight):
            s.sendall(frame(b"B", b""))
            log(f"[{NAME}/{KIND}] busy: refused a {weight:g}-unit job "
                f"({USAGE['units']:g}/{CAPACITY or MAX_JOBS:g} units, {USAGE['jobs']} jobs)")
            return
        try:
            s.sendall(frame(b"A", b""))
            log(f"[{NAME}/{KIND}] accepted a {weight:g}-unit job "
                f"({USAGE['units']:g}/{CAPACITY or MAX_JOBS:g} units)")
            self.run_job(s, f, req)
        finally:
            release(weight)

    def run_job(self, s, f, req):
        cwd = map_path(req.get("cwd") or "/")
        lock = threading.Lock()
        last_heard = [time.monotonic()]
        fenced = threading.Event()
        done = threading.Event()
        cur: dict = {"p": None}  # the ffmpeg currently running this job (a CPU-filter re-run replaces it)

        def fence(why):
            # Stop writing into the shared transcode dir before the shim gives up on us and
            # Jellyfin starts a replacement ffmpeg on another worker (TC_FENCE_AFTER < TC_DEAD_AFTER).
            if not fenced.is_set():
                fenced.set()
                p = cur["p"]
                log(f"[{NAME}/{KIND}] fencing: {why}; killing ffmpeg")
                if p is not None and p.poll() is None:
                    p.kill()

        def send(kind, data=b""):
            try:
                with lock:
                    s.sendall(frame(kind, data))
            except OSError:
                if not done.is_set():
                    fence("shim unreachable")

        def pump_in():
            try:
                while True:
                    head = recv_exact(f, 5)
                    if head is None:
                        break
                    kind, length = head[:1], struct.unpack(">I", head[1:])[0]
                    data = recv_exact(f, length) if length else b""
                    if data is None:
                        break
                    last_heard[0] = time.monotonic()
                    p = cur["p"]
                    if p is None or p.stdin is None:
                        continue
                    if kind == b"I":
                        p.stdin.write(data)
                        p.stdin.flush()
                    elif kind == b"C":
                        p.stdin.close()
            except (OSError, ValueError):
                pass
            if not done.is_set():
                fence("shim closed the connection")

        def watchdog():
            # never touches the socket: a partitioned send can block with the lock held
            while not done.is_set():
                time.sleep(0.25)
                if time.monotonic() - last_heard[0] > FENCE_AFTER:
                    fence(f"no frame from shim for {FENCE_AFTER:.0f}s")
                    return

        def heartbeat():
            while not done.wait(1.0):
                send(b"H")

        def pump(stream, kind):
            for chunk in iter(lambda: stream.read1(4096), b""):
                send(kind, chunk)

        def run(args):
            log(f"[{NAME}/{KIND}] start: {json.dumps([FFMPEG] + args)}")
            p = subprocess.Popen([FFMPEG] + args, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, cwd=cwd if os.path.isdir(cwd) else None)
            cur["p"] = p
            if fenced.is_set():
                p.kill()
            t_err = threading.Thread(target=pump, args=(p.stderr, b"E"), daemon=True)
            t_out = threading.Thread(target=pump, args=(p.stdout, b"O"), daemon=True)
            t_err.start()
            t_out.start()
            code = p.wait()
            t_err.join(2)
            t_out.join(2)
            return code

        threading.Thread(target=pump_in, daemon=True).start()
        threading.Thread(target=watchdog, daemon=True).start()
        threading.Thread(target=heartbeat, daemon=True).start()
        args, gpu = translate(req["args"])
        code = run(args)
        if code != 0 and gpu and not fenced.is_set() and not os.path.exists(first_segment(args)):
            # The GPU filter chain failed before producing anything (an unusual source, a filter
            # this card or driver rejects). Re-run with the CPU chain on this same worker; Jellyfin
            # is still waiting for the first segment and never sees the failed attempt.
            log(f"[{NAME}/{KIND}] GPU filters failed (exit {code}) before the first segment; "
                f"re-running with CPU filters")
            args, _ = translate(req["args"], gpu_filters=False)
            code = run(args)
        done.set()
        if not fenced.is_set():
            send(b"X", struct.pack(">i", code))
        log(f"[{NAME}/{KIND}] exit {code}")


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


if __name__ == "__main__":
    port = int(os.environ.get("TC_PORT", "9901"))
    CAPS = probe_caps()  # before listening: not ready (no hello) until the card is characterised
    log(f"[{NAME}/{KIND}] listening on :{port} ffmpeg={FFMPEG} pathmap={PATHMAP} outputs={CAPS['outputs']}")
    Server(("0.0.0.0", port), Handler).serve_forever()

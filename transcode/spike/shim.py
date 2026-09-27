#!/usr/bin/env python3
"""ffmpeg shim for Jellyfin: run HLS transcodes on a pool of workers, everything else locally.

Installed as /usr/lib/jellyfin-ffmpeg/ffmpeg (the real binary moves to ffmpeg.real).
Jellyfin runs with hardware acceleration "none", so it always emits a portable software
command line; each worker swaps in its own encoder/decoder (see agent.py).

Failover contract (MEASURED from Jellyfin 12.0 source, DynamicHlsController.cs):
when this process exits, Jellyfin's next segment request finds the job exited, treats the
transcode index as unknown, and starts a new ffmpeg (= a new shim) at that segment. So on a
lost worker we delete the newest segment (it may be partial — Jellyfin would serve it as-is)
and exit non-zero; the restart picks another worker.

Lost = the connection closed/reset (worker process died) OR no frame at all from the worker
for TC_DEAD_AFTER seconds (node died / network partition: no RST ever arrives). The agent
sends a heartbeat every second while ffmpeg runs, and fences itself — kills its ffmpeg — when
it hears nothing from us for TC_FENCE_AFTER (< TC_DEAD_AFTER), so by the time we give up on a
worker it has stopped writing into the shared transcode dir.

Wire format, both directions: 1-byte kind + 4-byte big-endian length + payload.
  shim -> agent: I stdin bytes, C stdin closed, H heartbeat
  agent -> shim: E stderr, O stdout, X exit code, H heartbeat

Env: TC_WORKERS="qsv=tc-worker-qsv:9901,nv=tc-worker-nv:9901,cpu=tc-worker-cpu:9901" (priority)
"""
import json
import os
import socket
import struct
import sys
import threading
import time

REAL = "/usr/lib/jellyfin-ffmpeg/ffmpeg.real"
LOG = os.environ.get("TC_SHIM_LOG", "/config/log/tc-shim.log")
DEAD_AFTER = float(os.environ.get("TC_DEAD_AFTER", "6"))


def log(msg):
    try:
        with open(LOG, "a") as f:
            f.write(f"{time.strftime('%H:%M:%S')} [{os.getpid()}] {msg}\n")
    except OSError:
        pass


def is_hls_transcode(args):
    return any(a.endswith(".m3u8") for a in args) and "-i" in args


def workers():
    out = []
    for item in os.environ.get("TC_WORKERS", "").split(","):
        if "=" in item:
            name, addr = item.split("=", 1)
            host, port = addr.rsplit(":", 1)
            out.append((name.strip(), host.strip(), int(port)))
    return out


def required_output(args):
    """The output token this job needs (see agent.OUTPUTS), or None for a stream copy."""
    codec = next((args[i + 1] for i, a in enumerate(args[:-1]) if a.startswith(("-codec:v", "-c:v"))), None)
    base = {"libx264": "h264", "libx265": "hevc", "libsvtav1": "av1"}.get(codec or "")
    if base is None:
        return None
    joined = " ".join(args)
    ten = any(m in joined for m in ("yuv420p10", "p010", "main10"))
    return {"hevc": "hevc10", "av1": "av1-10"}.get(base, base) if ten else base


def hello(s):
    """Read the agent's hello frame: its capabilities (JSON)."""
    head = recv_exact(s, 5)
    if head is None or head[:1] != b"R":
        raise OSError("bad hello")
    length = struct.unpack(">I", head[1:])[0]
    body = recv_exact(s, length) if length else b"{}"
    if body is None:
        raise OSError("bad hello")
    return json.loads(body or b"{}")


def connect(args, skip=(), need=None):
    """Hand the job to the first worker that is alive, can produce `need`, and has a free slot.

    A TCP connect alone proves nothing: the kernel completes the handshake into the listen
    backlog even when the agent is frozen (MEASURED: a SIGSTOPped worker still passed a tcpSocket
    readiness probe and accepted the shim). Require the agent's hello frame, then its accept (A)
    of the job; B = busy (at its TC_MAX_JOBS), try the next worker before any work starts.
    """
    for name, host, port in workers():
        if name in skip:
            continue
        try:
            s = socket.create_connection((host, port), timeout=1.5)
            caps = hello(s)
            if need and need not in caps.get("outputs", []):
                s.close()
                log(f"worker {name} cannot output {need} (has {caps.get('outputs')}); skipping")
                continue
            s.sendall(frame(b"J", json.dumps({"args": args, "cwd": os.getcwd()}).encode()))
            answer = recv_exact(s, 5)
            if answer is None or answer[:1] != b"A":
                s.close()
                log(f"worker {name} busy ({caps.get('units_used', caps.get('active'))}/{caps.get('capacity', caps.get('max_jobs'))} units); skipping")
                continue
            s.settimeout(DEAD_AFTER)
            return name, s
        except (OSError, ValueError) as e:
            log(f"worker {name} {host}:{port} unreachable: {e}")
    return None, None


def frame(kind, payload=b""):
    return kind + struct.pack(">I", len(payload)) + payload


def recv_exact(s, n):
    buf = b""
    while len(buf) < n:
        chunk = s.recv(n - len(buf))
        if not chunk:
            return None
        buf += chunk
    return buf


def newest_segment(args):
    """Path of the most recently written HLS segment for this job's output dir."""
    playlist = next(a for a in args if a.endswith(".m3u8"))
    folder, base = os.path.dirname(playlist), os.path.splitext(os.path.basename(playlist))[0]
    segs = [os.path.join(folder, f) for f in os.listdir(folder)
            if f.startswith(base) and not f.endswith(".m3u8")]
    return max(segs, key=os.path.getmtime) if segs else None


def first_segment(args):
    """The file Jellyfin's StartFfMpeg waits for: -hls_segment_filename % -start_number."""
    pattern = args[args.index("-hls_segment_filename") + 1]
    start = int(args[args.index("-start_number") + 1]) if "-start_number" in args else 0
    return pattern % start


def run_on(s, args, stdin_target):
    """Run the job on one connected worker. Returns (exit code, or None if the worker was lost; reason)."""
    lock = threading.Lock()
    done = threading.Event()

    def send(kind, payload=b""):
        with lock:
            s.sendall(frame(kind, payload))

    stdin_target[0] = send  # the job itself (J) was sent and accepted in connect()

    def heartbeat():
        while not done.wait(1.0):
            try:
                send(b"H")
            except OSError:
                return

    threading.Thread(target=heartbeat, daemon=True).start()
    try:
        while True:
            head = recv_exact(s, 5)
            if head is None:
                return None, "connection closed"
            kind, length = head[:1], struct.unpack(">I", head[1:])[0]
            payload = recv_exact(s, length) if length else b""
            if payload is None:
                return None, "connection closed"
            if kind == b"E":
                os.write(2, payload)
            elif kind == b"O":
                os.write(1, payload)
            elif kind == b"X":
                return struct.unpack(">i", payload)[0], "exited"
    except socket.timeout:
        return None, f"no frame for {DEAD_AFTER:.0f}s"
    except OSError as e:
        return None, f"connection error: {e}"
    finally:
        done.set()
        stdin_target[0] = None
        s.close()


def main():
    args = sys.argv[1:]
    if not is_hls_transcode(args):
        os.execv(REAL, [REAL] + args)

    stdin_target = [None]  # send() of the worker currently running the job

    def pump_stdin():
        # Jellyfin stops ffmpeg by writing "q" to stdin and throttles it with "p"/"u".
        while True:
            try:
                data = os.read(0, 4096)
            except OSError:
                return
            send = stdin_target[0]
            try:
                if send:
                    send(b"I", data) if data else send(b"C")
            except OSError:
                pass
            if not data:
                return

    threading.Thread(target=pump_stdin, daemon=True).start()
    first = first_segment(args)
    need = required_output(args)
    tried = set()
    while True:
        name, s = connect(args, tried, need)
        if s is None:
            log(f"no worker reachable (tried {sorted(tried) or 'none'}); running locally")
            os.execv(REAL, [REAL] + args)
        tried.add(name)
        log(f"transcode -> worker {name}: {' '.join(args)}")
        exit_code, reason = run_on(s, args, stdin_target)
        if exit_code != 0 and not os.path.exists(first):
            # Failed before the first segment (worker lost, or its ffmpeg exited non-zero). Exiting
            # non-zero now would wedge the session: Jellyfin 12 throws a 500 but leaves the job
            # registered with ActiveRequestCount=1, so every retry spins forever in
            # WaitForActiveTranscodingRequests (MEASURED; TranscodeManager.cs / DynamicHlsController.cs).
            # Jellyfin is still waiting for this file with no timeout, so re-run the identical job on
            # the next worker (then locally); a lost worker fenced itself (lease).
            log(f"worker {name} failed before the first segment ({reason}, exit {exit_code}); "
                f"re-running on another worker")
            continue
        if exit_code is not None:
            log(f"worker {name} finished with exit code {exit_code}")
            # os._exit: a daemon thread blocked on Jellyfin's stdin must not delay the exit Jellyfin
            # waits for (MEASURED: sys.exit lingered until stdin closed)
            os._exit(exit_code & 0xFF)
        seg = newest_segment(args)
        if seg:
            try:
                os.remove(seg)
            except OSError:
                pass
        log(f"worker {name} LOST mid-transcode ({reason}); removed possibly-partial {seg}; "
            f"exiting 255 so Jellyfin restarts")
        os._exit(255)


if __name__ == "__main__":
    try:
        main()
    except Exception as e:  # noqa: BLE001 — a shim traceback is a non-zero exit, i.e. the wedge above
        log(f"shim error ({e!r}); running locally")
        os.execv(REAL, [REAL] + sys.argv[1:])

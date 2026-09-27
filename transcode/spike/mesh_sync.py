#!/usr/bin/env python3
"""mesh-sync: keep Jellyfin's codec offers at the lowest common denominator of every worker.

Runs next to Jellyfin (sidecar). Every TC_SYNC_EVERY seconds it reads each configured worker's
capabilities from its hello frame (agent.probe_caps: output formats the card actually encoded at
startup), computes the intersection over ALL configured workers, and writes Jellyfin's encoding
options so clients are only offered outputs that any worker can take over mid-stream.

Why all configured workers, not just the live ones: a session started while only the Arc is up
must still be able to fail over to the P4 later, so the P4's limits apply even while it is down.
The last capabilities seen per worker are kept in TC_CAPS_FILE; a worker never seen counts as
H.264-only until it reports (conservative: offering less never breaks a stream).

Only output formats are constrained. Decode, scaling and tonemapping fall back per job on each
worker, so they never shrink what viewers are offered.

Env: TC_WORKERS (same as the shim), JF_URL (default http://127.0.0.1:8096), JF_API_KEY,
     TC_CAPS_FILE (default /config/tc-mesh-caps.json), TC_SYNC_EVERY (default 30), TC_SYNC_ONCE,
     TC_TRANSCODE_DIR (keeps Jellyfin's .jellyfin-transcode marker there, see keep_marker)
"""
import json
import os
import socket
import struct
import threading
import time
import urllib.request
from typing import Any

JF = os.environ.get("JF_URL", "http://127.0.0.1:8096")
KEY = os.environ.get("JF_API_KEY", "")
CAPS_FILE = os.environ.get("TC_CAPS_FILE", "/config/tc-mesh-caps.json")
EVERY = float(os.environ.get("TC_SYNC_EVERY", "30"))
TRANSCODE_DIR = os.environ.get("TC_TRANSCODE_DIR", "")
UNKNOWN = ["h264"]
# Jellyfin encoding option -> output token that must be common to every worker to enable it
OFFERS = {"AllowHevcEncoding": "hevc", "AllowAv1Encoding": "av1"}


def log(msg):
    print(f"{time.strftime('%H:%M:%S')} {msg}", flush=True)


def workers():
    out = []
    for item in os.environ.get("TC_WORKERS", "").split(","):
        if "=" in item:
            name, addr = item.split("=", 1)
            host, port = addr.rsplit(":", 1)
            out.append((name.strip(), host.strip(), int(port)))
    return out


def recv_exact(s, n):
    buf = b""
    while len(buf) < n:
        chunk = s.recv(n - len(buf))
        if not chunk:
            return None
        buf += chunk
    return buf


def read_caps(host, port):
    with socket.create_connection((host, port), timeout=2) as s:
        s.settimeout(2)
        head = recv_exact(s, 5)
        if head is None or head[:1] != b"R":
            raise OSError("bad hello")
        body = recv_exact(s, struct.unpack(">I", head[1:])[0])
        return json.loads(body or b"{}")


def jf(method, path, body=None) -> Any:
    req = urllib.request.Request(JF + path, json.dumps(body).encode() if body is not None else None,
                                 {"Authorization": f'MediaBrowser Token="{KEY}"',
                                  "Content-Type": "application/json"}, method=method)
    with urllib.request.urlopen(req, timeout=15) as r:
        data = r.read()
    return json.loads(data) if data else None


def load():
    try:
        with open(CAPS_FILE) as f:
            return json.load(f)
    except (OSError, ValueError):
        return {}


def save(state):
    tmp = CAPS_FILE + ".tmp"
    with open(tmp, "w") as f:
        json.dump(state, f, indent=1)
    os.replace(tmp, CAPS_FILE)


def sync(state):
    known = state.setdefault("workers", {})
    live = []
    for name, host, port in workers():
        try:
            caps = read_caps(host, port)
            if known.get(name, {}).get("outputs") != caps.get("outputs"):
                log(f"worker {name}: outputs {caps.get('outputs')} (was {known.get(name, {}).get('outputs')})")
            known[name] = {**caps, "seen": int(time.time())}
            live.append(name)
        except (OSError, ValueError) as e:
            if name not in known:
                log(f"worker {name} never seen ({e}); counting it as {UNKNOWN} until it reports")
    configured = [n for n, _, _ in workers()]
    sets = [set(known.get(n, {}).get("outputs", UNKNOWN)) for n in configured]
    common = sorted(set.intersection(*sets)) if sets else []
    if "h264" not in common:
        log(f"WARNING: not every worker can output h264 ({ {n: known.get(n, {}).get('outputs') for n in configured} })")
    state.update({"common": common, "live": live, "synced": int(time.time())})
    save(state)

    enc = jf("GET", "/System/Configuration/encoding")
    changes = {}
    for option, token in OFFERS.items():
        want = token in common
        if enc.get(option) != want:
            lacking = [n for n in configured if token not in known.get(n, {}).get("outputs", UNKNOWN)]
            why = f"every worker outputs {token}" if want else f"{', '.join(lacking)} cannot output {token}"
            changes[option] = want
            log(f"offer {option}: {enc.get(option)} -> {want} ({why})")
    if changes:
        enc.update(changes)
        jf("POST", "/System/Configuration/encoding", enc)
    return common


def keep_marker():
    """Keep Jellyfin's transcode-dir marker present.

    Jellyfin deletes every file in the transcode dir at startup, marker included
    (TranscodeManager.DeleteEncodedMediaCache), and each transcode re-creates a missing marker
    with an exclusive open (FileShare.None). Two transcodes starting together then race and one
    gets HTTP 500 "being used by another process" — MEASURED on the NFS scratch, where the window
    is a network round trip wide. A plain create here (no lock) closes the window within 1 s.
    """
    marker = os.path.join(TRANSCODE_DIR, ".jellyfin-transcode")
    while True:
        try:
            if os.path.isdir(TRANSCODE_DIR) and not os.path.exists(marker):
                os.close(os.open(marker, os.O_CREAT | os.O_WRONLY, 0o664))
                log(f"re-created {marker} (Jellyfin wipes it at startup)")
        except OSError as e:
            log(f"marker: {e}")
        time.sleep(1)


def main():
    if TRANSCODE_DIR:
        threading.Thread(target=keep_marker, daemon=True).start()
    state = load()
    while True:
        try:
            common = sync(state)
            if os.environ.get("TC_SYNC_ONCE"):
                print(json.dumps({"common": common, "workers": state["workers"]}, indent=1))
                return
        except Exception as e:  # noqa: BLE001 — keep syncing; Jellyfin may be restarting
            log(f"sync failed: {e!r}")
        time.sleep(EVERY)


if __name__ == "__main__":
    main()

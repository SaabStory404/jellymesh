#!/usr/bin/env python3
"""HLS player stand-in for the transcode-mesh lab.

  hls_client.py [seconds] [kill-after-seconds]

Requests an H.264 HLS stream of the HEVC test movie (forces a transcode), fetches segments at
playback pace with a ~12 s buffer, retries a failed segment like hls.js (1 s apart), and
optionally kills the NVENC worker mid-stream. Reports per-segment failures, the lowest buffer
level (negative = the viewer would have seen a stall), and which worker the shim used.
"""
import os
import re
import subprocess
import sys
import time
import uuid

import requests

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "jellymesh"))
os.environ.setdefault("JM_A", "http://localhost:18300")
from jm import A, auth_header, login  # noqa: E402

LAB = os.environ.get("TC_LAB", os.path.expanduser("~/.cache/tcmesh-lab"))
HERE = os.path.dirname(os.path.abspath(__file__))
BUFFER_TARGET = 12.0


def produced():
    """Segments ffmpeg has written for the newest transcode (how far ahead of the player it is)."""
    import glob
    lists = sorted(glob.glob(os.path.join(LAB, "transcodes", "*.m3u8")), key=os.path.getmtime)
    if not lists:
        return 0
    return len(glob.glob(lists[-1][:-5] + "*.ts"))


def main():
    total = float(sys.argv[1]) if len(sys.argv) > 1 else 60
    kill_after = float(sys.argv[2]) if len(sys.argv) > 2 else None
    tok, uid = login(A, device_id="tc-player")
    h = auth_header(tok, "tc-player")
    movie = requests.get(A + "/Items", headers=h, params={"Recursive": "true", "IncludeItemTypes": "Movie",
                         "Fields": "MediaSources"}, timeout=30).json()["Items"][0]
    msid = movie["MediaSources"][0]["Id"]
    ps = uuid.uuid4().hex
    params = {"MediaSourceId": msid, "VideoCodec": os.environ.get("TC_VCODEC", "h264"), "AudioCodec": "aac", "SegmentContainer": "ts",
              "MaxStreamingBitrate": 3000000, "VideoBitrate": 2500000, "AudioBitrate": 128000,
              "TranscodingMaxAudioChannels": 2, "PlaySessionId": ps, "DeviceId": "tc-player"}
    master = requests.get(f"{A}/Videos/{movie['Id']}/master.m3u8", headers=h, params=params, timeout=60)
    master.raise_for_status()
    variant = next(line for line in master.text.splitlines() if line and not line.startswith("#"))
    base = f"{A}/Videos/{movie['Id']}/"
    playlist = requests.get(base + variant, headers=h, timeout=60).text
    durs = [float(x) for x in re.findall(r"#EXTINF:([\d.]+)", playlist)]
    segs = [line for line in playlist.splitlines() if line and not line.startswith("#")]
    print(f"'{movie['Name']}': {len(segs)} segments of ~{durs[0]:.0f}s, session {ps[:8]}")

    shim_log = os.path.join(LAB, "config", "log", "tc-shim.log")
    log_start = os.path.getsize(shim_log) if os.path.exists(shim_log) else 0

    t0 = time.monotonic()
    have = 0.0          # seconds of video downloaded
    min_buffer = 99.0
    failures = 0
    sizes = []
    killed = False
    i = 0
    while i < len(segs) and time.monotonic() - t0 < total:
        elapsed = time.monotonic() - t0
        if kill_after is not None and not killed and elapsed >= kill_after:
            subprocess.run([os.path.join(HERE, "tc-lab.sh"), "nv-kill"], check=True)
            killed = True
            print(f"{elapsed:6.1f}s  KILLED the NVENC worker (agent + its ffmpeg); player at segment {i}, "
                  f"ffmpeg had written {produced()} segments")
        if have - elapsed > BUFFER_TARGET:
            time.sleep(0.2)
            continue
        ts = time.monotonic()
        try:
            r = requests.get(base + segs[i], headers=h, timeout=60)
            ok = r.status_code == 200 and len(r.content) > 1000
        except requests.RequestException:
            ok = False
            r = None
        if not ok:
            failures += 1
            code = r.status_code if r is not None else "conn"
            print(f"{time.monotonic() - t0:6.1f}s  segment {i} failed ({code}); retrying in 1 s")
            time.sleep(1)
            continue
        if i > 0:
            min_buffer = min(min_buffer, have - (time.monotonic() - t0))
        took = time.monotonic() - ts
        if took > 2 or i < 2:
            print(f"{time.monotonic() - t0:6.1f}s  segment {i} ok in {took:.2f}s ({len(r.content) // 1024} KiB)")
        sizes.append(len(r.content) if r is not None else 0)
        have += durs[i]
        i += 1

    requests.delete(f"{A}/Videos/ActiveEncodings", headers=h, params={"deviceId": "tc-player", "playSessionId": ps}, timeout=30)
    print(f"played {i} segments ({have:.0f}s of video) in {time.monotonic() - t0:.0f}s; "
          f"failed requests={failures}; lowest buffer={min_buffer:.1f}s (negative = visible stall)")
    later = sizes[2:] or sizes
    if later:
        print(f"   delivered bitrate (segments 2+): {sum(later) * 8 / (len(later) * durs[0]) / 1000:.0f} kbps (assumes {durs[0]:.0f} s segments)")
    if os.path.exists(shim_log):
        with open(shim_log) as f:
            f.seek(log_start)
            for line in f:
                if "transcode -> worker" in line:
                    print("   shim:", line.split(": -", 1)[0].strip()[:120])
                elif "LOST" in line or "unreachable" in line or "running locally" in line or "finished" in line:
                    print("   shim:", line.strip()[:160])


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Average bitrate and codec of the most recent transcode sessions in the lab's transcode dir.

  seg_stats.py [sessions=2] [segment-seconds=3]
"""
import glob
import os
import subprocess
import sys

LAB = os.environ.get("TC_LAB", os.path.expanduser("~/.cache/tcmesh-lab"))


def main():
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 2
    seg_s = float(sys.argv[2]) if len(sys.argv) > 2 else 3.0
    playlists = sorted(glob.glob(os.path.join(LAB, "transcodes", "*.m3u8")), key=os.path.getmtime, reverse=True)[:n]
    for p in playlists:
        base = p[:-5]
        segs = sorted(glob.glob(base + "*.ts"), key=lambda f: int(f[len(base):-3]))
        if not segs:
            continue
        sizes = [os.path.getsize(f) for f in segs]
        probe = subprocess.run(["ffprobe", "-v", "error", "-select_streams", "v:0", "-show_entries",
                                "stream=codec_name,width,height", "-of", "csv=p=0", segs[min(5, len(segs) - 1)]],
                               capture_output=True, text=True).stdout.strip()
        kbps = sum(sizes) * 8 / (len(segs) * seg_s) / 1000
        later = sizes[2:] or sizes
        print(f"{os.path.basename(base)[:12]}: {len(segs)} segments, avg {kbps:.0f} kbps "
              f"(segments 2+: {sum(later) * 8 / (len(later) * seg_s) / 1000:.0f} kbps), video {probe}")


if __name__ == "__main__":
    main()

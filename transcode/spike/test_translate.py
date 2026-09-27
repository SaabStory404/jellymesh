#!/usr/bin/env python3
"""Offline check of agent.translate() on Jellyfin 12.0's real software command lines (tc-lab logs).

  python3 test_translate.py        prints each kind's translation and asserts the essentials
"""
import importlib
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

SCALE = r"scale=trunc(min(max(iw\,ih*a)\,1920)/2)*2:trunc(ow/a/2)*2"
SDR = f"setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709,{SCALE},format=yuv420p"
HDR = (f"setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc,{SCALE},"
       "tonemapx=tonemap=bt2390:desat=0:peak=100:t=bt709:m=bt709:p=bt709:format=yuv420p")
SUBS = SDR + ",subtitles=f=/x.srt"


def job(vf, codec="libx264"):
    return ["-analyzeduration", "200M", "-i", "file:/media/movies/x.mkv", "-codec:v:0", codec,
            "-preset", "veryfast", "-crf", "23", "-vf", vf, "-f", "hls", "-hls_time", "3",
            "-start_number", "0", "-hls_segment_filename", "/transcodes/abc%d.ts", "-y", "/transcodes/abc.m3u8"]


def load(kind):
    os.environ["TC_KIND"] = kind
    os.environ["TC_LOG"] = "/dev/null"
    import agent
    return importlib.reload(agent)


def vf_of(args):
    return args[args.index("-vf") + 1]


for kind in ("qsv", "nvenc", "cpu"):
    a = load(kind)
    for label, vf in (("SDR", SDR), ("HDR", HDR), ("SUBS", SUBS)):
        out, gpu = a.translate(job(vf))
        print(f"{kind:5} {label:4} gpu={gpu!s:5} {vf_of(out)}")
        if kind == "cpu":
            assert not gpu and vf_of(out) == vf
        elif label == "SUBS":
            assert not gpu and vf_of(out) == vf and "-hwaccel_output_format" not in out
        else:
            assert gpu and "-hwaccel_output_format" in out
            assert (label == "HDR") == ("tonemap_" in vf_of(out))
            assert ("hwmap=derive_device=qsv" in vf_of(out)) == (kind == "qsv")
            assert r"iw\,ih*a" in vf_of(out)  # scale expression passed through intact
        cpu_out, cpu_gpu = a.translate(job(vf), gpu_filters=False)
        assert not cpu_gpu and vf_of(cpu_out) == vf
print("ok")

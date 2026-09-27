#!/usr/bin/env python3
"""Configure the transcode lab's Jellyfin: software pipeline + lowest-common-denominator codecs.

HardwareAccelerationType=none -> Jellyfin emits a portable software command line; the worker
agents swap in their own hardware encoder/decoder. AV1 and HEVC *encoding* are off because the
Tesla P4 (Pascal) cannot encode AV1 and its 10-bit HEVC encode is unconfirmed.
"""
import os
import sys

import requests

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "jellymesh"))
os.environ.setdefault("JM_A", "http://localhost:18300")
from jm import A, auth_header, login  # noqa: E402

SETTINGS = {
    "HardwareAccelerationType": "none",
    "TranscodingTempPath": "/transcodes",
    "AllowAv1Encoding": False,
    "AllowHevcEncoding": False,
    "EnableThrottling": True,
    "ThrottleDelaySeconds": 12,
    "EnableSegmentDeletion": False,
}


def main():
    tok, _ = login(A)
    h = auth_header(tok)
    enc = requests.get(A + "/System/Configuration/encoding", headers=h, timeout=30).json()
    enc.update(SETTINGS)
    requests.post(A + "/System/Configuration/encoding", headers=h, json=enc, timeout=30).raise_for_status()
    now = requests.get(A + "/System/Configuration/encoding", headers=h, timeout=30).json()
    print({k: now.get(k) for k in SETTINGS})
    movies = requests.get(A + "/Items", headers=h, params={"Recursive": "true", "IncludeItemTypes": "Movie"},
                          timeout=30).json()["Items"]
    print("movies:", [m["Name"] for m in movies])


if __name__ == "__main__":
    main()

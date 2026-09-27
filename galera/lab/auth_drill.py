#!/usr/bin/env python3
"""Login sessions across Jellyfin nodes on one database.

    auth_drill.py <login-node-url> <other-url> [<other-url> ...]

Creates a throwaway user (admin API key), logs in on the first node, uses the session token on
every other node, logs out on the first node, and checks the token is refused everywhere.
"""
import sys
import time
import uuid

import requests

KEY = 'MediaBrowser Token="jmlabkey0000000000000000000000001"'
CLIENT = 'MediaBrowser Client="drill", Device="drill", DeviceId="{dev}", Version="1.0"'
login, others = sys.argv[1], sys.argv[2:]
name, pw, dev = "drill-" + uuid.uuid4().hex[:6], "drill-" + uuid.uuid4().hex, uuid.uuid4().hex

u = requests.post(f"{login}/Users/New", headers={"Authorization": KEY}, json={"Name": name, "Password": pw}, timeout=30)
u.raise_for_status()
uid = u.json()["Id"]
try:
    r = requests.post(f"{login}/Users/AuthenticateByName", headers={"Authorization": CLIENT.format(dev=dev)},
                      json={"Username": name, "Pw": pw}, timeout=30)
    r.raise_for_status()
    auth = CLIENT.format(dev=dev) + f', Token="{r.json()["AccessToken"]}"'
    t0 = time.perf_counter()
    for url in [login] + others:
        code = requests.get(f"{url}/Users/Me", headers={"Authorization": auth}, timeout=30).status_code
        print(f"  token from {login} on {url}: {code} ({(time.perf_counter() - t0) * 1000:.0f} ms after login)")
    requests.post(f"{login}/Sessions/Logout", headers={"Authorization": auth}, timeout=30).raise_for_status()
    for url in [login] + others:
        code = requests.get(f"{url}/Users/Me", headers={"Authorization": auth}, timeout=30).status_code
        print(f"  after logout on {login}, {url}: {code} ({'refused' if code == 401 else 'STILL ACCEPTED'})")
finally:
    requests.delete(f"{login}/Users/{uid}", headers={"Authorization": KEY}, timeout=30)

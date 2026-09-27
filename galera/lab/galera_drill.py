#!/usr/bin/env python3
"""Galera drills against two Jellyfin nodes (jf-galera.sh) on the 3-node PXC lab.

    galera_drill.py consistency <url-a> <url-b>
        Toggle an item's played state on A; check the row on another Galera node straight away,
        and what B's API reports (B has its own in-process caches: no JellyMesh plugin here).
    galera_drill.py failover <url> <galera-container> [seconds]
        Hammer <url> with small reads, SIGKILL <galera-container> 5 s in, report every failed
        request and the longest gap, then restart the container and wait for it to rejoin.
"""
import os
import subprocess
import sys
import time

import requests

TOKEN = "jmlabkey0000000000000000000000001"
H = {"Authorization": f'MediaBrowser Token="{TOKEN}"'}


def get(base, path, **params):
    r = requests.get(base + path, headers=H, params=params, timeout=30)
    r.raise_for_status()
    return r.json()


def sql(container, q):
    r = subprocess.run(["podman", "exec", container, "mysql", "-uroot", "-plabroot", "-N", "jellyfin", "-e", q],
                       capture_output=True, text=True)
    return r.stdout.strip()


def consistency(a, b):
    uid = get(a, "/Users")[0]["Id"]
    item = get(a, "/Items", userId=uid, Recursive="true", IncludeItemTypes="Movie", Limit=1)["Items"][0]
    iid = item["Id"]
    before = get(b, f"/Items/{iid}", userId=uid)["UserData"]["Played"]
    target = not before
    t0 = time.perf_counter()
    r = requests.request("POST" if target else "DELETE", f"{a}/UserPlayedItems/{iid}", headers=H,
                         params={"userId": uid}, timeout=30)
    r.raise_for_status()
    t_write = time.perf_counter() - t0
    dashed = f"{iid[0:8]}-{iid[8:12]}-{iid[12:16]}-{iid[16:20]}-{iid[20:]}"
    row = sql("gl-db3", f"SELECT Played FROM UserData WHERE ItemId='{dashed}'")
    via_b = get(b, f"/Items/{iid}", userId=uid)["UserData"]["Played"]
    print(f"item {item['Name']!r}: played {before} -> {target} via A ({t_write * 1000:.0f} ms)")
    print(f"  gl-db3 row right after A's 200: Played={row}   (expected {int(target)})")
    print(f"  B's API right after: Played={via_b}   ({'consistent' if via_b == target else 'STALE (B cache)'})")
    waited = 0
    for delay in (1, 5, 30, 65):
        time.sleep(delay - waited)
        waited = delay
        v = get(b, f"/Items/{iid}", userId=uid)["UserData"]["Played"]
        print(f"  B after {delay:>2} s: Played={v}")
        if v == target:
            break
    # restore
    requests.request("DELETE" if target else "POST", f"{a}/UserPlayedItems/{iid}", headers=H,
                     params={"userId": uid}, timeout=30)


def failover(url, victim, seconds=30):
    uid = get(url, "/Users")[0]["Id"]
    fails, last_ok, worst_gap, n = [], time.perf_counter(), 0.0, 0
    start = time.perf_counter()
    killed = False
    while time.perf_counter() - start < seconds:
        if not killed and time.perf_counter() - start > 5:
            subprocess.run(["podman", "kill", victim], capture_output=True)
            kill_t = time.perf_counter() - start
            print(f"t={kill_t:5.2f}s  SIGKILL {victim}")
            killed = True
        t = time.perf_counter()
        try:
            requests.get(url + "/Items", headers=H, timeout=30,
                         params={"userId": uid, "Recursive": "true", "IncludeItemTypes": "Movie", "Limit": 5}).raise_for_status()
            now = time.perf_counter()
            worst_gap = max(worst_gap, now - last_ok)
            last_ok = now
        except Exception as e:  # noqa: BLE001 - report anything
            fails.append((t - start, time.perf_counter() - t, str(e)[:120]))
        n += 1
        time.sleep(0.1)
    print(f"{n} requests, {len(fails)} failed, longest gap between successes {worst_gap:.2f} s")
    for at, took, err in fails:
        print(f"  fail t={at:5.2f}s after {took:.2f}s: {err}")
    subprocess.run(["podman", "start", victim], capture_output=True)
    t0 = time.perf_counter()
    while time.perf_counter() - t0 < 300:
        if sql(victim, "SELECT 1") == "1":
            st = subprocess.run(["podman", "exec", victim, "mysql", "-uroot", "-plabroot", "-N", "-e",
                                 "SHOW STATUS WHERE Variable_name IN ('wsrep_local_state_comment','wsrep_cluster_size')"],
                                capture_output=True, text=True).stdout.split()
            if "Synced" in st:
                print(f"{victim} restarted and Synced after {time.perf_counter() - t0:.1f} s ({' '.join(st)})")
                return
        time.sleep(1)
    print(f"{victim} did not rejoin within 300 s")


def conflict(a, b, rounds=100):
    """Same user, same item, A and B write at the same moment: Galera certifies one and aborts the
    other (ER_LOCK_DEADLOCK); Jellyfin does not retry, so the loser's request errors."""
    from concurrent.futures import ThreadPoolExecutor
    uid = get(a, "/Users")[0]["Id"]
    iid = get(a, "/Items", userId=uid, Recursive="true", IncludeItemTypes="Movie", Limit=1)["Items"][0]["Id"]

    def progress(base, i):
        r = requests.post(f"{base}/UserItems/{iid}/UserData", headers=H, params={"userId": uid},
                          json={"PlaybackPositionTicks": 10_000_000 * (i + 1)}, timeout=30)
        return r.status_code

    # Start from "no row" so the first writes race on the insert, not just on the update.
    dashed = f"{iid[0:8]}-{iid[8:12]}-{iid[12:16]}-{iid[16:20]}-{iid[20:]}"
    sql(os.environ.get("DRILL_DB", "gl-db1"), f"DELETE FROM UserData WHERE ItemId='{dashed}'")
    codes = {}
    since = str(int(time.time()))
    with ThreadPoolExecutor(8) as ex:
        futs = [ex.submit(progress, base, i) for i in range(rounds) for base in (a, b)]
        for f in futs:
            c = f.result()
            codes[c] = codes.get(c, 0) + 1
    print(f"{2 * rounds} concurrent UserData writes to one row via A and B: status counts {codes}")
    for name in os.environ.get("DRILL_NODES", "jg-a,jg-b").split(","):
        log = subprocess.run(["podman", "logs", "--since", since, name], capture_output=True, text=True)
        text = log.stdout + log.stderr
        print(f"  {name}: {text.count('Deadlock found')} 'Deadlock found' (certification), "
              f"{text.count('Duplicate entry')} 'Duplicate entry' (insert race) log lines")


if __name__ == "__main__":
    if sys.argv[1] == "consistency":
        consistency(sys.argv[2], sys.argv[3])
    elif sys.argv[1] == "conflict":
        conflict(sys.argv[2], sys.argv[3])
    else:
        failover(sys.argv[2], sys.argv[3], int(sys.argv[4]) if len(sys.argv) > 4 else 30)

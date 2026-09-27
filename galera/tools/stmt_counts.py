#!/usr/bin/env python3
"""Statements and MySQL server time per Jellyfin API call (the bench.py call set), from
performance_schema digests on one Galera node. Compare stock vs patched Jellyfin.

    stmt_counts.py <jellyfin-url> [runs] [--db gl-db1]
"""
import argparse
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "../../../spikes/jellymesh"))
import requests  # noqa: E402

ap = argparse.ArgumentParser()
ap.add_argument("url")
ap.add_argument("runs", nargs="?", type=int, default=5)
ap.add_argument("--db", default="gl-db1")
a = ap.parse_args()
H = {"Authorization": 'MediaBrowser Token="jmlabkey0000000000000000000000001"'}


def get(path, **params):
    r = requests.get(a.url + path, headers=H, params=params, timeout=120)
    r.raise_for_status()
    return r.json()


def my(sql):
    return subprocess.run(["podman", "exec", a.db, "mysql", "-uroot", "-plabroot", "-N", "-e", sql],
                          capture_output=True, text=True).stdout.strip()


uid = get("/Users")[0]["Id"]
movie = get("/Items", userId=uid, Recursive="true", IncludeItemTypes="Movie", Limit=1)["Items"][0]["Id"]
series = get("/Items", userId=uid, Recursive="true", IncludeItemTypes="Series", Limit=1)["Items"][0]["Id"]
calls = {
    "home: UserViews": ("/UserViews", {"userId": uid}),
    "home: Resume": ("/UserItems/Resume", {"userId": uid, "Limit": 12}),
    "home: NextUp": ("/Shows/NextUp", {"userId": uid, "Limit": 24}),
    "home: Latest movies": ("/Items/Latest", {"userId": uid, "IncludeItemTypes": "Movie", "Limit": 16}),
    "grid: Movies 100": ("/Items", {"userId": uid, "Recursive": "true", "IncludeItemTypes": "Movie", "SortBy": "SortName", "Limit": 100}),
    "grid: Audio 200": ("/Items", {"userId": uid, "Recursive": "true", "IncludeItemTypes": "Audio", "SortBy": "Album,SortName", "Limit": 200}),
    "search: 'the'": ("/Items", {"userId": uid, "searchTerm": "the", "Recursive": "true", "Limit": 24}),
    "detail: movie": (f"/Items/{movie}", {"userId": uid}),
    "detail: series episodes": (f"/Shows/{series}/Episodes", {"userId": uid}),
    "people: 100": ("/Persons", {"Limit": 100}),
}
print(f"{a.url}  runs={a.runs}")
print(f"{'call':<26}{'stmts':>7}{'server ms':>11}{'wall ms':>9}")
for name, (path, params) in calls.items():
    get(path, **params)  # warm
    my("TRUNCATE performance_schema.events_statements_summary_by_digest")
    t0 = time.perf_counter()
    for _ in range(a.runs):
        get(path, **params)
    wall = (time.perf_counter() - t0) * 1000 / a.runs
    row = my("SELECT SUM(COUNT_STAR), SUM(SUM_TIMER_WAIT)/1e9 FROM performance_schema.events_statements_summary_by_digest "
             "WHERE SCHEMA_NAME='jellyfin' AND DIGEST_TEXT NOT LIKE 'SET NAMES%'").split()
    stmts, server = float(row[0]) / a.runs, float(row[1]) / a.runs
    print(f"{name:<26}{stmts:>7.0f}{server:>11.1f}{wall:>9.0f}")

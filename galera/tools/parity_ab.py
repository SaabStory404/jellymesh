#!/usr/bin/env python3
"""Response parity between two Jellyfin nodes on the same database (e.g. stock vs patched):
same item ids in the same order, same totals, same DTO fields, for the bench call set plus larger
pages.

    parity_ab.py <url-a> <url-b>
"""
import json
import sys

import requests

A, B = sys.argv[1], sys.argv[2]
H = {"Authorization": 'MediaBrowser Token="jmlabkey0000000000000000000000001"'}
VOLATILE = {"PlayAccess", "ServerId", "Etag"}  # per-node or per-request values


def get(base, path, **params):
    r = requests.get(base + path, headers=H, params=params, timeout=120)
    r.raise_for_status()
    return r.json()


uid = get(A, "/Users")[0]["Id"]
movie = get(A, "/Items", userId=uid, Recursive="true", IncludeItemTypes="Movie", Limit=1)["Items"][0]["Id"]
series = get(A, "/Items", userId=uid, Recursive="true", IncludeItemTypes="Series", Limit=1)["Items"][0]["Id"]
calls = {
    "UserViews": ("/UserViews", {"userId": uid}),
    "Resume": ("/UserItems/Resume", {"userId": uid, "Limit": 50}),
    "NextUp": ("/Shows/NextUp", {"userId": uid, "Limit": 50}),
    "Latest movies": ("/Items/Latest", {"userId": uid, "IncludeItemTypes": "Movie", "Limit": 50}),
    "Movies all": ("/Items", {"userId": uid, "Recursive": "true", "IncludeItemTypes": "Movie", "SortBy": "SortName", "Fields": "People,MediaSourceCount"}),
    "Audio 2000": ("/Items", {"userId": uid, "Recursive": "true", "IncludeItemTypes": "Audio", "SortBy": "Album,SortName", "Limit": 2000}),
    "Series all": ("/Items", {"userId": uid, "Recursive": "true", "IncludeItemTypes": "Series"}),
    "search the": ("/Items", {"userId": uid, "searchTerm": "the", "Recursive": "true", "Limit": 200}),
    "search star": ("/Items", {"userId": uid, "searchTerm": "star", "Recursive": "true", "Limit": 200}),
    "detail movie": (f"/Items/{movie}", {"userId": uid}),
    "series episodes": (f"/Shows/{series}/Episodes", {"userId": uid}),
    "Persons all": ("/Persons", {}),
    "Persons page 3": ("/Persons", {"StartIndex": 200, "Limit": 100}),
    "Persons for user": ("/Persons", {"userId": uid, "Limit": 300}),
}


def norm(o):
    if isinstance(o, dict):
        return {k: norm(v) for k, v in o.items() if k not in VOLATILE}
    if isinstance(o, list):
        return [norm(v) for v in o]
    return o


bad = 0
for name, (path, params) in calls.items():
    a, b = norm(get(A, path, **params)), norm(get(B, path, **params))
    items_a = a.get("Items", a) if isinstance(a, dict) else a
    items_b = b.get("Items", b) if isinstance(b, dict) else b
    n = len(items_a) if isinstance(items_a, list) else 1
    if a == b:
        print(f"  {name:<18} identical ({n} items)")
        continue
    bad += 1
    ids_a = [i.get("Id") for i in items_a] if isinstance(items_a, list) else None
    ids_b = [i.get("Id") for i in items_b] if isinstance(items_b, list) else None
    if ids_a != ids_b:
        print(f"  {name:<18} DIFFERENT ids/order: {len(ids_a or [])} vs {len(ids_b or [])}; "
              f"only A {len(set(ids_a or []) - set(ids_b or []))}, only B {len(set(ids_b or []) - set(ids_a or []))}")
    else:
        for ia, ib in zip(items_a if isinstance(items_a, list) else [items_a], items_b if isinstance(items_b, list) else [items_b]):
            if ia != ib:
                keys = sorted(k for k in set(ia) | set(ib) if ia.get(k) != ib.get(k))
                print(f"  {name:<18} DIFFERENT fields on {ia.get('Name')!r}: " + json.dumps({k: [ia.get(k), ib.get(k)] for k in keys})[:300])
                break
    if isinstance(a, dict) and a.get("TotalRecordCount") != b.get("TotalRecordCount"):
        print(f"  {'':<18} TotalRecordCount {a.get('TotalRecordCount')} vs {b.get('TotalRecordCount')}")
print("parity: all identical" if bad == 0 else f"parity: {bad} calls differ")
sys.exit(1 if bad else 0)

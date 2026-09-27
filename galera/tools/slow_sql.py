#!/usr/bin/env python3
"""Capture every SELECT one Jellyfin API call sends to a Galera lab node, time each one on the
server, and EXPLAIN ANALYZE the slowest.

    slow_sql.py <jellyfin-url> <path?query> <outdir> [--db gl-db1] [--top 2]

The general log (not performance_schema, whose SQL_TEXT stops at 1024 bytes) keeps full statements;
MySqlConnector interpolates parameters client-side, so they are directly runnable.
"""
import argparse
import os
import pathlib
import subprocess
import time
import urllib.request

ap = argparse.ArgumentParser()
ap.add_argument("url")
ap.add_argument("call")
ap.add_argument("out")
ap.add_argument("--db", default="gl-db1")
ap.add_argument("--top", type=int, default=2)
a = ap.parse_args()
pw = os.environ.get("GL_ROOTPW", "labroot")
token = os.environ.get("JM_TOKEN", "jmlabkey0000000000000000000000001")
out = pathlib.Path(a.out)
out.mkdir(parents=True, exist_ok=True)


def my(sql, db=None, raw=True):
    cmd = ["podman", "exec", "-i", a.db, "mysql", "-uroot", f"-p{pw}", "-N"] + (["--raw"] if raw else []) + ([db] if db else [])
    r = subprocess.run(cmd, input=sql.encode(), capture_output=True)
    if r.returncode:
        raise RuntimeError(r.stderr.decode().strip().splitlines()[-1])
    return r.stdout.decode(errors="replace")


def call():
    req = urllib.request.Request(a.url + a.call, headers={"Authorization": f'MediaBrowser Token="{token}"'})
    urllib.request.urlopen(req, timeout=120).read()


call()  # warm Jellyfin's own caches first
my("SET GLOBAL log_output='TABLE'; TRUNCATE mysql.general_log; SET GLOBAL general_log=ON;")
call()
my("SET GLOBAL general_log=OFF;")
rows = my("SELECT HEX(argument) FROM mysql.general_log WHERE command_type='Query' AND argument LIKE 'SELECT%';").split()
stmts = [bytes.fromhex(h).decode() for h in rows]
(out / "all.sql").write_text("".join(s + ";\n" for s in stmts))

timed = []
for s in stmts:
    t0 = time.perf_counter()
    my(s + ";", db="jellyfin")
    timed.append(((time.perf_counter() - t0) * 1000, s))
timed.sort(reverse=True)
total = sum(t for t, _ in timed)
print(f"{len(stmts)} SELECTs, {total:.0f} ms re-run total (incl. ~podman exec overhead each)")
for i, (ms, s) in enumerate(timed[: a.top], 1):
    (out / f"q{i}.sql").write_text(s + ";\n")
    try:
        plan = my("EXPLAIN ANALYZE " + s + ";", db="jellyfin")
    except RuntimeError as e:
        plan = f"explain failed: {e}"
    (out / f"q{i}.explain").write_text(plan.replace("\\n", "\n"))
    print(f"== q{i} {ms:.0f} ms ({len(s)} chars) -> {out}/q{i}.sql / .explain")

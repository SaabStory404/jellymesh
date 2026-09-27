#!/usr/bin/env python3
"""Put every lab node back on the pristine real-library data before a benchmark run.

    reset_data.py <real.db> [--sqlite s1,s2] [--galera a,b,c] [--dbmigrate path] [--mysql gl-db1:13301]

SQLite nodes get a fresh copy of <real.db>; the Galera database is dropped, recreated and filled
with jellyfin-dbmigrate from <real.db>. Jellyfin nodes are stopped while their data is replaced.
"""
import argparse
import os
import subprocess
import time

ap = argparse.ArgumentParser()
ap.add_argument("db")
ap.add_argument("--sqlite", default="")
ap.add_argument("--galera", default="")
ap.add_argument("--dbmigrate", default="jellyfin-dbmigrate")
ap.add_argument("--mysql", default="gl-db1:13301")
ap.add_argument("--lab", default=os.path.expanduser("~/.cache/galera-lab"))
a = ap.parse_args()
sq = [n for n in a.sqlite.split(",") if n]
ga = [n for n in a.galera.split(",") if n]
container, port = a.mysql.split(":")


def sh(*cmd, check=True):
    return subprocess.run(cmd, check=check, capture_output=True, text=True)


t0 = time.time()
sh("podman", "stop", "-t", "2", *[f"jg-{n}" for n in sq + ga], check=False)
for n in sq:
    d = f"{a.lab}/jf-{n}/data/data"
    sh("podman", "unshare", "rm", "-f", f"{d}/jellyfin.db-wal", f"{d}/jellyfin.db-shm")
    sh("podman", "unshare", "cp", a.db, f"{d}/jellyfin.db")
    sh("podman", "unshare", "chown", "1000:1000", f"{d}/jellyfin.db")
if ga:
    sh("podman", "exec", container, "mysql", "-uroot", "-plabroot", "-e",
       "DROP DATABASE IF EXISTS jellyfin; CREATE DATABASE jellyfin CHARACTER SET utf8mb4 COLLATE utf8mb4_bin;")
    conn = f"galera:Server=127.0.0.1;Port={port};Database=jellyfin;Uid=jellyfin;Pwd=jellyfin;SslMode=Disabled;AllowPublicKeyRetrieval=true"
    r = sh(a.dbmigrate, "copy", "--from", f"sqlite:{a.db}", "--to", conn)
    print(r.stdout.strip().splitlines()[-1])
sh("podman", "start", *[f"jg-{n}" for n in sq + ga])
print(f"reset {sq + ga} in {time.time() - t0:.0f} s; wait for the nodes to answer before benchmarking")

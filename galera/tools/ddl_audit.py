#!/usr/bin/env python3
"""Audit a MySQL DDL script generated from Jellyfin's EF model.

  ddl_audit.py <script.sql> [real-sqlite-db]

Lists bounded string columns (varchar(N)) with the longest value found in the real SQLite
library (truncation risk), and indexes that include longtext columns (MySQL rejects those
without a prefix length).
"""
import re
import sqlite3
import sys

sql = open(sys.argv[1]).read()
db = sqlite3.connect(f"file:{sys.argv[2]}?mode=ro", uri=True) if len(sys.argv) > 2 else None

tables = {}
for m in re.finditer(r"CREATE TABLE `(\w+)` \((.*?)\n\);", sql, re.S):
    cols = {}
    for c in re.finditer(r"`(\w+)` ([a-z]+(?:\(\d+(?:,\d+)?\))?)", m.group(2)):
        cols.setdefault(c.group(1), c.group(2))
    tables[m.group(1)] = cols

print("== bounded string columns (varchar) vs longest value in the real library")
for t, cols in sorted(tables.items()):
    for c, typ in cols.items():
        if typ.startswith("varchar"):
            n = int(typ[8:-1])
            longest = None
            if db:
                try:
                    longest = db.execute(f'select max(length("{c}")) from "{t}"').fetchone()[0]
                except sqlite3.Error:
                    longest = "?"
            flag = "  <-- TRUNCATES" if isinstance(longest, int) and longest > n else ""
            print(f"  {t}.{c}: {typ}  longest={longest}{flag}")

print("== indexes on longtext columns (need a prefix length)")
for m in re.finditer(r"CREATE (UNIQUE )?INDEX `(\w+)` ON `(\w+)` \(([^;]*?)\);", sql):
    uniq, name, t, cols = m.groups()
    colnames = re.findall(r"`(\w+)`", cols)
    text = [c for c in colnames if tables.get(t, {}).get(c) == "longtext"]
    if text:
        print(f"  {'UNIQUE ' if uniq else ''}{t}.{name}: {colnames}  longtext={text}")

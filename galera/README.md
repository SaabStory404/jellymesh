# JellyMesh Galera provider + SQLite ⇄ Galera migration

Run Jellyfin 12.1 on MySQL 8 / Percona XtraDB Cluster (Galera): every node keeps the whole
database in memory, any node can write, commits are certified cluster-wide.

- `Jellyfin.Database.Providers.Galera/` — the provider plugin (`DatabaseType=PLUGIN_PROVIDER`).
- `Jellyfin.DbMigrate/` — `jellyfin-dbmigrate`, moves a Jellyfin database between SQLite and
  Galera in **either direction** and verifies it.
- `pomelo/` — the MySQL EF Core provider underneath: Pomelo from the unmerged EF Core 10 PR
  (#2047, pinned sha) plus `jellymesh-pomelo.patch`; `build.sh` builds it into
  `~/.cache/jellymesh-vendor/pomelo` (the csproj's `PomeloBin`).
- `tools/` — `ddl_audit.py` (truncation / index checks on generated DDL), `digest_profile.sh`
  (statements + server time per API call), `slow_sql.py` (full SQL + EXPLAIN ANALYZE of the
  slowest statements of one call).
- `lab/galera-lab.sh` — Percona XtraDB Cluster nodes on podman (`rejoin n` after a crash);
  `lab/jf-galera.sh` — Jellyfin nodes on it; `lab/galera_drill.py` — consistency, failover and
  write-conflict drills.

## Switching a server between SQLite and Galera

Stop Jellyfin first. Then:

```bash
# SQLite -> Galera (database created with utf8mb4 / utf8mb4_bin)
jellyfin-dbmigrate copy   --from sqlite:/config/data/data/jellyfin.db \
                          --to "galera:Server=db;Database=jellyfin;Uid=jellyfin;Pwd=...;SslMode=..."
jellyfin-dbmigrate verify --from sqlite:/config/data/data/jellyfin.db --to "galera:..."
# install the plugin into /config/data/plugins/JellyMesh Galera_1.0.0.0/ and write database.xml:
#   DatabaseType=PLUGIN_PROVIDER, PluginName="JellyMesh Galera",
#   PluginAssembly=Jellyfin.Database.Providers.Galera.dll, ConnectionString=...

# Galera -> SQLite (back out)
jellyfin-dbmigrate copy   --from "galera:..." --to sqlite:/config/data/data/jellyfin.db.new
jellyfin-dbmigrate verify --from "galera:..." --to sqlite:/config/data/data/jellyfin.db.new
# move it into place and delete database.xml (Jellyfin defaults to SQLite)
```

`copy` creates the target schema with the target provider's own migrations, copies every table
in foreign-key order with its original keys (auto-increment counters follow), clears rows the
migrations seeded, and carries Jellyfin's code-migration history. `verify` compares every column
of every row by primary key.

## Results on a real library (MEASURED 2026-09-26)

19,259 items, 308,330 rows, 31 tables:

| Step | Time | Verify |
|---|---|---|
| SQLite → Galera (1 PXC node, podman) | 38.8 s (Oracle provider), 39.9 s (Pomelo) | every row and column identical |
| SQLite → Galera (3-node cluster) | 46.2 s | identical; node 3 has every row |
| Galera → new SQLite | 28.2 s | round trip identical to the original |

Jellyfin 12.1 on the 3-node cluster, p50 ms over 30 runs (`spikes/jellymesh/bench.py`), next to
the SQLite numbers from `spikes/jellymesh/README.md`:

| Call | SQLite | Galera (Pomelo + patch) | what is left |
|---|---|---|---|
| home: UserViews | 23 | 94 | Jellyfin N+1 (chapters/extras per view) |
| grid: Movies 100 | 61 | 61 | — (was 568 before the IN rewrite) |
| grid: Audio 200 | 134 | 633 | `DISTINCT` over whole rows incl. the `Data` blob + 200 N+1 stream reads |
| search 'the' | 39 (72 results) | 76 (72 results) | |
| detail: series episodes | — | 12 | |
| people: 100 | 58 | 590 | lower-name dedupe `NOT EXISTS` runs as a per-row range scan (339 ms; the hash antijoin plan is 33 ms) + 100 N+1 person reads |

Server time dominates the slow ones (`digest_profile.sh`: Movies grid was 603 of 630 ms inside
MySQL), so round trips are not the problem there; the two remaining outliers are Jellyfin query
shapes MySQL plans badly and are fixed in Jellyfin itself (docs/jellyfin-n1-hotspots.md), not in
the provider.

Drills (`lab/galera_drill.py`, MEASURED):

| Drill | Result |
|---|---|
| played on A, read row on another Galera node | present when A's 200 returns (synchronous) |
| …read through Jellyfin B's API | **stale after 65 s**: B's in-process UserData cache is never told. A shared database still needs the JellyMesh cache-invalidation plugin |
| SIGKILL B's Galera node under load (FailOver list) | 204 requests, **0 failed**, longest gap 2.9 s; node back and Synced 7 s after restart (IST) |
| SIGKILL the primary in single-writer mode | 206 requests, 0 failed, 2.9 s gap; the bootstrap node must come back as a joiner (`rejoin`) |
| 200 concurrent writes to one UserData row, A and B on **different** Galera nodes | **69 × HTTP 500** (35%): Galera certification conflicts (`Deadlock found`), Jellyfin does not retry |
| same, both Jellyfins on the **same** Galera node | 200 × 200, 0 conflicts |

So Galera is run **single-writer**: every Jellyfin lists the nodes in the same order
(`Server=db1,db2,db3;LoadBalance=FailOver`), the others are synchronous hot standbys. Multi-writer
would need retries around every Jellyfin transaction, which the provider cannot add (a retrying EF
execution strategy rejects Jellyfin's own `BeginTransaction` calls).

## What the provider had to fix (all MEASURED)

| Problem | Symptom | Fix |
|---|---|---|
| Oracle's provider (MySql.EntityFrameworkCore 10.0.9) turns indexed strings into `varchar(255)` | real `Path` values are 264 chars → truncation | key/unique strings `varchar(512)` (`ItemValues.Value` 700); other indexed strings `longtext` + prefix indexes |
| Oracle's SQL generator ignores `IndexPrefixLength` and column collations | invalid indexes on longtext; case-insensitive default collation | moved to Pomelo, which emits both; database default `utf8mb4_bin` set before migrations |
| Oracle's provider cannot translate or bind collection `Contains` | `/UserViews` 500 (`@p.Contains(u.ItemId)` untranslatable; NRE binding a `Guid[]`) | moved to Pomelo + `EnablePrimitiveCollectionsSupport` |
| Pomelo PR build: `JSON_TABLE` over a parameter never type-mapped | `/UserViews` 500 (`'@p' … does not have a type mapping`); Jellyfin binds id lists as `EF.Parameter(list)` | patch: `MySqlTypeMappingPostprocessor` |
| Pomelo PR build: renamed `JSON_TABLE` lost its path/COLUMNS | `/Items` 500 (`JSON_TABLE(@p2) AS p0` syntax error) | patch: `WithAlias`/`Clone` overrides |
| Pomelo float literals `100` are integers to MySQL | search 500 (`Unable to cast Int64 to Single`) | patch: literals carry an exponent (`100E0`) |
| MySQL runs UNION / nested-GROUP-BY `IN (subquery)` once per outer row | Movies grid 568 ms (COUNT: 19,259 dependent executions) | patch: non-correlated `IN (SELECT * FROM (…) AS jm_inN)` → 61 ms |
| Temp tables over whole `BaseItems` rows spill at the 16 MB default | Audio grid: 2 on-disk temp tables | `tmp_table_size=256M` in the node cnf (−60 ms) |
| No primitive-collection support | `KeyframeData.KeyframeTicks` written as `System.Collections.Generic.List\`1[System.Int64]` | JSON converter, same format as SQLite |
| MySQL `FLOAT` read over the text protocol | `AverageFrameRate` 23.976025 → 23.976 | floats stored as `DOUBLE` |
| `DateTime` precision | MySQL `datetime(6)` keeps µs; Jellyfin hashes `DateModified.Ticks` into image tags and chapter image file names, so every image tag changed after a migration (MEASURED, parity_ab.py) | DateTime stored as BIGINT ticks; verify compares exact ticks; SQLite and Galera API responses now byte-identical |
| PXC `pxc_strict_mode=ENFORCING` | rejects `GET_LOCK`, used by EF's migration lock | lab runs `PERMISSIVE`; migrations from one node |
| JellyfinDbContext calls the provider's `OnModelCreating` before its own configuration | model rules had no effect | rules run as a model-finalizing convention |

Risk: the provider depends on an unmerged community PR (pinned in `pomelo/build.sh`) plus our
patch; upstream Pomelo has no EF Core 10 release.

Also: Galera does not replicate named locks, so `GET_LOCK` cannot elect a cluster leader — use a
lease row or a Kubernetes Lease.

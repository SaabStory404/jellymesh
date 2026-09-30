# Galera provider: measurements, lab and history

This page holds the measurements, lab scripts, and change history for the Galera database provider. The operator path (build, configure, migrate) is in [galera/README.md](../../galera/README.md).

**Status:** Lab-verified. All numbers come from a single workstation with every node in a podman container, on a real library of 19,259 items, 308,330 rows, and 31 tables. Results were measured 2026-09-26 unless a row says otherwise.

## Migration timings

Method: `jellyfin-dbmigrate copy` then `verify`, n=1 per row, single workstation. This page carries the only copy of these rows; [RESULTS.md](../RESULTS.md#migration-timing), Migration timing links here. The Oracle provider row measures a provider that is no longer in the code.

| Step | Time | Verify |
|---|---|---|
| SQLite to Galera (1 PXC node), Oracle provider | 38.8 s | every row and column identical |
| SQLite to Galera (1 PXC node), Pomelo | 39.9 s | every row and column identical |
| SQLite to Galera (3-node cluster) | 46.2 s | identical; node 3 has every row |
| Galera to new SQLite | 28.2 s | round trip identical to the original |

## Latency versus SQLite

Method: Jellyfin 12.1 on the 3-node cluster, p50 in ms over 30 runs, results measured 2026-09-26. The numbers were produced with `spikes/jellymesh/bench.py`, and the SQLite column comes from `spikes/jellymesh/README.md`. Neither path is in this repository, so these numbers are Not measured from repo scripts and cannot be reproduced from the repo alone. They predate the `jellyfin-perf` patch; [RESULTS.md](../RESULTS.md#per-call-latency-and-statement-counts), Per-call latency has current numbers.

| Call | SQLite (ms) | Galera, Pomelo + patch (ms) | What was left at the time |
|---|---|---|---|
| Home: UserViews | 23 | 94 | Jellyfin N+1 (chapters and extras per view) |
| Grid: Movies 100 | 61 | 61 | n/a (568 ms before the `IN (subquery)` rewrite; end-to-end grid p50) |
| Grid: Audio 200 | 134 | 633 | `DISTINCT` over whole rows including the `Data` blob, plus 200 N+1 stream reads |
| Search "the" | 39 (72 results) | 76 (72 results) | n/a |
| Detail: series episodes | n/a | 12 | n/a |
| People: 100 | 58 | 590 | lower-name dedupe `NOT EXISTS` runs as a per-row range scan (339 ms; the hash antijoin plan is 33 ms) plus 100 N+1 person reads |

N+1 means one query per item where a single batched query would do. Server time dominated the slow calls: with `galera/tools/digest_profile.sh`, the Movies grid spent 603 of 630 ms inside MySQL. The remaining outliers are Jellyfin query shapes that MySQL plans badly, addressed in Jellyfin itself ([hotspot analysis](jellyfin-n1-hotspots.md)) and not in the provider.

## Drills

Method: `galera/lab/galera_drill.py` on the 3-node lab cluster, n=1 per row. No exact drill date is recorded; [RESULTS.md](../RESULTS.md#failure-drills), Failure drills says September 2026.

| Drill | Result |
|---|---|
| Play on A, read the row on another Galera node | Present when A's 200 returns (replication is synchronous). |
| Read the same row through Jellyfin B's API, stock Jellyfin | Stale after 65 s: B's in-process UserData cache is never told. `JELLYFIN_SHARED_DB=1` addresses this (see [jellyfin-perf/README.md](../../jellyfin-perf/README.md)). |
| SIGKILL B's Galera node under load, `FailOver` list | 204 requests, 0 failed, longest gap 2.9 s; node Synced 7 s after restart through IST (incremental state transfer). |
| SIGKILL the primary in single-writer mode | 206 requests, 0 failed, 2.9 s gap; the bootstrap node must come back as a joiner (`rejoin`). |
| 200 concurrent writes to one UserData row, A and B on different Galera nodes, stock Jellyfin | 69 HTTP 500 (35%): Galera certification conflicts (`Deadlock found`); stock Jellyfin does not retry. |
| Same, both Jellyfins on the same Galera node | 200 x 200, 0 conflicts. |

## Single-writer or multi-writer

For a stock Jellyfin build, run Galera single-writer: every Jellyfin lists the nodes in the same order (`Server=db1,db2,db3;LoadBalance=FailOver`) and the other nodes act as synchronous hot standbys. Multi-writer needs retries around Jellyfin's transactions, which the provider cannot add, because a retrying EF Core execution strategy rejects Jellyfin's own `BeginTransaction` calls.

The `jellyfin-perf` patch retries `UserDataManager.SaveUserData` on a write conflict (fresh context, jittered backoff, 6 attempts). [jellyfin-perf/README.md](../../jellyfin-perf/README.md) reports 200 of 200 writes succeeding on multi-writer Galera with the patch, and [RESULTS.md](../RESULTS.md) lists a "2 Jellyfins (multi-writer)" configuration with 0 errors. RESULTS.md states that the single-writer recommendation predates the retry patch.

## Cross-node caches

The provider does not solve stale caches by itself. With `JELLYFIN_SHARED_DB=1` the patched Jellyfin reads user data and login sessions from the database, and bughunt patch 15 evicts item-cache entries on other nodes through a table in the same database. Measured coherence, auth checks, and caveats are in [jellyfin-perf/README.md](../../jellyfin-perf/README.md). `galera/lab/auth_drill.py` checks that a token issued on one node is accepted on the others and refused everywhere after logout.

## Limits of the schema and of locks

- MySQL rejects indexes over 3072 bytes. The provider sizes `longtext` prefix indexes to fit, and `galera/tools/ddl_audit.py` lists `varchar(N)` columns and indexes that include `longtext` columns.
- PXC `pxc_strict_mode=ENFORCING` rejects `GET_LOCK`, which EF Core uses for its migration lock. The lab runs `PERMISSIVE` and migrates from one node.
- Galera does not replicate named locks, so `GET_LOCK` cannot elect a cluster leader. Use a lease row or a Kubernetes Lease, as the [leader plugin](../../leader/README.md) does.
- `tmp_table_size=256M` in the lab node configuration removed two on-disk temporary tables on the Audio grid (about 60 ms), because temporary tables over whole `BaseItems` rows spilled at the 16 MB default.

## Lab scripts

The lab is for development and drills. It uses fixed lab credentials and a fixed lab API key (set in the scripts), and it is not a deployment recipe. Preparing the Jellyfin config directory that `jf-galera.sh` reads from `JG_SRC` is Not documented yet (see the doc TODO list in [CONTRIBUTING](../../CONTRIBUTING.md)).

### galera/lab/galera-lab.sh

PXC 8.4 nodes (`docker.io/percona/percona-xtradb-cluster:8.4`) on the podman network `gl`.

| Command | Effect |
|---|---|
| `boot` | Starts node 1, which bootstraps the cluster. |
| `join <n>` | Starts node n (2, 3, ...) and joins it through node 1. |
| `rejoin <n>` | Restarts node n after a crash. Needed after the bootstrap node dies. |
| `db` | Creates database `jellyfin` (`utf8mb4` / `utf8mb4_bin`) and user `jellyfin` / `jellyfin`. |
| `status` | Prints wsrep (Galera write-set replication) cluster size and state per node. |
| `down` | Removes the nodes and their data volumes. |

| Variable | Default | Purpose |
|---|---|---|
| `GL_ROOTPW` | `labroot` | Root password of the lab nodes. |
| `GL_CERTS` | `$HOME/.cache/galera-lab/certs` | Lab CA and node certificates. |

Node n listens on host port `1330<n>` (node 1 on 13301). Nodes start with `--innodb-buffer-pool-size=768M --max-connections=500`. The generated configuration sets `pxc_strict_mode=PERMISSIVE`, `tmp_table_size=256M`, and TLS for `mysqld` and for the state snapshot transfer (SST, `[sst]`). All nodes share one lab CA, because the xtrabackup transfer aborts with "Could not find a CA file" otherwise.

A minimal cluster:

```bash
galera/lab/galera-lab.sh boot
galera/lab/galera-lab.sh join 2
galera/lab/galera-lab.sh join 3
galera/lab/galera-lab.sh db
galera/lab/galera-lab.sh status
```

### galera/lab/jf-galera.sh

Runs Jellyfin nodes on the lab cluster from a prepared real-library configuration directory.

| Command | Effect |
|---|---|
| `up <name> <host-port> <galera-nodes>` | Starts a node. `<galera-nodes>` may be a comma list such as `gl-db2,gl-db3,gl-db1`. If the node directory has no `database.xml`, writes the one shown in the [README](../../galera/README.md). |
| `reload <name> <host-port>` | Copies a rebuilt plugin in and restarts the node. |
| `down` | Removes the nodes. |

| Variable | Default | Purpose |
|---|---|---|
| `JG_LAB` | `$HOME/.cache/galera-lab` | Lab directory. |
| `JG_SRC` | `$HOME/.cache/dbsidecar/s` (not in the repo) | Prepared Jellyfin config directory. |
| `JG_PLUGIN` | none | Built provider output directory. Required when `up` creates a Galera node (writes a new `database.xml`); not needed with `JG_SQLITE`. |
| `JG_OVERLAY` | none | Rebuilt Jellyfin DLLs mounted over the image's. |
| `JG_IMG` | `ghcr.io/hotio/jellyfin:release-12.1` | Jellyfin image. |
| `JG_SQLITE` | none | Make an SQLite comparison node from this database file. |
| `JG_SHARED` | `0` | When `1`, sets `JELLYFIN_SHARED_DB=1` on the node (patched Jellyfin only). |
| `JG_PW_ENV` | `0` | When `1`, the password is given only as `JELLYMESH_DB_PASSWORD` and is stripped from `database.xml`. |
| `JG_MESH`, `JG_REDIS`, `JG_RC` | none, `gl-redis:6379`, `0` | Mesh mode. `JG_MESH` refers to a `mesh/` directory that is not in this repo, so that mode cannot run from the repo. |

### Drill and reset scripts

| Script | Signature | Purpose |
|---|---|---|
| `lab/galera_drill.py` | `galera_drill.py consistency <url-a> <url-b>` | Toggles an item's played state on A, then reads the row from container `gl-db3` (hard-coded, with the lab root password) and through B's API. |
| `lab/galera_drill.py` | `galera_drill.py failover <url> <galera-container> [seconds]` | Sends small reads to `<url>`, SIGKILLs the container 5 s in, reports failed requests and the longest gap, restarts the container, and waits for it to rejoin. Default 30 s. |
| `lab/galera_drill.py` | `galera_drill.py conflict <url-a> <url-b>` | Concurrent writes to one UserData row from two nodes (the 200-write rows above). Reads env `DRILL_DB` (default `gl-db1`) and `DRILL_NODES` (default `jg-a,jg-b`). |
| `lab/auth_drill.py` | `auth_drill.py <login-node-url> <other-url> [<other-url> ...]` | Creates a throwaway user, logs in on the first node, uses the token on the others, logs out, and checks the token is refused everywhere. |
| `lab/reset_data.py` | `reset_data.py <real.db> [--sqlite s1,s2] [--galera a,b,c] [--dbmigrate path] [--mysql gl-db1:13301] [--lab dir]` | Restores pristine real-library data before a benchmark: fresh SQLite copies, and a dropped and refilled Galera database via `jellyfin-dbmigrate`. `--lab` defaults to `~/.cache/galera-lab`. |

## Analysis tools

| Tool | Signature | Purpose |
|---|---|---|
| `tools/ddl_audit.py` | `ddl_audit.py <script.sql> [real-sqlite-db]` | Lists every index that includes a `longtext` column (check each for a prefix length). With the optional SQLite path, also lists `varchar(N)` columns with the longest value in the real library. |
| `tools/digest_profile.sh` | `digest_profile.sh <url> <path?query> [runs] [galera-container]` | Statements and server time per call from `performance_schema` digests. Environment: `JM_TOKEN`, `GL_ROOTPW`. Defaults: 10 runs, container `gl-db1`. |
| `tools/slow_sql.py` | `slow_sql.py <url> <path?query> <outdir> [--db gl-db1] [--top 2]` | Captures every SELECT one call sends (general log, full statements), times each, and runs `EXPLAIN ANALYZE` on the slowest. Reads `GL_ROOTPW` and `JM_TOKEN`. |
| `tools/stmt_counts.py` | `stmt_counts.py <url> [runs] [--db gl-db1]` | Statements and server time per API call from `performance_schema`. It adds a `spikes/jellymesh` path that is not in this repo to `sys.path` but imports nothing from it, so it runs as shipped. |
| `tools/parity_ab.py` | `parity_ab.py <url-a> <url-b>` | Compares responses from two Jellyfin nodes on the same database: item ids, order, totals, and DTO fields, ignoring `PlayAccess`, `ServerId`, and `Etag`. |

## Provider history

This table records problems met while building the provider and how each was fixed. Numbers are from the lab on the real library, n=1. It is a reference for anyone changing the model rules or the Pomelo patch.

| Problem | Symptom | Fix |
|---|---|---|
| Oracle's provider (`MySql.EntityFrameworkCore` 10.0.9) turns indexed strings into `varchar(255)` | Real `Path` values are 264 characters, so they truncate | Key and unique strings are `varchar(512)` (`ItemValues.Value` 700); other indexed strings are `longtext` with prefix indexes |
| Oracle's SQL generator ignores `IndexPrefixLength` and column collations | Invalid indexes on `longtext`; case-insensitive default collation | Moved to Pomelo, which emits both; the database default `utf8mb4_bin` is set before migrations |
| Oracle's provider cannot translate or bind collection `Contains` | `/UserViews` returned 500 (`@p.Contains(u.ItemId)` untranslatable; NullReferenceException binding a `Guid[]`) | Moved to Pomelo with `EnablePrimitiveCollectionsSupport` |
| Pomelo PR build: `JSON_TABLE` over a parameter was never type-mapped | `/UserViews` 500 (`'@p' ... does not have a type mapping`); Jellyfin binds id lists as `EF.Parameter(list)` | Patch: `MySqlTypeMappingPostprocessor` |
| Pomelo PR build: a renamed `JSON_TABLE` lost its path and `COLUMNS` | `/Items` 500 (`JSON_TABLE(@p2) AS p0` syntax error) | Patch: `WithAlias` and `Clone` overrides |
| Pomelo float literals `100` are integers to MySQL | Search returned 500 (`Unable to cast Int64 to Single`) | Patch: literals carry an exponent (`100E0`) |
| MySQL runs UNION and nested GROUP BY `IN (subquery)` once per outer row | Movies grid 568 ms end to end (COUNT: 19,259 dependent executions) | Patch: non-correlated `IN (SELECT * FROM (...) AS jm_inN)`, giving 61 ms end to end. The COUNT statement alone went from 286 ms to 5.9 ms server time (source: `galera/pomelo/build.sh`). |
| Temporary tables over whole `BaseItems` rows spill at the 16 MB default | Audio grid: 2 on-disk temporary tables | `tmp_table_size=256M` in the node configuration (60 ms less) |
| No primitive-collection support | `KeyframeData.KeyframeTicks` was written as `System.Collections.Generic.List`1[System.Int64]` | JSON converter, same format as SQLite |
| MySQL `FLOAT` read over the text protocol | `AverageFrameRate` 23.976025 came back as 23.976 | Floats are stored as `DOUBLE` |
| `DateTime` precision | MySQL `datetime(6)` keeps microseconds; Jellyfin hashes `DateModified.Ticks` into image tags and chapter image file names, so every image tag changed after a migration (found with `parity_ab.py`) | DateTime stored as `BIGINT` ticks; `verify` compares exact ticks; SQLite and Galera API responses are byte-identical in the parity check |
| PXC `pxc_strict_mode=ENFORCING` | Rejects `GET_LOCK`, used by EF Core's migration lock | The lab runs `PERMISSIVE`; migrations run from one node |
| `JellyfinDbContext` calls the provider's `OnModelCreating` before its own configuration | Model rules had no effect | The rules run as a model-finalizing convention |
| One new unpooled `master` connection per `/health` probe (1.00 new connection per probe, lab) | Each probe opened a connection | `GaleraDatabaseCreator` runs `CanConnect` on the pooled connection |

## Related docs

- [Galera provider README](../../galera/README.md)
- [Results](../RESULTS.md)
- [Jellyfin N+1 hotspots](jellyfin-n1-hotspots.md)
- [jellyfin-perf](../../jellyfin-perf/README.md)
- [Architecture and glossary](../architecture.md)
- [Contributing](../../CONTRIBUTING.md)

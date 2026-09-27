# JellyMesh

Run **several Jellyfin 12.1 servers on one shared MySQL / Galera database** — any server answers
any request, a node can die without a failover dance, and one database is the only state.

Stock Jellyfin assumes one process over one SQLite file. Three things stand in the way of more:
SQLite itself, query shapes that only perform well on SQLite, and per-process caches that go stale
the moment another server writes. JellyMesh fixes all three:

| Part | What it is |
|---|---|
| [`jellyfin-perf/`](jellyfin-perf/) | A patch to Jellyfin 12.1: N+1 query fixes (people, lyrics, dedupe), a Resume sort key MySQL can plan, retried user-data writes, and **`JELLYFIN_SHARED_DB=1`** — user data and login sessions read from the database instead of per-node snapshots. Responses byte-identical to stock on SQLite and MySQL. |
| [`galera/`](galera/) | A Jellyfin database provider plugin for MySQL 8.4 / Percona XtraDB Cluster (Galera), built on Pomelo (EF Core 10 PR + our fixes), plus `jellyfin-dbmigrate`: lossless SQLite ⇄ MySQL migration in either direction, with a row-by-row verifier. |
| [`image/`](image/) | Containerfile: hotio's Jellyfin 12.1 (digest-pinned) + the patch + provider + migration tool. Published as `ghcr.io/saabstory404/jellymesh-jellyfin`. |
| [`docs/RESULTS.md`](docs/RESULTS.md) | Everything measured: single-client and concurrent load, failover, write conflicts, cross-node coherence. |

## Headline numbers (real 19k-item library; details in docs/RESULTS.md)

- 2 Jellyfin servers on a 3-node Galera cluster, 9 users, 20% writes: **112 req/s at 32 clients**
  vs 55 for stock single-node SQLite. A write on one server is visible on the other at the next
  read; a login on one works on the other, and a logout revokes it on both.
- Killing a database node under load: **0 failed requests**.
- `/Persons` 103 → 4 SQL statements; the Audio grid 207 → 8.

## Status

Working and measured in a lab and being rolled out on one home cluster. Not reviewed or endorsed by
the Jellyfin project. Known gaps: live-session state ("now playing", remote control) and running
transcodes stay per server; scheduled tasks should run on one server; plugins that keep their own
SQLite files (e.g. Playback Reporting) are not shared-database safe.

## Licence

GPL-2.0, as Jellyfin. The Pomelo patch applies to
[Pomelo.EntityFrameworkCore.MySql](https://github.com/PomeloFoundation/Pomelo.EntityFrameworkCore.MySql)
(MIT), built from the community EF Core 10 pull request #2047.

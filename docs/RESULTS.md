# JellyMesh — measured results

All MEASURED on one workstation (12 cores, 62 GB; every node — Jellyfin, PXC, Redis — is a podman
container on the same host and bridge network), Jellyfin 12.1 (hotio image), the real library
(19,259 items, 308,330 rows). "Patched" = `jellymesh/jellyfin-perf` (query shapes + write retry);
"Galera" = `jellymesh/galera` provider (Pomelo + patch, ticks) on a 3-node PXC 8.4 cluster.
Every configuration's API responses were checked identical to stock SQLite
(`galera/tools/parity_ab.py`, 14 calls incl. all 8,677 persons and 2,000 tracks).

## Concurrent load (`spikes/jellymesh/load.py`, 30 s per cell, 20% user-data writes)

Clients loop over the home screen, grids, search, detail and people (weighted like a session);
writes are playback-progress reports, each client on its own 5 items. Data reset to the pristine
library before the run (`galera/lab/reset_data.py`). Latencies in ms.

| Configuration | clients | req/s | home p50/p95 | grid p50/p95 | detail p50/p95 | people p50/p95 | write p50/p95 | errors |
|---|---|---|---|---|---|---|---|---|
| SQLite stock | 1 | 41.5 | 16 / 37 | 48 / 114 | 20 / 30 | 56 / 63 | 6 / 10 | 0 |
| SQLite stock | 8 | 75.3 | 68 / 136 | 233 / 467 | 116 / 188 | 537 / 611 | 18 / 29 | 0 |
| SQLite stock | 32 | 55.4 | 413 / 696 | 1203 / 2312 | 707 / 1244 | 2387 / 3197 | 192 / 478 | 0 |
| SQLite patched | 1 | 44.8 | 17 / 38 | 50 / 93 | 14 / 20 | 30 / 45 | 7 / 9 | 0 |
| SQLite patched | 8 | 112.2 | 60 / 132 | 200 / 236 | 43 / 59 | 87 / 109 | 20 / 31 | 0 |
| SQLite patched | 32 | 93.6 | 327 / 510 | 738 / 1026 | 260 / 402 | 317 / 459 | 190 / 383 | 0 |
| Galera stock, 1 Jellyfin | 8 | 54.0 | 55 / 317 | 146 / 835 | 87 / 137 | 1243 / 1393 | 31 / 71 | 0 |
| Galera stock, 1 Jellyfin | 32 | 49.0 | 385 / 1499 | 655 / 2783 | 633 / 1302 | 2532 / 3713 | 278 / 582 | 0 |
| Galera patched, 1 Jellyfin | 1 | 27.6 | 22 / 102 | 57 / 166 | 22 / 28 | 50 / 60 | 13 / 23 | 0 |
| Galera patched, 1 Jellyfin | 8 | 104.2 | 48 / 220 | 133 / 348 | 46 / 62 | 105 / 124 | 24 / 34 | 0 |
| Galera patched, 1 Jellyfin | 32 | **115.4** | 237 / 475 | 397 / 622 | 283 / 424 | 282 / 396 | 186 / 287 | 0 |
| Galera patched, 2 Jellyfins (multi-writer) | 8 | 96.5 | 50 / 232 | 136 / 430 | 46 / 71 | 112 / 146 | 25 / 41 | 0 |
| Galera patched, 2 Jellyfins (multi-writer) | 32 | **120.2** | 204 / 570 | 484 / 857 | 246 / 423 | 262 / 384 | 132 / 225 | 0 |

Reading it:

- **One client: SQLite wins** (in-process, no network hop): 45 vs 28 req/s.
- **Under load the shared cluster wins**: SQLite peaks near 8 clients and degrades (writers
  serialize; 75 → 55 stock, 112 → 94 patched), patched Galera keeps climbing (104 → 115 → 120 with a
  second Jellyfin). Every node here shares 12 cores, so the 2-Jellyfin cells understate a real
  2-host deployment.
- **The Jellyfin patch matters more than the backend**: stock Galera is the worst cell under load
  (the People N+1 alone is 1.2-2.5 s); patched Galera is the best.
- **Resume was the cliff.** Before the DatePlayed sort-key fix, patched Galera did 9.8 req/s at one
  client: MySQL's Resume plan scanned the user's whole UserData per candidate (0.4-2.6 s with a few
  hundred in-progress items). With it: 27.6 / 104 / 115.

## One store vs Redis tier (2 patched Jellyfins, 9 users, 20% writes, `load.py --multiuser`)

Brian's constraint (2026-09-26): exactly one data store; no app-level copying, syncing or routing
of state. Cross-node staleness = write on A, first poll on B (`mesh/mesh_drill.py coherence`).

| Configuration | stores | cross-node user data | 8 clients req/s | 32 clients req/s | home p95 @32 | write p95 @32 |
|---|---|---|---|---|---|---|
| Option 1: Galera + Redis mesh + response cache | 2 (Galera + Redis) | ~36 ms (plugin) | 75.8 | 53.3 | 2088 | 479 |
| Galera, default per-node caches (incoherent) | 1 | stale 65 s+ | 81.2 | 104.7 | 734 | 250 |
| Galera, JELLYFIN_SHARED_DB, no item cache | 1 | first poll | 27.5 | 35.6 | 1951 | 609 |
| **Galera, JELLYFIN_SHARED_DB (one store)** | 1 | first poll (~40 ms) | 90.7 | 112.0 | 655 | 242 |
| Option 2: plain MySQL 8.4, JELLYFIN_SHARED_DB (one store) | 1 | first poll (~36 ms) | 89.1 | 109.3 | 670 | 248 |

All one-store rows: API responses identical to stock SQLite (`parity_ab.py`), 0 errors.
Galera vs plain MySQL: synchronous replication costs nothing measurable here, and Galera survives
a node kill with 0 failed requests; a single MySQL has no failover.

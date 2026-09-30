# Results

Everything this repository has actually measured: concurrent-load benchmarks, per-call latency, SQL statement counts, response parity, migration timing, failure drills, and transcode-pool numbers.

Almost all of it is lab work. Unless a section says otherwise, the numbers come from one workstation — 12 cores, 62 GB of memory, every node a podman container on one bridge network — or from the `tc-lab` k3s namespace on cluster GPU nodes. Two of them are not: the Dolby Vision library census and the real-title conversion proof ran against my production library and the production `jellyfin-qsv` ffmpeg on 2026-09-27. And one of them is not a measurement at all — that DV 7 -> 8.1 plays on my SHIELD is something I watched happen on 2026-09-29, not something I instrumented.

## Terms used on this page

| Term | Meaning |
|---|---|
| [Galera](architecture.md#glossary) | Synchronous multi-primary replication for MySQL |
| [PXC](architecture.md#glossary) | Percona XtraDB Cluster, a MySQL distribution that includes Galera |
| [Pomelo](architecture.md#glossary) | The EF Core provider for MySQL that the JellyMesh Galera provider builds on |
| [N+1](architecture.md#glossary) | One query per row of a first query instead of one batched query |
| IST | Incremental state transfer: a Galera node that rejoins receives only the writes it missed |
| `FailOver` list | The `LoadBalance=FailOver` connection-string form: Jellyfin uses the first listed database node and moves to the next when it fails |
| Joiner, `rejoin` | A Galera node that is (re)joining a cluster; `rejoin` is a command in `galera/lab/galera-lab.sh` |
| Oracle provider | Oracle's MySQL EF Core provider, which the repository no longer uses |
| HLS | HTTP Live Streaming: the client fetches a playlist and short video segments over HTTP |
| [Direct play](architecture.md#glossary) | The client fetches the original file unchanged |
| [Remux](architecture.md#glossary) | Repackaging streams into a new container without re-encoding video |
| [Range retry](architecture.md#glossary) | A client reissues a request with `Range: bytes=N-` after a disconnect |
| [Kestrel](architecture.md#glossary) | The ASP.NET web server inside Jellyfin |
| [QSV](architecture.md#glossary) | Intel Quick Sync Video, the encoder path on the Intel Arc card |
| [NVENC](architecture.md#glossary) | NVIDIA's hardware encoder, on the Tesla P4 |
| [mTLS](architecture.md#glossary) | Mutual TLS: both sides present certificates |
| VMAF | A 0 to 100 perceptual video-quality score; higher is better. Here it compares each encode against a CPU reference encode |
| Tone mapper | A filter that maps HDR brightness to SDR. The Arc path uses `tonemap_vaapi` (VAAPI is the Linux video-acceleration API); the P4 path uses `tonemap_cuda` (CUDA is NVIDIA's compute API) |
| NFS | Network file system, used for the shared media and scratch mounts |
| Page cache | The operating system's in-memory copy of recently read file data; "warm" means the file was read recently |
| DOVI record, RPU, NAL, EL, FEL, MEL, fMP4, dvvC | Dolby Vision and HEVC container terms, defined in [dolby-vision.md](dolby-vision.md) |

## Test bed and limits

### Test bed

| Item | Value |
|---|---|
| Host | One workstation, 12 cores, 62 GB memory; CPU model and storage were not recorded |
| Topology | Every node is a podman container on one bridge network on that host |
| Jellyfin | 12.1, base image `ghcr.io/hotio/jellyfin:release-12.1` |
| Library | Real library: 19,259 items; 308,330 rows in 31 tables |
| Database | 3-node PXC 8.4 running the Galera provider |
| Load shape | 9 users, 20% playback-progress writes in the shared-mode runs; 30 runs per call in the per-call runs; 30 s per cell in the load runs: Not documented outside the load driver, which is not in this repository |
| Dates | Per-call and migration runs: 2026-09-26. The concurrent-load run date is not recorded |

"Stock" is unmodified Jellyfin 12.1. "Patched" is Jellyfin 12.1 with the [`jellyfin-perf`](../jellyfin-perf/README.md) query and write-retry patch. "Galera" is the [JellyMesh Galera provider](../galera/README.md) (Pomelo plus a patch, DateTime stored as BIGINT ticks) on the 3-node PXC cluster.

### What the numbers do not show

- **Two-host deployments.** All nodes share the same 12 cores, so a two-Jellyfin cell competes with itself and with the database for CPU. Those cells understate what a real two-host deployment would do.
- **Cache state.** Cache state was not controlled. Each per-call figure is the p50 of 30 runs.
- **Other clients and libraries.** One library, one synthetic client mix.
- **Load driver.** The concurrent-load and per-call drivers are not in this repository, so those tables can't be regenerated from it (see [Reproducing these results](#reproducing-these-results)).

Where two sources disagree, I give both figures with their methods.

## Concurrent load

Clients loop over the home screen, grids, search, detail, and people calls, weighted like a session. The 20% writes are playback-progress reports, each client on its own 5 items. Data was reset to the pristine library before each run with `../galera/lab/reset_data.py`. Latencies are in milliseconds, and every cell is a single run.

The load driver is `spikes/jellymesh/load.py`, and the only place these cells survive is the previous revision of this page: `git show HEAD:docs/RESULTS.md`. `../jellyfin-perf/README.md` repeats a few of them. Rows for stock Galera and stock SQLite exist only for the client counts shown.

| Configuration | Clients | req/s | Home p50/p95 | Grid p50/p95 | Detail p50/p95 | People p50/p95 | Write p50/p95 | Errors |
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
| Galera patched, 1 Jellyfin | 32 | 115.4 | 237 / 475 | 397 / 622 | 283 / 424 | 282 / 396 | 186 / 287 | 0 |
| Galera patched, 2 Jellyfins (multi-writer) | 8 | 96.5 | 50 / 232 | 136 / 430 | 46 / 71 | 112 / 146 | 25 / 41 | 0 |
| Galera patched, 2 Jellyfins (multi-writer) | 32 | 120.2 | 204 / 570 | 484 / 857 | 246 / 423 | 262 / 384 | 132 / 225 | 0 |

The multi-writer configuration is [defined in architecture.md](architecture.md#single-writer-and-multi-writer): both Jellyfins write to the shared database. API responses were checked for parity against stock SQLite (see [Parity](#parity)).

### Reading the table

- **At one client, SQLite is faster.** It runs in-process with no network hop: 44.8 req/s for patched SQLite against 27.6 req/s for patched Galera.
- **At 8 clients, patched SQLite leads.** Patched SQLite reaches 112.2 req/s, against 104.2 for patched Galera with one Jellyfin and 96.5 with two.
- **At 32 clients, patched Galera leads.** It reaches 115.4 (one Jellyfin) and 120.2 (two), against 93.6 for patched SQLite. Stock and patched SQLite are both lower at 32 clients than at 8 in these runs (75.3 to 55.4 stock, 112.2 to 93.6 patched); I only ran three client counts (1, 8, 32), so where the peak sits is still open.
- **Patched Galera at 8 and 32 clients.** With one Jellyfin it moves from 104.2 to 115.4 req/s. With two Jellyfins it moves from 96.5 to 120.2 req/s.
- **Stock Galera is the slowest cell under load.** It reaches 54.0 and 49.0 req/s at 8 and 32 clients. Its people p50 was 1243 and 2532 ms, against 105 and 282 ms patched. The patch removes both the N+1 (103 statements to 4, see [Per-call latency and statement counts](#per-call-latency-and-statement-counts)) and a slow query plan (see [Provider-only latencies](#provider-only-latencies-history)); this table can't split the two effects.
- **Resume was the cliff.** A DatePlayed sort-key fix changed how MySQL plans the Resume query. Before it, MySQL made 166,800 lookups for 400 candidates, and Resume took 0.4 to 2.6 s with a few hundred in-progress items; after it, 0.14 s. The previous revision of this page also recorded 9.8 req/s for patched Galera at one client before the fix, against 27.6 after. The per-call table below shows Resume at 33.4 ms p50 on patched Galera. Nothing records the in-progress item count behind the 0.14 s figure or behind the per-call row, so the two don't reconcile.

## One store versus a Redis response-cache tier

Two patched Jellyfins need to agree on user data. Which design gives the best throughput without a second store?

I set the constraint on 2026-09-26: exactly one data store, with no application-level copying, syncing, or routing of state. The Redis tier violates it, so the design rules it out. Its plugin is a legacy lab plugin that is not in this repository, which also means the Redis row here is a legacy result I can't reproduce.

2 patched Jellyfins, 9 users, 20% writes, multi-user mode of the load driver (`load.py --multiuser`). Cross-node staleness is "write on A, first poll on B" (`mesh/mesh_drill.py coherence`). Latencies in ms, and each row is a single run.

| Configuration | Stores | Cross-node user data | 8 clients req/s | 32 clients req/s | Home p95 @32 | Write p95 @32 |
|---|---|---|---|---|---|---|
| Galera + Redis mesh + response cache (legacy lab plugin) | 2 | about 36 ms (plugin) | 75.8 | 53.3 | 2088 | 479 |
| Galera, default per-node caches (incoherent) | 1 | stale 65 s or more | 81.2 | 104.7 | 734 | 250 |
| Galera, `JELLYFIN_SHARED_DB`, no item cache | 1 | first poll | 27.5 | 35.6 | 1951 | 609 |
| Galera, `JELLYFIN_SHARED_DB` | 1 | first poll (about 40 ms) | 90.7 | 112.0 | 655 | 242 |
| Plain MySQL 8.4, `JELLYFIN_SHARED_DB` | 1 | first poll (about 36 ms) | 89.1 | 109.3 | 670 | 248 |

The 32-client result for the two-Jellyfin Galera cell in [Concurrent load](#concurrent-load) is 120.2 req/s; the `JELLYFIN_SHARED_DB` Galera row here is 112.0. Nothing records why. The runs are separate, and this table used the load driver's multi-user mode.

Shared-database mode (`JELLYFIN_SHARED_DB=1`) is described in [../jellyfin-perf/README.md](../jellyfin-perf/README.md).

### Coherence

- With `JELLYFIN_SHARED_DB=1` on two Jellyfins over the 3-node cluster, a write on node A was visible on node B at the first poll in 8 of 8 trials, about 40 ms including the poll itself.
- With default per-node caches, node B was still stale after 65 s.
- Removing the 5 s item cache in shared mode cut throughput at 32 clients from 112.0 to 35.6 req/s in the table above. `../jellyfin-perf/README.md` states the same effect as 105 to 36 req/s; the 104.7 req/s incoherent row is the nearest to 105, and the source doesn't say which row it means.

### Caveats

- An earlier revision of this page recorded that every one-store row returned API responses matching stock SQLite under `../galera/tools/parity_ab.py`, with 0 errors. The pairs I can still point at are the three in [Parity](#parity).
- Galera against plain MySQL: the two are within 3% in this single run (90.7 against 89.1 req/s at 8 clients, 112.0 against 109.3 at 32). Galera survived a node kill with 0 failed requests (see [Database node loss](#database-node-loss)); a single MySQL server has no failover.

## Per-call latency and statement counts

p50 in milliseconds over 30 runs against the real library, measured 2026-09-26. Jellyfin talked to one node of the 3-node PXC cluster over the podman network.

| Call | SQLite stock | SQLite patched | Galera stock | Galera patched |
|---|---|---|---|---|
| home: UserViews | 15.7 | 14.8 | 23.1 | 23.7 |
| home: Resume | 18.9 | 19.5 | 35.1 | 33.4 |
| home: NextUp | 7.4 | 7.7 | 7.6 | 7.9 |
| home: Latest movies | 11.3 | 11.7 | 15.0 | 15.8 |
| home: Latest shows | 23.8 | 24.4 | 38.8 | 38.5 |
| grid: Movies 100 | 51.2 | 49.8 | 57.7 | 58.7 |
| grid: Audio 200 | 108.2 | 83.8 | 345.0 | 159.6 |
| search "the" | 37.8 | 32.8 | 52.4 | 43.8 |
| detail: movie | 23.1 | 14.0 | 38.4 | 22.5 |
| detail: series episodes | 10.5 | 10.7 | 12.3 | 12.8 |
| people: 100 | 56.0 | 29.9 | 544.4 | 50.7 |

On Galera the largest changes are people (544.4 to 50.7 ms), the Audio grid (345.0 to 159.6 ms), movie detail (38.4 to 22.5 ms), and search "the" (52.4 to 43.8 ms). Every other call changes by less than 2 ms.

### SQL statements per call

`../galera/tools/stmt_counts.py` reads `performance_schema` digests on one Galera node, stock against patched Jellyfin; measured 2026-09-26. The script adds a `spikes/jellymesh` path that is not in this repository to `sys.path`, but it imports nothing from it, so it runs as shipped.

| Call | Stock | Patched |
|---|---|---|
| people: 100 | 103 | 4 |
| grid: Audio 200 | 207 | 8 |
| search "the" | 32 | 9 |
| detail: movie | 48 | 29 |

### A second set of counts

The N+1 design note, [engineering/jellyfin-n1-hotspots.md](engineering/jellyfin-n1-hotspots.md), lists counts from a different method. The two sets differ, and nothing in the repository records why.

| Source | Method as named by the source | Date given by the source | `/Persons` | Movie detail |
|---|---|---|---|---|
| `../jellyfin-perf/README.md` | `performance_schema` digests on Galera (`stmt_counts.py`) | 2026-09-26 (results section) | 103 to 4 | 48 to 29 |
| `engineering/jellyfin-n1-hotspots.md` | `pg_stat_statements`, "on the real-library lab" | Only the code-reading research is dated (2026-09-26) | 101 to 3 | about 90 to about 17 |

`../jellyfin-perf/README.md` calls its `performance_schema` counts the newer ones. In the note, the "before" counts are marked measured, while the "after" counts appear as "now to after" in a design table, so I read those as design targets. The note also lists `/UserViews` at about 14 statements, with a design target of 4 to 6.

### Limits of the patch

NextUp (3 statements, 8 ms) and the UserViews extras and chapters (24 ms) were left alone. The gain on this library was not worth the extra patch surface.

## Provider-only latencies (history)

These predate the `jellyfin-perf` patch and are superseded by the section above; use that one for current numbers. They compare SQLite against the Galera provider (Pomelo plus patch) on its own, with stock Jellyfin query shapes, and they show where the provider stopped being the bottleneck.

p50 ms over 30 runs, 3-node cluster, real library. The SQLite column came from `spikes/jellymesh/README.md`, so I can't verify it here.

| Call | SQLite | Galera (Pomelo + patch) | Remaining bottleneck at that time |
|---|---|---|---|
| home: UserViews | 23 | 94 | Jellyfin N+1 (chapters and extras per view) |
| grid: Movies 100 | 61 | 61 | none (568 before the IN-subquery rewrite) |
| grid: Audio 200 | 134 | 633 | `DISTINCT` over whole rows including the `Data` blob, plus 200 N+1 stream reads |
| search "the" | 39 (72 results) | 76 (72 results) | n/a |
| detail: series episodes | n/a | 12 | n/a |
| people: 100 | 58 | 590 | lower-name dedupe `NOT EXISTS` ran as a per-row range scan, plus 100 N+1 person reads |

Server time dominated the slow calls. Under `../galera/tools/digest_profile.sh` the Movies grid spent 603 of 630 ms inside MySQL, so network round trips were not the cause. That 630 ms figure predates the IN-subquery rewrite and differs from the 568 ms in the table; nothing explains the difference. The remaining outliers were Jellyfin query shapes that MySQL plans badly, which is why they were addressed in the patch rather than in the provider.

The `NOT EXISTS` range scan is 339 ms in `engineering/galera-provider.md` and 438 ms in `../jellyfin-perf/README.md`, and neither says why they differ. The hash antijoin plan is 33 ms. Two smaller notes from the same runs: `tmp_table_size=256M` removed two on-disk temporary tables on the Audio grid (about 60 ms), and each `/health` probe opened 1.00 new unpooled connection before the provider's health-probe change.

## Parity

`../galera/tools/parity_ab.py <url-a> <url-b>` compares whole JSON responses from two Jellyfin nodes on the same data: same item ids in the same order, same totals, same DTO fields, for the benchmark call set plus larger pages. Fourteen calls were compared, including every person (8,677), 2,000 tracks, and 330 movies with People. That's a sample, not an exhaustive proof for every endpoint.

| Pair compared | Calls | Result |
|---|---|---|
| SQLite stock against SQLite patched | 14 | Matched |
| Galera stock against Galera patched | 14 | Matched |
| SQLite stock against Galera stock | 14 | Matched |

Two things the comparison doesn't cover:

- It skips three fields that vary per node or per request: `PlayAccess`, `ServerId`, and `Etag` (they are listed as `VOLATILE` in `../galera/tools/parity_ab.py`).
- Resume and NextUp were empty on the parity library.

It does catch real problems. Earlier, DateTime values stored as MySQL `datetime(6)` changed image tags after a migration, because Jellyfin hashes `DateModified.Ticks` into image tags. Storing DateTime as BIGINT ticks fixed that, and this same script is what found it.

## Migration timing

`jellyfin-dbmigrate copy` then `verify` on the real library (19,259 items, 308,330 rows, 31 tables), measured 2026-09-26. The row and table counts are repeated in [operations.md](operations.md#migration-timing).

| Step | Time | Verify |
|---|---|---|
| SQLite to Galera, 1 PXC node, podman | 38.8 s (Oracle provider), 39.9 s (Pomelo) | every row and column identical |
| SQLite to Galera, 3-node cluster | 46.2 s | identical; node 3 has every row |
| Galera to new SQLite | 28.2 s | round trip identical to the original |

The 38.8 s figure was taken with the Oracle provider, which the repository no longer uses; it's kept as history. Commands are in [operations.md](operations.md#migrate-sqlite-to-galera-and-back).

## Failure drills

### Database node loss

`../galera/lab/galera_drill.py` against the 3-node cluster, some time in September 2026 — the log doesn't give a day.

| Drill | Result |
|---|---|
| Play on Galera node A, read the row on another Galera node | Present when A's 200 returns (synchronous replication) |
| Read the same row through Jellyfin B's API, default per-node caches | Stale after 65 s: B's in-process UserData cache is never told |
| SIGKILL Galera node B under load, `FailOver` list | 204 requests, 0 failed, longest gap 2.9 s; node back and Synced 7 s after restart (IST) |
| SIGKILL the primary in single-writer mode | 206 requests, 0 failed, 2.9 s gap; the bootstrap node must come back as a joiner (`rejoin`) |
| 200 concurrent writes to one UserData row, Jellyfin A and B on different Galera nodes, before the retry patch | 69 HTTP 500 (35%), Galera certification conflicts (`Deadlock found`) |
| Same, both Jellyfins on the same Galera node | 200 of 200 succeeded, 0 conflicts |
| Same 200 writes with the user-data save retry (fresh context, jittered backoff, 6 attempts) | 200 of 200 succeeded on multi-writer Galera and on SQLite |

Before the retry patch, stock Jellyfin on plain MySQL also failed 2 to 7 of 200 concurrent first progress reports for one item with HTTP 500, from a check-then-insert race.

`engineering/galera-provider.md` recommends single-writer as the conservative choice for a stock Jellyfin build. The retry patch changes that for user-data writes: the two-Jellyfin multi-writer rows in [Concurrent load](#concurrent-load) and the 200-of-200 result above were measured with the patch in place.

### Authentication across nodes

`../galera/lab/auth_drill.py <login-node-url> <other-url> ...` creates a throwaway user, logs in on the first node, uses the token on the others, logs out, and checks the token is refused everywhere. Neither the run count nor the date was recorded; `../jellyfin-perf/README.md` groups it with the shared-mode runs (2 Jellyfins on a 3-node Galera cluster, measured 2026-09-26 for that section). Whether it ran with `JELLYFIN_SHARED_DB=1` is an inference from that grouping — the script doesn't set it.

| Check | Result |
|---|---|
| Token issued on Jellyfin node B, used on Jellyfin node C | Accepted 150 ms later |
| Same token right after logout on node B | Refused on both nodes |
| A stock node given a token issued elsewhere | Answers 401 |

### Direct play across a replica kill

Measured 2026-09-29 for GitHub issue #13, one run per case: a 2-replica StatefulSet behind a Traefik failover service in the lab, with a 26,976,301-byte static FLAC fetched by `curl` through the real ingress path. The write-up is [engineering/direct-play-failover.md](engineering/direct-play-failover.md).

Direct play works across a failover only through a client retry, because a direct-play response is a static file on shared storage: any replica can serve any byte range, so a resume needs no session affinity. Traefik can't re-home a response that has already started.

| Case | Result |
|---|---|
| Hard kill (`kill -9` of the supervised `jellyfin` process), client without retry | Connection reset; `curl` had about 6.2 MB of 26 MB and did not resume |
| Hard kill, client that reissues the request with `Range: bytes=<received>-` (a separate run from the row above) | Attempt 1 ended at 8,407,732 bytes on the primary. Attempt 2 was a 206 from the fallback and ended at the test script's own 20 s cap (10,240,000 bytes). Attempt 3 was a 206 that completed (8,328,569 bytes). The final file matched the source |
| Viewer-visible gap on the retry path | About 5 to 6 s in a single run: about 5.1 s until the client detected the reset, about 0.35 s until the retry was issued |
| Graceful pod delete | Kestrel drained for about 21 s after the pod was told to stop. One usable trial: a 20 to 25 s download finished inside the drain window |

Four caveats go with that table:

- The detection time is a `curl` over HTTP/2 figure. Other clients may detect a dead connection faster or slower; I haven't measured them.
- One usable graceful-delete trial doesn't show that graceful restarts are safe for a full-length stream. I expect a long stream to be cut when the grace period ends, but I haven't measured it.
- An authenticated Range retry was not measured. `/Videos/{id}/stream` and `/Audio/{id}/stream` answered range requests without a token; reading the code, that looks like stock Jellyfin behavior.
- `kubectl get pods` RESTARTS didn't increase for the in-container kill, because s6 respawns the process without restarting the container.

I haven't measured how clients behave. The client matrix in the write-up is inherited from public issue trackers and is inconclusive: only Moonfin documents a reconnect path, and that documentation covers Live TV. For the Android TV app I couldn't confirm behavior on a real backend death, for Swiftfin and Infuse I found no explicit reconnect-with-range documentation, and I didn't look at the web client at all.

HLS transcode failover splits two ways by configuration.

- Without the shared transcode directory, or for local (non-pool) ffmpeg jobs, a failover restarts ffmpeg on the surviving replica; `../transcode/docs/SHARED-TRANSCODE.md` files that under "Not covered". The [ROADMAP](ROADMAP.md#sticky-server-failover) records a Safari remux measured on 2026-09-27: it restarted on the fallback and again on the primary three minutes later, so one failover cost two restarts, and the viewer reported audio drifting out of sync. Keeping a failed-over session on one replica is still to do.
- With `JELLYMESH_SHARED_TRANSCODE_DIR=1` and the pool, drills A to C below showed no new ffmpeg.

The operations procedure is in [operations.md](operations.md#traefik-activepassive-failover).

### Shared transcode directory across replicas

Opt-in with `JELLYMESH_SHARED_TRANSCODE_DIR=1`, measured in the lab on 2026-09-28. Each drill ran 3 concurrent HLS sessions (1080p H.264 to 720p, 3 s segments, 12 s player buffer) through the Traefik failover route, with 2 Jellyfin replicas (`jm-jf-0` primary, `jm-jf-1` fallback) and one NVENC agent whose card model I didn't record, over plaintext. The client is `../transcode/spike/shared_dir_drill.py` and the write-up is `../transcode/docs/SHARED-TRANSCODE.md`. "Slow" means a segment request over 3 s, not counting each session's cold first segment, which took 3.7 to 4.8 s.

| Drill | Failed segments | Slow segments | Slowest in failover window | New ffmpeg for the sessions |
|---|---|---|---|---|
| A: delete the serving replica's pod at +45 s | 0 of 162 | 0 | 0.55 s | none |
| B: `kill -9` the serving Jellyfin process at +45 s | 0 of 162 | 0 | 1.33 s | none |
| C: restart the other replica while session 0 is paused 150 s, with progress pings sent only to that replica | 0 of 223 | 0 | 0.43 s | none |

I'm reading "failed" as a segment request that didn't succeed; the source only defines "slow".

Drill C found that before a code change the paused session's job was killed 60 s after the pause by the owner's timer. The change is in `../jellyfin-perf/bughunt/16-shared-transcode-dir.patch` (it touches `PlaystateController`). After the viewers stopped, each detached job ended 60 s later. Before the job was throttled to about 60 s of video ahead of its viewer (the jm7 draft), each orphan wrote 287 to 315 segments.

## Transcode pool results

The pool is the Rust GPU transcode pool described in [../transcode/README.md](../transcode/README.md). Full method and tables are in [engineering/transcode-calibration.md](engineering/transcode-calibration.md) and [engineering/transcode-plan.md](engineering/transcode-plan.md). All of it is lab work except where a row says otherwise.

### Bitrate delivery (calibration)

Calibration runs in the `tc-lab` k3s namespace on cluster GPU nodes: one node with an Intel Arc A380 (QSV) and one node with a Tesla P4 (NVENC). Each run covers 3 titles, 2 codecs, and 3 rate-control rungs (3, 8, and 15 Mbps caps), comparing delivered bitrate to the requested cap. The runs are P0 (2026-09-26/27), P5 (2026-09-27), and the pool-r1 check (2026-09-28).

| Card and setting | Delivered bitrate as a share of the cap | Over the cap? | Run |
|---|---|---|---|
| Arc, calibrated | 94.1 to 96.6% | No, in these runs (3 titles, 2 codecs, 3 rungs) | P5 |
| P4, calibrated (no AQ) | 93.7 to 102.4% | Yes, up to +2.4% (h264 at 3M). Sample A values come from the AQ variant run | P5 |
| Arc, legacy mapping (`-global_quality`) | Mean 16% (hevc) and 22% (h264) of the cap, range 4 to 60% | No, in these runs | P0 |
| Arc, legacy mapping | 7.4 to 22.4% at the 8M cap | No, in these runs | P5 |
| P4, legacy mapping (`-cq`) | Mean 54% (hevc) and 85% (h264), range 19 to 102% | Yes, up to 102% | P0 |
| P4, legacy mapping | h264 96 to 100%, hevc 35 to 47% at the 8M cap | No, in these runs | P5 |

The two Arc legacy ranges come from different runs and don't conflict. The P0 figures average over all caps and titles, with the 4 to 60% range spanning them; the P5 figures cover only the 8M cap.

Two further results affect how to read the table:

- **NVENC overshoot.** NVENC delivers 7 to 9% above its `-b:v`, so the shipped setting targets 90% of the cap. In the pool-r1 check (2026-09-28, both cards, 2160p-class sources to 1080p, 20 s), a 4M cap gave Arc 94.1 to 96.1% and P4 93.5 to 98.4%. A 60M cap gave Arc 93.0 to 95.7% and P4 81.1 to 82.6%; no run was over the cap.
- **Arc driver crash.** `h264_qsv` with `-look_ahead_depth` crashes with SIGSEGV on the Arc driver in every combination tried, so the renderer doesn't emit it for `h264_qsv`.

### Quality (VMAF)

VMAF scores exist for only part of the runs, so read them with that in mind. In the P0 run, 48 of 164 encodes have VMAF scores, and the 3 and 15 Mbps candidate rungs were not scored at all. The P5 run then scored the Sample B and Sample C cross-card table, the Sample A Arc rows, and the equal-bitrate comparison. P5 has no P4 Sample A rows for the shipped configuration, because the source left the library mid-run, so the shipped-configuration Sample A cross-card comparison is still open. The nearest proxy is the AQ variant (Arc minus P4 of +0.33 to +1.26), and AQ is not in the shipped configuration.

A gap is Arc VMAF minus P4 VMAF, so a negative gap means the P4 scored higher. Samples are named A (1080p SDR), B (1620p SDR), and C (4K HDR).

| Comparison | Result | Run |
|---|---|---|
| Arc versus P4 gap with the legacy mapping | -5.4 to -7.8 VMAF | P0 |
| Sample B, calibrated | Within +/-1.26 at 3, 8, and 15M for h264 and hevc | P5 |
| Sample C, calibrated | Fails the +/-1.5 gate: -2.28 to -2.83. Attributed to the two tone mappers (`tonemap_vaapi` on Arc, `tonemap_cuda` on P4) | P5 |
| Arc calibrated versus Arc legacy at equal bitrate | +0.83 to +1.49 VMAF | P5 |
| P4 h264 at 8M, calibrated versus legacy `-cq` at equal bitrate | About 0.5 VMAF lower (Sample B: 89.71 against 90.19) | P5 |
| Arc and P4 h264 at 8M versus x264 `-preset slow` | About 3.2 VMAF lower | P0 |

The CPU worker runs at 0.18 to 0.59 times realtime for one job at `-preset slow` (P0), so it's a fallback, not a playback tier.

### Startup latency

18 sessions on 2026-09-26 (23:33 to 23:36 CDT): 6 per title, sequential, h264 at 8 Mbps and 1080p, through the lab Jellyfin, all on the Arc, warm cache. Each session used a fresh `PlaySessionId` and a slightly different bitrate to force a real transcode.

This is the baseline with Jellyfin's `-probesize 1G` and warm NFS. With only 6 runs per title, the second column is the maximum of 6, not a p95.

| First segment, request to last byte (seconds) | p50 | Max of 6 |
|---|---|---|
| Sample A, 1080p SDR | 0.875 | 1.187 |
| Sample B, 1620p SDR | 0.798 | 0.894 |
| Sample C, 4K DV or HDR10 | 1.345 | 1.385 |

- Jellyfin pre-ffmpeg work, shim scheduling, and agent admission together take about 0.15 s (p50 0.151 to 0.164); the agent's spawn is 1 ms. The control plane is not the dominant latency.
- On the Arc worker (3 runs per title, warm NFS), the ffmpeg input probe with `-analyzeduration 200M -probesize 1G` took 0.235, 0.019, and 0.449 s for Samples A, B, and C.
- **Probe clamp.** The agent limits ffmpeg's `-probesize` and `-analyzeduration` with `TC_PROBE_CLAMP` (default `50M,5M`: probesize 50 MB and analyzeduration 5,000,000 microseconds; it lowers Jellyfin's values and never raises them; `0` turns it off, per `transcode/crates/agent/src/config.rs`). `engineering/transcode-plan.md` records that the 1G probe cost 8 to 12 s over the tower's 1 GbE link, that the clamp kept the mapped streams unchanged on all 3 titles, and that the first segment then took 0.78 to 0.88 s (2026-09-27). That's a separate run from the table above, and there is no per-title breakdown for it.
- Cold-cache startup, P4 startup, and CPU-worker startup are all unmeasured. Until a cold-cache run is done I can't call the startup target (p50 below 1.5 s, p95 below 3 s) met.

### Performance measurements

| Measurement | Value | Context |
|---|---|---|
| Shim overhead | 0.33 ms, against 71 ms for the Python prototype (the original spike in `transcode/spike/`) | Lab, n not stated |
| Static binary size | Agent 3.1 MB, shim 2.1 MB | musl static build |
| GPU-resident scale and tone map | Arc 4K HDR 268 fps against 29 fps; P4 98 against 51 fps. The lower figure in each pair is the non-GPU-resident path | Spike, not the native pool |
| Native drill, graceful pod delete mid-4K-HDR | Agent drained in 0.9 s, Jellyfin resumed at segment 22, 0 failed requests, lowest buffer 1.3 s | Lab, 2026-09-27 |
| Native pool serving a GPU-less Jellyfin | 0 failed requests, first segment 1.09 s | Lab, 2026-09-27 |

### Robustness checks

The protocol suite (`transcode/spike/proto_test.sh`) has 21 numbered cases per `.github/workflows/transcode.yml`. Cases 17, 20, and 21 have lettered steps (17a and 17b, 20a to 20d, 21a to 21h), so the suite runs 32 steps in all. `../transcode/README.md` states the same count. `engineering/transcode-plan.md` records "14/14" in its P1 checklist, which is the older figure.

| Measurement | Value | Context |
|---|---|---|
| Command allowlist | 134 real commands pass, 11 attacks rejected | Protocol suite case 13 |
| Fuzzing | 4 targets (`validate`, `render`, `filters`, `trickplay`), 10 min each with 4 parallel workers: 0 crashes, timeouts, or out-of-memory kills. Total executions 13.1M, 2.5M, 3.0M, 7.9M | Lab; the plan lists the counts in the same order as the targets and does not label each count |

### Dolby Vision 7 -> 8.1 conversion measurements

DV 7 -> 8.1 is opt-in and off by default, and it's the one piece of this I'd call production. It has been playing as Dolby Vision on my Android TV since 2026-09-29 and I've tested it repeatedly, and since 2026-09-30 the EAC3 track passes through from the SHIELD to an AV receiver as Dolby Digital Plus. That's me watching it work, not a measurement. The feature itself is described in [dolby-vision.md](dolby-vision.md); what follows are the lab and library numbers behind it.

| Measurement | Value | Context |
|---|---|---|
| Real in-band DV7 FEL title, 30 s window | 729 of 729 RPUs rewritten, 2,459 EL NALs dropped; init-segment DOVI record profile 8, compatibility id 1 (plain remux: profile 7) | Prod `jellyfin-qsv` ffmpeg 8.1.2, one title, 2026-09-27 |
| Conversion time for that window | 1.772 s against 1.217 s for a plain remux (about 17 times realtime) | Page-cache warm; the plain remux ran first; cold-NFS cost not measured |
| Library census | 336 video files, 165 with a DOVI record, 122 profile 7 (76 FEL, 46 MEL), 43 profile 8; 0 of 122 carry the RPU only in a Matroska Block Addition | Production library, prod `jellyfin-qsv`, 2026-09-27 |
| FEL enhancement-layer residual | Mean residual between 0.07% and 1.03% of the 10-bit range on 4 FEL titles. Highlight-to-full ratio (highlight-masked mean residual divided by full-frame mean residual): 1.618, 0.563, 0.885, 1.393 | Proxy, not a VMAF comparison |
| DV-removed fallback on the same title | `remove_dovi` leaves 336 small NAL 63 units (about 10 bytes each); A/V start identical to a plain remux | Prod `jellyfin-qsv` ffmpeg, one title |

The method behind the FEL residual row is written up in `engineering/gh15-fel-visibility.md`.

Three gaps in the record:

- A converting job reads the source about twice, because the pipeline runs two ffmpeg invocations (`transcode/crates/agent/src/dv81.rs`). That is from reading the code; what the second read costs on cold NFS is unmeasured.
- The `fallback_no_rpu` gate has never been exercised on a real title, because my library has none.
- The `hvcE` RPU reader is tracked in GitHub issue #4 and is still [planned](ROADMAP.md).

## Reproducing these results

| Script | Reproduces | Usage |
|---|---|---|
| `galera/lab/galera-lab.sh` | The 3-node PXC lab used by the Galera sections | `galera/lab/galera-lab.sh boot`, then `join 2`, `join 3`, `db`, `status`; `down` removes it |
| `galera/lab/jf-galera.sh` | Jellyfin nodes on that cluster | `up <name> <host-port> <galera-nodes>` (needs `JG_PLUGIN`) |
| `galera/lab/reset_data.py` | Pristine data before a benchmark | `reset_data.py <real.db> [--sqlite s1,s2] [--galera a,b,c] [--dbmigrate path] [--mysql gl-db1:13301]` |
| `galera/tools/parity_ab.py` | [Parity](#parity) | `parity_ab.py <url-a> <url-b>` |
| `galera/tools/stmt_counts.py` | [SQL statements per call](#sql-statements-per-call) (adds a `spikes/` path that is not in the repo; runs as shipped) | `stmt_counts.py <jellyfin-url> [runs] [--db gl-db1]` |
| `galera/tools/digest_profile.sh` | Server time per call | `digest_profile.sh <url> <path?query> [runs] [galera-container]` |
| `galera/tools/slow_sql.py` | Slowest statements of one call, with `EXPLAIN ANALYZE` | `slow_sql.py <url> <path?query> <outdir> [--db gl-db1] [--top 2]` |
| `galera/tools/ddl_audit.py` | Truncation and index checks on generated DDL | `ddl_audit.py <script.sql> [real-sqlite-db]` |
| `galera/lab/galera_drill.py` | [Database node loss](#database-node-loss) | `galera_drill.py consistency <url-a> <url-b>`; `galera_drill.py failover <url> <galera-container> [seconds]` |
| `galera/lab/auth_drill.py` | [Authentication across nodes](#authentication-across-nodes) | `auth_drill.py <login-node-url> <other-url> [<other-url> ...]` |
| `transcode/spike/shared_dir_drill.py` | [Shared transcode directory](#shared-transcode-directory-across-replicas) | `JM_TOKEN=<api key> shared_dir_drill.py <url> --title SUBSTR [--sessions 3] [--seconds 150]`; more options in the script header |

Not reproducible from this repository:

- **Concurrent load, per-call latency, and coherence.** The drivers are `spikes/jellymesh/load.py`, `spikes/jellymesh/bench.py`, and `mesh/mesh_drill.py`, and none of them is in the repository.
- **Transcode calibration, startup latency, and the Dolby Vision census and proof.** No script here reproduces them. The raw calibration rows are in `transcode/calibration/` (`2026-09-26-p0.csv`, `2026-09-27-p5.csv`, `2026-09-28-pool-r1.csv`).

Fuzzing is reproducible: [CONTRIBUTING.md](../CONTRIBUTING.md#building-and-testing-the-rust-workspace) gives the local command for the CI `fuzz-smoke` job. The remaining gaps are listed under [open documentation gaps](../CONTRIBUTING.md#open-documentation-gaps).

# jellyfin-perf

A patch overlay for Jellyfin 12.1 that fixes N+1 (one query per row) and slow query shapes on MySQL and Galera (a multi-primary MySQL cluster), and adds an opt-in shared-database mode. It also carries the bughunt series (patches 00-19): individual fixes found in a targeted review of Jellyfin 12.1 ([engineering/bughunt.md](../docs/engineering/bughunt.md)). Terms are defined in the [glossary](../docs/architecture.md#glossary).

## Status

| Part | Status |
|---|---|
| `jellyfin-12.1-perf.patch` (query shapes, retries, transcode fixes) | Implemented |
| Shared-database mode, `JELLYFIN_SHARED_DB=1` | Implemented, opt-in, off by default. Lab-verified (see [Results](#results)) |
| Bughunt series 00-19 | Implemented; applied by default (`BUGHUNT=1`) |
| Dolby Vision 7 -> 8.1 patches (13, 17, 18, 19) | Production (2026-09-29), opt-in, off by default [^prod] |
| Shared transcode directory (03, 16) | Implemented, opt-in, off by default. Lab-verified (16) |

[^prod]: Production is reported by the maintainer: DV 7 -> 8.1 is deployed and plays as Dolby Vision on an Android TV (SHIELD), tested repeatedly. [engineering/bughunt.md](../docs/engineering/bughunt.md) still marks 13 and 18 as MIXED evidence. AVR Dolby Digital Plus passthrough: Not measured.

## What it does

`jellyfin-12.1-perf.patch` fixes statement counts and slow query plans that show up when Jellyfin runs on MySQL or Galera instead of SQLite. The query-shape changes return the same rows in the same order with the same DTOs (data transfer objects, the JSON response models); see [Parity](#parity). The bughunt patches change behavior; the table in [Bughunt series 00-19](#bughunt-series-00-19) shows which are gated by a flag.

### Changes in `jellyfin-12.1-perf.patch`

Test bed for figures sourced to docs/RESULTS.md: single workstation, 3-node PXC, 19,259-item library, 2026-09-26.

| Change | Where | Problem and measured result | Source |
|---|---|---|---|
| Persons for a page load in one query (`ILibraryManager.GetPersons`), with a per-name fallback if the batch throws | `LibraryManager.GetPeopleItems`, `DtoService.AttachPeople` | `GetPerson(name)` did a full item load per row. On Galera, `/Persons?Limit=100` took 103 statements stock and 4 patched; movie detail 48 and 29 | [RESULTS.md](../docs/RESULTS.md#per-call-latency-and-statement-counts), Per-call latency (performance_schema) |
| Person dedupe by `GroupBy(lower(Name)).Min(Id)` instead of a correlated `NOT EXISTS` | `PeopleRepository.GetPeople` | MySQL ran the `NOT EXISTS` as a range scan per row: 339 ms against 33 ms for the hash antijoin plan | [RESULTS.md](../docs/RESULTS.md#provider-only-latencies-history), Provider-only latencies |
| `HasLyrics` for a page of tracks in one query (`IMediaStreamRepository.GetItemIdsWithStreamType`) | `DtoService` | Removes one full stream load per Audio item. On Galera, the 200-track grid took 207 statements stock and 8 patched | [RESULTS.md](../docs/RESULTS.md#per-call-latency-and-statement-counts), Per-call latency |
| Dedupe by id sub-select instead of `DISTINCT` over whole rows | `BaseItemRepository.ApplyGroupingFilter` | `DISTINCT` hashed the multi-KB `Data` JSON of every row into temp tables. Not measured separately | From code reading |
| "Date played" sort key (`DatePlayed`): alternate versions come from the `PrimaryVersionId` index (MAX over a per-alternate lookup), combined with the item's own value by `CASE`, instead of MAX over a correlated `UNION ALL` | `OrderMapper` | MySQL drove the alternate side from every UserData row of the user per sorted row (166,800 lookups for 400 rows, per the patch comment). Resume took 0.4-2.6 s with a few hundred in-progress items and 0.14 s after the fix | Patch comment; [RESULTS.md](../docs/RESULTS.md#concurrent-load), Concurrent load |
| Early-exit wedge fix: unregister a transcode whose ffmpeg exited non-zero before its first segment | `TranscodeManager` | Without it, a retry of that play session waits for a segment that never appears. Found by the transcode-pool work | From code reading |
| Transcode marker created race-safely | `BaseApplicationPaths` | Two transcodes right after a wipe raced on `.jellyfin-transcode` (`FileShare.None`) and one returned HTTP 500 | From code reading |
| With `JELLYFIN_SHARED_DB=1`, the startup wipe of the transcode directory only deletes files older than 6 h | `TranscodeManager.DeleteEncodedMediaCache` | Several replicas share one transcode directory; a restart must not delete another replica's live segments | From code reading |
| User-data save retried on a write conflict (6 attempts, fresh context, jittered backoff) | `UserDataManager.SaveUserData` | Check-then-insert is atomic only where writes serialize (SQLite). On MySQL, 7 of 200 concurrent first progress reports returned HTTP 500 (patch comment). On multi-writer Galera, 69 of 200 concurrent writes to one row returned HTTP 500 (35%) before the retry | Patch comment; [RESULTS.md](../docs/RESULTS.md#failure-drills), Failure drills |

Not patched: NextUp (3 statements, 8 ms) and UserViews extras and chapters (24 ms). The gain on the test library does not justify the added patch surface.

### Shared-database mode

Set `JELLYFIN_SHARED_DB=1` on every node that shares one database (Galera, MySQL, and similar). Patch 15 adds one table, `JellyMeshItemInvalidation`, in the same database. The Galera provider (Pomelo, the EF Core MySQL provider, plus a patch; DateTime stored as ticks) is described in [galera/README.md](../galera/README.md).

| Behavior with `JELLYFIN_SHARED_DB=1` | Detail |
|---|---|
| User data | Read from the database on every call. It bypasses the per-node LRU and the user-data rows embedded in cached items. Measured cross-node coherence: see [Results](#results) |
| Item cache | Entries live 5 s (`SharedCacheTtlMs = 5000` in the patch), so metadata changed by another node's scan shows within 5 s at most. The cache holds folders and views. Without it, throughput at 32 clients fell from 112.0 to 35.6 req/s ([RESULTS.md](../docs/RESULTS.md#one-store-versus-a-redis-response-cache-tier), One store versus a Redis tier); the patch comment cites 105 -> 36 req/s |
| Item invalidation (patch 15) | A poll on every node reads `JellyMeshItemInvalidation` and evicts changed items before the 5 s expiry. The table is created with `CREATE TABLE IF NOT EXISTS` on first use |
| Login sessions | Devices are looked up in the database instead of the startup snapshot |

Per node: live sessions ("now playing", remote control), client capabilities, and running transcodes. Plugins that keep their own SQLite file are not shared-database safe.

### Bughunt series 00-19

`bughunt/NN-*.patch` apply on top of `jellyfin-12.1-perf.patch`, in numeric order. Patches with a flag in the Opt-in column are off unless the flag is set; the others apply unconditionally. Patch 14 is described in `image/Containerfile.jm7`; its per-patch write-up is Not documented yet (doc TODO in [CONTRIBUTING.md](../CONTRIBUTING.md)).

> **Warning:** set `JELLYMESH_DOVI_P7_TO_81=1` only where the transcode pool is in the ffmpeg path. Without the shim, stock ffmpeg copies raw profile 7 while the playlist advertises 8.1 (source: patch 13 analysis in [engineering/bughunt.md](../docs/engineering/bughunt.md), known issue 5).

| # | Topic | Opt-in flag | Status |
|---|---|---|---|
| 00 | Build fix: test project compiles again | none | Implemented |
| 01 | Kill timer: deferred delete skips when a replacement job is already registered on the same output | none | Implemented |
| 02 | Kill timer: paused HLS/DASH jobs get a 180 s grace instead of 60 s | none | Implemented |
| 03 | Shared transcode directory: wait on a fresh shared-dir segment before starting a local ffmpeg (K2 incident fix) | `JELLYMESH_SHARED_TRANSCODE_DIR=1` | Implemented, opt-in, off by default |
| 04 | Segment-wait loops exit on client or proxy abort | none | Implemented |
| 05 | HLS remux: `-noaccurate_seek` for the transcoded audio track (A/V drift fix) | none | Implemented |
| 06 | Subtitle selection prefers a same-language text track over a tied PGS/VobSub track | none | Implemented |
| 07 | `PlaybackProgress` writes use a bounded write path | none | Implemented |
| 08 | MediaSources and MediaStreams batching for a page of items: 3N -> 3 statements | none | Implemented |
| 09 | A positionless Stop no longer assumes `Played=true` without a local session record | none | Implemented |
| 10 | Display-preferences first write retries instead of returning HTTP 500 | none | Implemented |
| 11 | Trickplay deletion disposes its `DbContext` promptly | none | Implemented |
| 12 | `serviceworker.js` served with `Cache-Control: no-cache`; throttled warning on a stale web client | none | Implemented |
| 13 | Dolby Vision 7 -> 8.1 decision half | `JELLYMESH_DOVI_P7_TO_81=1` | Production (2026-09-29), opt-in, off by default |
| 14 | `UserDataChangeNotifier` survives a database failover | none | Implemented |
| 15 | Shared item-cache invalidation via `JellyMeshItemInvalidation` | `JELLYFIN_SHARED_DB=1`; opt out with `JELLYFIN_SHARED_INVALIDATION=0` | Implemented, opt-in, off by default |
| 16 | Shared transcode directory plus session keepalive, lease-scoped cleanup, seek takeover. For HLS jobs with the flag on, Jellyfin sets `JELLYMESH_KEEPALIVE` in the ffmpeg process environment | `JELLYMESH_SHARED_TRANSCODE_DIR=1` | Implemented, opt-in, off by default. Lab-verified |
| 17 | Dolby Vision 7 -> 8.1: fMP4 HLS so the init segment's `dvvC` marks Dolby Vision | `JELLYMESH_DOVI_P7_TO_81=1` | Production (2026-09-29), opt-in, off by default |
| 18 | Dolby Vision 7 -> 8.1: non-negative fMP4 timestamps, 128-character audio codec list, TrueHD/MLP dropped from candidates | `JELLYMESH_DOVI_P7_TO_81=1` | Production (2026-09-29), opt-in, off by default |
| 19 | Dolby Vision 7 -> 8.1: EncoderValidator detects `eac3`; TrueHD/MLP with 6 or more channels encodes to EAC3 5.1 at 640 kb/s when the client lists `eac3`, otherwise AAC | `JELLYMESH_DOVI_P7_TO_81=1` | Production (2026-09-29), opt-in, off by default |

Patch 13 decides and marks the job; the conversion itself runs in the transcode pool. See [Dolby Vision 7 -> 8.1](../docs/dolby-vision.md) for how to enable and verify it.

#### Interface changes for plugin authors

The perf patch and the bughunt series add members to plugin-facing interfaces. A plugin that implements one of these interfaces fails to compile or load against the patched assemblies; a plugin that only calls them keeps binding. The table is built from the patches. The series also changes the value of the public constant `AudioCodecListValidationRegexStr` (patch 18).

| Patch | Interface | Change |
|---|---|---|
| perf | `ILibraryManager` | Non-default member `GetPersons` added |
| perf | `IMediaStreamRepository` | Non-default member `GetItemIdsWithStreamType` added |
| 07 | `IUserDataManager` | One defaulted member added |
| 08 | `IMediaStreamRepository`, `IMediaAttachmentRepository`, `IMediaSegmentManager`, `IMediaSourceManager` | Non-default members added |

Check third-party plugins that replace one of these services before loading them.

## Requirements

| Requirement | Detail |
|---|---|
| `git` | Network access to `https://github.com/jellyfin/jellyfin.git` for the first run |
| .NET SDK | `dotnet build` of `Jellyfin.Server.csproj`; output path is `bin/Release/net10.0` |
| Network | First run also restores NuGet packages |

## Build

Run from the repository root:

```bash
# Perf patch plus bughunt 00-19 (default)
BUGHUNT=1 ./jellyfin-perf/build.sh
# Perf patch only
BUGHUNT=0 ./jellyfin-perf/build.sh
```

`build.sh` runs these steps:

1. Clones Jellyfin into `$JF_SRC` if it is not there, then fetches tags.
2. Checks out `v12.1` and runs `git clean` (`bin/` and `obj/` are ignored, so the build cache survives).
3. Applies `jellyfin-12.1-perf.patch`, then every `bughunt/[0-9][0-9]-*.patch` in numeric order unless `BUGHUNT=0`.
4. Runs `dotnet build Jellyfin.Server/Jellyfin.Server.csproj -c Release`.
5. Copies seven assemblies into `$JF_OVERLAY`.

The seven assemblies are always all copied, regardless of `BUGHUNT`: `Emby.Server.Implementations`, `Jellyfin.Server.Implementations`, `MediaBrowser.Controller`, `MediaBrowser.MediaEncoding`, `MediaBrowser.Model`, `Jellyfin.Api`, and `jellyfin`. The perf patch alone touches the first four; the series adds the last three.

## Configure

Build-time variables are read from `build.sh`. All runtime variables are listed in [docs/configuration.md](../docs/configuration.md).

| Variable | Default | Effect |
|---|---|---|
| `JF_SRC` | `$HOME/.cache/jellymesh-vendor/jellyfin-src` | Jellyfin source checkout |
| `JF_OVERLAY` | `$HOME/.cache/jellymesh-vendor/jellyfin-perf` | Output directory for the seven assemblies |
| `BUGHUNT` | `1` | `0` skips the bughunt series |

Runtime variables on the Jellyfin server, from the patches:

| Variable | Default | Patch | Effect |
|---|---|---|---|
| `JELLYFIN_SHARED_DB` | unset (off) | perf, 15 | `1` turns on shared-database mode, including the 6 h startup-wipe rule |
| `JELLYFIN_SHARED_INVALIDATION` | on when shared mode is on | 15 | `0` disables the invalidation poll |
| `JELLYFIN_SHARED_INVALIDATION_POLL_MS` | `250` | 15 | Poll interval in ms; values under 50 or unparsable values fall back to 250 |
| `JELLYMESH_SHARED_TRANSCODE_DIR` | unset (off) | 03, 16 | `1` lets several replicas use one transcode directory |
| `JELLYMESH_KEEPALIVE` | set by Jellyfin | 16 | Keepalive file path placed in the HLS ffmpeg environment (read by the transcode shim) when the shared-directory flag is on; you do not set it |
| `JELLYMESH_DOVI_P7_TO_81` | unset (off) | 13, 17, 18, 19 | `1` enables DV 7 -> 8.1; needs the transcode pool in the ffmpeg path |

## Use

The overlay is a set of DLLs that replace the stock ones in a Jellyfin 12.1 image. Production images layer the overlay on a digest-pinned hotio Jellyfin base image; see [docs/operations.md](../docs/operations.md) for the image build and rollout steps. In the lab, mount the overlay over the stock image:

```bash
JG_OVERLAY="$HOME/.cache/jellymesh-vendor/jellyfin-perf" galera/lab/jf-galera.sh up <name> <host-port> <galera-nodes>
```

`JG_OVERLAY` is read by `galera/lab/jf-galera.sh`; see [galera/README.md](../galera/README.md).

## Test

The patched Jellyfin test suites cover the integrated tree (perf patch plus the series, Release build), run from the patched checkout in `$JF_SRC`.

| Suite |
|---|
| `Jellyfin.Api.Tests` |
| `Jellyfin.Controller.Tests` |
| `Jellyfin.MediaEncoding.Tests` |
| `Jellyfin.MediaEncoding.Hls.Tests` |
| `Jellyfin.Model.Tests` |
| `Jellyfin.Server.Implementations.Tests` |
| `Jellyfin.Server.Tests` |
| `Jellyfin.Providers.Tests` |

Per-run counts and per-patch tests are in [engineering/bughunt.md](../docs/engineering/bughunt.md); some rows there are marked MIXED, meaning part of the claim is not measured. Test command: Not documented yet (doc TODO in [CONTRIBUTING.md](../CONTRIBUTING.md)).

Response parity between two builds is checked with `galera/tools/parity_ab.py <url-a> <url-b>`.

### Results

All numbers below: single workstation (12 cores, 62 GB), every node in a podman container on one bridge network, the real library (19,259 items), 30 runs per call, p50 only (no spread reported here), warm cache. Measured 2026-09-26. Method and caveats: [docs/RESULTS.md](../docs/RESULTS.md).

Per-call p50 latency in ms ("Galera" is the JellyMesh Galera provider on a 3-node PXC cluster, Jellyfin talking to one node):

| Call | SQLite stock | SQLite patched | Galera stock | Galera patched |
|---|---|---|---|---|
| home: UserViews | 15.7 | 14.8 | 23.1 | 23.7 |
| home: Resume | 18.9 | 19.5 | 35.1 | 33.4 |
| home: NextUp | 7.4 | 7.7 | 7.6 | 7.9 |
| home: Latest movies | 11.3 | 11.7 | 15.0 | 15.8 |
| home: Latest shows | 23.8 | 24.4 | 38.8 | 38.5 |
| grid: Movies 100 | 51.2 | 49.8 | 57.7 | 58.7 |
| grid: Audio 200 | 108.2 | 83.8 | 345.0 | 159.6 |
| search 'the' | 37.8 | 32.8 | 52.4 | 43.8 |
| detail: movie | 23.1 | 14.0 | 38.4 | 22.5 |
| detail: series episodes | 10.5 | 10.7 | 12.3 | 12.8 |
| people: 100 | 56.0 | 29.9 | 544.4 | 50.7 |

Statements per call on Galera, counted with `galera/tools/stmt_counts.py` from performance_schema:

| Call | Stock | Patched |
|---|---|---|
| people: 100 | 103 | 4 |
| grid: Audio 200 | 207 | 8 |
| search 'the' | 32 | 9 |
| detail: movie | 48 | 29 |

The hotspot write-up in [engineering/jellyfin-n1-hotspots.md](../docs/engineering/jellyfin-n1-hotspots.md) counted `/Persons` as 101 -> 3 statements with pg_stat_statements. The table above uses the newer performance_schema counts on Galera.

#### Parity

`galera/tools/parity_ab.py` compares whole JSON responses. Scope and caveats are in [docs/RESULTS.md](../docs/RESULTS.md#parity), Parity.

| Comparison | Scope | Result |
|---|---|---|
| SQLite stock vs SQLite patched | 14 calls, including every person (8,677), 2,000 tracks, and 330 movies with People | Identical |
| Galera stock vs Galera patched | same | Identical |
| SQLite stock vs Galera stock | same | Identical |

This is a sample of calls, not an exhaustive guarantee. Resume and NextUp were empty on the parity library, so those two calls carry no coverage.

#### Shared mode: coherence and load

Method: 2 Jellyfins on a 3-node Galera cluster, 9 users, 20% writes, 30 s per cell ([RESULTS.md](../docs/RESULTS.md#one-store-versus-a-redis-response-cache-tier), One store versus a Redis tier; the load driver is not in this repository). Both Jellyfins share one host's 12 cores, so the cells understate a real two-host deployment.

| Measurement | Result | Clients | Source |
|---|---|---|---|
| Write on node A visible on node B, `JELLYFIN_SHARED_DB=1` | First poll, 8 of 8 trials, about 40 ms including the poll. The poll interval used in that test is Not documented yet; the code default is 250 ms | n/a | [RESULTS.md](../docs/RESULTS.md#one-store-versus-a-redis-response-cache-tier), One store versus a Redis tier |
| Same, default per-node caches | Still stale after 65 s | n/a | same |
| Throughput, `JELLYFIN_SHARED_DB=1` on Galera | 90.7 / 112.0 req/s | 8 / 32 | same |
| Throughput, default per-node caches (incoherent) | 81.2 / 104.7 req/s | 8 / 32 | same |
| Throughput, `JELLYFIN_SHARED_DB=1` with the item cache removed | 27.5 / 35.6 req/s | 8 / 32 | same |
| Throughput, Redis response-cache tier | 75.8 / 53.3 req/s | 8 / 32 | same |
| Auth drill (`galera/lab/auth_drill.py`) | Token issued on node B accepted on node C 150 ms later; refused on both right after logout on B; a stock node answers 401 to a token issued elsewhere | n/a | [RESULTS.md](../docs/RESULTS.md#failure-drills), Failure drills |

## Limitations

- The patch targets Jellyfin tag `v12.1` only; a new Jellyfin release needs a rebase of the patch and the series.
- Per-node state stays per node (see [Shared-database mode](#shared-database-mode)). Client-to-node affinity is only partly addressed by patch 16.
- At 1 client SQLite is faster than Galera (44.8 vs 27.6 req/s, patched; [RESULTS.md](../docs/RESULTS.md#concurrent-load), Concurrent load); the shared cluster is faster under load.
- The perf patch and patches 07 and 08 change plugin-facing interfaces (see [Interface changes for plugin authors](#interface-changes-for-plugin-authors)).
- Dolby Vision conversion needs the transcode pool in the ffmpeg path. AVR Dolby Digital Plus passthrough: Not measured.
- Statement counts for patch 08 (3N -> 3) come from unit tests; a live-lab count is listed as a follow-up in the engineering log.

## Related docs

- [Bug hunt engineering log](../docs/engineering/bughunt.md)
- [Hotspot analysis](../docs/engineering/jellyfin-n1-hotspots.md)
- [Results](../docs/RESULTS.md)
- [Configuration reference](../docs/configuration.md)
- [Operations](../docs/operations.md)
- [Dolby Vision 7 -> 8.1](../docs/dolby-vision.md)
- [Architecture](../docs/architecture.md)
- [Galera provider](../galera/README.md)
- [Transcode pool](../transcode/README.md)

## License

GPL-2.0, as Jellyfin. See [LICENSE](../LICENSE).

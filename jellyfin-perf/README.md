# Jellyfin 12.1 query patch (JellyMesh fork)

`jellyfin-12.1-perf.patch` applies to jellyfin tag `v12.1` (ee91c75). It changes query shapes
only: same rows, same order, same DTOs (verified below). `build.sh` builds the three server
assemblies it touches; the lab mounts them over the stock image
(`JG_OVERLAY=… lab/jf-galera.sh up`).

| Change | Where | Why |
|---|---|---|
| Persons for a page loaded in **one** query (`ILibraryManager.GetPersons`), falling back to the per-name loop if the batch throws | `LibraryManager.GetPeopleItems`, `DtoService.AttachPeople` | `GetPerson(name)` did a full item load per row: `/Persons?Limit=100` = 103 statements, movie detail 48 |
| Person dedupe by `GroupBy(lower(Name)).Min(Id)` instead of a correlated `NOT EXISTS` | `PeopleRepository.GetPeople` | MySQL ran the `NOT EXISTS` as a range scan per candidate (438 ms) |
| `HasLyrics` for a page of tracks in one query (`IMediaStreamRepository.GetItemIdsWithStreamType`) | `DtoService` | one full stream load per Audio item: 200 statements per 200-track grid |
| Dedupe by id sub-select instead of `DISTINCT` over whole rows | `BaseItemRepository.ApplyGroupingFilter` | DISTINCT hashed the multi-KB `Data` JSON of every row into temp tables |
| "Date played" sort key: alternate versions looked up from the `PrimaryVersionId` index (MAX over the item's alternates of a per-alternate lookup), combined with the item's own by CASE, instead of MAX over a correlated UNION ALL | `OrderMapper` (DatePlayed) | MySQL drove the alternate side from every UserData row of the user, per sorted row: 166,800 lookups for 400 candidates. Resume 0.4-2.6 s with a few hundred in-progress items → 0.14 s |
| User-data save retried (fresh context, jittered backoff, 6 attempts) on a write conflict | `UserDataManager.SaveUserData` | check-then-insert is atomic only where writes serialize (SQLite). On MySQL, concurrent first progress reports for one item raced on the insert (stock: 2-7 of 200 → HTTP 500); multi-writer Galera aborted commits on certification (35% → 500). Patched: 200/200 in both, 200/200 on SQLite |

## Shared-database mode (`JELLYFIN_SHARED_DB=1`)

For several Jellyfin nodes on one database (Galera, MySQL, …) with **no plugin and no second
store**: user data is always read from the database (no per-node LRU, and not from the rows embedded
in cached item objects), and the item cache keeps entries for 5 s only (it holds folders/videos;
without it every request re-read them: 105 → 36 req/s). User data is exact across nodes; metadata
another node's scan changes shows within 5 s. Login sessions (Devices) are looked up in the database
instead of the startup snapshot (MEASURED, `galera/lab/auth_drill.py`: token issued on node B
accepted on node C 150 ms later, refused on both right after logout on B; a stock node answers 401
to a token issued elsewhere). Still per node by design: live sessions ("now playing", remote
control), client capabilities, running transcodes.

MEASURED, 2 Jellyfins on 3-node Galera, 9 users, 20% writes: write on A visible on B at the first
poll (8/8, ~40 ms incl. the poll itself; stock: still stale after 65 s); responses identical to stock
SQLite; 90.7 / 112.0 req/s at 8 / 32 clients (default caches, incoherent: 81.2 / 104.7; with the
Redis response-cache tier: 75.8 / 53.3).

## Results (MEASURED 2026-09-26, real library, single host, 30 runs, p50 ms)

| Call | SQLite stock | SQLite patched | Galera stock | Galera patched |
|---|---|---|---|---|
| home: UserViews | 15.7 | 14.8 | 23.1 | 23.7 |
| home: Resume | 18.9 | 19.5 | 35.1 | 33.4 |
| home: NextUp | 7.4 | 7.7 | 7.6 | 7.9 |
| home: Latest movies | 11.3 | 11.7 | 15.0 | 15.8 |
| home: Latest shows | 23.8 | 24.4 | 38.8 | 38.5 |
| grid: Movies 100 | 51.2 | 49.8 | 57.7 | 58.7 |
| grid: Audio 200 | 108.2 | **83.8** | 345.0 | **159.6** |
| search 'the' | 37.8 | 32.8 | 52.4 | 43.8 |
| detail: movie | 23.1 | **14.0** | 38.4 | **22.5** |
| detail: series episodes | 10.5 | 10.7 | 12.3 | 12.8 |
| people: 100 | 56.0 | **29.9** | 544.4 | **50.7** |

"Galera" = the JellyMesh Galera provider (Pomelo + patch, ticks storage) on a 3-node PXC cluster,
Jellyfin talking to one node over the podman network.

Statements per call on Galera (`galera/tools/stmt_counts.py`, performance_schema):

| Call | stock | patched |
|---|---|---|
| people: 100 | 103 | 4 |
| grid: Audio 200 | 207 | 8 |
| search 'the' | 32 | 9 |
| detail: movie | 48 | 29 |

Parity (`galera/tools/parity_ab.py`, whole JSON responses incl. every person (8,677), 2,000
tracks, 330 movies with People): **identical** for SQLite stock vs SQLite patched, Galera stock vs
Galera patched, and SQLite stock vs Galera stock.

Not changed, and why: NextUp (3 statements, 8 ms here) and UserViews extras/chapters (24 ms) are
not worth fork surface on this library.

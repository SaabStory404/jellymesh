# Jellyfin 12.1 query-per-row (N+1) hotspots and fixes

Source: code reading of tag v12.1 by a research subagent (2026-09-26); statement counts marked
MEASURED come from pg_stat_statements on the real-library lab, the rest are code-derived.
Paths relative to the jellyfin repo.

Already batched upstream in 12.1: `DtoService.GetBaseItemDtos` (Emby.Server.Implementations/Dto/
DtoService.cs:171-270) batches UserData, resume data, child/played counts, artists, PersonInfo rows
and alternate-version ids. The grid endpoint does not loop per row.

Caches: `LibraryManager._cache` (LRU, ProcessorCount×100) only holds folders, Video, LiveTvChannel
and MusicArtist (`RegisterItem`, LibraryManager.cs:325-344) — **Person/Audio/Genre/Studio are never
cached**, so every `GetPerson` hits the DB. User data is served from memory (no fix needed).

| # | Endpoint | Per-row call | Statements now → after |
|---|---|---|---|
| 1 | `GET /Items/{id}` (detail) | `AttachPeople` → `_libraryManager.GetPerson(c)` per cast member (DtoService.cs:939-954) → `RetrieveItem` (full row + 6 Includes) | ~90 (MEASURED) → ~17 |
| 2 | `GET /Persons` | `GetPeopleItems` → `GetPerson(i.Name)` per row (LibraryManager.cs:3620-3639) | 101 (MEASURED) → 3 |
| 3 | `/Items?Fields=People` | same `AttachPeople` loop per item | items × cast → +1 |
| 4 | `/Shows/NextUp` | per series: `GetAllVersions()` twice (TVSeriesManager.cs:129, 230, 265) | ~4 per series → 1 total |
| 5 | detail: media sources/extras | `GetVersionInfo` per version; `GetExtras()` run twice (DtoService.cs:1501, Movie.cs:33) | ~13 → ~9 |
| 6 | `/UserViews` | per view: `GetChapters` (DtoService.cs:1464) + `GetExtras` (1501/1513) | ~14 (MEASURED) → 4-6 |

## Fix 1 — batch person lookups (#1-#3)

- New `IItemRepository.RetrieveItems(IReadOnlyList<Guid> ids)`: same Includes as `RetrieveItem`,
  `WhereOneOrMany(ids)`, `AsSplitQuery()` (7 statements regardless of count; keeps LinkedChildren
  so a later save does not delete them).
- New `LibraryManager.GetPersons(IEnumerable<string> names)`: map names → ids with
  `GetItemByNameId<Person>(Person.GetPath(name))` (no DB), dedupe on id, one `RetrieveItems`,
  name → Person map (OrdinalIgnoreCase), tolerate missing ids.
- Use it in `GetPeopleItems` (keep page order and `IsVisible`), `AttachPeople` (optional prefetched
  map) and `GetBaseItemDtos` (one `GetPersons` per page next to `peopleBatch`).
- Hazard: one bad row must not lose the page — fall back to the per-name loop on failure.

## Fix 2 — NextUp version probes (#4)

In `GetNextUpBatched`: collect NextUp/LastWatched ids, call
`GetItemIdsWithAlternateVersions(ids)` once, and skip `GetAllVersions()` for items with no
alternates and no `PrimaryVersionId` (same guard as DtoService.cs:1430-1432).

## Fix 3 — UserViews / detail extras and chapters (#5, #6)

- Chapters only for `IHasMediaSources` (DtoService.cs:1462).
- Skip `GetExtras` for UserView/CollectionFolder/AggregateFolder/UserRootFolder; reuse the loaded
  extras for `LocalTrailerCount` instead of a second query.

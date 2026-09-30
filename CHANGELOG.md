# Changelog

This file lists what changed in JellyMesh by container image tag and by date. Changes without a known date are listed as undated.

Status: Living document. Format is date-based and modeled on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); it has no version headings or comparison links.

## How to read this file

- Status labels are Implemented, Production, Lab-verified, and Planned, as defined in [docs/ROADMAP.md](docs/ROADMAP.md). "Production" is reported by the maintainer and is not re-measured.
- "Measured" means a run produced the number; the method is in the linked doc. "Not measured" means no run exists. "From code reading" means the statement comes from reading source, not from a run.
- Terms such as Galera, fMP4, `dvvC`, RPU, EL, FEL/MEL, Lease, and Range retry are defined in the glossary at the end of [docs/architecture.md](docs/architecture.md).
- Dates are dates in `git log` (commit dates), not release dates. Publication dates of images are not recorded in the repository.

## Versioning

- Container images are tagged `12.1-jmN` (for example `12.1-jm8.4`) and pushed as `ghcr.io/saabstory404/jellymesh-jellyfin`. The `12.1` prefix is the Jellyfin version the image is built on.
- The repository has one git tag, `v12.1-jm8.2` (`git tag`). Whether GitHub Releases exist was not checked.
- Some image digests are recorded in the repository, for example `12.1-jm8.4@sha256:e6645621643a1143e80383ad3360ea28787906cea216f0bd6d17fc4ed6c524f8` in the patch 19 row of [docs/engineering/bughunt.md](docs/engineering/bughunt.md).
- The Rust transcode workspace in [transcode/](transcode/README.md) is at version 0.1.0 (`transcode/Cargo.toml`).

## Image tags

The date column is the date the Containerfile first appears in `git log`. Each Containerfile lives in [image/](image/); how to build is in [docs/getting-started.md](docs/getting-started.md). The base `Containerfile`, `12.1-jm3`, and `12.1-jm4` are not listed; `Containerfile.jm5` builds from jm4. Which tag runs on the maintainer's cluster is not recorded in the repository.

| Tag | Adds | First committed | Status | Notes |
|-----|------|-----------------|--------|-------|
| `12.1-jm5` | Rust transcode shim installed as Jellyfin's ffmpeg; the real ffmpeg moves to `ffmpeg.real`. Trickplay batch class and seek affinity in the pool (`a68398a`). | 2026-09-27 | Implemented | Shim is inert while `TC_WORKERS_DNS` and `TC_WORKERS` are unset (see [docs/configuration.md](docs/configuration.md)). |
| `12.1-jm6` | Bughunt patches 00-13; shim and sync with the DV7 -> 8.1 path. `JELLYMESH_DOVI_P7_TO_81` and `JELLYMESH_SHARED_TRANSCODE_DIR` are opt-in flags. | 2026-09-27 | Implemented, opt-in | Lab finding recorded for jm7: `cp -f` in jm6's plugin install rewrote a shared DLL under a running replica (`BadImageFormatException`, "Bad IL range"). |
| `12.1-jm7` | Patch 14 (`UserDataChangeNotifier` survives a database failover). Patch 15 (`JELLYFIN_SHARED_DB=1` enables cross-node item-cache invalidation; `JELLYFIN_SHARED_INVALIDATION=0` disables it). Galera provider redacts quoted passwords containing `;`. Rename-based `install-plugins.sh`. | 2026-09-27 | Implemented | Invalidation env vars: [jellyfin-perf/README.md](jellyfin-perf/README.md). |
| `12.1-jm7.1` | Fewer database round trips: pooled `CanConnect`, `PurgeDatabase` restores `FOREIGN_KEY_CHECKS` in a `finally`, single-connection invalidation poller. | 2026-09-28 | Implemented | n/a |
| `12.1-jm8` | Patch 16 (extends the shared transcode directory from patch 03 with keepalive, lease-scoped cleanup, and seek takeover) and the detach-aware shim. | 2026-09-28 | Implemented, opt-in; Lab-verified | See [transcode/docs/SHARED-TRANSCODE.md](transcode/docs/SHARED-TRANSCODE.md). |
| `12.1-jm8.1` | pool-r1 shim and sync binaries (NVENC targets 90% of the rate cap, drain resumes a paused job, calibration cap check). Builds `FROM` jm7.1. | 2026-09-28 | Implemented | n/a |
| `12.1-jm8.2` | Patch 17: DV7 -> 8.1 served as fMP4 HLS so the init segment carries `dvvC`; `hvc1` tagging. It copies TrueHD into fMP4. | 2026-09-28 | Implemented, superseded | Measured on an NVIDIA SHIELD: video plays and the TrueHD track has no audio (header of `image/Containerfile.jm8.3`). Not recommended for Android TV clients. |
| `12.1-jm8.3` | Patches 17-18. TrueHD/MLP is dropped from the copy candidates and transcoded instead. Lab tag `12.1-jm8.3-lab-gh14` (issue #14). | 2026-09-28 | Lab-verified (lab tag only) | Lab run measured that the EAC3 preference did not take effect: with the Android TV client's codec list the audio encoded to AAC. Fixed in patch 19. |
| `12.1-jm8.4` | Patches 17-19. A TrueHD/MLP source with 6 or more channels encodes to EAC3 5.1 (640 kb/s) when the client lists `eac3`, otherwise AAC. AC3 and EAC3 sources are copied. | 2026-09-28 | Lab-verified | Lab-verified 2026-09-29 in tc-lab (patch 19 row of [docs/engineering/bughunt.md](docs/engineering/bughunt.md)). |

From code reading, the `Containerfile.jm8.2`, `jm8.3`, and `jm8.4` files each build `FROM` `12.1-jm8.1`, not from each other. Behavior with the opt-in environment variables unset was not separately measured for every tag.

## Unreleased

### Changed

- Dolby Vision 7 -> 8.1 conversion is Production (2026-09-29, reported by the maintainer): it plays as Dolby Vision in the Android TV app on an NVIDIA SHIELD, tested repeatedly. The deployed image tag is deployment-specific and not recorded here. See [docs/dolby-vision.md](docs/dolby-vision.md).
- Dolby Digital Plus passthrough is Production (2026-09-30, reported by the maintainer): the EAC3 5.1 track from the TrueHD to EAC3 transcode (patches 18 and 19) passes through from the SHIELD to the AV receiver.

### Documentation

- Documentation is being restructured into a Diataxis layout; the index is [docs/README.md](docs/README.md). This is a working-tree change and is not committed at the time of writing.
- Moved files (old path to new path): `docs/BUGHUNT.md` to [docs/engineering/bughunt.md](docs/engineering/bughunt.md); `docs/direct-play-failover.md` to [docs/engineering/direct-play-failover.md](docs/engineering/direct-play-failover.md); `docs/jellyfin-n1-hotspots.md` to `docs/engineering/jellyfin-n1-hotspots.md`; `tools/gh15-fel-visibility/gh-15-notes.md` to [docs/engineering/gh15-fel-visibility.md](docs/engineering/gh15-fel-visibility.md); `transcode/docs/PLAN.md` to [docs/engineering/transcode-plan.md](docs/engineering/transcode-plan.md); `transcode/calibration/README.md` to `docs/engineering/transcode-calibration.md`.
- New pages: [getting-started](docs/getting-started.md), [architecture](docs/architecture.md), [configuration](docs/configuration.md), [operations](docs/operations.md), [troubleshooting](docs/troubleshooting.md).
- Status labels are defined in [docs/ROADMAP.md](docs/ROADMAP.md).

## 2026-09-29

### Added

- Patch 19 (image `12.1-jm8.4`), Lab-verified 2026-09-29 in tc-lab: a TrueHD source in a DV7 -> 8.1 fMP4 job encodes to EAC3 5.1 (640 kb/s) when the client lists `eac3`, otherwise AAC. Patch 18 (image `12.1-jm8.3`) was the lab-tested precursor (lab work dated 2026-09-28); its EAC3 preference failed in the lab and patch 19 fixes the cause. Method and results: [docs/engineering/bughunt.md](docs/engineering/bughunt.md).
- Direct-play failover measurements (issue #13; commit dated 2026-09-28, doc updated 2026-09-29). Measured, n=1, two-replica lab: after a hard kill of the serving process the stream resets, and a client that retries with an HTTP Range request resumes byte-identically on the other replica, with a viewer-visible gap of about 5 to 6 seconds. Direct play needs the client's Range retry. See [docs/engineering/direct-play-failover.md](docs/engineering/direct-play-failover.md).
- Dolby Vision FEL visibility investigation (issue #15; notes committed 2026-09-28, Job run 2026-09-29): a residual-energy proxy measured on four FEL titles, published as [docs/engineering/gh15-fel-visibility.md](docs/engineering/gh15-fel-visibility.md). Ratios were 0.56x to 1.62x; the doc calls the result suggestive, not conclusive, recommends not prioritizing server-side FEL reconstruction, and sets `close_issue=false` in the notes.
- DNS worker discovery and offer status merged (PR #23, 2026-09-29).

## 2026-09-28

### Added

- Shared transcode directory, Lab-verified. Measured in a lab with three player sessions through the Traefik failover route: a pod delete and a `kill -9` of the serving replica each gave 0 of 162 failed segment requests, and no new ffmpeg process started for the sessions. Source: [transcode/docs/SHARED-TRANSCODE.md](transcode/docs/SHARED-TRANSCODE.md). Patch 03 introduced the opt-in flag `JELLYMESH_SHARED_TRANSCODE_DIR=1`; patch 16 (`12.1-jm8`) extends it. See [jellyfin-perf/README.md](jellyfin-perf/README.md).
- Bughunt patches 16 (jm8, `f381dac`), 17 (jm8.2, `8666785`), 18 (jm8.3, `2e27a6a`), and 19 (jm8.4, `f2f8c2e`) committed on this date. Patches 14 and 15 are listed under 2026-09-27 for the image (jm7); their merge commit `60f5d3e` is dated 2026-09-28 and the WIP commit `e490526` 2026-09-27.
- Image `12.1-jm7.1`: fewer database round trips (see the image table).
- Pool work in `12.1-jm8.1`: NVENC targets 90% of the cap (`d70e1d0`), drain resumes a paused job (`aad2c98`), calibration cap check (`3ed52b3`).

### Changed

- `GaleraDatabaseCreator` (`f958a50`): `CanConnect` runs `SELECT 1` on the pooled connection instead of opening a new unpooled connection for each `/health` probe. Measured in the lab before the change: 1.00 new connection per probe (source: [galera/README.md](galera/README.md); test-bed detail is in [docs/engineering/galera-provider.md](docs/engineering/galera-provider.md)).

## 2026-09-27

### Added

- Bughunt run against Jellyfin 12.1 with patches 00-13, opt-in where behavior changes; parity check of responses with perf-only versus perf plus bughunt patches was 17 of 17 identical (`b718a87`). See [docs/engineering/bughunt.md](docs/engineering/bughunt.md) and [jellyfin-perf/README.md](jellyfin-perf/README.md).
- Bughunt patches 14 and 15 first committed as WIP (`e490526`) and shipped in `12.1-jm7`.
- Transcode pool added in commit `9ada384`: `tcpool-shim`, `tcpool-agent`, `tcpool-sync`, `tcpool-ir`, `tcpool-proto` (package names from each crate's `Cargo.toml`). See [transcode/README.md](transcode/README.md).
- Decisions recorded for the transcode pool in the plan imported with `9ada384`: Rust control plane, two existing GPUs plus a CPU spill worker, no AV1, batch work at low priority. See [docs/engineering/transcode-plan.md](docs/engineering/transcode-plan.md).
- Dolby Vision 7 -> 8.1 path wired into the agent job path (`ce4b08a`). See [docs/dolby-vision.md](docs/dolby-vision.md).
- Real-title proof of the DV7 -> 8.1 filter (`9d9515d`), Measured 2026-09-27 on the `jellyfin-qsv` image with jellyfin-ffmpeg 8.1.2, using the `examples/dv81_filter` example (the agent's dv81 code between two ffmpeg processes), not the full pool path: one FEL title, 30 s window, 729 of 729 RPUs rewritten and 2459 enhancement-layer NAL units dropped. Source: [docs/engineering/transcode-plan.md](docs/engineering/transcode-plan.md) and [docs/dolby-vision.md](docs/dolby-vision.md).
- Library census for Dolby Vision, Measured 2026-09-27 with header-only ffprobe on the maintainer's movie library: 336 video files, 165 with a DOVI record, 122 profile 7 (FEL 76, MEL 46, from a 3 s scan per title), 43 profile 8. In-band RPU was found on all 122 profile 7 titles and the hvcE-only class had 0 of 122. An earlier count, 124 of 329 movies as profile 7, has no recorded method; the newer census is used. Source: [docs/engineering/transcode-plan.md](docs/engineering/transcode-plan.md).
- Calibrated per-encoder rate control and NVENC without AQ (`56951b3`), lab-measured (P5).
- Roadmap entries for playback observability, sticky server failover, and a plugin compatibility layer. See [docs/ROADMAP.md](docs/ROADMAP.md).
- CI: `fuzz-smoke` job in `.github/workflows/transcode.yml` (`a0ff35d`).

## 2026-09-26

### Added

- Initial repository: JellyMesh, several Jellyfin 12.1 servers on one MySQL/Galera database (`433fc8f`), with the Galera provider, `jellyfin-dbmigrate`, and the `jellyfin-perf` patch.
- Leader plugin (Kubernetes Lease task leader) and transcode fixes (`46a3f92`).
- Shared-image layout for Jellyfin and workers (`46a3f92`).
- Galera provider reads the database password from `JELLYMESH_DB_PASSWORD` (`cfb3ae0`).
- Initial measurement set, single workstation, lab: migration timings, per-call latency, statement counts, and concurrent load. Figures and methods: [docs/RESULTS.md](docs/RESULTS.md).

## Related docs

- [README.md](README.md)
- [docs/README.md](docs/README.md)
- [docs/ROADMAP.md](docs/ROADMAP.md)
- [docs/RESULTS.md](docs/RESULTS.md)
- [docs/dolby-vision.md](docs/dolby-vision.md)
- [docs/engineering/bughunt.md](docs/engineering/bughunt.md)
- [CONTRIBUTING.md](CONTRIBUTING.md)

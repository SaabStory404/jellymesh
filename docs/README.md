# JellyMesh documentation

This directory indexes every JellyMesh document by what the reader needs to do: learn, get a task done, look something up, or understand why. It also holds the engineering records, which are dated working logs of measurements and investigations.

**Status:** Implemented. Every page listed below exists in the repository.

## How the docs are organized

The docs follow the Diataxis split, which sorts pages by reader need: tutorial, how-to, reference, explanation. Each page under `docs/`, except `docs/engineering/`, has a Status line and ends with a Related docs list.

Terms such as Galera, PXC, Pomelo, Lease, QSV, NVENC, mTLS, fMP4, dvvC and RPU are defined in the [glossary](architecture.md#glossary) at the end of the architecture page.

Pick a reading path by role.

| You are | Start here | Then |
| --- | --- | --- |
| Evaluating the project | [Repository README](../README.md) | [Architecture](architecture.md), [Results](RESULTS.md) |
| Trying it in a lab | [Getting started](getting-started.md) | [Configuration](configuration.md) |
| Running it on a cluster | [Operations](operations.md) | [Troubleshooting](troubleshooting.md), [Configuration](configuration.md) |
| Interested in Dolby Vision 7 to 8.1 | [Dolby Vision](dolby-vision.md) | [transcode README](../transcode/README.md) |
| Changing the code | [Contributing](../CONTRIBUTING.md) | Component READMEs below |
| Reporting a vulnerability | [Security](../SECURITY.md) | n/a |
| Looking for history or raw measurements | [Engineering records](#engineering-records) | [Results](RESULTS.md) |

## Index

### Tutorial

| Page | What it gives you |
| --- | --- |
| [Getting started](getting-started.md) | Two Jellyfin lab nodes on a three-node Galera cluster, ending with a visible shared-state check. |

### How-to guides

| Page | What it gives you |
| --- | --- |
| [Operations](operations.md) | Images, database setup, migration, plugins, the HA route, the leader plugin, the transcode pool, upgrades and rollback. |
| [Troubleshooting](troubleshooting.md) | Symptom, cause and fix, drawn only from documented behavior. |

### Reference

| Page | What it gives you |
| --- | --- |
| [Configuration](configuration.md) | Environment variables and settings, verified against code. Covers `leader/` and `image/` settings. |
| [Results](RESULTS.md) | Benchmarks, per-call latency, statement counts, parity, migration timing, failover drills and transcode measurements, each with method and caveats. |
| [galera/README.md](../galera/README.md) | The database provider plugin, the Pomelo build, `jellyfin-dbmigrate`, lab scripts, tools and tests. |
| [jellyfin-perf/README.md](../jellyfin-perf/README.md) | The Jellyfin performance patch, `JELLYFIN_SHARED_DB=1` mode and the bughunt series 00-19. |
| [transcode/README.md](../transcode/README.md) | The Rust GPU transcode pool: shim, agent, sync, build, run and test. |
| [transcode/docs/SHARED-TRANSCODE.md](../transcode/docs/SHARED-TRANSCODE.md) | Component-local reference for the shared transcode directory across Jellyfin replicas. |
| [transcode/deploy/CONTRACT.md](../transcode/deploy/CONTRACT.md) | The deployment contract for the transcode pool (ports, discovery, manifests). It partly predates the code: its header says nothing was applied to the cluster and no image was pushed (2026-09-27), and [transcode/README.md](../transcode/README.md) lists statements that no longer match the code. |

The `leader/` plugin and the `image/` Containerfiles have their own READMEs: [leader](../leader/README.md) and [image](../image/README.md). Deployment steps are in [operations.md](operations.md), and settings are in [configuration.md](configuration.md).

Other places that hold material the pages above cite:

| Location | Contents |
| --- | --- |
| [transcode/corpus/README.md](../transcode/corpus/README.md) | The test corpus for the transcode pool. |
| `tools/gh15-fel-visibility/` | The scripts and per-title JSON results named by the [issue 15 record](engineering/gh15-fel-visibility.md). |
| `transcode/calibration/` | Raw calibration CSV files cited by the [calibration record](engineering/transcode-calibration.md). |
| `transcode/deploy/k8s/` | Kubernetes manifests for the transcode pool. |

### Explanation

| Page | What it gives you |
| --- | --- |
| [Architecture](architecture.md) | How the parts fit, why each exists, how failure is handled, and the glossary. |
| [Dolby Vision](dolby-vision.md) | What the live Dolby Vision 7 to 8.1 conversion does, its status, how to enable and verify it, and its limits. |
| [Roadmap](ROADMAP.md) | The status matrix and future work, each item with Why, What and Done when. |

## Status labels

Every page uses these four labels, in a Status column or line. A plain qualifier may follow, for example "Implemented, opt-in, off by default".

| Label | Meaning |
| --- | --- |
| Implemented | The code is in the repository. |
| Production | Deployed on the maintainer's cluster, as reported by the maintainer. The Dolby Vision 7 to 8.1 report is dated 2026-09-29. Dolby Digital Plus passthrough of the EAC3 track to an AV receiver is reported working on 2026-09-30. |
| Lab-verified | Measured in a lab only. |
| Planned | Not implemented. It links to [ROADMAP.md](ROADMAP.md) or an issue. |

## Engineering records

Files under `docs/engineering/` are working logs written during development. They keep the original detail, including dated measurements, and some statements predate later work. Each record opens with a status line, and where a statement was superseded it carries an update note pointing to the current page. Read the linked current page first, and use the record for method and history.

| Record | Purpose | Dates | Current behavior |
| --- | --- | --- | --- |
| [bughunt.md](engineering/bughunt.md) | Findings, fixes and verification of the Jellyfin 12.1 bug hunt and patch series 00-19. | 2026-09-27 to 2026-09-29 | [Dolby Vision](dolby-vision.md), [jellyfin-perf](../jellyfin-perf/README.md) |
| [transcode-plan.md](engineering/transcode-plan.md) | Design plan, live checklist and measurement journal for the transcode pool. | 2026-09-26 to 2026-09-28 | [transcode/README.md](../transcode/README.md), [Dolby Vision](dolby-vision.md) |
| [transcode-calibration.md](engineering/transcode-calibration.md) | Rate-control calibration of the Intel Arc, Tesla P4 and CPU workers, with VMAF and startup latency, and the P5 applied rate-control results. | 2026-09-26 to 2026-09-28 | [transcode/README.md](../transcode/README.md) |
| [gh15-fel-visibility.md](engineering/gh15-fel-visibility.md) | Investigation of issue 15 (the issue is on the repository tracker) of enhancement-layer visibility on bright content in FEL titles. | 2026-09-27 to 2026-09-29 | [Dolby Vision](dolby-vision.md) |
| [direct-play-failover.md](engineering/direct-play-failover.md) | Lab measurement for issue 13 (the issue is on the repository tracker) of direct play across a replica restart or crash. | 2026-09-29 | [Operations](operations.md) |
| [jellyfin-n1-hotspots.md](engineering/jellyfin-n1-hotspots.md) | Code-reading notes on query-per-row (N+1) hotspots in Jellyfin 12.1 and the fixes designed for them. | 2026-09-26 | [jellyfin-perf/README.md](../jellyfin-perf/README.md) |

## Report a documentation problem

Open an issue or a pull request as described in [CONTRIBUTING.md](../CONTRIBUTING.md). Include the page, the statement and the file in the repository that contradicts it. Missing facts are listed there under open documentation gaps, along with the banner and glossary-linking conventions; pages state a missing fact in one plain sentence rather than guess.

## Related docs

- [Repository README](../README.md)
- [Architecture](architecture.md)
- [Roadmap](ROADMAP.md)
- [Contributing](../CONTRIBUTING.md)
- [Security](../SECURITY.md)
- [Changelog](../CHANGELOG.md)
- [Engineering records](engineering/bughunt.md)

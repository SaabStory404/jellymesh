# JellyMesh documentation

Everything I've written down about JellyMesh, sorted by what you're trying to do: learn it, get a task done, look something up, or understand why it works the way it does. The engineering records at the bottom are dated working logs of measurements and investigations.

## How the docs are organized

If a term is unfamiliar, the [glossary](architecture.md#glossary) at the end of the architecture page covers Galera, PXC, Pomelo, Lease, QSV, NVENC, mTLS, fMP4, dvvC and RPU.

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
| [Troubleshooting](troubleshooting.md) | Symptom, likely cause, a check, and the fix. Nothing in it is guessed. |

### Reference

| Page | What it gives you |
| --- | --- |
| [Configuration](configuration.md) | Every environment variable and setting, with defaults copied from the code. Covers `leader/` and `image/` settings. |
| [Results](RESULTS.md) | Benchmarks, per-call latency, statement counts, parity, migration timing, failover drills and transcode measurements, each with method and caveats. |
| [galera/README.md](../galera/README.md) | The database provider plugin, the Pomelo build, `jellyfin-dbmigrate`, lab scripts, tools and tests. |
| [jellyfin-perf/README.md](../jellyfin-perf/README.md) | The Jellyfin performance patch, `JELLYFIN_SHARED_DB=1` mode and the bughunt series 00-19. |
| [transcode/README.md](../transcode/README.md) | The Rust GPU transcode pool: shim, agent, sync, build, run and test. |
| [transcode/docs/SHARED-TRANSCODE.md](../transcode/docs/SHARED-TRANSCODE.md) | Component-local reference for the shared transcode directory across Jellyfin replicas. |
| [transcode/deploy/CONTRACT.md](../transcode/deploy/CONTRACT.md) | The deployment contract for the transcode pool: ports, discovery, manifests. Parts of it predate the code. As of 2026-09-27 nothing in it had been applied to the cluster and no image had been pushed, and [transcode/README.md](../transcode/README.md) lists statements in it that no longer match the code. |

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
| [Dolby Vision](dolby-vision.md) | What the live Dolby Vision 7 to 8.1 conversion does, where it stands, how to enable and verify it, and its limits. |
| [Roadmap](ROADMAP.md) | What's done, what isn't, and what's next, each item with Why, What and Done when. |

## Engineering records

The files under `docs/engineering/` are working logs I wrote while building this. They keep the original detail, dated measurements included, which means some of what they say has since been overtaken. The last column below names the page that holds the current answer; read that first and use the record for method and history.

| Record | Purpose | Dates | Current behavior |
| --- | --- | --- | --- |
| [bughunt.md](engineering/bughunt.md) | Findings, fixes and verification of the Jellyfin 12.1 bug hunt and patch series 00-19. | 2026-09-27 to 2026-09-29 | [Dolby Vision](dolby-vision.md), [jellyfin-perf](../jellyfin-perf/README.md) |
| [transcode-plan.md](engineering/transcode-plan.md) | Design plan, live checklist and measurement journal for the transcode pool. | 2026-09-26 to 2026-09-28 | [transcode/README.md](../transcode/README.md), [Dolby Vision](dolby-vision.md) |
| [transcode-calibration.md](engineering/transcode-calibration.md) | Rate-control calibration of the Intel Arc, Tesla P4 and CPU workers, with VMAF and startup latency, and the P5 applied rate-control results. | 2026-09-26 to 2026-09-28 | [transcode/README.md](../transcode/README.md) |
| [gh15-fel-visibility.md](engineering/gh15-fel-visibility.md) | Investigation of issue 15 (on the repository tracker) of enhancement-layer visibility on bright content in FEL titles. | 2026-09-27 to 2026-09-29 | [Dolby Vision](dolby-vision.md) |
| [direct-play-failover.md](engineering/direct-play-failover.md) | Lab measurement for issue 13 (on the repository tracker) of direct play across a replica restart or crash. | 2026-09-29 | [Operations](operations.md) |
| [jellyfin-n1-hotspots.md](engineering/jellyfin-n1-hotspots.md) | Code-reading notes on query-per-row (N+1) hotspots in Jellyfin 12.1 and the fixes designed for them. | 2026-09-26 | [jellyfin-perf/README.md](../jellyfin-perf/README.md) |

## Report a documentation problem

Open an issue or a pull request the way [CONTRIBUTING.md](../CONTRIBUTING.md) describes. Tell me the page, the statement, and the file in the repository that contradicts it. Facts these pages can't supply are already listed there under open documentation gaps; where one is missing, a page says so in a plain sentence rather than guessing.

For the whole project, start at the [repository README](../README.md). What changed under each image tag is in the [changelog](../CHANGELOG.md).

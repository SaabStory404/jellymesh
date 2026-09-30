# JellyMesh Jellyfin image

The `image/` directory holds the Containerfiles that build the JellyMesh Jellyfin image, `ghcr.io/saabstory404/jellymesh-jellyfin`, as a chain of tags on top of hotio's Jellyfin 12.1. Each tag adds patches, the Galera provider, or the transcode pool shim. No workflow builds this image, so you build it yourself with podman. I haven't recorded which tag my own cluster runs.

## What it does

The base is `ghcr.io/hotio/jellyfin:release-12.1`, pinned by digest in `image/Containerfile`. On top of it the image carries the [jellyfin-perf](../jellyfin-perf/README.md) patch assemblies, the Galera database provider and Leader plugin under `/opt/jellymesh/plugins`, the Rust `tcpool-shim` as Jellyfin's ffmpeg, `tcpool-sync`, and `jellyfin-dbmigrate`.

### Tag lineage

Each `image/Containerfile.jmN` builds one tag on top of an earlier published tag, referenced by digest. `image/Containerfile` is the jm3-era base and is kept as history; it cannot be built from the repo alone.

| Tag | FROM | Adds | Patches |
| --- | --- | --- | --- |
| `12.1-jm5` | `12.1-jm4` | The `tcpool-shim` installed as Jellyfin's ffmpeg. The real binary moves to `ffmpeg.real` (`TC_FFMPEG_REAL`). The shim is inert while `TC_WORKERS_DNS` and `TC_WORKERS` are unset. | n/a |
| `12.1-jm6` | `12.1-jm5` | Bughunt patch series and the Dolby Vision-capable shim. | 00-13 |
| `12.1-jm7` | `12.1-jm6` | Patch 14 (user-data change notifier survives a database failover), patch 15 (cross-node item-cache invalidation under `JELLYFIN_SHARED_DB=1`), Galera password redaction, rename-based `install-plugins.sh`. | 00-15 |
| `12.1-jm7.1` | `12.1-jm7` | Fewer database round trips: `/health` runs `SELECT 1` on the pooled connection, `PurgeDatabase` re-enables foreign key checks in a `finally`, the patch 15 poller keeps one connection. | 00-15 |
| `12.1-jm8` | `12.1-jm7.1` | Patch 16 (shared transcode directory, keepalive, lease-scoped cleanup, seek takeover) and a detach-aware shim. | 00-16 |
| `12.1-jm8.1` | `12.1-jm7.1` | The `pool-r1` shim and sync binaries with jm8's overlay. | 00-16 |
| `12.1-jm8.2` | `12.1-jm8.1` | Patch 17. Known bad for Android TV audio: it copies TrueHD into fMP4. | 00-17 |
| `12.1-jm8.3-lab-gh14` | `12.1-jm8.1` | Patches 17-18. Built by `image/Containerfile.jm8.3` as `12.1-jm8.3-lab-gh14` and deployed to a lab only; superseded by jm8.4 because the EAC3 preference did not take effect until patch 19. | 00-18 |
| `12.1-jm8.4` | `12.1-jm8.1` | Patches 17-19. The newest tag in the repo and the one the Dolby Vision docs refer to. | 00-19 |

The lineage is not a single chain. jm8.1 is built from jm7.1, and jm8.2, jm8.3, and jm8.4 are siblings that are each built from jm8.1.

Patch 16 was numbered 14 before the jm7.1 rebase. The startup-wipe scoping and the shared-directory segment wait for `JELLYMESH_SHARED_TRANSCODE_DIR` are patch 03 (`03-k2-incident-and-shared-dir`), which `Containerfile.jm6` names as the origin of that flag; patch 16 adds the keepalive, cleanup, and takeover parts. For what patches 13 and 17-19 do, see [Dolby Vision](../docs/dolby-vision.md).

Why jm8.2 is marked bad: on my SHIELD the official Android TV app showed Dolby Vision video with no audio on the default TrueHD track, while AC3 played.

### Image internals

From `image/Containerfile` and the later Containerfile headers:

- `/opt/jellymesh/SHA256SUMS` lists every file the image adds. jm7 and jm7.1 refresh the lines of the files they replace.
- The s6 `run` script is replaced so `TMPDIR` can be overridden per replica.
- `libe_sqlite3.so` sits next to `/opt/jellymesh/jellyfin-dbmigrate`.
- Transcode code lives under `/opt/jellymesh/transcode`; jm5 removes its `*.py` files.
- jm7 adds the `JellyMeshItemInvalidation` table, created with `CREATE TABLE IF NOT EXISTS` on first use. Opt out with `JELLYFIN_SHARED_INVALIDATION=0`. It also redacts quoted passwords that contain `;` in the logged connection string.
- jm7.1 depends on `ConnectionReset=false` in the connection string. Its initContainer image must be jm7.1 too, because `install-plugins.sh` copies the Galera DLL from the image.
- jm8 and later pair with `pool-r1` agents (commit `3ed52b3` is recommended in the Containerfile headers). Any agent works: an old agent ignores the keepalive field, so the job is fenced on shim loss as before.

## Requirements

- podman.
- A staging directory (build context) that is not in the repo. What each Containerfile needs is in the next section.
- A published parent tag: each `FROM` names the parent by digest in `ghcr.io/saabstory404/jellymesh-jellyfin`.
- For the shim and sync binaries, `transcode/deploy/build-musl.sh` (a rust container by default; `TC_BUILD=host` uses the host's cargo and `musl-gcc`).

## Build

Build the overlay with `BUGHUNT=1 ./jellyfin-perf/build.sh`, then build the image from your staging directory:

```bash
BUGHUNT=1 ./jellyfin-perf/build.sh
podman build -f image/Containerfile.jm8.4 \
  -t ghcr.io/saabstory404/jellymesh-jellyfin:12.1-jm8.4 <ctx>
```

`<ctx>` is your staging directory. You assemble it yourself; it isn't shipped here. What it has to hold differs per Containerfile, and each Containerfile's header comment repeats the list:

| Containerfile | Context holds |
| --- | --- |
| `Containerfile.jm8.4`, `.jm8.3`, `.jm8.2` | `overlay/` only. The header of jm8.4 names `JF_OVERLAY=~/.cache/jellymesh-vendor/jellyfin-gh19` as the overlay source. Built `FROM` the published jm8.1 digest. |
| `Containerfile.jm8.1`, `.jm8`, `.jm6` | `overlay/` plus the static musl binaries `tcpool-shim` and `tcpool-sync`. |
| `Containerfile.jm7.1` | `overlay/` plus `galera/` (the built provider DLL). |
| `Containerfile.jm7` | `overlay/`, `galera/`, and `install-plugins.sh`. |
| `Containerfile.jm5` | The static musl binaries only. The header uses `transcode/target/x86_64-unknown-linux-musl/release` as the context. |
| `Containerfile` (jm3-era base) | Cannot be built from the repo; see Limitations. |

### Pool images

The workflow `transcode-images.yml` publishes `ghcr.io/saabstory404/tcpool-agent:<sha>` and `ghcr.io/saabstory404/tcpool-shim:<sha>`. It runs on push to `main` when `transcode/**` or the workflow file changes, and on manual dispatch. It installs `musl-tools` because the `ring` crypto crate needs a musl C compiler to build.

Tags are commit SHAs; there is no `latest`. For a local build, run `transcode/deploy/build-musl.sh`, then `transcode/deploy/build-images.sh [tag]`. The registry defaults to `ghcr.io/saabstory404` (`TC_REGISTRY`); the tag defaults to the short git SHA, else `dev`. `build-images.sh` refuses a binary that is not statically linked.

The agent image is built from the hotio Jellyfin image (for `jellyfin-ffmpeg`) and runs as uid 1000. It keeps the base image's `LD_PRELOAD` jemalloc on purpose, because the ffmpeg it spawns inherits that allocator. It has no `render` group; GPU access comes from the pod's `supplementalGroups` (GID 993 in the manifests). It starts through `tcpool-entry`, a shell PID 1 that relays SIGTERM so drain works and keeps the agent signalable for freeze drills. The shim image is `FROM scratch` and holds only the binaries.

## Configure

The image reads the environment variables listed in [configuration](../docs/configuration.md): `JELLYMESH_DB_PASSWORD`, `JELLYFIN_SHARED_DB`, `JELLYMESH_SHARED_TRANSCODE_DIR`, `JELLYMESH_DOVI_P7_TO_81`, and the `TC_*` shim settings. New patch behavior is off by default; each patch header says "Flag off = unchanged".

## Run or use

Jellyfin loads plugins only from `$JELLYFIN_DATA_DIR/plugins`. The image carries them under `/opt/jellymesh/plugins/<folder>/`, so an initContainer runs `image/install-plugins.sh` against the shared config volume.

The script copies each `*.dll` into `${JELLYFIN_DATA_DIR:-/config/data}/plugins/<folder>`. It skips a file that is byte-identical (`cmp`). For a changed file it writes `<dll>.jmnew` and renames it over the target, so the file gets a new inode.

The script never overwrites a DLL in place, because an in-place overwrite crashed a live replica with `BadImageFormatException` ("Bad IL range"). I hit that in a podman lab, with a jm6 process running under a jm7 Galera DLL copy. The Galera plugin folder is `JellyMesh Galera_1.0.0.0` (the name contains a space).

## Test

There is no test for the image build. The patch series has its own tests, listed in [jellyfin-perf](../jellyfin-perf/README.md) and `docs/engineering/bughunt.md`.

## Limitations

- The staging context is not in the repo, and `image/Containerfile` refers to `stage_jm3.py`, which is not in the repo either. The base image cannot be rebuilt from the repo alone.
- The jm8.x Containerfiles take the shim and sync binaries from jm8.1. They do not rebuild them.

## License

GPL-2.0 for the repository as a whole ([LICENSE](../LICENSE)).

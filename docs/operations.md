# Operations

This guide shows how to deploy and run JellyMesh on Kubernetes (k3s) or podman. It covers images, database, migration, the leader plugin, failover, the transcode pool, upgrades, and monitoring.

**Status:** Implemented. Dolby Vision 7 -> 8.1 is deployed in production and plays as Dolby Vision on the Android TV app (user-reported by the maintainer, 2026-09-29). Dolby Digital Plus passthrough of the EAC3 track from the SHIELD to an AV receiver works (maintainer report, 2026-09-30; patch 19 row in [bughunt](engineering/bughunt.md)).

Terms such as Galera, Lease, QSV, NVENC, mTLS, and fMP4 are defined in the [glossary](architecture.md#glossary).

## What the repo ships

JellyMesh is a set of parts you assemble. The repo ships code, Containerfiles, and the transcode pool manifests; it does not ship the cluster around them.

| Item | In the repo | Status |
| --- | --- | --- |
| Jellyfin image Containerfiles (`image/`) | Yes; the build context is not | Implemented |
| Galera provider plugin and `jellyfin-dbmigrate` (`galera/`) | Yes | Implemented |
| Leader plugin (`leader/`) | Yes, source only | Implemented |
| Transcode pool manifests (`transcode/deploy/k8s/`, files `00` to `60`) | Yes | Implemented |
| Traefik failover route | No; described from lab evidence | Lab-verified shape only |

You provide the following. This guide gives the parameters that the code and the lab measurements define, and marks any snippet as an example.

| Item you write | Where the repo helps |
| --- | --- |
| Jellyfin StatefulSet or Deployment manifests | [configuration](configuration.md) lists the environment variables |
| Lease RBAC (Role and RoleBinding) | Example Role in [leader](../leader/README.md) |
| MySQL or Percona XtraDB Cluster (PXC) manifests | Lab script `galera/lab/galera-lab.sh` only |
| Kuma monitors | Nothing; see [Monitoring](#monitoring) |

## Images

**Status:** Implemented. No workflow builds the Jellyfin image; you build it with podman.

The image is `ghcr.io/saabstory404/jellymesh-jellyfin`. The newest tag in the repo is `12.1-jm8.4`, and it is the tag the Dolby Vision docs refer to. The tag production runs is deployment-specific and not recorded here.

The tag lineage, the per-Containerfile build context, and the image internals are in [image](../image/README.md). The staging context is assembled per deployment and not shipped here; the header comment of each Containerfile lists what it expects. The newest tags need less than the older ones:

| Containerfile | Context holds |
| --- | --- |
| `Containerfile.jm8.4` (also `.jm8.3` and `.jm8.2`) | `overlay/` only, built `FROM` the published jm8.1 digest |
| `Containerfile.jm8.1`, `.jm8`, `.jm6` | `overlay/` plus static musl binaries |
| `Containerfile.jm7`, `.jm7.1` | `overlay/` plus `galera/` (jm7 also `install-plugins.sh`) |
| `Containerfile.jm5` | Static musl binaries only |

Build the overlay, then build the image from your staging directory:

```bash
BUGHUNT=1 ./jellyfin-perf/build.sh
podman build -f image/Containerfile.jm8.4 \
  -t ghcr.io/saabstory404/jellymesh-jellyfin:12.1-jm8.4 <ctx>
```

The plugin install step, the pool images (`tcpool-agent`, `tcpool-shim`), and `transcode-images.yml` are described in [image](../image/README.md).

## Database

**Status:** Implemented, lab-verified on a three-node PXC 8.4 cluster; see [results](RESULTS.md).

### Server requirements

- MySQL 8.4 or Percona XtraDB Cluster 8.4. The provider pins `MySqlServerVersion` 8.4.0 in code and does not auto-detect. Only PXC 8.4 was run in the lab.
- The database must exist (`galera/README.md`). The provider runs `ALTER DATABASE ... utf8mb4 / utf8mb4_bin` from `MigrationBackupFast`, which Jellyfin calls before it runs migrations, and creates every string column as `utf8mb4_bin`.
- `jellyfin-dbmigrate copy` does not create the database or set its collation.

The lab script `galera/lab/galera-lab.sh db` creates the database and a user with all privileges on it. The privilege grant is a lab choice, not a documented requirement. The password is a placeholder:

```bash
podman exec gl-db1 mysql -uroot -p"$ROOTPW" -e "
  CREATE DATABASE IF NOT EXISTS jellyfin CHARACTER SET utf8mb4 COLLATE utf8mb4_bin;
  CREATE USER IF NOT EXISTS 'jellyfin'@'%' IDENTIFIED BY 'jellyfin';
  GRANT ALL ON jellyfin.* TO 'jellyfin'@'%';"
```

The lab connection string uses `SslMode=Disabled` and `AllowPublicKeyRetrieval=true`. Do not use those values in production; use TLS and a Kubernetes Secret (`galera/README.md`). The lab cluster script, its ports, and its drills are described in [galera](../galera/README.md).

### Point Jellyfin at the database

Jellyfin loads the provider through `database.xml` with `DatabaseType` set to `PLUGIN_PROVIDER`, `PluginName` set to `JellyMesh Galera`, `PluginAssembly` set to `Jellyfin.Database.Providers.Galera.dll`, and a `ConnectionString`. The provider registers under the key `Jellyfin-Galera` and throws at start if the connection string is missing.

Set `JELLYMESH_DB_PASSWORD` to override the `Password` in the connection string. This keeps the password out of `database.xml`, which lands in config backups; mount it from a Kubernetes Secret. The provider logs the connection string with the password masked.

### Strict mode

With `pxc_strict_mode=ENFORCING`, PXC rejects `GET_LOCK`, which EF Core's migration lock uses. The lab cluster runs `PERMISSIVE` and migrates from one node. The repo documents only the lab setting.

### Single-writer or multi-writer

| Layout | How | Measured behavior |
| --- | --- | --- |
| Single-writer | Every Jellyfin lists the nodes in the same order: `Server=db1,db2,db3;LoadBalance=FailOver`. Other nodes are synchronous standbys. | Primary SIGKILL under load: 206 requests, 0 failed, longest gap 2.9 s (lab, `galera/lab/galera_drill.py`, [galera provider notes](engineering/galera-provider.md) drills table). The bootstrap node must come back as a joiner (`rejoin`). |
| Multi-writer | Each Jellyfin writes to a different node. | Needs the retry in the perf patch: `SaveUserData` retries with a fresh context, jittered backoff, and 6 attempts. The lab measured 200 of 200 concurrent writes succeeding with the retry; without it, 200 concurrent writes to one row across nodes gave 69 HTTP 500 responses ("Deadlock found"). |

Single-writer is the simpler layout. A separate lab run killed a non-primary node under load with a `FailOver` list: 204 requests, 0 failed, longest gap 2.9 s, and the node was Synced 7 s after restart by incremental state transfer (IST). Both runs are single lab runs of `galera_drill.py` on the three-node lab cluster (measured 2026-09).

### Backups are your job

The provider does not back up the database. `MigrationBackupFast` only sets the charset and collation and logs a warning. `RestoreBackupFast` logs a critical "cannot restore" message and does nothing. `DeleteBackup` and `RunShutdownTask` are no-ops. Jellyfin's own pre-migration backup and restore therefore do not work on this provider. Back up the cluster with your own tooling (the code comment names xtrabackup).

### Plain MySQL

Plain MySQL 8.4 with `JELLYFIN_SHARED_DB=1` measured nearly as fast as Galera: 89.1 and 109.3 requests/s at 8 and 32 clients, versus 90.7 and 112.0 on Galera (lab, single workstation; source [results](RESULTS.md)). It has no failover, and the deployment has no wsrep metrics exporter.

### Trickplay tiles

Trickplay database rows are shared through the database, but tile files are not unless `SaveTrickplayWithMedia` is true. The default is false, which puts tiles on per-node storage. Use `SaveTrickplayWithMedia` or shared storage for the tile directory (From code reading and [bughunt](engineering/bughunt.md)).

## Migrate SQLite to Galera and back

**Status:** Implemented. `jellyfin-dbmigrate` is built in the image at `/opt/jellymesh/jellyfin-dbmigrate`.

A database spec is `sqlite:<path>` or `galera:<connection string>`. In the commands below, `$GALERA` stands for one `galera:` spec that you set once:

```bash
GALERA="galera:Server=db;Database=jellyfin;Uid=jellyfin;Pwd=...;SslMode=..."
```

| Mode | Command shape | What it does |
| --- | --- | --- |
| `model` | `jellyfin-dbmigrate model --from <db>` | Lists tables, row counts, and shadow properties. |
| `copy` | `jellyfin-dbmigrate copy --from <db> --to <db>` | Runs the target provider's migrations, disables foreign key (FK) checks, deletes ALL rows in every non-empty target table, copies every table in FK order in batches of 1000 with original keys, carries code-migration history, re-enables FK checks, and prints total rows and seconds. |
| `verify` | `jellyfin-dbmigrate verify --from <db> --to <db>` | Compares every non-shadow property of every row by primary key; DateTime compares by ticks. Exit code 1 on any difference, 2 on a usage error. |

`copy` is destructive to populated target tables: it deletes all rows in every target table that holds rows before it copies. Point it at an empty or disposable target. DateTime values are stored as BIGINT ticks (`galera/README.md`).

To move from SQLite to Galera:

1. Stop Jellyfin.
2. Create the database and user (see [Database](#database)).
3. Copy the data:

    ```bash
    jellyfin-dbmigrate copy --from sqlite:/config/data/data/jellyfin.db --to "$GALERA"
    ```

4. Verify it:

    ```bash
    jellyfin-dbmigrate verify --from sqlite:/config/data/data/jellyfin.db --to "$GALERA"
    ```

5. Install the plugin and write `database.xml` as in [Point Jellyfin at the database](#point-jellyfin-at-the-database).
6. Start Jellyfin.

### Migration timing

Method: real library, 19,259 items, 308,330 rows, 31 tables; lab, single workstation, 2026-09-26 (source `docs/engineering/galera-provider.md`). Verify reported identical on each, and the round trip was identical. The README also lists a 38.8 s result for an earlier Oracle provider; that provider is no longer in the code, so it is left out here.

| Step | Time |
| --- | --- |
| SQLite to Galera, 1 PXC node (Pomelo, the EF Core MySQL provider) | 39.9 s |
| SQLite to Galera, 3-node cluster | 46.2 s |
| Galera to new SQLite | 28.2 s |

### Roll back

Keep the original SQLite file until you are satisfied.

1. Stop Jellyfin.
2. Copy from Galera into a new SQLite file and verify it:

    ```bash
    jellyfin-dbmigrate copy   --from "$GALERA" --to sqlite:/config/data/data/jellyfin.db.new
    jellyfin-dbmigrate verify --from "$GALERA" --to sqlite:/config/data/data/jellyfin.db.new
    ```

3. Move `jellyfin.db.new` into place as `jellyfin.db` and delete `database.xml`. Jellyfin defaults to SQLite.

### Build the tool from source

`jellyfin-dbmigrate` needs `Jellyfin.Database.Providers.Sqlite.dll` from the Jellyfin 12.1 image, which is not on NuGet. Copy it out with `podman cp <container>:/usr/lib/jellyfin/bin/Jellyfin.Database.Providers.Sqlite.dll`, place it under `$(JellyfinBin)` (default `$HOME/.cache/jellymesh-vendor`), and build the Pomelo fork first with `galera/pomelo/build.sh`. That script pins a commit of the `upgrade/10.0.0` branch of the sufficit Pomelo fork (upstream PR #2047, unmerged) and applies `jellymesh-pomelo.patch`. Details are in [galera](../galera/README.md).

## Leader plugin

**Status:** Implemented. Outside Kubernetes the plugin makes the node the leader and does no election.

The leader plugin makes scheduled tasks run once across all replicas by electing one replica with a Lease. It needs `get`, `create`, `update`, and `patch` on `leases` in the pod's namespace, and replica `HOSTNAME` values must differ. Requirements, the example Role, environment variables, failover time (up to the lease seconds plus one tick, about 18 s at the default, From code reading), and limits are in [leader](../leader/README.md).

## Traefik active/passive failover

**Status:** Lab-verified shape. An example route with the lab values is in [deploy/examples/traefik-failover.yaml](../deploy/examples/traefik-failover.yaml); adjust namespace, host and TLS for your cluster. The measurements below are single lab runs (n=1) from [the direct-play failover log](engineering/direct-play-failover.md) unless a row names another source.

The lab route uses these values, all from the header of that log. The names `jm-jf-0` and `jm-jf-1` are example names from the lab.

| Setting | Lab value |
| --- | --- |
| Traefik service | `TraefikService` `jm-failover`, `failover` mode |
| Primary and fallback | `jm-jf-0` and `jm-jf-1`, a two-replica `jm-jf` StatefulSet |
| Failover trigger | `errors.status: 502-504` |
| `ServersTransport` | `jm-fast-dial`, `dialTimeout` 500 ms |

### Measured behavior

| Case | Result | Context and source |
| --- | --- | --- |
| Direct play (streaming the original file), hard kill of the Jellyfin process, 26,976,301-byte FLAC | The connection resets. Plain `curl` received about 6.2 MB and did not resume. | Lab, n=1, [direct-play-failover](engineering/direct-play-failover.md) |
| Same, with a client that retries with an HTTP Range request (a Range retry asks for the un-received tail of the file) | Resumed on the fallback and finished byte-identical. Detection took about 5.1 s and the retry about 0.35 s, so the gap was 5-6 s (order of magnitude only). The first attempt got 8,407,732 bytes, then a 206 range retry on `jm-jf-1`. | Lab, n=1, same log |
| Graceful pod delete | Kestrel (the ASP.NET web server in Jellyfin) kept serving open connections for about 21 s. One usable trial finished inside that window. | Lab, n=1, same log |
| HLS (HTTP Live Streaming) transcode through failover, without `JELLYMESH_SHARED_TRANSCODE_DIR` | A failover meant a fresh transcode on the survivor: ffmpeg restarts. | From code reading, [SHARED-TRANSCODE](../transcode/docs/SHARED-TRANSCODE.md) "Goal"; not measured as a separate row |
| HLS transcode through failover, with `JELLYMESH_SHARED_TRANSCODE_DIR=1` | No new ffmpeg for the sessions in three lab drills. See [Shared transcode directory](#shared-transcode-directory-for-replicas). | Lab, 2026-09-28, [SHARED-TRANSCODE](../transcode/docs/SHARED-TRANSCODE.md) |
| Return to the primary once healthy | A second ffmpeg restart. A Safari remux (video copy, DTS to AAC) restarted on the fallback and again on the primary three minutes later; the viewer reported A/V drift. | Measured 2026-09-27, viewer report, [ROADMAP](ROADMAP.md) "Sticky server failover" |

Direct play is a static file on shared storage, which is why a Range retry works. Clients must retry with Range. No client was measured: the client table in the log comes from public trackers (Android TV ExoPlayer, Moonfin, Swiftfin, Infuse), not from measurements.

Graceful restarts are not safe for a feature-length direct-play stream. A stream that is still open when the 30 s grace period ends is cut at SIGKILL, so the Range-retry path is needed for rolling restarts too (log, "What actually breaks").

To hard-kill a replica in a drill, target the supervised `jellyfin` process, found with `pgrep -f /usr/bin/jellyfin`. PID 1 is `s6-svscan`, and killing it has no effect. `kubectl` `RESTARTS` does not increment for an in-container SIGKILL, because s6-overlay (the process supervisor in the image) respawns the process. Use readiness probe events or `ps` to detect it.

In the lab, `/Videos/{id}/stream` and `/Audio/{id}/stream` answered range requests with no token or session. This is stock Jellyfin behavior: those two actions carry no `[Authorize]` attribute (code read, recorded in the log). An authenticated Range retry across replicas was not measured. The lab state was `12.1-jm7.1`, and production was not touched.

Sticky failover, which keeps a viewer on the fallback, is Planned: see [ROADMAP](ROADMAP.md). The reason is that Traefik sends a failed-over viewer back to the primary as soon as it is healthy, so one failover costs two ffmpeg restarts. The planned fix is a sticky cookie on the route plus an A/V start-PTS check after each forced restart.

### Plugins blocked on the fallback

Intro Skipper, Playback Reporting, and Kodi Sync Queue do not work on the fallback server today (see [ROADMAP](ROADMAP.md)). Plugins with their own SQLite files, for example Playback Reporting, are not shared-DB safe until the plugin compatibility layer in the roadmap lands.

## Transcode pool

**Status:** Implemented. Dolby Vision 7 -> 8.1 is in production (user-reported, 2026-09-29); other deployment details of the maintainer's cluster are deployment-specific and not recorded here.

The pool replaces Jellyfin's ffmpeg with a shim that sends HLS transcodes to per-GPU agents over gRPC with mTLS. Component detail is in [transcode](../transcode/README.md).

### Apply order

Apply the manifests in `transcode/deploy/k8s/` in numeric order:

| File | Contents |
| --- | --- |
| `00-namespace.yaml` | Namespace. |
| `10-tls.yaml` | cert-manager pool CA and the agent and client certificates (mTLS). |
| `15-scratch.yaml` | PV `transcode-scratch-media` and PVC `transcode-scratch` (namespace `media`, bound by `volumeName`), 200Gi RWX (ReadWriteMany) NFS. The server address is a placeholder; edit it. |
| `20-agents.yaml` | Three DaemonSets: `qsv`, `nvenc`, `cpu`. |
| `30-service.yaml` | Headless Service `tcpool-agents` (gRPC 9901, health 9902, metrics 9903), plus a Service `tcpool-sync-metrics` for the sync sidecar. |
| `40-pdb.yaml` | Three PodDisruptionBudgets (PDB, a limit on voluntary evictions). |
| `50-rbac.yaml` | Service accounts only; no Role. |
| `60-servicemonitor.yaml` | Optional; needs the Prometheus operator CRDs, and applying it fails without them. |

Replace the `GIT_SHA` placeholder in `20-agents.yaml` with the commit of the images you built. Edit the NFS server and export in `15-scratch.yaml`.

### Scratch volume

The scratch mount options are load-bearing (`15-scratch.yaml`, CONTRACT):

```yaml
mountOptions:
  - nfsvers=4.2
  - lookupcache=positive
  - actimeo=1
```

With NFS defaults, a freshly written segment stayed invisible to the other node for 12-23 s (measured, per `15-scratch.yaml` and `transcode/deploy/CONTRACT.md`; the test-bed is not recorded there). `actimeo=1` also keeps lease and keepalive mtimes visible across nodes within about 1 s (`SHARED-TRANSCODE.md`).

Set `storageClassName: ""` on both the PV and the PVC. The cluster that the manifests were written for had two default StorageClasses, so an unqualified RWX claim bound non-deterministically. The 200Gi is the claim size, not an enforced quota; the quota lives on the NFS export.

Two rules apply to the scratch directory (CONTRACT.md, "Two rules about the scratch directory"):

1. `TranscodingTempPath` must be a subdirectory of the mount, one per Jellyfin identity (for example `/transcodes/jf`), never the mount root. Jellyfin wipes its whole transcode path at startup, so two Jellyfins sharing the root means restarting either one deletes the other's live segments.
2. Leave the root's ownership alone. It is `root:media` (gid 1000) mode `1777`, with `.jellyfin-transcode` owned `root:root 0644`. Jellyfin cannot delete that marker, so its startup wipe throws and is swallowed, and the racy marker re-create path never fires. That path returned HTTP 500 to two viewers starting at once (measured twice, per CONTRACT).

### Values you must set

From `20-agents.yaml`, `40-pdb.yaml`, `50-rbac.yaml`, and CONTRACT.md:

| Setting | Value | Why |
| --- | --- | --- |
| `TC_CAPACITY` | Arc (`qsv`) 14, P4 (`nvenc`) 6, CPU 3 | A hard admission ceiling. With neither `TC_CAPACITY` nor `TC_MAX_JOBS` the agent defaults to 1000 units, which is effectively unbounded. |
| `TC_WEIGHT_4K` | Arc 2.3, P4 2, CPU 3 | The Arc and P4 values were measured on the cards; the CPU value is an estimate. |
| `TC_TLS_REQUIRED` | `1` | Missing certificate variables are a hard error, so an agent cannot fall back to plaintext. |
| `TC_HW_FILTERS` | `0` on the CPU worker only | The CPU worker has no GPU filter chain. |
| Health port 9902 | Plaintext gRPC health | For the kubelet only, because its gRPC probe cannot speak TLS. Port 9901 is gRPC over mTLS. |
| PDB `maxUnavailable` | `1`, never `minAvailable` | A class with one pod would otherwise block `kubectl drain`. |
| `automountServiceAccountToken` | `false` | Neither process talks to the Kubernetes API. |

### Node labels

The DaemonSets select nodes by label:

| DaemonSet | Selector |
| --- | --- |
| `qsv` | `intel.feature.node.kubernetes.io/gpu: "true"` |
| `nvenc` | `nvidia.com/gpu.present: "true"` |
| `cpu` | `kubernetes.io/hostname: <your-cpu-node-hostname>` |

Edit the `cpu` selector in `20-agents.yaml` to name your CPU host.

### Install the shim and point it at the pool

The `12.1-jm5` and later images already install the shim as Jellyfin's ffmpeg. Set `TC_WORKERS_DNS` in the Jellyfin container to the headless Service, as CONTRACT.md gives it:

```yaml
- name: TC_WORKERS_DNS
  value: tcpool-agents.media.svc.cluster.local.:9901
```

Keep the trailing dot: it stops the resolver walking the search list. The shim resolves the A records on every transcode, and each record is one worker.

Set Jellyfin's hardware acceleration to none; Jellyfin then emits a software ffmpeg command and the agent rewrites it for its card. Run `tcpool-sync` as a sidecar of the Jellyfin pod so the codec offers match the workers; its settings are `JF_URL`, `JF_API_KEY`, and `TC_WORKERS_DNS` (see [configuration](configuration.md)). The sync crate header (`transcode/crates/sync/src/main.rs`) says sync resolves `TC_WORKERS_DNS` the same way the shim does.

### Shared transcode directory for replicas

**Status:** Implemented, opt-in, off by default. Lab-verified on 2026-09-28.

Set `JELLYMESH_SHARED_TRANSCODE_DIR=1` on every Jellyfin replica that shares a transcode directory. Both replicas use the same subdirectory of the scratch mount (never its root, see the two scratch rules above) as `TranscodingTempPath`. `HOSTNAME` must differ between replicas. Roll the agents first, then Jellyfin.

Lab proof (`jm-lab`, 2026-09-28, [SHARED-TRANSCODE](../transcode/docs/SHARED-TRANSCODE.md)): each drill ran three concurrent HLS sessions through the failover route, with an NVENC agent built from the branch.

| Drill | Failed segments | New ffmpeg for the sessions |
| --- | --- | --- |
| A: pod delete of the serving replica at +45 s | 0 of 162 | None |
| B: `kill -9` of the serving Jellyfin process at +45 s | 0 of 162 | None |
| C: restart of the non-serving replica while one session was paused 150 s | 0 of 223 | None |

Limits, all from `SHARED-TRANSCODE.md`:

- Agent tunables: `TC_DETACH` (`0` turns detaching off), `TC_ORPHAN_IDLE_SECS` (60), `TC_ORPHAN_PAUSED_SECS` (180), `TC_ORPHAN_MAX_SECS` (6 h), `TC_ORPHAN_LEAD_MAX_SECS` (60), `TC_ORPHAN_LEAD_RESUME_SECS` (30), and `TC_ORPHAN_POS_STALE_SECS` (60).
- A new shim against an old agent gets no detach, and its takeover request goes unanswered, so it falls back to `follow()` after 8 s. An old shim against a new agent never sends a keepalive.
- A StatefulSet keeps its pod name across restarts, which is the weaker `HOSTNAME` case. It is safe only because a freshly started replica has no job for that output.
- Local (non-pool) ffmpeg jobs do not detach. They die with their replica.
- Trickplay `TC_BATCH` shared-volume wiring is not covered.

### Rollback to a GPU Jellyfin

`jellyfin-qsv` and `jellyfin-nvenc` stay defined at `replicas: 0` in `arr-stack/k3s/30-media.yaml` through the canary (CONTRACT.md, "Rollback"). To roll back, scale one of them up and set `encoding.xml` `hwaccel` back to `qsv`. The pool's own objects can stay applied; with no shim pointing at them the agents are idle.

### Known gaps

The shim's "refuse a major ffmpeg mismatch" check is not implemented, so the agent image's base digest pin is the only guard between the agent's ffmpeg and Jellyfin's (`transcode/deploy/Containerfile.agent`). The manifests refer to `deploy/k8s/README.md` and `k8s/jellyfin-patch.md` for the Jellyfin-side configuration; that configuration is deployment-specific and not shipped here, and `transcode/deploy/CONTRACT.md` states what the Jellyfin pod must provide.

## Enable Dolby Vision conversion

**Status:** Production, opt-in, off by default (user-reported, 2026-09-29).

Only enable it where the pool shim is in the transcode path: without a shim, stock ffmpeg copies raw profile 7 while the playlist advertises 8.1. The full requirements, the client profile check, and limits are in [Dolby Vision](dolby-vision.md).

1. Run a `jellymesh-jellyfin` tag with bughunt patches 00-19 (`12.1-jm8.4`), and at least one `tcpool-agent`.
2. Set `TC_WORKERS_DNS` on the Jellyfin container (see above).
3. Add the flag to the Jellyfin container and restart Jellyfin:

    ```yaml
    - name: JELLYMESH_DOVI_P7_TO_81
      value: "1"
    ```

4. Play a profile 7 title from a client that advertises `DOVI` or `DOVIWithHDR10`. Then read the agent metric `tcpool_dv81_total{outcome="converted"}`; it increments for each converted session.

To roll back, unset the variable and restart Jellyfin. The flag is off unless it is set (bughunt patch 13, [bughunt](engineering/bughunt.md)).

## Upgrades and rollback

**Status:** Implemented. No automated upgrade tooling exists in the repo.

1. Build and push the new tag (see [Images](#images)), or pull a published one.
2. For pool changes, roll the agents first, then Jellyfin. A draining agent flips its health to NOT_SERVING on SIGTERM and leaves DNS within about 3 s, because `publishNotReadyAddresses` is deliberately unset.
3. Change the Jellyfin image tag. The initContainer (a pod container that runs before the main one) refreshes plugins by rename, so a replica that is still running keeps its mapped file.
4. Roll one replica at a time so the other serves the route.

To roll back, set the previous tag. Tags `12.1-jm7.1` and earlier lack later patches; do not use `12.1-jm8.2` for Android TV audio. To turn off a patch feature, unset `JELLYMESH_DOVI_P7_TO_81` or `JELLYMESH_SHARED_TRANSCODE_DIR`; both are off unless set. If you upgrade Jellyfin itself, rebuild the Galera provider against the new `Jellyfin.Database.Implementations`.

## Monitoring

**Status:** Implemented for the pool. Kuma monitors and a chaos CronJob are Planned (see [ROADMAP](ROADMAP.md)). There is no mysqld or wsrep exporter in the deployment, which the roadmap lists as a gap.

Agents and `tcpool-sync` serve Prometheus text when `TC_METRICS_PORT` is set. The ports come from different files:

| Component | Variable | Port | Source |
| --- | --- | --- | --- |
| Agent | `TC_METRICS_PORT` | 9903 | `transcode/deploy/k8s/20-agents.yaml` |
| `tcpool-sync` | `TC_METRICS_PORT` | 9904 (expected) | Service `tcpool-sync-metrics` in `30-service.yaml`, `60-servicemonitor.yaml`, and the comments in `alerts.yaml` |

No manifest sets `TC_METRICS_PORT` for sync. Sync runs as a sidecar of the Jellyfin pod, outside `transcode/deploy/k8s/`, so 9904 is the port the Service, ServiceMonitor, and alerts expect. The Jellyfin-side sidecar configuration is deployment-specific and not shipped here. To match `30-service.yaml`, the sync container needs `TC_METRICS_PORT=9904`, a container port named `sync-metrics`, and the pod label `app.kubernetes.io/component: tcpool-sync`. Sync also serves a `/status` JSON view on the same port.

### Agent metrics

| Metric | Type |
| --- | --- |
| `tcpool_capacity_units` | gauge |
| `tcpool_units_used` | gauge |
| `tcpool_jobs_active` | gauge |
| `tcpool_jobs_total{outcome}` | counter |
| `tcpool_job_seconds` | histogram |
| `tcpool_job_speed` | gauge, per active job; not exported while a job is paused |
| `tcpool_probe_output` | gauge |
| `tcpool_gpu_tonemap` | gauge |
| `tcpool_draining` | gauge |
| `tcpool_orphans_paused` | gauge, detached jobs held paused now |
| `tcpool_dv81_total{outcome}` | counter |
| `tcpool_build_info` | gauge |
| `tcpool_batch_units_used` | gauge |
| `tcpool_batch_jobs_active` | gauge |
| `tcpool_batch_headroom_units` | gauge |

The `tcpool_dv81_total` outcomes are `converted`, `fallback_no_rpu`, `fallback_not_p7`, and `fallback_error`. Types come from `crates/agent/src/metrics.rs`.

### Sync metrics

| Metric | Type |
| --- | --- |
| `tcpool_pool_survivable` | gauge |
| `tcpool_pool_worker_live` | gauge |
| `tcpool_pool_workers_configured` | gauge |
| `tcpool_pool_workers_live` | gauge |
| `tcpool_pool_capacity_units` | gauge |
| `tcpool_pool_common_output` | gauge |
| `tcpool_jellyfin_offer` | gauge |
| `tcpool_jellyfin_offer_intent` | gauge |
| `tcpool_sync_last_success_timestamp_seconds` | gauge |

### Alert rules

The rules are in `transcode/deploy/alerts.yaml`. Conditions and durations are copied from that file:

| Alert | Expression | For |
| --- | --- | --- |
| `TranscodePoolNotSurvivable` | `tcpool_pool_survivable == 0` | `10m` |
| `TranscodeWorkerDown` | `tcpool_pool_worker_live == 0` | `5m` |
| `TranscodeJobsSlow` | `tcpool_job_speed < 1.0` | `60s` |
| `TranscodePolicyRefusals` | `increase(tcpool_jobs_total{outcome="refused_policy"}[10m]) > 0` | `0m` |
| `TranscodeSyncStale` | `time() - tcpool_sync_last_success_timestamp_seconds > 5 * 60` | `0m` |

`alerts.yaml` is not applied to the cluster. Its header describes a merge into the single `rules.yml` key of a `vmalert-rules` ConfigMap, followed by a vmalert restart, because vmalert does not hot-reload the file. The scrape targets for the agent Service (9903) and the sync Service (9904) must be added separately, wherever your Prometheus or vmagent scrape config lives.

## Related docs

- [Architecture](architecture.md): components, request paths, and the glossary.
- [Configuration reference](configuration.md): environment variables and ports.
- [Dolby Vision](dolby-vision.md): how the conversion works, its limits, and verification.
- [Troubleshooting](troubleshooting.md): symptoms and fixes.
- [Results](RESULTS.md): measurements with method and test-bed.
- [Roadmap](ROADMAP.md): planned work, including sticky failover and the plugin compatibility layer.
- [Image](../image/README.md): tag lineage, build contexts, and image internals.
- [Leader plugin](../leader/README.md): Lease election and task forwarding.
- [Galera component](../galera/README.md): provider, migration tool, lab, and drills.
- [Perf and bughunt patches](../jellyfin-perf/README.md): the patch series.
- [Transcode pool](../transcode/README.md): shim, agent, and sync.
- [Contributing](../CONTRIBUTING.md): build steps and the open documentation gaps.

# Operations

Deploying JellyMesh means building an image, standing up a database, migrating into it, adding the leader plugin, putting a failover route in front, and wiring up the transcode pool — then keeping all of that upgraded and monitored. This page walks that in order, for Kubernetes (k3s) or podman. When something is deployed but misbehaving rather than refusing to come up, [troubleshooting](troubleshooting.md) is the better page.

Dolby Vision 7 -> 8.1 is the one piece I'd call production. It's been running on my cluster and playing as Dolby Vision on the Android TV app since 2026-09-29, and since 2026-09-30 the EAC3 track has been passing through from the SHIELD to my AV receiver as Dolby Digital Plus (patch 19 row in [bughunt](engineering/bughunt.md)).

## What the repo ships

JellyMesh is a set of parts you assemble. The repo ships the code, the Containerfiles, and the transcode pool manifests; the cluster around them is yours.

| Item | In the repo |
| --- | --- |
| Jellyfin image Containerfiles (`image/`) | Yes; the build context is not |
| Galera provider plugin and `jellyfin-dbmigrate` (`galera/`) | Yes |
| Leader plugin (`leader/`) | Yes, source only |
| Transcode pool manifests (`transcode/deploy/k8s/`, files `00` to `60`) | Yes |
| Traefik failover route | No; described from lab evidence |

The rest you write yourself. Where the code or a lab run fixes a parameter, you'll find it below; anything else in a snippet is an example.

| Item you write | Where the repo helps |
| --- | --- |
| Jellyfin StatefulSet or Deployment manifests | [configuration](configuration.md) lists the environment variables |
| Lease RBAC (Role and RoleBinding) | Example Role in [leader](../leader/README.md) |
| MySQL or Percona XtraDB Cluster (PXC) manifests | Lab script `galera/lab/galera-lab.sh` only |
| Kuma monitors | Nothing; see [Monitoring](#monitoring) |

## Images

Nothing in CI builds the Jellyfin image. You build it with podman.

The image is `ghcr.io/saabstory404/jellymesh-jellyfin`. The newest tag in the repo is `12.1-jm8.4`, and it's the tag the Dolby Vision docs refer to. Which tag a given deployment actually runs isn't recorded here.

Tag lineage, the per-Containerfile build context, and the image internals are in [image](../image/README.md). You assemble the staging context yourself — each Containerfile's header comment lists what it expects — and the newer tags need less in it than the older ones:

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

The plugin install step, the pool images (`tcpool-agent`, `tcpool-shim`), and `transcode-images.yml` are all covered in [image](../image/README.md).

## Database

The [Galera](architecture.md#glossary) provider puts Jellyfin's database on a MySQL-compatible cluster instead of SQLite. I've only ever run it against a three-node PXC 8.4 cluster in the lab; the numbers from that are in [results](RESULTS.md).

### Server requirements

- MySQL 8.4 or Percona XtraDB Cluster 8.4. The provider pins `MySqlServerVersion` 8.4.0 in code and doesn't auto-detect, and PXC 8.4 is the only server I've run it against.
- The database has to exist before you start. The provider runs `ALTER DATABASE ... utf8mb4 / utf8mb4_bin` from `MigrationBackupFast`, which Jellyfin calls before it runs migrations, and creates every string column as `utf8mb4_bin`.
- `jellyfin-dbmigrate copy` does not create the database or set its collation.

The lab script `galera/lab/galera-lab.sh db` creates the database and a user with all privileges on it. The blanket grant is a lab convenience rather than a requirement, and the password is a placeholder:

```bash
podman exec gl-db1 mysql -uroot -p"$ROOTPW" -e "
  CREATE DATABASE IF NOT EXISTS jellyfin CHARACTER SET utf8mb4 COLLATE utf8mb4_bin;
  CREATE USER IF NOT EXISTS 'jellyfin'@'%' IDENTIFIED BY 'jellyfin';
  GRANT ALL ON jellyfin.* TO 'jellyfin'@'%';"
```

That lab connection string uses `SslMode=Disabled` and `AllowPublicKeyRetrieval=true`. Don't carry those into production — use TLS and a Kubernetes Secret. The lab cluster script, its ports, and its drills are in [galera](../galera/README.md).

### Point Jellyfin at the database

Jellyfin loads the provider through `database.xml` with `DatabaseType` set to `PLUGIN_PROVIDER`, `PluginName` set to `JellyMesh Galera`, `PluginAssembly` set to `Jellyfin.Database.Providers.Galera.dll`, and a `ConnectionString`. The provider registers under the key `Jellyfin-Galera` and throws at start if the connection string is missing.

Set `JELLYMESH_DB_PASSWORD` to override the `Password` in the connection string. That keeps the password out of `database.xml`, which lands in config backups; mount it from a Kubernetes Secret. The provider logs the connection string with the password masked.

### Strict mode

PXC with `pxc_strict_mode=ENFORCING` rejects `GET_LOCK`, and `GET_LOCK` is what EF Core's migration lock uses. My lab cluster runs `PERMISSIVE` and migrates from one node, and that's the only setting I can tell you anything about.

### Single-writer or multi-writer

| Layout | How | Measured behavior |
| --- | --- | --- |
| Single-writer | Every Jellyfin lists the nodes in the same order: `Server=db1,db2,db3;LoadBalance=FailOver`. Other nodes are synchronous standbys. | SIGKILL of the primary under load: 206 requests, 0 failed, longest gap 2.9 s. The bootstrap node has to come back as a joiner (`rejoin`). |
| Multi-writer | Each Jellyfin writes to a different node. | Needs the retry from the perf patch: `SaveUserData` retries with a fresh context, jittered backoff, and 6 attempts. With the retry, 200 of 200 concurrent writes succeeded; without it, 200 concurrent writes to one row across nodes gave 69 HTTP 500 responses ("Deadlock found"). |

Single-writer is the simpler layout. A separate run killed a non-primary node under load with a `FailOver` list: 204 requests, 0 failed, longest gap 2.9 s, and the node was Synced 7 s after restart by incremental state transfer (IST). Both of those are one run each of `galera_drill.py` against the three-node lab cluster in 2026-09; the drills table is in [galera provider notes](engineering/galera-provider.md).

### Backups are your job

The provider does not back up the database. `MigrationBackupFast` only sets the charset and collation and logs a warning. `RestoreBackupFast` logs a critical "cannot restore" message and does nothing. `DeleteBackup` and `RunShutdownTask` are no-ops. So Jellyfin's own pre-migration backup and restore don't work on this provider at all, and backing up the cluster is on you with your own tooling — the code comment names xtrabackup.

### Plain MySQL

Plain MySQL 8.4 with `JELLYFIN_SHARED_DB=1` came out nearly as fast as Galera: 89.1 and 109.3 requests/s at 8 and 32 clients, against 90.7 and 112.0 on Galera, all on one lab workstation ([results](RESULTS.md)). You get no failover for it, and the deployment has no wsrep metrics exporter.

### Trickplay tiles

Trickplay database rows are shared through the database, but the tile files are not unless `SaveTrickplayWithMedia` is true. The default is false, which leaves tiles on per-node storage. Either set `SaveTrickplayWithMedia` or put the tile directory on shared storage. That comes from reading the code and from [bughunt](engineering/bughunt.md), not from a drill.

## Migrate SQLite to Galera and back

`jellyfin-dbmigrate` is built into the image at `/opt/jellymesh/jellyfin-dbmigrate`.

A database spec is `sqlite:<path>` or `galera:<connection string>`. In the commands below, `$GALERA` stands for one `galera:` spec that you set once:

```bash
GALERA="galera:Server=db;Database=jellyfin;Uid=jellyfin;Pwd=...;SslMode=..."
```

| Mode | Command shape | What it does |
| --- | --- | --- |
| `model` | `jellyfin-dbmigrate model --from <db>` | Lists tables, row counts, and shadow properties. |
| `copy` | `jellyfin-dbmigrate copy --from <db> --to <db>` | Runs the target provider's migrations, disables foreign key (FK) checks, deletes ALL rows in every non-empty target table, copies every table in FK order in batches of 1000 with original keys, carries code-migration history, re-enables FK checks, and prints total rows and seconds. |
| `verify` | `jellyfin-dbmigrate verify --from <db> --to <db>` | Compares every non-shadow property of every row by primary key; DateTime compares by ticks. Exit code 1 on any difference, 2 on a usage error. |

`copy` is destructive: before it copies anything it deletes every row in every target table that isn't already empty. Point it at an empty or disposable target. DateTime values are stored as BIGINT ticks.

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

These came off my real library — 19,259 items, 308,330 rows, 31 tables — on one workstation on 2026-09-26. `verify` reported identical on each, and the round trip was identical too. There's also a 38.8 s result recorded for an earlier Oracle provider; that provider is gone from the code, so it's left out here.

| Step | Time |
| --- | --- |
| SQLite to Galera, 1 PXC node (Pomelo, the EF Core MySQL provider) | 39.9 s |
| SQLite to Galera, 3-node cluster | 46.2 s |
| Galera to new SQLite | 28.2 s |

### Roll back

Keep the original SQLite file until you're satisfied.

1. Stop Jellyfin.
2. Copy from Galera into a new SQLite file and verify it:

    ```bash
    jellyfin-dbmigrate copy   --from "$GALERA" --to sqlite:/config/data/data/jellyfin.db.new
    jellyfin-dbmigrate verify --from "$GALERA" --to sqlite:/config/data/data/jellyfin.db.new
    ```

3. Move `jellyfin.db.new` into place as `jellyfin.db` and delete `database.xml`. Jellyfin defaults to SQLite.

### Build the tool from source

This one takes some setup. `jellyfin-dbmigrate` needs `Jellyfin.Database.Providers.Sqlite.dll` from the Jellyfin 12.1 image, which isn't on NuGet: copy it out with `podman cp <container>:/usr/lib/jellyfin/bin/Jellyfin.Database.Providers.Sqlite.dll` and place it under `$(JellyfinBin)` (default `$HOME/.cache/jellymesh-vendor`). You also have to build the Pomelo fork first, with `galera/pomelo/build.sh`. That script pins a commit of the `upgrade/10.0.0` branch of the sufficit Pomelo fork (upstream PR #2047, unmerged) and applies `jellymesh-pomelo.patch`. The details are in [galera](../galera/README.md).

## Leader plugin

The leader plugin keeps scheduled tasks from running on every replica at once: it elects one replica with a Lease. It needs `get`, `create`, `update`, and `patch` on `leases` in the pod's namespace, and the replicas' `HOSTNAME` values have to differ. Outside Kubernetes there's no election at all — the node simply becomes the leader.

Requirements, the example Role, the environment variables, and the limits are in [leader](../leader/README.md), including failover time: up to the lease seconds plus one tick, about 18 s at the default. That last figure is what the code does, not something I've timed.

## Traefik active/passive failover

The repo doesn't ship this route, only the shape I ran in the lab. There's an example carrying the lab values in [deploy/examples/traefik-failover.yaml](../deploy/examples/traefik-failover.yaml); fix up the namespace, host and TLS for your own cluster. Everything measured below is a single lab run from [the direct-play failover log](engineering/direct-play-failover.md) unless the row names another source.

The lab route used these values, all from that log's header. `jm-jf-0` and `jm-jf-1` are just the names my lab StatefulSet happened to use.

| Setting | Lab value |
| --- | --- |
| Traefik service | `TraefikService` `jm-failover`, `failover` mode |
| Primary and fallback | `jm-jf-0` and `jm-jf-1`, a two-replica `jm-jf` StatefulSet |
| Failover trigger | `errors.status: 502-504` |
| `ServersTransport` | `jm-fast-dial`, `dialTimeout` 500 ms |

### Measured behavior

| Case | Result | Context and source |
| --- | --- | --- |
| Direct play (streaming the original file), hard kill of the Jellyfin process, 26,976,301-byte FLAC | The connection resets. Plain `curl` received about 6.2 MB and did not resume. | One lab run, [direct-play-failover](engineering/direct-play-failover.md) |
| Same, with a client that retries with an HTTP Range request (a Range retry asks for the un-received tail of the file) | Resumed on the fallback and finished byte-identical. Detection took about 5.1 s and the retry about 0.35 s, so the gap was 5-6 s (order of magnitude only). The first attempt got 8,407,732 bytes, then a 206 range retry on `jm-jf-1`. | One lab run, same log |
| Graceful pod delete | Kestrel (the ASP.NET web server in Jellyfin) kept serving open connections for about 21 s. One usable trial finished inside that window. | One lab run, same log |
| HLS (HTTP Live Streaming) transcode through failover, without `JELLYMESH_SHARED_TRANSCODE_DIR` | A failover meant a fresh transcode on the survivor: ffmpeg restarts. | From the code and [SHARED-TRANSCODE](../transcode/docs/SHARED-TRANSCODE.md) "Goal"; not measured on its own |
| HLS transcode through failover, with `JELLYMESH_SHARED_TRANSCODE_DIR=1` | No new ffmpeg for the sessions in three lab drills. See [Shared transcode directory](#shared-transcode-directory-for-replicas). | Lab, 2026-09-28, [SHARED-TRANSCODE](../transcode/docs/SHARED-TRANSCODE.md) |
| Return to the primary once healthy | A second ffmpeg restart. A Safari remux (video copy, DTS to AAC) restarted on the fallback and again on the primary three minutes later; the viewer reported A/V drift. | Measured 2026-09-27, viewer report, [ROADMAP](ROADMAP.md) "Sticky server failover" |

Direct play is a static file on shared storage, which is the only reason a Range retry works — but the client has to make one. I didn't measure any client: the client table in the log comes from public trackers (Android TV ExoPlayer, Moonfin, Swiftfin, Infuse), not from anything I ran.

Graceful restarts aren't safe for a feature-length direct-play stream either. A stream still open when the 30 s grace period ends gets cut at SIGKILL, so you need the Range-retry path for rolling restarts too (log, "What actually breaks").

To hard-kill a replica in a drill, go after the supervised `jellyfin` process; `pgrep -f /usr/bin/jellyfin` finds it. PID 1 is `s6-svscan` and killing that does nothing. `kubectl`'s `RESTARTS` count won't move for an in-container SIGKILL either, because s6-overlay (the process supervisor in the image) respawns the process — watch readiness probe events or `ps` instead.

In the lab, `/Videos/{id}/stream` and `/Audio/{id}/stream` answered range requests with no token and no session. That's stock Jellyfin behavior: neither action carries an `[Authorize]` attribute, which I read in the code rather than tested. I never measured an authenticated Range retry across replicas. All of this ran on `12.1-jm7.1` in the lab; production wasn't touched.

Traefik sends a failed-over viewer straight back to the primary as soon as it's healthy, so a single failover costs two ffmpeg restarts. Keeping the viewer on the fallback — sticky failover — is still planned: see [ROADMAP](ROADMAP.md). The fix I have in mind is a sticky cookie on the route plus an A/V start-PTS check after each forced restart.

### Plugins blocked on the fallback

Intro Skipper, Playback Reporting, and Kodi Sync Queue don't work on the fallback server today. Any plugin with its own SQLite file — Playback Reporting being the obvious one — isn't shared-DB safe until the plugin compatibility layer on the [roadmap](ROADMAP.md) lands.

## Transcode pool

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

Replace the `GIT_SHA` placeholder in `20-agents.yaml` with the commit of the images you built, and edit the NFS server and export in `15-scratch.yaml`.

### Scratch volume

The scratch mount options are load-bearing:

```yaml
mountOptions:
  - nfsvers=4.2
  - lookupcache=positive
  - actimeo=1
```

With NFS defaults, a freshly written segment stayed invisible to the other node for 12-23 s. That was measured, but nothing records what it was measured on. `actimeo=1` also keeps lease and keepalive mtimes visible across nodes within about 1 s.

Set `storageClassName: ""` on both the PV and the PVC. The cluster these manifests were written for had two default StorageClasses, so an unqualified RWX claim bound non-deterministically. The 200Gi is the claim size, not an enforced quota — the quota lives on the NFS export.

Two rules about the scratch directory, both from CONTRACT.md:

1. `TranscodingTempPath` must be a subdirectory of the mount, one per Jellyfin identity (for example `/transcodes/jf`), never the mount root. Jellyfin wipes its whole transcode path at startup, so two Jellyfins sharing the root means restarting either one deletes the other's live segments.
2. Leave the root's ownership alone. It is `root:media` (gid 1000) mode `1777`, with `.jellyfin-transcode` owned `root:root 0644`. Jellyfin can't delete that marker, so its startup wipe throws and is swallowed, and the racy marker re-create path never fires. That path has returned HTTP 500 to two viewers starting at once, measured twice.

### Values you must set

These come from `20-agents.yaml`, `40-pdb.yaml`, `50-rbac.yaml`, and CONTRACT.md:

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

The `12.1-jm5` and later images already install the shim as Jellyfin's ffmpeg. Set `TC_WORKERS_DNS` in the Jellyfin container to the headless Service, the way CONTRACT.md gives it:

```yaml
- name: TC_WORKERS_DNS
  value: tcpool-agents.media.svc.cluster.local.:9901
```

Keep the trailing dot — it stops the resolver walking the search list. The shim resolves the A records on every transcode, and each record is one worker.

Set Jellyfin's hardware acceleration to none. Jellyfin then emits a software ffmpeg command and the agent rewrites it for its card. Run `tcpool-sync` as a sidecar of the Jellyfin pod so the codec offers match the workers; its settings are `JF_URL`, `JF_API_KEY`, and `TC_WORKERS_DNS` (see [configuration](configuration.md)). Sync resolves `TC_WORKERS_DNS` the same way the shim does, per the crate header in `transcode/crates/sync/src/main.rs`.

### Shared transcode directory for replicas

This is opt-in and off unless you set it, and I've only verified it in the lab.

Set `JELLYMESH_SHARED_TRANSCODE_DIR=1` on every Jellyfin replica that shares a transcode directory. Both replicas use the same subdirectory of the scratch mount (never its root, see the two scratch rules above) as `TranscodingTempPath`. `HOSTNAME` must differ between replicas. Roll the agents first, then Jellyfin.

The drills ran on `jm-lab` on 2026-09-28, three concurrent HLS sessions each through the failover route, with an NVENC agent built from the branch ([SHARED-TRANSCODE](../transcode/docs/SHARED-TRANSCODE.md)):

| Drill | Failed segments | New ffmpeg for the sessions |
| --- | --- | --- |
| A: pod delete of the serving replica at +45 s | 0 of 162 | None |
| B: `kill -9` of the serving Jellyfin process at +45 s | 0 of 162 | None |
| C: restart of the non-serving replica while one session was paused 150 s | 0 of 223 | None |

The limits:

- Agent tunables: `TC_DETACH` (`0` turns detaching off), `TC_ORPHAN_IDLE_SECS` (60), `TC_ORPHAN_PAUSED_SECS` (180), `TC_ORPHAN_MAX_SECS` (6 h), `TC_ORPHAN_LEAD_MAX_SECS` (60), `TC_ORPHAN_LEAD_RESUME_SECS` (30), and `TC_ORPHAN_POS_STALE_SECS` (60).
- A new shim against an old agent gets no detach, and its takeover request goes unanswered, so it falls back to `follow()` after 8 s. An old shim against a new agent never sends a keepalive.
- A StatefulSet keeps its pod name across restarts, which is the weaker `HOSTNAME` case. It is safe only because a freshly started replica has no job for that output.
- Local (non-pool) ffmpeg jobs do not detach. They die with their replica.
- Trickplay `TC_BATCH` shared-volume wiring is not covered.

### Rollback to a GPU Jellyfin

`jellyfin-qsv` and `jellyfin-nvenc` stay defined at `replicas: 0` in `arr-stack/k3s/30-media.yaml` through the canary. To roll back, scale one of them up and set `encoding.xml` `hwaccel` back to `qsv`. The pool's own objects can stay applied; with no shim pointing at them the agents just sit idle.

### Known gaps

The shim's "refuse a major ffmpeg mismatch" check was never written, so the base digest pin in `transcode/deploy/Containerfile.agent` is the only thing keeping the agent's ffmpeg and Jellyfin's in step. The manifests point at `deploy/k8s/README.md` and `k8s/jellyfin-patch.md` for the Jellyfin-side configuration, which isn't in this repo; `transcode/deploy/CONTRACT.md` is where you'll find what the Jellyfin pod has to provide.

## Enable Dolby Vision conversion

The conversion is off unless you turn it on, and only turn it on where the pool shim is in the transcode path: without a shim, stock ffmpeg copies raw profile 7 while the playlist advertises 8.1. The full requirements, the client profile check, and the limits are in [Dolby Vision](dolby-vision.md).

1. Run a `jellymesh-jellyfin` tag with bughunt patches 00-19 (`12.1-jm8.4`), and at least one `tcpool-agent`.
2. Set `TC_WORKERS_DNS` on the Jellyfin container (see above).
3. Add the flag to the Jellyfin container and restart Jellyfin:

    ```yaml
    - name: JELLYMESH_DOVI_P7_TO_81
      value: "1"
    ```

4. Play a profile 7 title from a client that advertises `DOVI` or `DOVIWithHDR10`. Then read the agent metric `tcpool_dv81_total{outcome="converted"}`; it increments for each converted session.

To roll back, unset the variable and restart Jellyfin. The flag is off unless it's set (bughunt patch 13, [bughunt](engineering/bughunt.md)).

## Upgrades and rollback

There's no upgrade tooling in the repo, so this is four steps by hand:

1. Build and push the new tag (see [Images](#images)), or pull a published one.
2. For pool changes, roll the agents first, then Jellyfin. A draining agent flips its health to NOT_SERVING on SIGTERM and leaves DNS within about 3 s, because `publishNotReadyAddresses` is deliberately unset.
3. Change the Jellyfin image tag. The initContainer (a pod container that runs before the main one) refreshes plugins by rename, so a replica that is still running keeps its mapped file.
4. Roll one replica at a time so the other serves the route.

To roll back, set the previous tag. Tags `12.1-jm7.1` and earlier lack the later patches, and don't use `12.1-jm8.2` for Android TV audio. To turn off a patch feature, unset `JELLYMESH_DOVI_P7_TO_81` or `JELLYMESH_SHARED_TRANSCODE_DIR`; both are off unless set. If you upgrade Jellyfin itself, rebuild the Galera provider against the new `Jellyfin.Database.Implementations`.

## Monitoring

The pool is the only part with metrics. Kuma monitors and a chaos CronJob are still planned, and there's no mysqld or wsrep exporter anywhere in the deployment; both are gaps on the [roadmap](ROADMAP.md).

Agents and `tcpool-sync` serve Prometheus text when `TC_METRICS_PORT` is set, but they get their ports from different places:

| Component | Variable | Port |
| --- | --- | --- |
| Agent | `TC_METRICS_PORT` | 9903 |
| `tcpool-sync` | `TC_METRICS_PORT` | 9904 (expected) |

The agent's port is set in `transcode/deploy/k8s/20-agents.yaml`. No manifest sets `TC_METRICS_PORT` for sync at all: sync runs as a sidecar of the Jellyfin pod, outside `transcode/deploy/k8s/`, so 9904 is only the port that the Service `tcpool-sync-metrics` in `30-service.yaml`, the `60-servicemonitor.yaml`, and the comments in `alerts.yaml` all expect. That Jellyfin-side sidecar configuration isn't in this repo. To match `30-service.yaml`, the sync container needs `TC_METRICS_PORT=9904`, a container port named `sync-metrics`, and the pod label `app.kubernetes.io/component: tcpool-sync`. Sync also serves a `/status` JSON view on the same port.

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

The `tcpool_dv81_total` outcomes are `converted`, `fallback_no_rpu`, `fallback_not_p7`, and `fallback_error`.

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

The rules live in `transcode/deploy/alerts.yaml`, and the conditions and durations here are copied straight from it:

| Alert | Expression | For |
| --- | --- | --- |
| `TranscodePoolNotSurvivable` | `tcpool_pool_survivable == 0` | `10m` |
| `TranscodeWorkerDown` | `tcpool_pool_worker_live == 0` | `5m` |
| `TranscodeJobsSlow` | `tcpool_job_speed < 1.0` | `60s` |
| `TranscodePolicyRefusals` | `increase(tcpool_jobs_total{outcome="refused_policy"}[10m]) > 0` | `0m` |
| `TranscodeSyncStale` | `time() - tcpool_sync_last_success_timestamp_seconds > 5 * 60` | `0m` |

Nothing applies `alerts.yaml` to the cluster for you. Its header describes the merge into the single `rules.yml` key of a `vmalert-rules` ConfigMap, followed by a vmalert restart, because vmalert doesn't hot-reload the file. The scrape targets for the agent Service (9903) and the sync Service (9904) have to be added separately, wherever your Prometheus or vmagent scrape config lives.

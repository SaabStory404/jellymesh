# Configuration reference

Every environment variable, port, file setting, and Kubernetes object JellyMesh reads, with the defaults taken from the code. A default of "unset" means the feature is off or a built-in fallback applies. Dolby Vision 7 -> 8.1 (`JELLYMESH_DOVI_P7_TO_81`) is the one setting that has been running on my own cluster rather than just in the lab, since 2026-09-29.

## Jellyfin server settings

These variables are read by the patched Jellyfin server built from `jellyfin-perf/`. Only the value `1` enables a flag unless the Effect column says otherwise.

| Variable | Applies to | Default | Effect |
| --- | --- | --- | --- |
| `JELLYFIN_SHARED_DB` | Jellyfin server | unset (off) | `1` makes user data, login sessions, and item lookups read from the shared database, and keeps item-cache entries for 5 s only. The startup wipe of the transcode directory then deletes only files older than 6 h. |
| `JELLYFIN_SHARED_INVALIDATION` | Jellyfin server, with `JELLYFIN_SHARED_DB=1` | on | `0` disables cross-node item-cache invalidation. Any other value keeps it on. The mechanism uses a table named `JellyMeshItemInvalidation` in the same database, created with `CREATE TABLE IF NOT EXISTS`. |
| `JELLYFIN_SHARED_INVALIDATION_POLL_MS` | Jellyfin server, with `JELLYFIN_SHARED_DB=1` | 250 | Poll interval in milliseconds. Values under 50, and values that do not parse, fall back to 250. |
| `JELLYMESH_SHARED_TRANSCODE_DIR` | Jellyfin server | unset (off) | `1` lets several replicas use one transcode directory: keepalive files are written and cleanup is lease-scoped; the 6 h startup-wipe rule is gated by `JELLYFIN_SHARED_DB=1`, not by this flag. Set it only where replicas share the directory. |
| `JELLYMESH_KEEPALIVE` | Jellyfin server (writer), `tcpool-shim` (reader) | set by Jellyfin | Path of the per-session keepalive file. You do not set it: Jellyfin passes it to the shim, which forwards it to the agent as `Job.keepalive_path`. |
| `JELLYMESH_DOVI_P7_TO_81` | Jellyfin server | unset (off) | `1` enables the Dolby Vision 7 -> 8.1 decision path for HLS jobs (patch 13), switches HLS to fMP4 so the init segment carries `dvvC` (patch 17), and drops TrueHD/MLP from the audio candidates (patch 18). TrueHD/MLP with 6 or more channels is encoded to EAC3 5.1 at 640 kb/s when the client lists `eac3`, otherwise to AAC (patch 19). Needs the transcode pool in the transcode path. See [dolby-vision.md](dolby-vision.md). |
| `JELLYFIN_DATA_DIR` | `image/install-plugins.sh` (initContainer) | `/config/data` | Directory whose `plugins/` subfolder receives the JellyMesh plugin DLLs. |
| `JELLYMESH_DB_PASSWORD` | Galera provider | unset | When non-empty, replaces the password in the `database.xml` connection string. Use it to keep the password out of config backups, for example from a Kubernetes Secret. |

> **Warning:** With `JELLYMESH_DOVI_P7_TO_81` set and no pool in the transcode path, stock ffmpeg stream-copies the raw profile 7 video while the playlist advertises profile 8.1 ([engineering log, Known issue #5](engineering/bughunt.md)).

When the shim is installed but the pool is unreachable, the shim applies a Dolby Vision removal rewrite.

### Legacy lab variables

`galera/lab/jf-galera.sh` also passes `JELLYMESH_SHARED_DB=1`, `JELLYMESH_REDIS`, and `JELLYMESH_RESPONSE_CACHE` when `JG_MESH` is set. The script also sets `JELLYMESH_NODE` to the node name. They belong to a Redis mesh plugin that is not in this repository, so treat them as lab-only legacy and unsupported. A grep over the repository finds `JELLYMESH_SHARED_DB`, `JELLYMESH_REDIS`, `JELLYMESH_RESPONSE_CACHE`, and `JELLYMESH_NODE` only in `galera/lab/jf-galera.sh` and this page, and none in the patches or plugins. Use `JELLYFIN_SHARED_DB` instead (`JG_SHARED=1` sets it in the same script).

## database.xml settings for the Galera provider

Jellyfin loads the provider from a plugin folder named `JellyMesh Galera_1.0.0.0`. Point Jellyfin at it with `database.xml` in the root of the Jellyfin config directory (`/config/database.xml` in the lab containers, which mount the node's config directory at `/config`). The plugin folder is created by `image/install-plugins.sh`. The example below is the lab file written by `galera/lab/jf-galera.sh`.

> **Warning:** These are lab values. Do not use `Pwd=jellyfin` or `SslMode=Disabled` in production; set `JELLYMESH_DB_PASSWORD` and choose a real `SslMode`.

```xml
<?xml version="1.0" encoding="utf-8"?>
<DatabaseConfigurationOptions xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">
  <DatabaseType>PLUGIN_PROVIDER</DatabaseType>
  <LockingBehavior>NoLock</LockingBehavior>
  <CustomProviderOptions>
    <PluginName>JellyMesh Galera</PluginName>
    <PluginAssembly>Jellyfin.Database.Providers.Galera.dll</PluginAssembly>
    <ConnectionString>Server=db1,db2,db3;Database=jellyfin;Uid=jellyfin;Pwd=jellyfin;SslMode=Disabled;AllowPublicKeyRetrieval=true;LoadBalance=FailOver</ConnectionString>
  </CustomProviderOptions>
</DatabaseConfigurationOptions>
```

| Setting | Value | Notes |
| --- | --- | --- |
| `DatabaseType` | `PLUGIN_PROVIDER` | Tells Jellyfin to load a database provider from a plugin. |
| `LockingBehavior` | `NoLock` | Jellyfin's own setting; the provider does not read it. `NoLock` is the value the lab runs. |
| `PluginName` | `JellyMesh Galera` | Plugin name as written in the lab file. |
| `PluginAssembly` | `Jellyfin.Database.Providers.Galera.dll` | Provider assembly. |
| `ConnectionString` | MySqlConnector syntax | Required. `Server=` takes a comma-separated node list; `LoadBalance=FailOver` tries nodes in listed order, so give every Jellyfin the same order. The lab file uses `SslMode=Disabled` and `AllowPublicKeyRetrieval=true`; pick your own `SslMode` for production. |

A few more things the provider does:

- It registers under the key `Jellyfin-Galera` (`[JellyfinDatabaseProviderKey("Jellyfin-Galera")]`).
- A missing connection string throws `InvalidOperationException` with the text `database.xml must set CustomProviderOptions/ConnectionString for Jellyfin-Galera`.
- It fixes the server version at MySQL 8.4.0 and does not auto-detect it, so startup opens no extra connection. Run a MySQL 8.4 server or PXC 8.4, as the lab does (`percona/percona-xtradb-cluster:8.4`).
- Startup logs the connection string with the password replaced by `*****`. Masking also covers a quoted password that contains `;`.
- `JELLYMESH_DB_PASSWORD` is applied before masking.

## Leader plugin

The leader plugin runs each Jellyfin scheduled task once across the cluster by holding a Kubernetes [Lease](architecture.md#glossary). It has no configuration page. The [roadmap](ROADMAP.md) lists scheduled-task leader election as done.

| Variable | Applies to | Default | Effect |
| --- | --- | --- | --- |
| `HOSTNAME` | Leader plugin | `Environment.MachineName` | Identity written as the Lease holder. Must differ between replicas. |
| `JELLYMESH_LEASE` | Leader plugin | `jellyfin-tasks` | Name of the `coordination.k8s.io/v1` Lease in the pod's namespace. |
| `JELLYMESH_LEASE_SECONDS` | Leader plugin | 15 | Lease duration. The loop ticks every `max(1, seconds / 5)` s, which is 3 s at the default. A value that does not parse as an integer falls back to 15. |
| `KUBERNETES_SERVICE_HOST` | Leader plugin | set by Kubernetes | Kubernetes detection needs this variable non-empty and the ServiceAccount token file present. Outside Kubernetes the node acts as leader. |
| `KUBERNETES_SERVICE_PORT` | Leader plugin | 443 | API server port. |

Three more details, fixed in the code:

- The pod's ServiceAccount needs the verbs `get`, `create`, `update`, and `patch` on `leases` in its namespace.
- A task started on a follower is cancelled there and forwarded through the Lease annotation `jellymesh.io/run-task`, with the value `<task key>|<unix ms>`.
- On graceful stop the leader patches `holderIdentity` to null for instant handover.

## tcpool-agent

One `tcpool-agent` runs per GPU (or CPU) worker. It reads these at startup. A bare `crates/...` path in this section and the two that follow is relative to `transcode/`.

| Variable | Applies to | Default | Effect |
| --- | --- | --- | --- |
| `TC_KIND` | agent | `cpu` | Backend: `qsv`, `nvenc`, or `cpu`. Any other value stops startup. |
| `TC_NAME` | agent | value of `TC_KIND` | Worker name shown to the shim and in metrics. |
| `NODE_NAME` | agent | empty | Node name; the manifests set it from `spec.nodeName`. |
| `TC_FFMPEG` | agent | `/usr/lib/jellyfin-ffmpeg/ffmpeg` | ffmpeg binary. `ffprobe` is taken from the same directory. |
| `TC_PORT` | agent | 9901 | gRPC listen port. |
| `TC_HEALTH_PORT` | agent | 9902 when TLS is on, otherwise no extra port | Plaintext gRPC health port for kubelet probes. The manifests set 9902. |
| `TC_METRICS_PORT` | agent | unset (no metrics server) | Port for Prometheus `/metrics`. Same variable name as in `tcpool-sync`, separate process. The manifests set 9903. |
| `TC_PATHMAP` | agent | empty | Comma-separated `from=to` path rewrites. Each `from` substring is replaced with `to` in every argument, in list order (`map_path` in `crates/ir/src/lib.rs`). The shipped manifests leave it unset because the agents mount media at the same path as Jellyfin. |
| `TC_CAPACITY` | agent | 0 | Admission ceiling in weighted units. When set above 0, it wins over `TC_MAX_JOBS` and the weight variables below apply. |
| `TC_MAX_JOBS` | agent | 0 | Flat job count, weight 1 per job. Used only when `TC_CAPACITY` is not above 0. With neither set, capacity is 1000 units (effectively unbounded). |
| `TC_WEIGHT_1440` | agent | 2.0 | Weight of a 1440p job. Ignored (weight 1.0) unless `TC_CAPACITY` is above 0. |
| `TC_WEIGHT_4K` | agent | 3.0 | Weight of a 4K job. Same condition. |
| `TC_WEIGHT_COPY` | agent | 0.25 | Weight of a video-copy job. Same condition. |
| `TC_OUTPUTS` | agent | unset (no restriction) | Comma-separated list restricting which outputs the agent advertises. Tokens: `h264`, `hevc`, `hevc10`, `av1`, `av1-10` (`OUTPUTS` in `crates/agent/src/probe.rs`). The agent advertises only outputs that pass its startup probe and appear in the list. |
| `TC_HW_FILTERS` | agent | on | Any value other than `0` keeps GPU-resident filters on; `0` disables them. |
| `TC_RC` | agent | `calibrated` | Rate control: `calibrated` or `legacy`. |
| `TC_PROBE_CLAMP` | agent | `50M,5M` | `probesize,analyzeduration` clamp in ffmpeg size syntax (for example `50M`): bytes for the first value, microseconds for the second, so the default caps analysis at 5 s (`clamp_probe` in `crates/ir/src/lib.rs`). The clamp only lowers Jellyfin's values and is skipped for commands with `-filter_complex`. A value without a comma, such as `0`, or with unparsable sizes turns the clamp off. |
| `TC_FENCE_AFTER` | agent | 3 s | Time after which an agent that lost its shim stops ffmpeg. |
| `TC_STALL_AFTER` | agent | 20 s | Playback job is treated as stalled after this long without progress. |
| `TC_FIRST_PROGRESS_GRACE` | agent | 45 s | Grace period before the first progress report. |
| `TC_BATCH_STALL_AFTER` | agent | 300 s | Stall limit for batch (trickplay) jobs. |
| `TC_BATCH_FIRST_PROGRESS_GRACE` | agent | 300 s | First-progress grace for batch jobs. |
| `TC_BATCH_WEIGHT` | agent | 1.0 | Weight of a batch job. |
| `TC_ACCEPT_BATCH` | agent | true | Whether the agent accepts batch jobs. |
| `TC_BATCH_HEADROOM` | agent | 0.0 | Weighted capacity units kept free for playback when admitting batch jobs. |
| `TC_INPUT_ROOTS` | agent | `/media,/data/media` | Comma-separated roots the allowlist accepts for inputs. |
| `TC_READ_ROOTS` | agent | `/config/data/data/subtitles,/config/data/data/attachments` | Extra read-only roots for subtitles and fonts. |
| `TC_OUTPUT_ROOT` | agent | `/transcodes` | The only root the allowlist accepts for HLS outputs. |
| `TC_TRICKPLAY_OUTPUT_ROOT` | agent, shim | unset | Root for trickplay frames. Unset means the agent refuses every trickplay job. |
| `TC_DETACH` | agent | true | Detach a job when its shim disappears, so another replica can take over. Part of the shared transcode directory design. |
| `TC_ORPHAN_IDLE_SECS` | agent | 60 | A detached job ends after this long without a keepalive touch. |
| `TC_ORPHAN_PAUSED_SECS` | agent | 180 | Same, when the last touch said the client was paused. |
| `TC_ORPHAN_MAX_SECS` | agent | 21600 (6 h) | Absolute lifetime of a detached job. |
| `TC_ORPHAN_LEAD_MAX_SECS` | agent | 60 | Throttle a detached job when it runs this far ahead of the viewer. |
| `TC_ORPHAN_LEAD_RESUME_SECS` | agent | 30 | Resume when the lead falls to this value. |
| `TC_ORPHAN_POS_STALE_SECS` | agent | 60 | A viewer position older than this is treated as stale. |

Boolean variables read through `env_bool` (not `TC_HW_FILTERS`, which is off only for `0`) are off for `0`, `false`, `no`, and `off`, case-insensitive; any other non-empty value is on, and unset or empty gives the default.

`TC_LOG=/dev/stdout` appears in `transcode/deploy/Containerfile.agent`, but a grep over `transcode/crates` finds nothing that reads it.

### TLS variables (agent, shim, sync)

All three binaries read the same variables through the same code.

| Variable | Applies to | Default | Effect |
| --- | --- | --- | --- |
| `TC_TLS_CERT` | agent, shim, sync | unset | PEM certificate path. |
| `TC_TLS_KEY` | agent, shim, sync | unset | PEM key path. |
| `TC_TLS_CA` | agent, shim, sync | unset | PEM CA path. The agent requires client certificates signed by this CA. |
| `TC_TLS_REQUIRED` | agent, shim, sync | unset | Any non-empty value other than `0` makes missing `TC_TLS_*` a startup error (agent exits with code 2); `0` allows plaintext. The manifests set it. |
| `TC_TLS_SERVER_NAME` | shim, sync (client side) | `tcpool-agent` | Server name the client verifies against the agent certificate. |

Set all three of cert, key, and CA, or none. Setting only some is a startup error. With none set and `TC_TLS_REQUIRED` unset or `0`, the agent serves plaintext with [mTLS](architecture.md#glossary) off.

## tcpool-shim

The shim replaces Jellyfin's ffmpeg binary. It sends HLS transcodes to a worker and runs the real ffmpeg for everything else.

| Variable | Applies to | Default | Effect |
| --- | --- | --- | --- |
| `TC_WORKERS_DNS` | shim | unset | `host[:port]` of the headless Service. Each A or AAAA record is one worker. Port defaults to 9901. The lookup makes up to 3 attempts, 300 ms apart. Keep the trailing dot, as in `tcpool-agents.media.svc.cluster.local.:9901`, so the resolver skips the search list. |
| `TC_WORKERS` | shim | unset | Static list `name=host:port,...`, merged with DNS results. |
| `TC_FFMPEG_REAL` | shim | `/usr/lib/jellyfin-ffmpeg/ffmpeg.real` | Real ffmpeg the shim runs locally. Must exist. |
| `TC_SHIM_LOG` | shim | `/config/log/tc-shim.log` | Shim log file. |
| `TC_BATCH` | shim | unset (off) | `1` sends trickplay extraction to the pool. Needs `TC_TRICKPLAY_OUTPUT_ROOT` set as well. |
| `TC_AFFINITY` | shim | on | `0` disables seek affinity (no read, write, or clear of the `<md5>.worker` file). |
| `TC_AFFINITY_TTL_SECS` | shim | 21600 (6 h) | Lifetime of an affinity file. Unset or unparsable values give the default. |
| `TC_TLS_*` | shim | see TLS table | Client certificate and CA for the gRPC connection. |

### Dolby Vision marker

When the Jellyfin decision patch gates a job, it adds the argv pair `-metadata:s:v:0 JELLYMESH_DOVI_P7_TO_81=1` on the output video stream. The pool also accepts `-metadata:s:v:0 TC_DV81=1` as an alias. This is an ffmpeg argument, not an environment variable, and `config.rs` has no agent environment variable for it.

## tcpool-sync

`tcpool-sync` intersects worker outputs and sets Jellyfin's HEVC and AV1 offers to the lowest common denominator.

| Variable | Applies to | Default | Effect |
| --- | --- | --- | --- |
| `JF_URL` | sync | empty in code; `transcode/deploy/CONTRACT.md` documents `http://127.0.0.1:8096` | Jellyfin base URL, or a comma-separated list with one entry per replica. |
| `JF_API_KEY` | sync | empty | Jellyfin API key, sent as `MediaBrowser Token="..."`. |
| `TC_CAPS_FILE` | sync | `/config/tc-mesh-caps.json` | State file. |
| `TC_SYNC_EVERY` | sync | 30 | Seconds between sync rounds. |
| `TC_STARTUP_GRACE` | sync | 120 | Seconds after start before sync acts. |
| `TC_TRANSCODE_DIR` | sync | unset | When set and non-empty, sync keeps a `.jellyfin-transcode` marker in this directory. `transcode/deploy/CONTRACT.md` gives `/transcodes/jf` as the production value and says sync re-creates the marker because Jellyfin wipes it at startup. |
| `TC_SYNC_ONCE` | sync | unset | When set (any value), sync runs one round and exits. |
| `TC_METRICS_PORT` | sync | unset (no server) | Serves Prometheus `/metrics` and `/status` JSON on one port. Same variable name as in the agent, separate process. The Service in `30-service.yaml` uses 9904. |
| `TC_WORKERS_DNS`, `TC_WORKERS` | sync | unset | Same meaning as for the shim. |
| `TC_TLS_*` | sync | see TLS table | Same as above. |

## Build-time and lab variables

| Variable | Applies to | Default | Effect |
| --- | --- | --- | --- |
| `JF_SRC` | `jellyfin-perf/build.sh` | `$HOME/.cache/jellymesh-vendor/jellyfin-src` | Jellyfin checkout (tag `v12.1`). |
| `JF_OVERLAY` | `jellyfin-perf/build.sh` | `$HOME/.cache/jellymesh-vendor/jellyfin-perf` | Output directory for the seven changed assemblies. |
| `BUGHUNT` | `jellyfin-perf/build.sh` | 1 | `0` applies only the perf patch; otherwise all `bughunt/NN-*.patch` files apply in numeric order. |
| `POMELO_SRC` | `galera/pomelo/build.sh` | `$HOME/.cache/jellymesh-vendor/pomelo-src` | Pomelo checkout. |
| `POMELO_OUT` | `galera/pomelo/build.sh` | `$HOME/.cache/jellymesh-vendor/pomelo` | Patched Pomelo output. |
| `PomeloBin` | Galera provider csproj (MSBuild property) | `$(HOME)/.cache/jellymesh-vendor/pomelo` | Directory holding `Pomelo.EntityFrameworkCore.MySql.dll`. |
| `JellyfinBin` | dbmigrate csproj (MSBuild property) | `$(HOME)/.cache/jellymesh-vendor` | Directory holding `Jellyfin.Database.Providers.Sqlite.dll`. |
| `JmDesign` | Galera provider csproj | unset | `true` keeps host assemblies in the output so `dotnet ef migrations add` can run. |
| `TC_BUILD` | `transcode/deploy/build-musl.sh` | `container` | `host` builds with the host cargo and musl-gcc. |
| `TC_BUILDER_IMAGE` | `transcode/deploy/build-musl.sh` | `docker.io/library/rust:1-bookworm` | Builder image. |
| `TC_REGISTRY` | `transcode/deploy/build-images.sh` | `ghcr.io/saabstory404` | Image registry prefix. |
| `GL_ROOTPW` | `galera/lab/galera-lab.sh`, `galera/tools/digest_profile.sh` | `labroot` (lab script) | Lab MySQL root password. |
| `GL_CERTS` | `galera/lab/galera-lab.sh` | `$HOME/.cache/galera-lab/certs` | Lab TLS certificates. |
| `JG_LAB`, `JG_SRC`, `JG_PLUGIN`, `JG_IMG`, `JG_OVERLAY`, `JG_PW_ENV`, `JG_SHARED`, `JG_SQLITE` | `galera/lab/jf-galera.sh` | `JG_LAB` `$HOME/.cache/galera-lab`; `JG_SRC` `$HOME/.cache/dbsidecar/s`; `JG_IMG` `ghcr.io/hotio/jellyfin:release-12.1`; others unset | Lab node settings. `JG_PLUGIN` is required. `JG_PW_ENV=1` passes the password as `JELLYMESH_DB_PASSWORD`; `JG_SHARED=1` sets `JELLYFIN_SHARED_DB=1`. |

`JG_MESH`, `JG_REDIS`, and `JG_RC` in the same script drive the legacy mesh plugin described above.

## Ports

| Port | Protocol | Component | Notes |
| --- | --- | --- | --- |
| 9901 | gRPC over mTLS | agent | Default of `TC_PORT`. Discovery uses the headless Service `tcpool-agents`. |
| 9902 | plaintext gRPC health | agent | Kubelet gRPC probes cannot speak TLS. Serves health only. |
| 9903 | HTTP | agent metrics | Set by `TC_METRICS_PORT` in the manifests. |
| 9904 | HTTP | sync metrics and `/status` | Named `sync-metrics` in the `tcpool-sync-metrics` Service. |
| `1330<n>` (13301 for node 1, 13302 for node 2) | MySQL | Galera lab node n | Host port from the `podman run` line in `galera/lab/galera-lab.sh`. The script's header comment says 13306 for the first node; the code does not match it. Jellyfin's own port (8096) and production MySQL (3306) are not covered here. |

## Kubernetes objects in transcode/deploy/k8s

Apply the files in numeric order.

| File | Object | Key values |
| --- | --- | --- |
| `00-namespace.yaml` | Namespace | Namespace `media` is used by the other files. |
| `10-tls.yaml` | cert-manager Issuer and Certificates | Self-signed issuer, pool CA (RSA 2048, duration 8760h, renew 720h), agent and client leaf certificates (RSA 2048, 2160h = 90 days, renew 720h = 30 days). |
| `15-scratch.yaml` | PV `transcode-scratch-media`, PVC `transcode-scratch` in namespace `media` | 200Gi, ReadWriteMany, NFS path `/mnt/pool/transcode-scratch`, `persistentVolumeReclaimPolicy: Retain`, `storageClassName: ""`, PVC bound by `volumeName`. Mount options `nfsvers=4.2`, `lookupcache=positive`, `actimeo=1` are required: with NFS defaults a new segment stayed invisible to the other node for 12-23 s. 200Gi is the claim size; the quota lives on the NFS export. `192.0.2.120` is a placeholder address: replace it. |
| `20-agents.yaml` | Three DaemonSets (qsv, nvenc, cpu) | Values for my hardware, not recommendations: `TC_CAPACITY` 14 and `TC_WEIGHT_4K` 2.3 (qsv, Arc), 6 and 2 (nvenc, P4), 3 and 3 (cpu). Selectors: `intel.feature.node.kubernetes.io/gpu` (qsv); `nvidia.com/gpu.present` plus a request for `nvidia.com/gpu: 1` (nvenc); `kubernetes.io/hostname: dl380` (cpu; cluster-specific, check before applying). The cpu DaemonSet sets `TC_HW_FILTERS=0`. All three set `TC_INPUT_ROOTS=/data/media` (the code default is `/media,/data/media`), `TC_HEALTH_PORT=9902`, `TC_METRICS_PORT=9903`, `prometheus.io` scrape annotations, and pod `fsGroup: 1000`. Image tag placeholder `GIT_SHA`; `terminationGracePeriodSeconds` 30; TLS required. |
| `30-service.yaml` | Services | Headless `tcpool-agents` (ports 9901-9903) and `tcpool-sync-metrics` (9904). |
| `40-pdb.yaml` | Three PodDisruptionBudgets | `maxUnavailable: 1` per class. |
| `50-rbac.yaml` | ServiceAccounts | Service accounts only, no Role. |
| `60-servicemonitor.yaml` | ServiceMonitors | Optional. Needs Prometheus Operator CRDs (`monitoring.coreos.com`); applying it without them fails. Use one of three scrape mechanisms: the `prometheus.io` annotations already on the agent pod templates (port 9903, `/metrics`), a vmagent pod-SD job, or these ServiceMonitors. Two at once double every `rate()`. The manifest refers to `deploy/k8s/README.md`, which does not exist yet. The sync ServiceMonitor has no endpoints until the label and named port from `k8s/jellyfin-patch.md` land. |

A few operational details the manifests and `transcode/deploy/CONTRACT.md` settle:

- Secrets: `tcpool-ca`, `tcpool-agent-tls` (90 d duration, 30 d renew), `tcpool-client-tls`, and `tcpool-sync` with key `api-key`, which is created out of band.
- Mounts the agent needs: media read-only at `/data/media`, `/transcodes` read-write, `/tls` with `defaultMode: 0440`, and an `/tmp` emptyDir for the CUDA JIT cache.
- Set `fsGroup: 1000` on the pod. Without it the agent exits with code 2, and the shim logs `tls misconfigured ... running LOCALLY` and CPU-encodes without notice.
- The headless Service leaves `publishNotReadyAddresses` unset on purpose, so a draining agent drops out of DNS.
- The agent drains itself when the mtime of the certificate files changes. CA rotation requires deleting the two leaf Secrets.

The Jellyfin StatefulSet, Traefik failover route, and Lease RBAC manifest are not in the repository. Their names and settings appear only as prose in [direct-play-failover.md](engineering/direct-play-failover.md); the verbs the Lease needs are listed in the [Leader plugin](#leader-plugin) section. `transcode/deploy/CONTRACT.md` points at `k8s/jellyfin-patch.md`, which is also absent.

## Metrics and alerts

The agent exports `tcpool_capacity_units`, `tcpool_units_used`, `tcpool_jobs_active`, `tcpool_batch_jobs_active`, `tcpool_batch_units_used`, `tcpool_batch_headroom_units`, `tcpool_jobs_total{outcome}`, `tcpool_job_seconds`, `tcpool_job_speed`, `tcpool_probe_output`, `tcpool_gpu_tonemap`, `tcpool_draining`, `tcpool_orphans_paused`, `tcpool_dv81_total{outcome}`, and `tcpool_build_info`. The `outcome` label of `tcpool_jobs_total` takes `accepted`, `busy`, `refused_policy`, `exit_ok`, `exit_error`, `fenced`, `stalled`, `drained`, `gpu_filter_fallback`, `busy_headroom`, `preempted`, `batch_accepted`, `detached`, `taken_over`, or `orphan_expired`; the `outcome` label of `tcpool_dv81_total` takes `converted`, `fallback_no_rpu`, `fallback_not_p7`, or `fallback_error`. Each metric's type is written as a `# TYPE` line in the `/metrics` output, and the doc comments in `transcode/crates/agent/src/metrics.rs` describe every metric and its labels.

`tcpool-sync` exports `tcpool_pool_worker_live`, `tcpool_pool_workers_configured`, `tcpool_pool_workers_live`, `tcpool_pool_capacity_units`, `tcpool_pool_common_output`, `tcpool_pool_survivable`, `tcpool_jellyfin_offer`, `tcpool_jellyfin_offer_intent`, and `tcpool_sync_last_success_timestamp_seconds`.

The alerts live in `transcode/deploy/alerts.yaml`, all with severity `warning`. That file is a rule-group fragment to merge into the `vmalert-rules` ConfigMap, not a standalone apply, and vmalert needs a rollout restart afterward. Durations are written as in the file.

| Alert | Condition | For |
| --- | --- | --- |
| `TranscodePoolNotSurvivable` | `tcpool_pool_survivable == 0` | 10m |
| `TranscodeWorkerDown` | `tcpool_pool_worker_live == 0` | 5m |
| `TranscodeJobsSlow` | `tcpool_job_speed < 1.0` | 60s |
| `TranscodePolicyRefusals` | `increase(tcpool_jobs_total{outcome="refused_policy"}[10m]) > 0` | 0m |
| `TranscodeSyncStale` | `time() - tcpool_sync_last_success_timestamp_seconds > 5 * 60`; also fires when the metric has never been set | 0m |

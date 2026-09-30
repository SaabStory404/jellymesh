# Configuration reference

This page lists every environment variable, port, file setting, and Kubernetes object that JellyMesh reads, with defaults copied from code. It is a reference: for procedures see [operations.md](operations.md), and for how the parts fit see [architecture.md](architecture.md).

**Status:** Implemented. Defaults are copied from the source files named in each table. Dolby Vision 7 -> 8.1 (`JELLYMESH_DOVI_P7_TO_81`) is Production as of 2026-09-29, as reported by the maintainer.

## How to read this page

Variable tables use the columns Variable, Applies to, Default, Effect, Source file; the database, ports, Kubernetes, and alert tables have their own headers. A default of "unset" means the feature is off or a built-in fallback applies. Status labels appear only where a setting is opt-in: "Implemented, opt-in, off by default" means the code is in the repo and nothing changes until you set the variable.

Source paths are relative to the repository root, except in the `tcpool-agent`, `tcpool-shim`, and `tcpool-sync` sections, where a bare `config.rs` or `crates/...` path is relative to `transcode/` (`config.rs` means `transcode/crates/agent/src/config.rs`). "Not documented yet" marks a fact the sources do not state; see the [doc TODO list](../CONTRIBUTING.md#doc-todo-list). Only the variables read through `env_bool` follow the `0`/`false`/`no`/`off` rule; other variables state their own rule.

Terms used here, including Galera, Pomelo, Lease, QSV, NVENC, mTLS, fMP4, dvvC, and RPU, are defined in the [glossary](architecture.md#glossary). Glossary entries for HLS, trickplay, seek affinity, and headless Service are Not documented yet.

## Jellyfin server settings

These variables are read by the patched Jellyfin server built from `jellyfin-perf/`. Only the value `1` enables a flag unless the Effect column says otherwise.

| Variable | Applies to | Default | Effect | Source file |
| --- | --- | --- | --- | --- |
| `JELLYFIN_SHARED_DB` | Jellyfin server | unset (off) | `1` makes user data, login sessions, and item lookups read from the shared database, and keeps item-cache entries for 5 s only. The startup wipe of the transcode directory then deletes only files older than 6 h. Implemented, opt-in, off by default. | `jellyfin-perf/jellyfin-12.1-perf.patch`, `jellyfin-perf/bughunt/07-progress-write-stall.patch`, `jellyfin-perf/bughunt/15-shared-item-cache-invalidation.patch` |
| `JELLYFIN_SHARED_INVALIDATION` | Jellyfin server, with `JELLYFIN_SHARED_DB=1` | on | `0` disables cross-node item-cache invalidation. Any other value keeps it on. The mechanism uses a table named `JellyMeshItemInvalidation` in the same database, created with `CREATE TABLE IF NOT EXISTS`. | `jellyfin-perf/bughunt/15-shared-item-cache-invalidation.patch` |
| `JELLYFIN_SHARED_INVALIDATION_POLL_MS` | Jellyfin server, with `JELLYFIN_SHARED_DB=1` | 250 | Poll interval in milliseconds. Values under 50, and values that do not parse, fall back to 250. | `jellyfin-perf/bughunt/15-shared-item-cache-invalidation.patch` |
| `JELLYMESH_SHARED_TRANSCODE_DIR` | Jellyfin server | unset (off) | `1` lets several replicas use one transcode directory: keepalive files are written and cleanup is lease-scoped; the 6 h startup-wipe rule is gated by `JELLYFIN_SHARED_DB=1`, not by this flag. Set it only where replicas share the directory. Implemented, opt-in, off by default. | `jellyfin-perf/bughunt/03-k2-incident-and-shared-dir.patch`, `jellyfin-perf/bughunt/16-shared-transcode-dir.patch` |
| `JELLYMESH_KEEPALIVE` | Jellyfin server (writer), `tcpool-shim` (reader) | set by Jellyfin | Path of the per-session keepalive file. You do not set it: Jellyfin passes it to the shim, which forwards it to the agent as `Job.keepalive_path`. | `jellyfin-perf/bughunt/16-shared-transcode-dir.patch`, `transcode/crates/ir/src/shared.rs` |
| `JELLYMESH_DOVI_P7_TO_81` | Jellyfin server | unset (off) | `1` enables the Dolby Vision 7 -> 8.1 decision path for HLS jobs (patch 13), switches HLS to fMP4 so the init segment carries `dvvC` (patch 17), and drops TrueHD/MLP from the audio candidates (patch 18). TrueHD/MLP with 6 or more channels is encoded to EAC3 5.1 at 640 kb/s when the client lists `eac3`, otherwise to AAC (patch 19). Needs the transcode pool in the transcode path. See [dolby-vision.md](dolby-vision.md). Implemented, opt-in, off by default; Production as of 2026-09-29 (maintainer report). | `jellyfin-perf/bughunt/13-dv7-to-81-decision.patch`, `jellyfin-perf/bughunt/17-dv81-fmp4-hls.patch`, `jellyfin-perf/bughunt/18-dv81-fmp4-transcoded-audio.patch`, `jellyfin-perf/bughunt/19-dv81-fmp4-eac3-encoder.patch` |
| `JELLYFIN_DATA_DIR` | `image/install-plugins.sh` (initContainer) | `/config/data` | Directory whose `plugins/` subfolder receives the JellyMesh plugin DLLs. | `image/install-plugins.sh` |
| `JELLYMESH_DB_PASSWORD` | Galera provider | unset | When non-empty, replaces the password in the `database.xml` connection string. Use it to keep the password out of config backups, for example from a Kubernetes Secret. | `galera/Jellyfin.Database.Providers.Galera/GaleraDatabaseProvider.cs` |

> **Warning:** With `JELLYMESH_DOVI_P7_TO_81` set and no pool in the transcode path, stock ffmpeg stream-copies the raw profile 7 video while the playlist advertises profile 8.1 ([engineering log, Known issue #5](engineering/bughunt.md)).

When the shim is installed but the pool is unreachable, the shim applies a Dolby Vision removal rewrite (`transcode/crates/shim/src/main.rs`; `transcode/deploy/CONTRACT.md`).

### Legacy lab variables

`galera/lab/jf-galera.sh` also passes `JELLYMESH_SHARED_DB=1`, `JELLYMESH_REDIS`, and `JELLYMESH_RESPONSE_CACHE` when `JG_MESH` is set. The script also sets `JELLYMESH_NODE` to the node name. They belong to a Redis mesh plugin that is not in this repository. Treat them as lab-only legacy and unsupported. From code reading: a grep over the repository finds `JELLYMESH_SHARED_DB`, `JELLYMESH_REDIS`, `JELLYMESH_RESPONSE_CACHE`, and `JELLYMESH_NODE` only in `galera/lab/jf-galera.sh` and this page, and none in the patches or plugins. Use `JELLYFIN_SHARED_DB` instead (`JG_SHARED=1` sets it in the same script).

## database.xml settings for the Galera provider

Jellyfin loads the provider from a plugin folder named `JellyMesh Galera_1.0.0.0`. Point Jellyfin at it with `database.xml`; the file's location is Not documented yet (see the [doc TODO list](../CONTRIBUTING.md#doc-todo-list)). The plugin folder is created by `image/install-plugins.sh`. The example below is the lab file written by `galera/lab/jf-galera.sh`.

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

| Setting | Value | Notes | Source file |
| --- | --- | --- | --- |
| `DatabaseType` | `PLUGIN_PROVIDER` | Tells Jellyfin to load a database provider from a plugin. | `galera/lab/jf-galera.sh` |
| `LockingBehavior` | `NoLock` | Value used in the lab file. Not documented yet whether other values are supported. | `galera/lab/jf-galera.sh` |
| `PluginName` | `JellyMesh Galera` | Plugin name as written in the lab file. | `galera/lab/jf-galera.sh` |
| `PluginAssembly` | `Jellyfin.Database.Providers.Galera.dll` | Provider assembly. | `galera/lab/jf-galera.sh` |
| `ConnectionString` | MySqlConnector syntax | Required. `Server=` takes a comma-separated node list; `LoadBalance=FailOver` tries nodes in listed order, so give every Jellyfin the same order. The lab file uses `SslMode=Disabled` and `AllowPublicKeyRetrieval=true`; pick your own `SslMode` for production. | `galera/lab/jf-galera.sh`, `galera/README.md` |

From code reading of `galera/Jellyfin.Database.Providers.Galera/GaleraDatabaseProvider.cs`:

- The provider registers under the key `Jellyfin-Galera` (`[JellyfinDatabaseProviderKey("Jellyfin-Galera")]`).
- A missing connection string throws `InvalidOperationException` with the text `database.xml must set CustomProviderOptions/ConnectionString for Jellyfin-Galera`.
- The provider fixes the server version at MySQL 8.4.0 and does not auto-detect it, so startup opens no extra connection. Operator impact beyond that is Not documented yet.
- Startup logs the connection string with the password replaced by `*****`. Masking also covers a quoted password that contains `;`.
- `JELLYMESH_DB_PASSWORD` is applied before masking.

## Leader plugin

**Status:** Implemented.

The leader plugin runs each Jellyfin scheduled task once across the cluster by holding a Kubernetes [Lease](architecture.md#glossary). It has no configuration page (`leader/Plugin.cs`). The [roadmap](ROADMAP.md) lists scheduled-task leader election as done.

| Variable | Applies to | Default | Effect | Source file |
| --- | --- | --- | --- | --- |
| `HOSTNAME` | Leader plugin | `Environment.MachineName` | Identity written as the Lease holder. Must differ between replicas. | `leader/LeaseLeaderService.cs` |
| `JELLYMESH_LEASE` | Leader plugin | `jellyfin-tasks` | Name of the `coordination.k8s.io/v1` Lease in the pod's namespace. | `leader/LeaseLeaderService.cs` |
| `JELLYMESH_LEASE_SECONDS` | Leader plugin | 15 | Lease duration. The loop ticks every `max(1, seconds / 5)` s, which is 3 s at the default. A value that does not parse as an integer falls back to 15. | `leader/LeaseLeaderService.cs` |
| `KUBERNETES_SERVICE_HOST` | Leader plugin | set by Kubernetes | Kubernetes detection needs this variable non-empty and the ServiceAccount token file present. Outside Kubernetes the node acts as leader. | `leader/LeaseLeaderService.cs` |
| `KUBERNETES_SERVICE_PORT` | Leader plugin | 443 | API server port. | `leader/LeaseLeaderService.cs` |

Other fixed details from code reading:

- The pod's ServiceAccount needs the verbs `get`, `create`, `update`, and `patch` on `leases` in its namespace.
- A task started on a follower is cancelled there and forwarded through the Lease annotation `jellymesh.io/run-task`, with the value `<task key>|<unix ms>`.
- On graceful stop the leader patches `holderIdentity` to null for instant handover.

## tcpool-agent

**Status:** Implemented.

One `tcpool-agent` runs per GPU (or CPU) worker. It reads the variables below at startup. Defaults come from `transcode/crates/agent/src/config.rs` unless the last column names another file.

| Variable | Applies to | Default | Effect | Source file |
| --- | --- | --- | --- | --- |
| `TC_KIND` | agent | `cpu` | Backend: `qsv`, `nvenc`, or `cpu`. Any other value stops startup. | `config.rs` |
| `TC_NAME` | agent | value of `TC_KIND` | Worker name shown to the shim and in metrics. | `config.rs` |
| `NODE_NAME` | agent | empty | Node name; the manifests set it from `spec.nodeName`. | `config.rs` |
| `TC_FFMPEG` | agent | `/usr/lib/jellyfin-ffmpeg/ffmpeg` | ffmpeg binary. `ffprobe` is taken from the same directory. | `config.rs` |
| `TC_PORT` | agent | 9901 | gRPC listen port. | `config.rs` |
| `TC_HEALTH_PORT` | agent | 9902 when TLS is on, otherwise no extra port | Plaintext gRPC health port for kubelet probes. The manifests set 9902. | `crates/agent/src/main.rs` |
| `TC_METRICS_PORT` | agent | unset (no metrics server) | Port for Prometheus `/metrics`. Same variable name as in `tcpool-sync`, separate process. The manifests set 9903. | `config.rs`, `crates/agent/src/main.rs` |
| `TC_PATHMAP` | agent | empty | Comma-separated `from=to` path rewrites (`from` is rewritten to `to`). Example values: Not documented yet. | `config.rs` |
| `TC_CAPACITY` | agent | 0 | Admission ceiling in weighted units. When set above 0, it wins over `TC_MAX_JOBS` and the weight variables below apply. | `config.rs` |
| `TC_MAX_JOBS` | agent | 0 | Flat job count, weight 1 per job. Used only when `TC_CAPACITY` is not above 0. With neither set, capacity is 1000 units (effectively unbounded). | `config.rs` |
| `TC_WEIGHT_1440` | agent | 2.0 | Weight of a 1440p job. Ignored (weight 1.0) unless `TC_CAPACITY` is above 0. | `config.rs` |
| `TC_WEIGHT_4K` | agent | 3.0 | Weight of a 4K job. Same condition. | `config.rs` |
| `TC_WEIGHT_COPY` | agent | 0.25 | Weight of a video-copy job. Same condition. | `config.rs` |
| `TC_OUTPUTS` | agent | unset (no restriction) | Comma-separated list restricting which outputs the agent advertises. Accepted values: Not documented yet. | `config.rs` |
| `TC_HW_FILTERS` | agent | on | Any value other than `0` keeps GPU-resident filters on; `0` disables them. | `config.rs` |
| `TC_RC` | agent | `calibrated` | Rate control: `calibrated` or `legacy`. | `config.rs` |
| `TC_PROBE_CLAMP` | agent | `50M,5M` | `probesize,analyzeduration` clamp in ffmpeg size syntax (for example `50M`); the unit of the second value is Not documented yet. A value without a comma, such as `0`, or with unparsable sizes turns the clamp off. | `config.rs` |
| `TC_FENCE_AFTER` | agent | 3 s | Time after which an agent that lost its shim stops ffmpeg. | `config.rs` |
| `TC_STALL_AFTER` | agent | 20 s | Playback job is treated as stalled after this long without progress. | `config.rs` |
| `TC_FIRST_PROGRESS_GRACE` | agent | 45 s | Grace period before the first progress report. | `config.rs` |
| `TC_BATCH_STALL_AFTER` | agent | 300 s | Stall limit for batch (trickplay) jobs. | `config.rs` |
| `TC_BATCH_FIRST_PROGRESS_GRACE` | agent | 300 s | First-progress grace for batch jobs. | `config.rs` |
| `TC_BATCH_WEIGHT` | agent | 1.0 | Weight of a batch job. | `config.rs` |
| `TC_ACCEPT_BATCH` | agent | true | Whether the agent accepts batch jobs. | `config.rs` |
| `TC_BATCH_HEADROOM` | agent | 0.0 | Weighted capacity units kept free for playback when admitting batch jobs. | `config.rs` |
| `TC_INPUT_ROOTS` | agent | `/media,/data/media` | Comma-separated roots the allowlist accepts for inputs. | `config.rs`, `crates/ir/src/validate.rs` |
| `TC_READ_ROOTS` | agent | `/config/data/data/subtitles,/config/data/data/attachments` | Extra read-only roots for subtitles and fonts. | `config.rs`, `crates/ir/src/validate.rs` |
| `TC_OUTPUT_ROOT` | agent | `/transcodes` | The only root the allowlist accepts for HLS outputs. | `config.rs`, `crates/ir/src/validate.rs` |
| `TC_TRICKPLAY_OUTPUT_ROOT` | agent, shim | unset | Root for trickplay frames. Unset means the agent refuses every trickplay job. | `config.rs`, `crates/shim/src/main.rs` |
| `TC_DETACH` | agent | true | Detach a job when its shim disappears, so another replica can take over. Part of the shared transcode directory design. | `config.rs` |
| `TC_ORPHAN_IDLE_SECS` | agent | 60 | A detached job ends after this long without a keepalive touch. | `config.rs` |
| `TC_ORPHAN_PAUSED_SECS` | agent | 180 | Same, when the last touch said the client was paused. | `config.rs` |
| `TC_ORPHAN_MAX_SECS` | agent | 21600 (6 h) | Absolute lifetime of a detached job. | `config.rs` |
| `TC_ORPHAN_LEAD_MAX_SECS` | agent | 60 | Throttle a detached job when it runs this far ahead of the viewer. | `config.rs` |
| `TC_ORPHAN_LEAD_RESUME_SECS` | agent | 30 | Resume when the lead falls to this value. | `config.rs` |
| `TC_ORPHAN_POS_STALE_SECS` | agent | 60 | A viewer position older than this is treated as stale. | `config.rs` |

Boolean variables read through `env_bool` in `config.rs` (not `TC_HW_FILTERS`, which is off only for `0`) are off for `0`, `false`, `no`, and `off` (case-insensitive); any other non-empty value is on, and unset or empty gives the default.

`TC_LOG=/dev/stdout` appears in `transcode/deploy/Containerfile.agent`. From code reading: a grep over `transcode/crates` finds no reader for `TC_LOG`.

### TLS variables (agent, shim, sync)

All three binaries read the same variables through `transcode/crates/proto/src/tls.rs`.

| Variable | Applies to | Default | Effect | Source file |
| --- | --- | --- | --- | --- |
| `TC_TLS_CERT` | agent, shim, sync | unset | PEM certificate path. | `crates/proto/src/tls.rs` |
| `TC_TLS_KEY` | agent, shim, sync | unset | PEM key path. | `crates/proto/src/tls.rs` |
| `TC_TLS_CA` | agent, shim, sync | unset | PEM CA path. The agent requires client certificates signed by this CA. | `crates/proto/src/tls.rs` |
| `TC_TLS_REQUIRED` | agent, shim, sync | unset | Any non-empty value other than `0` makes missing `TC_TLS_*` a startup error (agent exits with code 2); `0` allows plaintext. The manifests set it. | `crates/proto/src/tls.rs`, `transcode/deploy/CONTRACT.md` |
| `TC_TLS_SERVER_NAME` | shim, sync (client side) | `tcpool-agent` | Server name the client verifies against the agent certificate. | `crates/proto/src/tls.rs` |

Set all three of cert, key, and CA, or none. Setting only some is a startup error. With none set and `TC_TLS_REQUIRED` unset or `0`, the agent serves plaintext with [mTLS](architecture.md#glossary) off.

## tcpool-shim

**Status:** Implemented.

The shim replaces Jellyfin's ffmpeg binary. It sends HLS transcodes to a worker and runs the real ffmpeg for everything else. Source: `transcode/crates/shim/src/main.rs` and `transcode/crates/shim/src/affinity.rs`.

| Variable | Applies to | Default | Effect | Source file |
| --- | --- | --- | --- | --- |
| `TC_WORKERS_DNS` | shim | unset | `host[:port]` of the headless Service. Each A or AAAA record is one worker. Port defaults to 9901. The lookup makes up to 3 attempts, 300 ms apart. Keep the trailing dot, as in `tcpool-agents.media.svc.cluster.local.:9901`, so the resolver skips the search list (`transcode/deploy/CONTRACT.md`). | `crates/shim/src/main.rs` |
| `TC_WORKERS` | shim | unset | Static list `name=host:port,...`, merged with DNS results. | `crates/shim/src/main.rs` |
| `TC_FFMPEG_REAL` | shim | `/usr/lib/jellyfin-ffmpeg/ffmpeg.real` | Real ffmpeg the shim runs locally. Must exist. | `crates/shim/src/main.rs` |
| `TC_SHIM_LOG` | shim | `/config/log/tc-shim.log` | Shim log file. | `crates/shim/src/main.rs` |
| `TC_BATCH` | shim | unset (off) | `1` sends trickplay extraction to the pool. Needs `TC_TRICKPLAY_OUTPUT_ROOT` set as well. Implemented, opt-in, off by default. | `crates/shim/src/main.rs` |
| `TC_AFFINITY` | shim | on | `0` disables seek affinity (no read, write, or clear of the `<md5>.worker` file). | `crates/shim/src/affinity.rs` |
| `TC_AFFINITY_TTL_SECS` | shim | 21600 (6 h) | Lifetime of an affinity file. Unset or unparsable values give the default. | `crates/shim/src/affinity.rs` |
| `TC_TLS_*` | shim | see TLS table | Client certificate and CA for the gRPC connection. | `crates/proto/src/tls.rs` |

### Dolby Vision marker

When the Jellyfin decision patch gates a job, it adds the argv pair `-metadata:s:v:0 JELLYMESH_DOVI_P7_TO_81=1` on the output video stream. The pool also accepts `-metadata:s:v:0 TC_DV81=1` as an alias (`transcode/crates/ir/src/lib.rs`). This is an ffmpeg argument, not an environment variable. No agent environment variable for it appears in `config.rs`.

## tcpool-sync

**Status:** Implemented.

`tcpool-sync` intersects worker outputs and sets Jellyfin's HEVC and AV1 offers to the lowest common denominator. Source: `transcode/crates/sync/src/main.rs`.

| Variable | Applies to | Default | Effect | Source file |
| --- | --- | --- | --- | --- |
| `JF_URL` | sync | empty in code; `transcode/deploy/CONTRACT.md` documents `http://127.0.0.1:8096` | Jellyfin base URL, or a comma-separated list with one entry per replica. | `crates/sync/src/main.rs` |
| `JF_API_KEY` | sync | empty | Jellyfin API key, sent as `MediaBrowser Token="..."`. | `crates/sync/src/main.rs` |
| `TC_CAPS_FILE` | sync | `/config/tc-mesh-caps.json` | State file. | `crates/sync/src/main.rs` |
| `TC_SYNC_EVERY` | sync | 30 | Seconds between sync rounds. | `crates/sync/src/main.rs` |
| `TC_STARTUP_GRACE` | sync | 120 | Seconds after start before sync acts. | `crates/sync/src/main.rs` |
| `TC_TRANSCODE_DIR` | sync | unset | When set and non-empty, sync keeps a `.jellyfin-transcode` marker in this directory. `transcode/deploy/CONTRACT.md` gives `/transcodes/jf` as the production value and says sync re-creates the marker because Jellyfin wipes it at startup. | `crates/sync/src/main.rs` |
| `TC_SYNC_ONCE` | sync | unset | When set (any value), sync runs one round and exits. | `crates/sync/src/main.rs` |
| `TC_METRICS_PORT` | sync | unset (no server) | Serves Prometheus `/metrics` and `/status` JSON on one port. Same variable name as in the agent, separate process. The Service in `30-service.yaml` uses 9904. | `crates/sync/src/main.rs`, `crates/sync/src/metrics.rs` |
| `TC_WORKERS_DNS`, `TC_WORKERS` | sync | unset | Same meaning as for the shim. | `crates/sync/src/main.rs` |
| `TC_TLS_*` | sync | see TLS table | Same as above. | `crates/proto/src/tls.rs` |

## Build-time and lab variables

| Variable | Applies to | Default | Effect | Source file |
| --- | --- | --- | --- | --- |
| `JF_SRC` | `jellyfin-perf/build.sh` | `$HOME/.cache/jellymesh-vendor/jellyfin-src` | Jellyfin checkout (tag `v12.1`). | `jellyfin-perf/build.sh` |
| `JF_OVERLAY` | `jellyfin-perf/build.sh` | `$HOME/.cache/jellymesh-vendor/jellyfin-perf` | Output directory for the seven changed assemblies. | `jellyfin-perf/build.sh` |
| `BUGHUNT` | `jellyfin-perf/build.sh` | 1 | `0` applies only the perf patch; otherwise all `bughunt/NN-*.patch` files apply in numeric order. | `jellyfin-perf/build.sh` |
| `POMELO_SRC` | `galera/pomelo/build.sh` | `$HOME/.cache/jellymesh-vendor/pomelo-src` | Pomelo checkout. | `galera/pomelo/build.sh` |
| `POMELO_OUT` | `galera/pomelo/build.sh` | `$HOME/.cache/jellymesh-vendor/pomelo` | Patched Pomelo output. | `galera/pomelo/build.sh` |
| `PomeloBin` | Galera provider csproj (MSBuild property) | `$(HOME)/.cache/jellymesh-vendor/pomelo` | Directory holding `Pomelo.EntityFrameworkCore.MySql.dll`. | `galera/Jellyfin.Database.Providers.Galera/Jellyfin.Database.Providers.Galera.csproj` |
| `JellyfinBin` | dbmigrate csproj (MSBuild property) | `$(HOME)/.cache/jellymesh-vendor` | Directory holding `Jellyfin.Database.Providers.Sqlite.dll`. | `galera/Jellyfin.DbMigrate/Jellyfin.DbMigrate.csproj` |
| `JmDesign` | Galera provider csproj | unset | `true` keeps host assemblies in the output so `dotnet ef migrations add` can run. | `galera/Jellyfin.Database.Providers.Galera/Jellyfin.Database.Providers.Galera.csproj` |
| `TC_BUILD` | `transcode/deploy/build-musl.sh` | `container` | `host` builds with the host cargo and musl-gcc. | `transcode/deploy/build-musl.sh` |
| `TC_BUILDER_IMAGE` | `transcode/deploy/build-musl.sh` | `docker.io/library/rust:1-bookworm` | Builder image. | `transcode/deploy/build-musl.sh` |
| `TC_REGISTRY` | `transcode/deploy/build-images.sh` | `ghcr.io/saabstory404` | Image registry prefix. | `transcode/deploy/build-images.sh` |
| `GL_ROOTPW` | `galera/lab/galera-lab.sh`, `galera/tools/digest_profile.sh` | `labroot` (lab script) | Lab MySQL root password. | `galera/lab/galera-lab.sh` |
| `GL_CERTS` | `galera/lab/galera-lab.sh` | `$HOME/.cache/galera-lab/certs` | Lab TLS certificates. | `galera/lab/galera-lab.sh` |
| `JG_LAB`, `JG_SRC`, `JG_PLUGIN`, `JG_IMG`, `JG_OVERLAY`, `JG_PW_ENV`, `JG_SHARED`, `JG_SQLITE` | `galera/lab/jf-galera.sh` | `JG_LAB` `$HOME/.cache/galera-lab`; `JG_SRC` `$HOME/.cache/dbsidecar/s`; `JG_IMG` `ghcr.io/hotio/jellyfin:release-12.1`; others unset | Lab node settings. `JG_PLUGIN` is required. `JG_PW_ENV=1` passes the password as `JELLYMESH_DB_PASSWORD`; `JG_SHARED=1` sets `JELLYFIN_SHARED_DB=1`. | `galera/lab/jf-galera.sh` |

`JG_MESH`, `JG_REDIS`, and `JG_RC` in the same script drive the legacy mesh plugin described above.

## Ports

| Port | Protocol | Component | Notes | Source file |
| --- | --- | --- | --- | --- |
| 9901 | gRPC over mTLS | agent | Default of `TC_PORT`. Discovery uses the headless Service `tcpool-agents`. | `crates/agent/src/config.rs`, `transcode/deploy/k8s/30-service.yaml` |
| 9902 | plaintext gRPC health | agent | Kubelet gRPC probes cannot speak TLS. Serves health only. | `crates/agent/src/main.rs`, `transcode/deploy/k8s/20-agents.yaml` |
| 9903 | HTTP | agent metrics | Set by `TC_METRICS_PORT` in the manifests. | `transcode/deploy/k8s/20-agents.yaml` |
| 9904 | HTTP | sync metrics and `/status` | Named `sync-metrics` in the `tcpool-sync-metrics` Service. | `transcode/deploy/k8s/30-service.yaml` |
| `1330<n>` (13301 for node 1, 13302 for node 2) | MySQL | Galera lab node n | Host port from the `podman run` line. The script's header comment says 13306 for the first node; the code does not match it. Jellyfin's own port (8096) and production MySQL (3306) are not covered on this page. | `galera/lab/galera-lab.sh` |

## Kubernetes objects in transcode/deploy/k8s

Apply the files in numeric order.

**Status:** Implemented. Whether the pool as a whole runs in production is not documented yet; only Dolby Vision 7 -> 8.1 is reported as Production (maintainer report, 2026-09-29).

| File | Object | Key values | Source file |
| --- | --- | --- | --- |
| `00-namespace.yaml` | Namespace | Namespace `media` is used by the other files. | `transcode/deploy/k8s/` |
| `10-tls.yaml` | cert-manager Issuer and Certificates | Self-signed issuer, pool CA (RSA 2048, duration 8760h, renew 720h), agent and client leaf certificates (RSA 2048, 2160h = 90 days, renew 720h = 30 days). | `10-tls.yaml` |
| `15-scratch.yaml` | PV `transcode-scratch-media`, PVC `transcode-scratch` in namespace `media` | 200Gi, ReadWriteMany, NFS path `/mnt/pool/transcode-scratch`, `persistentVolumeReclaimPolicy: Retain`, `storageClassName: ""`, PVC bound by `volumeName`. Mount options `nfsvers=4.2`, `lookupcache=positive`, `actimeo=1` are required: with NFS defaults a new segment stayed invisible to the other node for 12-23 s (manifest comment). 200Gi is the claim size; the quota lives on the NFS export. `192.0.2.120` is a placeholder address: replace it. | `15-scratch.yaml` |
| `20-agents.yaml` | Three DaemonSets (qsv, nvenc, cpu) | Manifest values for the maintainer's hardware, not recommendations: `TC_CAPACITY` 14 and `TC_WEIGHT_4K` 2.3 (qsv, Arc), 6 and 2 (nvenc, P4), 3 and 3 (cpu). Selectors: `intel.feature.node.kubernetes.io/gpu` (qsv); `nvidia.com/gpu.present` plus a request for `nvidia.com/gpu: 1` (nvenc); `kubernetes.io/hostname: dl380` (cpu; cluster-specific, check before applying). The cpu DaemonSet sets `TC_HW_FILTERS=0`. All three set `TC_INPUT_ROOTS=/data/media` (the code default is `/media,/data/media`), `TC_HEALTH_PORT=9902`, `TC_METRICS_PORT=9903`, `prometheus.io` scrape annotations, and pod `fsGroup: 1000`. Image tag placeholder `GIT_SHA`; `terminationGracePeriodSeconds` 30; TLS required. | `20-agents.yaml` |
| `30-service.yaml` | Services | Headless `tcpool-agents` (ports 9901-9903) and `tcpool-sync-metrics` (9904). | `30-service.yaml` |
| `40-pdb.yaml` | Three PodDisruptionBudgets | `maxUnavailable: 1` per class. | `40-pdb.yaml` |
| `50-rbac.yaml` | ServiceAccounts | Service accounts only, no Role. | `50-rbac.yaml` |
| `60-servicemonitor.yaml` | ServiceMonitors | Optional. Needs Prometheus Operator CRDs (`monitoring.coreos.com`); applying it without them fails. Use one of three scrape mechanisms: the `prometheus.io` annotations already on the agent pod templates (port 9903, `/metrics`), a vmagent pod-SD job, or these ServiceMonitors. Two at once double every `rate()`. The manifest refers to `deploy/k8s/README.md`, which does not exist yet. The sync ServiceMonitor has no endpoints until the label and named port from `k8s/jellyfin-patch.md` land. | `60-servicemonitor.yaml` |

Operational details from `transcode/deploy/CONTRACT.md` and the manifests:

- Secrets: `tcpool-ca`, `tcpool-agent-tls` (90 d duration, 30 d renew), `tcpool-client-tls`, and `tcpool-sync` with key `api-key`, which is created out of band.
- Mounts the agent needs: media read-only at `/data/media`, `/transcodes` read-write, `/tls` with `defaultMode: 0440`, and an `/tmp` emptyDir for the CUDA JIT cache.
- Set `fsGroup: 1000` on the pod. Without it the agent exits with code 2, and the shim logs `tls misconfigured ... running LOCALLY` and CPU-encodes without notice.
- The headless Service leaves `publishNotReadyAddresses` unset on purpose, so a draining agent drops out of DNS.
- The agent drains itself when the mtime of the certificate files changes. CA rotation requires deleting the two leaf Secrets.

The Jellyfin StatefulSet, Traefik failover route, and Lease RBAC manifest are not in the repository. Their names and settings appear only as prose in [direct-play-failover.md](engineering/direct-play-failover.md); the verbs the Lease needs are listed in the [Leader plugin](#leader-plugin) section. `transcode/deploy/CONTRACT.md` points at `k8s/jellyfin-patch.md`, which is also absent.

## Metrics and alerts

Agent metrics (`transcode/crates/agent/src/metrics.rs`): `tcpool_capacity_units`, `tcpool_units_used`, `tcpool_jobs_active`, `tcpool_batch_jobs_active`, `tcpool_batch_units_used`, `tcpool_batch_headroom_units`, `tcpool_jobs_total{outcome}`, `tcpool_job_seconds`, `tcpool_job_speed`, `tcpool_probe_output`, `tcpool_gpu_tonemap`, `tcpool_draining`, `tcpool_orphans_paused`, `tcpool_dv81_total{outcome}`, `tcpool_build_info`. The `outcome` label of `tcpool_jobs_total` takes `accepted`, `busy`, `refused_policy`, `exit_ok`, `exit_error`, `fenced`, `stalled`, `drained`, `gpu_filter_fallback`, `busy_headroom`, `preempted`, `batch_accepted`, `detached`, `taken_over`, or `orphan_expired` (`metrics.rs`). Types, other labels, and descriptions for each metric are Not documented yet. The `outcome` label of `tcpool_dv81_total` takes `converted`, `fallback_no_rpu`, `fallback_not_p7`, or `fallback_error`.

Sync metrics (`transcode/crates/sync/src/metrics.rs`): `tcpool_pool_worker_live`, `tcpool_pool_workers_configured`, `tcpool_pool_workers_live`, `tcpool_pool_capacity_units`, `tcpool_pool_common_output`, `tcpool_pool_survivable`, `tcpool_jellyfin_offer`, `tcpool_jellyfin_offer_intent`, `tcpool_sync_last_success_timestamp_seconds`.

Alerts in `transcode/deploy/alerts.yaml`, all with severity `warning`. The file is a rule-group fragment to merge into the `vmalert-rules` ConfigMap, not a standalone apply, and vmalert needs a rollout restart afterward. Durations are written as in the file.

| Alert | Condition | For |
| --- | --- | --- |
| `TranscodePoolNotSurvivable` | `tcpool_pool_survivable == 0` | 10m |
| `TranscodeWorkerDown` | `tcpool_pool_worker_live == 0` | 5m |
| `TranscodeJobsSlow` | `tcpool_job_speed < 1.0` | 60s |
| `TranscodePolicyRefusals` | `increase(tcpool_jobs_total{outcome="refused_policy"}[10m]) > 0` | 0m |
| `TranscodeSyncStale` | `time() - tcpool_sync_last_success_timestamp_seconds > 5 * 60`; also fires when the metric has never been set | 0m |

## Related docs

- [architecture.md](architecture.md): how the parts fit, glossary
- [operations.md](operations.md): images, database setup, plugins, transcode pool, upgrades
- [ROADMAP.md](ROADMAP.md): planned work
- [RESULTS.md](RESULTS.md): measurements
- [dolby-vision.md](dolby-vision.md): enabling and verifying Dolby Vision 7 -> 8.1
- [troubleshooting.md](troubleshooting.md): symptoms, causes, fixes
- [../transcode/README.md](../transcode/README.md): transcode pool component
- [../galera/README.md](../galera/README.md): Galera provider component
- [../jellyfin-perf/README.md](../jellyfin-perf/README.md): patch series component

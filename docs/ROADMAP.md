# Roadmap and status

What JellyMesh does today, what I actually run in production, and what is planned. Use it to decide whether a gap blocks you, or to pick something to contribute. Issue numbers such as #4 refer to the project's issue tracker.

## Status matrix

| Component or feature | Status | Evidence |
|---|---|---|
| Galera database provider (`galera/`) | In the repo, measured in the lab | Provider, Pomelo build, and lab drills in [galera/README.md](../galera/README.md); numbers in [RESULTS.md](RESULTS.md), all from a single workstation with every node on it. |
| `jellyfin-dbmigrate` (SQLite to Galera copy and verify) | In the repo, measured in the lab | [galera/README.md](../galera/README.md); migration timing on a 19,259-item library, single workstation, in [RESULTS.md](RESULTS.md). |
| `jellyfin-perf` patch (batched queries, retried user-data writes) | In the repo, measured in the lab | [jellyfin-perf/README.md](../jellyfin-perf/README.md); per-call latency and statement counts in [RESULTS.md](RESULTS.md). |
| Bughunt patch series 00-19 | In the repo | [jellyfin-perf/README.md](../jellyfin-perf/README.md), [engineering/bughunt.md](engineering/bughunt.md). Patches 03, 13, 16, 17, 18, and 19 are gated by `JELLYMESH_*` flags that are off by default, and patch 15 follows `JELLYFIN_SHARED_DB`. The rest are unconditional fixes. Patch 02, for instance, raises the paused-job kill grace from 60 s to 180 s with no flag. |
| Shared-DB mode (`JELLYFIN_SHARED_DB=1`) | In the repo, opt-in and off by default, measured in the lab | [jellyfin-perf/README.md](../jellyfin-perf/README.md); cross-node coherence at the first poll, about 40 ms on a single workstation, in [RESULTS.md](RESULTS.md). |
| Leader plugin (`leader/`, Kubernetes Lease) | In the repo; failover time not measured | `leader/LeaseLeaderService.cs`. Scheduled tasks run once, cluster-wide. Reading the code, failover is bounded by the lease duration (default 15 s) unless the leader stops gracefully; I have not timed it. |
| Shared transcode directory (`JELLYMESH_SHARED_TRANSCODE_DIR=1`) | In the repo, opt-in and off by default, measured in the lab | [transcode/docs/SHARED-TRANSCODE.md](../transcode/docs/SHARED-TRANSCODE.md); one lab drill carried 3 sessions across a pod delete and a `kill -9`, recorded in [RESULTS.md](RESULTS.md). |
| Transcode pool core (`tcpool-shim`, `tcpool-agent`, `tcpool-sync`, `tcpool-ir`, `tcpool-proto`) | In the repo, measured in the lab | [transcode/README.md](../transcode/README.md); CI in `.github/workflows/transcode.yml`. |
| Dolby Vision 7 to 8.1 live conversion | Running in production since 2026-09-29, opt-in and off by default | It is deployed on my cluster and plays as Dolby Vision on the Android TV app on an NVIDIA SHIELD; I have tried it repeatedly. See [dolby-vision.md](dolby-vision.md), and [operations.md](operations.md#enable-dolby-vision-conversion) for turning it on. |
| TrueHD to EAC3 5.1 for the Android TV app | In the repo; measured in the lab 2026-09-29, and in production since 2026-09-29 as part of the Dolby Vision path | Patches 18 and 19 in [engineering/bughunt.md](engineering/bughunt.md). The Dolby Vision path plays on my TV, and row 19 there records this working on the TV. Dolby Digital Plus passthrough from the SHIELD to the AV receiver works too; I checked on 2026-09-30. |
| Traefik active/passive failover | Only drilled in the lab | A direct-play drill with curl, one run, in [engineering/direct-play-failover.md](engineering/direct-play-failover.md); the route itself is described in [operations.md](operations.md#traefik-activepassive-failover). No Traefik or Kubernetes manifests for the route are in this repository. |
| Transcode pool plugin and sync `/status` (read-only dashboard) | In the repo | `transcode/plugin/Api/TcPoolController.cs` (`GET /TcPool/Status`) and `transcode/crates/sync/src/status.rs`. The plan still lists both as unchecked; see the [transcode plan](engineering/transcode-plan.md). |
| Server-side Dolby Vision FEL reconstruction | Not prioritized, an open question | Residual-energy measurement on 4 titles in [engineering/gh15-fel-visibility.md](engineering/gh15-fel-visibility.md): mixed results, 2 of 4 up and 2 of 4 flat or down. The requested vs-nlq and VMAF/PSNR measurement was never run, and issue #15 is still open. See [Known gaps](#known-gaps-and-limitations). |
| Playback observability | Planned | [Playback observability](#playback-observability) |
| Sticky server failover | Planned | [Sticky server failover](#sticky-server-failover) |
| Plugin compatibility layer | Planned | [Plugin compatibility layer](#plugin-compatibility-layer) |
| Transcode pool items (cost model, hedged start, and others) | Planned | [Planned work in the transcode pool](#planned-work-in-the-transcode-pool) |

## Known gaps and limitations

These are found and not fixed. They come before the planned work because they are what decides whether a gap blocks you. They come out of the "found, not fixed" review and the operational notes in [engineering/bughunt.md](engineering/bughunt.md), and each one is something you may need to work around.

| Gap | Effect | Evidence |
|---|---|---|
| Cross-node ping affinity | Progress pings are per node. In a shared-transcode-directory HA pair, a job owner's kill timer can fire while the client is still served through the other replica. It needs session-aware ping routing, or kill-timer ownership that does not depend on which node last saw a ping. Patch 16 adds a shared-directory keepalive; a kill timer that does not depend on which node last saw a ping is not in the series. | Read in the code, plus one lab incident: [engineering/bughunt.md](engineering/bughunt.md) |
| Forgot-password PIN file is node-local | The PIN is written to a local JSON file, not the shared database. The redeem request can land on a node that never had it. | Read in the code: [engineering/bughunt.md](engineering/bughunt.md) (`c1`) |
| `MaxActiveSessions` is per node | The cap is checked against an in-memory dictionary per process, so a user can exceed it by splitting sessions across replicas. | Read in the code: [engineering/bughunt.md](engineering/bughunt.md) (`c2`) |
| Traefik logs `api_key` in cleartext | The query parameter appears in the request-path log field for two lab services that authenticate by query string. Move them to header auth or scrub the field in the log pipeline. A Jellyfin patch cannot fix it. | [engineering/bughunt.md](engineering/bughunt.md), operational notes |
| No `mysqld` or `wsrep` metrics exporter | Galera write stalls (contention, certification aborts, flow control) cannot be measured end to end from the deployment. | [engineering/bughunt.md](engineering/bughunt.md), operational notes |
| Flaky `PlaylistTests` unconfirmed | `PlaylistTests.IsVisible_PlaylistWithOneAllowedItem_StaysVisible` failed in full-suite runs on the patch 13 branch alone, passes standalone, and did not reproduce in 5 full runs on the integrated tree. No patch touches it. | [engineering/bughunt.md](engineering/bughunt.md), operational notes |
| Leader forwarding uses one annotation slot | Two forwarded tasks in quick succession can overwrite each other. The Lease RBAC needs get, create, update, and patch on `leases`. | Read in the code, `leader/LeaseLeaderService.cs`; [architecture.md](architecture.md) |
| Arc loss can stall a failed-over session | Accepted residual risk in the pool plan: with only the two existing cards, an Arc loss at full load can still stall sessions until emergency degrade (P13) is finished. | [engineering/transcode-plan.md](engineering/transcode-plan.md), section 7 |
| Live session state is per server | Now playing, remote control, client capabilities, and running transcodes stay per server. | [architecture.md](architecture.md) |
| Plugins with their own SQLite files | Plugins such as Playback Reporting are not shared-DB safe until the [plugin compatibility layer](#plugin-compatibility-layer) lands. | [Plugin compatibility layer](#plugin-compatibility-layer) |
| `hvcE` sources fall back to HDR10 | A DV7 source with `hvcE` RPU data converts to HDR10 until P1 ships. 0 of 122 profile 7 files in my own library census need it. | [dolby-vision.md](dolby-vision.md); P1, issue #4 |
| FEL enhancement layer is dropped on conversion | FEL sources lose the enhancement layer. Whether reconstruction is worth building is an open question: 4 titles measured with mixed results, vs-nlq and VMAF/PSNR not run, issue #15 left open. The issue's original VMAF 99.88 / PSNR 60.8 dB figures could not be reproduced from any artifact. | [engineering/gh15-fel-visibility.md](engineering/gh15-fel-visibility.md), issue #15 |

No issue tracks the bughunt gaps above; open one before starting work.

## Planned work

Each item states why it exists, what it changes, and what "done" means. Nothing here has a scheduled release.

### Playback observability

**Why.** Diagnosing a stalled or restarted playback currently means correlating Traefik, Jellyfin, ffmpeg, and GPU logs by hand. Stock Jellyfin writes text logs and can expose a generic ASP.NET `/metrics` endpoint. The ffmpeg per-job logs hold speed, fps, and pauses, but they sit on disk uncorrelated with the rest. The 2026-09-27 investigations behind this item aren't written down in this repository.

OpenTelemetry (OTel, see the [glossary](architecture.md#glossary)) auto-instrumentation gives request latency, errors, and database query traces, and it can feed the existing Coroot, VictoriaMetrics, and VictoriaLogs stack. It has no notion of a playback, though, so the value here is domain telemetry.

**What.** One timeline per play session, keyed by Jellyfin's `PlaySessionId`, emitted as structured events with OpenTelemetry (OTel) as the transport.

| Event | Source |
|---|---|
| Playback decision (direct, remux, or transcode) and the reasons Jellyfin computes and discards, such as unsupported audio codec, subtitle burn-in, bitrate cap, or Dolby Vision profile | Jellyfin core patch |
| Transcode lifecycle: server, card, ffmpeg speed and fps over time, throttle pauses, kill timer, and every restart with its cause (seek, failover, worker lost, track change) | Core patch and transcode pool (worker, lease, and affinity are already known there) |
| Delivery: per-segment request latency, time spent waiting for a segment, errors | Core patch or Traefik access logs |
| Viewer experience inferred on the server: time to first frame, stalls (reported position not advancing while wall-clock time does), restarts during a remux (A/V drift risk) | Derived |
| Client-side buffer events and dropped frames, only where the client can report them; other clients rely on server-side inference. | Optional |

Each session closes with a scorecard: time to first frame, stall time, restarts, decision and reasons, server, card, and bitrate. Dashboards per user, device, and title sit on top of the scorecards, and alerts are drawn from them: "session X: 2 restarts during a remux, possible A/V drift", or "client Y always transcodes because of DTS audio".

**Done when** a forced ffmpeg restart appears in the session view with its cause, a session with a stall shows its stall time and the server and card that served it, and an alert rule fires on a session with two restarts during a remux.

### Sticky server failover

**Why.** The high-availability ([HA](architecture.md#glossary)) route is a Traefik failover service with a primary and a fallback. The transcode plan says it sends a failed-over viewer back to the primary as soon as the primary is healthy again, so one failover costs two ffmpeg restarts.

That happened once, on 2026-09-27 ([transcode plan](engineering/transcode-plan.md), sticky-failover item): a Safari [remux](architecture.md#glossary) (video copy, DTS to AAC) restarted at 49:12 on the fallback and again at 52:48 on the primary, and the viewer reported audio drifting out of sync. The plan calls that route "prod's Traefik failover route", and an example of it is in [deploy/examples/traefik-failover.yaml](../deploy/examples/traefik-failover.yaml). A remux restart resumes video on a source [keyframe](architecture.md#glossary) and audio at the exact second, which may be what caused the drift (the plan notes "remux restarts can drift A/V"), but nothing establishes that for this report.

**What.** New sessions prefer the primary. A session that failed over stays where it is until it ends, using a sticky cookie on the route. Prototype and drill it in the lab first, including an audio and video start-[PTS](architecture.md#glossary) check after every forced restart.

**Done when** a primary restart during playback costs at most one transcode restart, and the drill shows no A/V offset growth.

Until this lands:

- Direct play: in one curl drill ([direct-play failover report](engineering/direct-play-failover.md)), a client that retried with an HTTP Range request resumed on the fallback and a plain curl did not. I haven't measured how real clients behave (Android TV, Moonfin, Swiftfin, Infuse); the report's client table comes from public trackers.
- HLS transcode: the two restarts come from the 2026-09-27 Safari observation above. [architecture.md](architecture.md#failover-behavior) lists a controlled Traefik failover of an HLS transcode session as not measured.

### Plugin compatibility layer

Scope: high availability for unmodified third-party plugins.

**Why.** Two Jellyfin servers share one config tree, and third-party plugins were written for one process. Some keep their own SQLite or LiteDB files. The ones I've looked at keep settings in XML that each process caches. Some run background work. Installs write DLLs that the other server has loaded.

Today the mitigation is to block stateful plugins on the fallback server. Intro Skipper, Playback Reporting, and Kodi Sync Queue are blocked there, which pauses their features during a failover. The goal is plugins that work unmodified.

**What.** Handle each kind of plugin state where JellyMesh already has control, so plugins keep working unmodified.

| Plugin state | Owner | Mechanism | Status |
|---|---|---|---|
| Scheduled tasks | Core task manager | The Leader plugin runs each task once, cluster-wide. | Done |
| Settings XML | Jellyfin core (`BasePlugin.SaveConfiguration`) | Core patch: store plugin configs in the shared database, and signal the other server to reload on change. | Planned |
| Background services and startup hooks | Core registration | Core hook: services of plugins classified as stateful run on the leader only. | Planned |
| Install and update | Core installer | Core patch: stage, swap atomically, and restart the other server in turn. | Planned |
| The plugin's own SQLite file | The plugin | Replicate the file, not the plugin (details below the table). | Planned |
| In-memory state (login flows, queues) | The plugin process | Mitigation only: routing with sticky sessions, moving only on failure. The root cause needs plugin changes and is out of scope. | Planned, mitigation only |

For the SQLite row, a replicated SQLite filesystem layer ([LiteFS](architecture.md#glossary), say) over each plugin's data folder gives one writer with live read-only copies elsewhere. The primary is tied to the Leader [Lease](architecture.md#glossary). The few writes on a non-primary, such as playback logging, are queued and replayed. LiteFS is an example, not a commitment.

A per-plugin compatibility table decides the treatment. It records whether a plugin has its own database, runs background services, or reloads its config live, and most of those fields are detected from the plugin folder. Unknown plugins default to primary-only, so HA degrades a plugin's features instead of corrupting its data.

**Done when** Intro Skipper, Playback Reporting, and Kodi Sync Queue work on both servers without modification, a settings change on one server reaches the other without a restart, and a plugin install never breaks the server that has the old DLLs loaded.

### Planned work in the transcode pool

Scope: the unchecked items in [engineering/transcode-plan.md](engineering/transcode-plan.md). One of them, P13, is partly implemented. On 2026-09-30 I searched `transcode/crates/*/src` for EWMA, hedge, power-of-two, and a shim ffmpeg major-version comparison and found no matches; I did not search for the other items by name. The table below is a subset of the plan's unchecked items, and the rest follow it.

| ID | Item | What it changes | Tracking |
|---|---|---|---|
| P1 | `hvcE` RPU reader | Read the Dolby Vision RPU from a Matroska Block Addition (mapping type `hvcE`), so sources that carry it convert instead of falling back to HDR10. In a census of my own library, 0 of 122 profile 7 files need it (ffprobe side-data and NAL scan on production jellyfin-qsv, 2026-09-27, in the plan). | Issue #4 |
| P2 | Online cost model, quality-aware placement, power-of-two choices | Place jobs from an [EWMA](architecture.md#glossary) of measured unpaused speed and a quality score, and pick between two candidate workers. | Plan, cost-model item |
| P3 | Hedged start | If no first segment arrives after about 4 s, start a second attempt on another worker and keep the first to finish. | Plan, hedged-start item |
| P4 | Probe cache, readahead, FS-Cache trial | Reduce source-read cost. A cold-cache latency run is still owed. | Plan, readahead item |
| P5 | Preset scaling by headroom | Choose the encoder preset from spare capacity. Concurrency at `medium` (QSV) and `p5` (NVENC) is not measured. | Plan, preset-scaling item |
| P6 | HEVC 10-bit verification | Verify HEVC 10-bit end to end through both GPU chains. | Plan, 10-bit item |
| P7 | Chaos CronJob and Kuma monitor | Run weekly drills against a canary and report to a push monitor. | Plan, chaos item |
| P8 | Remaining native drills | Freeze (SIGSTOP), network partition, and a `kubectl drain` of a GPU node during 3 concurrent sessions, with 0 failed requests as the target. | Plan, drill items |
| P9 | Sync intent versus effective | Report the difference between the intended and effective HEVC and AV1 offers, pin the hardware acceleration setting and transcode path, and alert on drift. | Plan, sync item |
| P10 | Plugin drain and intent controls | The plugin page is read-only today (see the matrix). Controls are the next step in the plan. | Plan, plugin item |
| P11 | Arc tone-map honoring the UI algorithm | The Arc path uses a fixed `tonemap_vaapi` curve and ignores the UI algorithm, peak, and desaturation settings. Either honor them through the OpenCL route or document the fixed curve. | Plan, Arc tonemap item |
| P12 | Shim ffmpeg major-version mismatch refusal | Refuse to run when the shim and the worker ffmpeg major versions differ. | Plan, version item |
| P13 | Emergency-degrade gauge (partly implemented) | Emergency degrade is marked in progress in the plan. The sync `/status` JSON reserves an `emergency` field that is null, and the agent exports no emergency gauge. The comment at the top of `transcode/crates/sync/src/status.rs` says the agent publishes it on `:9903`; that comment is stale. | Plan, section 7 |

Other unchecked plan items not in the table:

- Rollout: agents in namespace `media` with TLS, a 7-day canary Jellyfin with 0 pool-attributable failures, a GPU-less JellyMesh Deployment with `jellyfin-qsv` and `jellyfin-nvenc` kept at replicas 0, and alert rules, Kuma monitors, and a runbook.
- Tower uplink: 1 GbE saturates on a remux probe; a faster link or a bond is the listed option.
- Trickplay resume from the high-water mark, which the plan lists with four documented hazards.

**Done when** each item ships with a test or a lab drill and an entry in [RESULTS.md](RESULTS.md). The `hvcE` reader is done when a source with `hvcE` RPU data converts to profile 8.1. Per-item acceptance criteria are the checklist entries in the plan.

## Contributing

These items are open for help. [../CONTRIBUTING.md](../CONTRIBUTING.md) has the build and test commands and the documentation rules.

| Item | Size | Good first step |
|---|---|---|
| Transcode pool items (drills, preset scaling, 10-bit verification, shim version refusal) | S to M | Pick one row (P1 to P13) from the [pool table](#planned-work-in-the-transcode-pool) and add a test or a lab drill with its result. |
| `hvcE` RPU reader | M | Start from issue #4 and `transcode/crates/agent/src/dv81.rs`. |
| Sticky server failover | M | Prototype the cookie on the route in a lab and include the A/V start-PTS check. |
| Known gaps | M | Ping affinity and the PIN file are code changes in the Jellyfin patch series (`jellyfin-perf/bughunt/`). |
| Documentation | S | The [open documentation gaps](../CONTRIBUTING.md#open-documentation-gaps) in CONTRIBUTING.md. |

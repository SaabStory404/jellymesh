# JellyMesh roadmap (future releases)

Items agreed but not scheduled. Each one says why it exists and what "done" means.

## Playback observability (next after the transcode pool ships)

**Why.** Jellyfin tells you *what* was watched, never *why* a playback went bad. Built in: text logs
and an optional generic ASP.NET `/metrics` endpoint. Playback Reporting / Jellystat: who watched
what, when, and direct-play vs transcode — no start time, stalls, restarts or reason codes. ffmpeg's
per-job logs have speed/fps/pauses but sit on disk uncorrelated. MEASURED cost, 2026-09-27: one
"choppy playback" report took six parallel investigators and half an hour stitching Traefik,
Jellyfin, ffmpeg and GPU data to learn the client stopped pulling segments; one "audio out of sync"
report took two log searches across two servers to find two transcode restarts. Both should have
been one row in a dashboard.

**Why not just APM.** OpenTelemetry auto-instrumentation gives request latency, errors and DB query
traces — worth having (and it feeds the existing Coroot / VictoriaMetrics / VictoriaLogs stack), but
it knows nothing about playback. The value is domain telemetry.

**What.** One timeline per play session, keyed by Jellyfin's `PlaySessionId`, emitted as structured
events (OTel as transport):

| Event | Source |
|---|---|
| Playback decision (direct / remux / transcode) **and the reasons** Jellyfin computes and discards (unsupported audio codec, subtitle burn-in, bitrate cap, DV profile, ...) | core patch |
| Transcode lifecycle: server, card, ffmpeg speed/fps over time, throttle pauses, kill timer, **every restart and its cause** (seek, failover, worker lost, track change) | core patch + transcode pool (worker, lease, affinity already known) |
| Delivery: per-segment request latency, time spent waiting for a segment, errors | core patch / Traefik access logs |
| Viewer experience inferred server-side: time to first frame, stalls (reported position not advancing while wall-clock time does), restarts during a remux (A/V drift risk) | derived |
| Client-side buffer events / dropped frames where we control the client (a small jellyfin-web script); Moonfin/Kodi rely on server-side inference | optional |

Each session closes with a **scorecard** (time to first frame, stall time, restarts, decision +
reasons, server, card, bitrate) → dashboards per user / device / title, and alerts such as
"session X: 2 restarts during a remux, possible A/V drift" or "client Y always transcodes because of
DTS audio".

**Done when** any "it played badly" report can be answered from one session view — decision, reasons,
where it transcoded, restarts, stalls — without reading logs, and a bad session raises an alert
before anyone complains.

## Sticky server failover

**Why.** The HA route (a Traefik failover service, primary + fallback) sends a failed-over viewer
back to the primary as soon as it is healthy again. One failover therefore costs two ffmpeg
restarts. MEASURED 2026-09-27: a Safari remux (video copy, DTS -> AAC) restarted on the fallback
and again on the primary three minutes later, and the viewer reported audio drifting out of sync.

**What.** New sessions prefer the primary; a session that failed over stays where it is until it
ends (sticky cookie on the route). Prototype and drill in the lab first, including an audio/video
start-PTS check after every forced restart.

**Done when** a primary restart during playback costs at most one transcode restart and the drill
shows no A/V offset growth.

## Plugin compatibility layer (HA for unmodified third-party plugins)

**Why.** Two Jellyfin servers share one config tree. Third-party plugins were written for a single
process: some keep their own SQLite/LiteDB files, all keep settings in XML that each process caches,
some run background work, and installs write DLLs the other server has loaded. Today the only safe
answer is to block stateful plugins on the fallback (Intro Skipper, Playback Reporting and Kodi Sync
Queue are blocked there), which pauses their features during a failover. Plugin authors should not
have to know about HA.

**What.** Handle each kind of plugin state where JellyMesh already has control, so plugins keep
working unmodified:

| Plugin state | Owner | Mechanism |
|---|---|---|
| Settings XML | Jellyfin core (`BasePlugin.SaveConfiguration`) | core patch: store plugin configs in the shared database, signal the other server to reload on change |
| Scheduled tasks | core task manager | done: the Leader plugin runs each task once, cluster-wide |
| Background services / startup hooks | core registration | core hook: services of plugins classified stateful run on the leader only |
| Install / update | core installer | core patch: stage, swap atomically, restart the other server in turn |
| The plugin's own SQLite file | the plugin | replicate the file, not the plugin: a replicated SQLite filesystem layer (e.g. LiteFS) over each plugin's data folder, one writer with live read-only copies elsewhere, primary tied to the Leader Lease; queue/replay the few writes that happen on a non-primary (e.g. playback logging) |
| In-memory state (login flows, queues) | the plugin process | not transparently fixable; handled by routing (sticky sessions, move only on failure) |

A per-plugin compatibility table (own DB? background services? live config reload?), mostly
auto-detected from the plugin folder, decides the treatment. Unknown plugins default to
primary-only, so HA degrades a plugin's features instead of corrupting its data.

**Done when** Intro Skipper, Playback Reporting and Kodi Sync Queue work on both servers without
modification, a settings change on one server reaches the other without a restart, and a plugin
install never breaks the server that has the old DLLs loaded.

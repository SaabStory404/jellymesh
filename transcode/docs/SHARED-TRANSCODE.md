# Shared transcode directory across Jellyfin replicas

Status: implemented behind `JELLYMESH_SHARED_TRANSCODE_DIR=1` (Jellyfin, bughunt patch 14) and an
additive protocol field (pool). Off by default; with the flag unset both sides behave exactly as before.

## Goal

Both HA replicas (primary + fallback behind the Traefik failover route, shared Galera DB, Leader
plugin) use **one** transcode directory. When the serving replica dies or drains mid-stream, the
other replica keeps serving the *same* HLS output: no ffmpeg restart, no slow segment. Before this,
each replica had its own subdirectory and a failover meant a fresh transcode on the survivor.

## What was in the way

| Problem | Where | Fix |
|---|---|---|
| The pool agent fenced (killed) a job the moment its shim's connection closed, so the job died with the replica that started it | `agent/job.rs` | **Detach**: a job whose shim sent a keepalive path keeps running without its shim |
| The lease (`<stem>.tcpool.lock`) was heartbeated only by the shim, so it went stale with the replica | shim `lease.rs` | the agent heartbeats it too, from job start |
| Progress pings are per node: the owner's kill timer fired while the client pinged the other replica (MEASURED in jm-lab, BUGHUNT follow-up 1) | `TranscodeManager` | pings and segment requests on **any** replica touch a shared session keepalive; the owner's kill timer re-arms while it is fresh |
| A seek on the non-owner waited 15 s for a segment the other writer would never produce, then started a second ffmpeg that could only `follow()` the lease forever | `DynamicHlsController` (patch 03), shim `follow()` | seek detection against the other writer's edge (Jellyfin's own restart gap); the new shim **takes the output over** |
| Session-derived output names are identical on both replicas, so the old owner's cleanup could delete the new writer's segments | `DeletePartialStreamFiles` | skip when a fresh lease on the output is held by another host |
| Startup wipe of the transcode dir | `DeleteEncodedMediaCache` | already scoped (6 h cutoff) under `JELLYFIN_SHARED_DB=1`; now also under the shared-dir flag |

## Ownership: the lease is the single source of truth

One writer per output, as before: the shim that wins `O_EXCL` on `<stem>.tcpool.lock` runs the job;
the token is `host:pid:nanos`. What changes is who keeps the lease alive and how it changes hands:

```
<transcode dir>/<stem>.m3u8, <stem>N.ts       the output (stem = MD5(MediaPath-UA-DeviceId-PlaySessionId))
<transcode dir>/<stem>.tcpool.lock             lease: holder token; heartbeated by the shim AND the agent
<transcode dir>/<stem>.tcpool.takeover         takeover request: the holder token it wants gone
<transcode dir>/<stem>.worker                  seek-affinity pin (unchanged)
<transcode dir>/.jellymesh-alive/<PlaySessionId>   session keepalive: "playing" | "paused", mtime = last seen
```

Jellyfin passes the keepalive path to ffmpeg (the shim) in `JELLYMESH_KEEPALIVE`; the shim sends it
in `Job.keepalive_path` (proto field 4). The agent validates it under `TC_OUTPUT_ROOT`.

## Job states on the agent

```
            shim connected                                 shim gone (FIN, send error, or silent > TC_FENCE_AFTER)
 ATTACHED ----------------------------------------------> DETACHED (only if keepalive_path set, a lease on disk,
    |  (unchanged: stdin keys, stderr, heartbeats)          |         and a 16+ hex-char output stem)
    |                                                       |  ffmpeg keeps writing; stderr drained, not forwarded;
    |                                                       |  a throttler-paused ffmpeg is sent `u`
    |                                                       |
    +-- takeover file names our token --> TAKEN_OVER <------+  kill ffmpeg, free the lease, keep the files
    |                                                       +-- keepalive stale (TC_ORPHAN_IDLE_SECS 60, or
    |                                                       |   TC_ORPHAN_PAUSED_SECS 180 after a paused ping)
    |                                                       |   --> ORPHAN_EXPIRED: kill ffmpeg, delete <stem>*
    |                                                       |       and the keepalive
    |                                                       +-- ffmpeg exits on its own: free the lease, delete
    |                                                           <stem>* once the keepalive goes stale
    +-- (fence on shim loss when not detachable: unchanged)
```

`TC_DETACH=0` turns detaching off; `TC_ORPHAN_MAX_SECS` (6 h) caps a detached job. Metrics:
`tcpool_jobs_total{outcome="detached"|"taken_over"|"orphan_expired"}`. An agent drain (SIGTERM) still
ends detached jobs after their next segment, like any job.

## Cross-replica keepalive (the kill-timer blocker)

- `PingTranscodingJob` (progress/ping, any replica) touches the keepalive **before** the local job
  lookup, preserving the paused state when the ping does not say (`/Sessions/Playing/Ping`).
- `GetDynamicSegment` (any replica) touches it on every segment request.
- The owner's kill timer, when its own pings are older than the timeout, reads the keepalive and
  re-arms for the remaining time (`GetKillTimeoutMs(type, paused-from-file)`: 60 s playing, 180 s
  paused). So a paused session whose pings land on the other replica is not killed.
- Touches are throttled to one write per session per 2 s unless the paused state flips.
- A stop reported to the non-owner does not stop the owner's job at once: it ages out after the
  kill timeout (60 s), which is stock Jellyfin behaviour for a lost stop.

## Serving from either replica

The master and variant playlists are generated from the request, not read from disk, so either
replica serves them. Segments:

1. The segment exists: served from disk by whichever replica got the request (stock path).
2. It does not, and this replica has no *live* local job (none, or an exited one): look at the
   newest segment on disk. If the request is a seek relative to it (behind it, or more than
   `24 / SegmentLength` ahead: Jellyfin's own restart rule) restart at once. Otherwise, if that
   writer is fresh, wait for the segment (patch 03), accepting the segment alone when a fresh pool
   lease exists (pool writers publish with `temp_file`, so a visible segment is complete).
3. A restart on the non-owner starts ffmpeg (the shim). The shim finds the lease held and, because
   it carries a keepalive (shared mode), writes a takeover request naming the holder's token and
   waits up to 8 s for the lease; the holder's agent ends its ffmpeg within a second and frees it.
   A holder that never answers (an agent without this change) falls back to the old `follow()`.
   The old owner's shim (if its replica is alive) gets `Exit.taken_over`, exits 0 and touches
   nothing; its Jellyfin's cleanup then finds the new writer's lease and skips the delete.

## Cleanup and startup

- Jellyfin's per-job delete (`DeletePartialStreamFiles`) is skipped when a fresh lease on that
  output is held by another host. Stale or own leases delete as before.
- `DeleteEncodedMediaCache` keeps files newer than 6 h in shared mode (lease/pin/keepalive files included).
- The agent deletes a detached job's `<stem>*` only when its viewer is gone and nobody else holds the lease.
- `TranscodingSegmentCleaner` (EnableSegmentDeletion) runs only on the owner's job and only
  behind that job's own download position: unchanged.

## Deployment contract

- Both replicas: the same `TranscodingTempPath` (a subdirectory of the scratch mount, never its
  root: CONTRACT.md rule 1), `JELLYMESH_SHARED_TRANSCODE_DIR=1`, and the NFS mount options from
  CONTRACT.md (`actimeo=1` keeps lease and keepalive mtimes visible within ~1 s across nodes).
- `HOSTNAME` must differ between replicas (it does: pod names). Lease tokens and the cleanup guard use it.
- Agents: `TC_OUTPUT_ROOT` must contain the transcode directory (it already does: `/transcodes`).
- Mixed versions: a new shim against an old agent sends a field the old agent ignores (no detach,
  takeover unanswered -> follow after 8 s). An old shim against a new agent never sends a keepalive
  (fence-on-loss, unchanged). Roll agents first, then Jellyfin.

## Not covered

- Trickplay `TC_BATCH` shared-volume wiring: independent of this change, still P3 manifest work.
- A replica that dies *between* taking the lease and the agent's `Accepted` leaves nothing running;
  the survivor's next request finds no fresh writer and restarts (the pre-existing path).
- Local (non-pool) ffmpeg jobs do not detach: they die with their replica and the survivor
  restarts the transcode, as today.

# Direct play across a replica restart/crash (issue #13)

MEASURED 2026-09-29 in jm-lab (2-replica `jm-jf` StatefulSet, `TraefikService` `jm-failover`:
`failover` mode, primary `jm-jf-0`, fallback `jm-jf-1`, `errors.status: 502-504`,
`ServersTransport` `jm-fast-dial` dialTimeout 500ms). Test file: a 26,976,301-byte static FLAC
(any large static direct-play file exercises the same code path as video; auth is not required —
`/Videos/{id}/stream?static=true` answered range requests unauthenticated in this lab). Client:
plain `curl` against the real ingress path (`https://jm-lab.lan` → Traefik LB `10.10.20.200`),
not a synthetic pod, so results reflect the actual proxy hop viewers use.

## What actually breaks

A direct-play response is one long-lived HTTP connection to whichever replica got picked at
request time. Traefik's `failover` TraefikService cannot re-home a response that has already
started — it has no way to splice a half-sent byte stream onto a different backend. This is
inherent to the failover primitive, not a misconfiguration.

Two kill methods were tried:
- **Graceful `kubectl delete pod`** (default 30s `terminationGracePeriodSeconds`): Kestrel drains
  in-flight requests during shutdown. In both graceful-delete trials the entire in-flight download
  (up to 20-30s of streamed bytes at the test rate) completed with **no visible interruption** —
  the connection outlived the graceful-shutdown window. This is a real result, not a test escape:
  a rolling restart of jm-lab is unlikely to cut off an in-flight direct-play stream at all, as
  long as no viewer's connection outlasts the grace period.
- **Hard kill** (`kill -9` on the in-container `jellyfin` PID, s6-supervised — the base image is
  hotio's s6-overlay, so PID 1 is `s6-svscan`/supervisors, not `jellyfin`; killing PID 1 has no
  effect, the real target is the supervised `jellyfin` PID, found via `pgrep -f /usr/bin/jellyfin`):
  the in-flight connection reset immediately. A no-retry client (plain `curl`, no `--retry`/range
  logic) got a partial file (~6.2 MB of 26 MB) and a hard I/O error; it did not resume, matching
  the reported Android TV behaviour in the issue.

Note: `kubectl get pods` RESTARTS did not increment for the in-container SIGKILL — s6 respawns the
process without the container restarting, so RESTARTS is not a reliable crash signal for this
image; use readiness-probe failure events or `ps` inside the pod instead.

## What recovers it — client-side Range retry

A client that, on disconnect, reissues the same URL with `Range: bytes=<last-received>-` recovered
the full file across a hard kill of the primary replica, confirmed via Traefik access logs
(`msg_ServiceName`/`msg_ServiceAddr`, VictoriaLogs `container:"traefik"`):

| Attempt | Service (backend) | Status | Bytes | Notes |
|---|---|---|---|---|
| 1 | `jm-jf-0` (primary, killed at T+11s) | 200 (interrupted) | 8,407,732 of 26,976,301 | connection reset mid-body |
| 2 | `jm-jf-1` (fallback) | 206 | 10,240,000 | `Range: bytes=8407732-`, correct resume offset |
| 3 | `jm-jf-1` | 206 | 8,328,569 | completed the file, byte-identical to source |

The `failover` TraefikService did its job for the *retry*, not the *original request*: jm-jf-0 was
unhealthy (readiness probe failing) by the time the client's Range retry landed, so failover sent
that fresh request straight to jm-jf-1, which correctly served the requested range from the shared
media mount. **This works because direct play is a static file on shared storage** — any replica
can serve any byte range of it identically, so the resume is correct without session affinity or
sticky cookies.

Gap duration (SIGKILL to first byte of resumed stream), from Traefik log timestamps:
kill → client detects the reset: ~5.1s (dominated by how long the client's read call was already
blocked, not by anything server-side); detect → retry issued: ~0.35s (bounded by the test
script's own loop, not the proxy); retry → 206: immediate. Total viewer-visible gap ≈ 5-6s,
essentially all client-side detection latency.

## Recommendation

No proxy-side fix is possible or needed: Traefik cannot resume a half-sent response, and once
jm-jf-0 fails its readiness probe, `failover` already sends every *new* request (including a
client's own retry) to the healthy replica — that is the whole of what a proxy can contribute
here. The gap is closed entirely on the client side, by any client that retries a dropped
direct-play connection with an HTTP `Range` request for the un-received tail. That behaviour
should be documented as a requirement/recommendation for jellymesh viewers, and any client lacking
it (see matrix below) should be flagged as the actual bug target, not the proxy config.

## Client behaviour matrix

MEASURED here: none (this lab test used plain `curl`, not the real clients). Everything below is
**INHERITED** from public issue trackers/docs, not measured against jm-lab, and may be stale or
version-dependent:

| Client | Reconnect-with-range on drop? | Source |
|---|---|---|
| Android TV official app (ExoPlayer/Media3) | ExoPlayer has a configurable `LoadErrorHandlingPolicy` with retry+backoff (`minLoadableRetryCount` default 3) for load errors, which for `DefaultHttpDataSource` includes reopening with the correct byte range; whether the Jellyfin Android TV app's specific config uses this to survive a full backend death (vs surfacing "Player error, will retry... giving up") is not confirmed — multiple open issues describe "will retry... too many errors, giving up" on unrelated playback errors, so behaviour on a real backend-death disconnect is unverified. |
| Moonfin | Documented (its own repo) 3-step reconnect: stall/freeze detection (8s stall or 15s no-first-frame) then reconnect attempts at 4s/10s/20s backoff; on the 3rd attempt it can fall back to direct-stream (remux) instead of direct play. This is the most explicit resume behaviour found in this search. |
| Swiftfin (tvOS/iOS, VLCKit/MPVKit-based playback) | No explicit reconnect-with-range documentation found; a cited report shows Swiftfin (MPVKit) failing to even start playback on high-jitter links where AVFoundation-based clients (official app, Safari) succeed — suggests its HTTP/demux layer is less tolerant of interruption generally, not that it lacks a retry path specifically. |
| Web (jellyfin-web) | Not researched this session. |
| Infuse | No specific interruption-recovery documentation found; commonly used as a fallback player for files Swiftfin can't handle, not evaluated here for resume behaviour. |

## Lab state

jm-lab restored to prior state: `jm-jf` StatefulSet 2/2 ready, both replicas on the pre-test image
(`12.1-jm7.1`), no resources added or left behind. No changes made to jm-lab config, no changes to
prod (`ns media`) at any point.

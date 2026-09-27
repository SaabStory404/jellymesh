# tcpool deployment contract

What the transcode pool publishes and what it needs from whoever runs Jellyfin. Written for the
JellyMesh session (PR #155): everything below is settled on this side, so JellyMesh can build
against it without reading the Rust.

Status 2026-09-27: nothing here is applied to the cluster and no image has been pushed. Manifests
live in `k8s/`, the Jellyfin-side snippets in `k8s/jellyfin-patch.md`.

## Images

| Image | What it is | Built by |
|---|---|---|
| `ghcr.io/saabstory404/tcpool-agent:<git-sha>` | the worker. `FROM ghcr.io/hotio/jellyfin:release-12.1` pinned by digest `sha256:e3a9ba58...`, carrying jellyfin-ffmpeg 8.1.2-Jellyfin. Runs as uid 1000. | `.github/workflows/tcpool-images.yml`, `deploy/Containerfile.agent` |
| `ghcr.io/saabstory404/tcpool-shim:<git-sha>` | artifact only, `FROM scratch`: `/tcpool-shim` and `/tcpool-sync`, static-pie musl. Not runnable. | same workflow, `deploy/Containerfile.shim` |

Tags are the commit sha, never `:latest` — a deploy diff compares manifests, and a mutable tag makes
that diff lie about what is running. Build both locally with `deploy/build-images.sh`.

**Base image must match Jellyfin's.** The agent spawns jellyfin-ffmpeg rather than linking libav*,
and Jellyfin emits filter graphs specific to its own fork. The pin above matches the planned
`ghcr.io/saabstory404/jellymesh-jellyfin:12.1-jm3` (itself `FROM` hotio `release-12.1`). Bump the
two together. **OPEN:** prod Jellyfin today is still `release-12.0`, and the plan's "the shim refuses
a major ffmpeg mismatch" is not implemented — the digest pin is the only guard.

## Ports

| Port | Protocol | Who dials it |
|---|---|---|
| 9901 | gRPC over **mTLS** — `Hello`, `Run` | the shim, `tcpool-sync` |
| 9902 | gRPC health, **plaintext** | the kubelet only. The kubelet's gRPC probe cannot speak TLS, so health is served twice; 9902 exposes health and nothing else. |
| 9903 | `/metrics` — **reserved, not yet served** | nobody. `TC_METRICS_PORT` is set in the manifests and the agent does not read it (plan §2 Observability is open). No ServiceMonitor. |
| 9904 | `/metrics` for `tcpool-sync` — **reserved** | nobody, same reason. |

Discovery is the headless Service `tcpool-agents` in namespace `media`, `clusterIP: None`, port 9901.
The shim resolves its A records on every transcode and treats each as one worker.
`publishNotReadyAddresses` is deliberately unset: a draining agent reports health `NOT_SERVING`,
fails readiness, and leaves DNS within ~3 s, which is what stops a rolling upgrade handing new jobs
to a worker that is shutting down.

## TC_* environment

### Agent (set by `k8s/20-agents.yaml`)

| Variable | Value | Note |
|---|---|---|
| `TC_KIND` | `qsv` \| `nvenc` \| `cpu` | picks the renderer |
| `TC_NAME` | `qsv-$(NODE_NAME)` etc. | what the shim logs. `NODE_NAME` must be defined earlier in the env list for `$(...)` to expand |
| `NODE_NAME` | `fieldRef: spec.nodeName` | |
| `TC_CAPACITY` | Arc `14`, P4 `6`, CPU `3` | resolution-weighted units, a hard admission ceiling. **Must be set**: with neither this nor `TC_MAX_JOBS` the agent defaults to 1000 units, i.e. unbounded |
| `TC_WEIGHT_4K` | Arc `2.3`, P4 `2`, CPU `3` | MEASURED on the cards; the CPU value is an estimate |
| `TC_FFMPEG` | `/usr/lib/jellyfin-ffmpeg/ffmpeg` | image default |
| `TC_HEALTH_PORT` | `9902` | |
| `TC_METRICS_PORT` | `9903` | reserved, see above |
| `TC_TLS_REQUIRED` | `1` | a hard error if the cert vars are missing, so a prod pod can never fall back to plaintext |
| `TC_TLS_CERT` / `TC_TLS_KEY` / `TC_TLS_CA` | `/tls/tls.crt`, `/tls/tls.key`, `/tls/ca.crt` | all three or none |
| `TC_INPUT_ROOTS` | `/data/media` | allowlist: inputs must be under here |
| `TC_OUTPUT_ROOT` | `/transcodes` | allowlist: outputs only under here |
| `TC_READ_ROOTS` | `/config/data/data/subtitles,/config/data/data/attachments` | subtitle burn-in and attachment fonts, read-only |
| `TC_HW_FILTERS` | `0` on the CPU worker only | |
| `TC_PATHMAP` | **unset** | not needed: the agents mount `/data/media` at `/data/media`, the same path prod Jellyfin uses, so the command line's paths are already valid |

### Shim (inside Jellyfin, installed as `/usr/lib/jellyfin-ffmpeg/ffmpeg`)

| Variable | Value |
|---|---|
| `TC_WORKERS_DNS` | `tcpool-agents.media.svc.cluster.local.:9901` — keep the trailing dot; it stops the resolver walking the search list. Port defaults to 9901 if omitted |
| `TC_WORKERS` | unset in prod. Static fallback `name=host:port,...`, merged in for any address DNS did not return |
| `TC_TLS_REQUIRED` | `1` |
| `TC_TLS_CERT` / `TC_TLS_KEY` / `TC_TLS_CA` | `/tls/tls.crt`, `/tls/tls.key`, `/tls/ca.crt` (the **client** secret) |
| `TC_TLS_SERVER_NAME` | leave unset. Defaults to `tcpool-agent`, a SAN on the agent certificate; the shim overrides the domain name with it, which is what lets it dial bare pod IPs |
| `TC_FFMPEG_REAL` | `/usr/lib/jellyfin-ffmpeg/ffmpeg.real` (default) — **must exist.** Everything that is not an HLS transcode execs it, and so does every fallback path |
| `TC_SHIM_LOG` | `/config/log/tc-shim.log` (default) |

### tcpool-sync (sidecar, one replica)

`JF_URL` (`http://127.0.0.1:8096`), `JF_API_KEY` (from the `tcpool-sync` Secret),
`TC_CAPS_FILE` (`/config/tc-mesh-caps.json`), `TC_SYNC_EVERY` (`30`),
`TC_TRANSCODE_DIR` (`/transcodes/jf` — its own subdirectory), and the same `TC_TLS_*` client set.

**OPEN:** `tcpool-sync` reads `TC_WORKERS`, not `TC_WORKERS_DNS` (`crates/sync/src/main.rs`). Either
port the DNS discovery to it or give it an explicit `TC_WORKERS`. Not a playback blocker: a stale
offer set is a quality bug, not a stall.

## Secrets

| Secret | Contents | Created by |
|---|---|---|
| `tcpool-ca` | the pool CA (`tls.crt`, `tls.key`, `ca.crt`) | cert-manager, from the `tcpool-ca` Certificate |
| `tcpool-agent-tls` | agent server identity, SANs `tcpool-agent`, `tcpool-agents`, `tcpool-agents.media.svc.cluster.local`. 90 d, renewed at 30 d | cert-manager |
| `tcpool-client-tls` | client identity for the shim and sync, `client auth` only. 90 d / 30 d | cert-manager |
| `tcpool-sync` | key `api-key`: a Jellyfin API key | **out of band.** Not in this repo and must not be |

Its own CA, not the cluster step-ca: a CA that signs nothing else means any certificate it issued
is by construction a pool identity. Keys are RSA 2048 / PKCS8 with `rotationPolicy: Always`; the
agent watches the three files' mtime and drains itself when they change, so a rotation restarts the
worker instead of cutting a session. Moving to ECDSA is a measured change, not a free one.

cert-manager does **not** reissue leaves when the CA itself rotates, so after a CA rotation the pool
would run on mixed trust for up to 60 days. Rotating the CA therefore means deleting
`tcpool-agent-tls` and `tcpool-client-tls` in the same change.

## Required mounts

Every agent, and any Jellyfin that uses the pool:

| Mount | Source | Mode |
|---|---|---|
| `/data/media` | hostPath `/data/media` | read-only. Same path in and out, so no path mapping |
| `/transcodes` | PVC `transcode-scratch` (RWX, static PV `transcode-scratch-media`, NFS `192.0.2.120:/mnt/pool/transcode-scratch`) | read-write |
| `/config/data/data/subtitles`, `/config/data/data/attachments` | hostPath under `/srv/appdata/media/jellyfin/...` | read-only, agents only (subtitle burn-in) |
| `/tls` | the matching Certificate secret | read-only, `defaultMode: 0440` **and `fsGroup: 1000` on the pod** |
| `/tmp` | emptyDir | read-write. The CUDA JIT cache lives here; without it every ffmpeg recompiled its kernels (~12 s on the P4) |

`fsGroup: 1000` is not optional. A secret volume's files are `root:root` without it, so an agent
running as uid 1000 gets EACCES on its own private key: with `TC_TLS_REQUIRED=1` the agent exits 2
and CrashLoops, and on the Jellyfin side the shim logs `tls misconfigured ... running LOCALLY` and
silently CPU-encodes every session. The kubelet applies `fsGroup` to secret and emptyDir volumes
only - the in-tree NFS PVC and the hostPaths are unmanaged - so there is no recursive chown of the
scratch.

The two Jellyfin data hostPaths use `type: Directory`, not `DirectoryOrCreate`: a missing directory
should fail the pod visibly rather than have the kubelet plant a `root:root` directory inside prod
Jellyfin's own config tree, which Jellyfin (uid 1000) could then never write subtitles into. This
means **both GPU nodes need `/srv/appdata/media/jellyfin/data/data/{subtitles,attachments}` to
exist**. INHERITED, not checked: the placer moves Jellyfin between the tower and the dl380, so that
config path is presumably present (or shared) on both - verify before the first apply.

`storageClassName: ""` on both the PV and the PVC is required, not cosmetic: the cluster has two
StorageClasses both marked default, so an unqualified RWX claim binds non-deterministically.

The scratch `mountOptions` are load-bearing: `nfsvers=4.2, lookupcache=positive, actimeo=1`. With
the defaults a freshly written segment stayed invisible to other nodes for 12–23 s, which is
Jellyfin waiting on a segment that already exists.

## Two rules about the scratch directory

1. **`TranscodingTempPath` must be a subdirectory**, one per Jellyfin identity (e.g.
   `/transcodes/jf`), never the mount root. Jellyfin's `TranscodeManager` wipes its whole transcode
   path at startup, so two Jellyfins sharing the root means restarting either one deletes the
   other's live segments. `TC_OUTPUT_ROOT=/transcodes` allows anything beneath it, so the
   subdirectory needs no agent-side change.
2. **Leave the root's ownership alone.** It is `root:media` (gid 1000) mode `1777`, with
   `.jellyfin-transcode` owned `root:root 0644`, on purpose. Jellyfin runs as uid 1000 and cannot
   delete that marker, so its startup wipe throws, the exception is swallowed, `File.Exists` stays
   true, and the racy marker re-create path — which returned HTTP 500 to two viewers starting at
   once, MEASURED twice — never fires.

## Dolby Vision 7 -> 8.1 (P5, not deployed yet)

For the Jellyfin-side decision patch (bug-hunt session, brief item 5): when your patch decides a
DV profile-7 source should be remuxed as DV 8.1 for the requesting client, append this exactly
once, on the output video stream, to the ffmpeg argv you already emit:

```
-metadata:s:v:0 TC_DV81=1
```

That's the only contract on your side. It's a real, harmless ffmpeg option (arbitrary output
metadata), so nothing breaks if the pool is unreachable and the shim execs your argv unmodified —
the output just carries one extra, inert metadata key. Do not gate on whether the pool is present;
emit the marker whenever your decision says DV7->8.1, unconditionally.

**Pool-side behavior (branch `dv81-wire`, not in an image yet).** An agent built from it acts on
the marker for a video-copy (remux) PLAYBACK job: if ffprobe says the source is DV profile 7 and
the source's video actually carries its RPU in-band (checked on the first bytes of the stream), it
remuxes with the RPU rewritten to profile 8.1 and the enhancement layer dropped; the HLS init
segment then carries a DOVI record `profile: 8 ... compatibility id: 1`. Every other case runs your
argv unchanged minus the marker, i.e. exactly today's remux: a source that is not DV7, a DV7 MKV
whose RPU sits in a Matroska Block Addition (`hvcE` — a common muxing, not converted yet), an argv
without `-copyts` or with more than one `-i`, or any failure before the first segment. Output
paths, segment naming, stdin keys and stderr progress are the same as the plain remux. Counted in
`tcpool_dv81_total{outcome}`. So the badge changes only for in-band-RPU DV7 titles; details and
measurements in `transcode/docs/PLAN.md` P5.

## Rollback

Unchanged and unconditional. The pooled Jellyfin is GPU-less, so `jellyfin-qsv` and
`jellyfin-nvenc` stay defined at `replicas: 0` in `arr-stack/k3s/30-media.yaml` through the canary.
Rolling back = scale one of them back up (via the placer) and set `encoding.xml` `hwaccel` back to
`qsv`. The pool's own objects can stay applied; with no shim pointing at them the agents are idle.

## Kuma

`scripts/kuma-coverage.py --check` scans Ingress hosts and CronJob names only, so these DaemonSets
need no `k8s/kuma/monitors.yaml` entry to pass CI. The plan's dead-man push from `tcpool-sync` is a
deliberate follow-up, not an omission.

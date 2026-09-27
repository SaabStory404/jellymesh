# Transcode pool: production plan

Status: PLAN, 2026-09-26. It builds on the spike in `transcode/spike/` (PR #154). The prod rollout is coordinated with JellyMesh (PR #155, `arr-stack/k3s/jellymesh/COMBINED-ROLLOUT.md`).

The goal is to treat every transcode-capable device as **one pool**:
- load spreads across all of them;
- any device can take over any session without the viewer noticing;
- quality is as high as each device allows;
- the control plane is native code.

## 0. Where performance actually comes from

**The transcoding hot path was never Python.** Every frame goes through jellyfin-ffmpeg (C), and on the GPU chains the frames never leave the GPU. Rewriting the shim, agent and sync in native code buys:
- robustness and a typed protocol;
- tiny static binaries;
- a shim that starts in an estimated 1–3 ms instead of an estimated ~40 ms (neither measured yet; P1 gates on < 5 ms);
- no interpreter in a network-facing process.

It does **not** add frames per second. The real performance levers, in order of size (all MEASURED in the spike unless marked):

| Lever | Effect |
|---|---|
| GPU-resident scale + tone map | Arc 4K HDR 268 vs 29 fps; P4 98 vs 51 fps |
| Spreading load by measured cost (weighted units) | Arc 14× 1080p at ≥1.4× realtime where a flat count wasted ~60% of it |
| Preset scaling by headroom (§4) | slower presets when the pool is idle: better quality at the same bitrate. Faster presets under failover: est. 1.5–2× throughput (to measure) |
| Startup latency (§5) | first segment in 2–9 s today, mostly probing over NFS |
| Emergency degrade on card loss (§7) | keeps an Arc loss from stalling sessions, with only the two existing cards |

## 1. Target architecture

```
Jellyfin replicas (GPU-less, hwaccel=none, JellyMesh)
  └─ tcpool-shim  (static Rust binary installed as /usr/lib/jellyfin-ffmpeg/ffmpeg)
       │  gRPC over mTLS: ScheduleQuery → Run(stream)
       ▼
  tcpool-agent  (one per GPU; DaemonSet per vendor class, scheduled by device-plugin/NFD labels)
       ├─ capability + cost probe at startup (outputs, decoders, filters, calibration)
       ├─ admission control (weighted units), progress watchdog, fencing lease
       ├─ job IR → per-backend renderer (QSV / NVENC / CPU) → jellyfin-ffmpeg child
       └─ /metrics, gRPC health
  tcpool-sync   (one replica, leader-elected): lowest common denominator → Jellyfin encoding offers
  shared scratch: pool/transcode-scratch (NFS, 200 GiB quota, lookupcache=positive, actimeo=1)
```

**No central scheduler.** Each shim reads the load of every live agent (they're behind a headless Service, so DNS lists them) and scores them. The agent's admission check is the atomic authority: a stale view just means a `Busy` reply and the next-best candidate. This keeps the current proven property that no single component can stop playback.

### Language and components

- **Rust** for all three binaries:
  - static musl builds;
  - tokio for async, tonic for gRPC, rustls for TLS, `prometheus` for metrics;
  - memory-safe for a service that spawns processes from network input;
  - no runtime to ship into the Jellyfin image.
- Go would also work. Rust wins on the shim's cold start and binary size, and on not having a GC in the agent's event loop.
- **Not in scope: linking libav\* in-process.** jellyfin-ffmpeg carries Jellyfin-specific filters and the `p`/`u`/`q` stdin control that Jellyfin's throttler depends on. Spawning the matching jellyfin-ffmpeg keeps behaviour identical to what Jellyfin expects.

Workspace layout (`transcode/`):
- `proto` (`tcpool.proto`)
- `ir` (Jellyfin command parser plus per-backend renderers)
- `shim`
- `agent`
- `sync`
- `bench` (quality, capacity and chaos harness)

### Protocol

gRPC with mTLS, certificates from the existing step-ca (`60-step-ca.yaml`).

| RPC | Purpose |
|---|---|
| `Hello() → Caps` | outputs, decoders, filters, capacity, units in use, quality rank per codec, agent version |
| `Run(stream ClientMsg) ↔ stream ServerMsg` | client sends `Job`, stdin bytes and heartbeats. Server sends `Accepted` / `Busy`, stderr, progress, heartbeats and `Exit` |

- The Kubernetes readiness probe uses gRPC health. It's native since 1.27, which replaces the Python exec probe.
- The heartbeat, fencing and "re-run before the first segment" rules are the proven spike semantics, kept exactly: 6 s dead-after, 3 s fence.

## 2. Production hardening

| Item | Design | Why |
|---|---|---|
| Authn | mTLS; only the Jellyfin shim identity may call `Run` | today anything in the cluster can make a worker run ffmpeg |
| Command allowlist | the IR parser accepts only Jellyfin's grammar; inputs must be under the media roots; outputs only under the transcode dir; no `concat:`, `http:` or other protocols; unknown shapes run on the CPU chain or are rejected | defense in depth behind mTLS; ffmpeg can read and write files |
| Progress watchdog | agent runs ffmpeg with `-progress pipe:3` and tracks `out_time`. It knows when Jellyfin has paused it (`p`/`u` over stdin), so throttling never counts as a hang. No progress for 15 s while unpaused → kill, shim fails over | today a hung ffmpeg under a healthy agent goes undetected |
| Graceful drain | preStop marks the agent not-ready and stops admitting. Each running job finishes its current segment, then the agent ends it so Jellyfin restarts it on another worker (the drill-proven path). `terminationGracePeriodSeconds` ≥ 15 | rolling upgrades and node drains with no viewer impact |
| Discovery | headless Service `tcpool-agents`; the shim resolves A records. No `TC_WORKERS` list | adding a card = the DaemonSet schedules a pod; nothing to edit |
| Placement | DaemonSet per class (`gpu.intel.com/i915`, `nvidia.com/gpu`, CPU) selected by device-plugin/NFD labels. One agent per device; the agent reports the device's PCI ID in `Caps` | new or multi-GPU nodes are picked up automatically |
| Version pinning | worker jellyfin-ffmpeg == the version in Jellyfin's image; the agent reports it in `Caps` and the shim refuses a mismatch in its major version | Jellyfin emits filter names specific to its ffmpeg |
| Observability | per agent: units used/capacity, jobs, fps and speed per job, admissions/refusals, failovers, watchdog kills, probe results, GPU engine busy (NVML; i915 fdinfo `drm-engine-*`) → VictoriaMetrics. Alerts: pool capacity < one card, any job < 1.0× realtime for 60 s, agent down, probe regression | failures in a pool are silent unless measured |
| Kuma | a dead-man push from `tcpool-sync` (pool healthy and offers in sync) plus the existing Jellyfin monitor | the repo rule: every new workload gets a monitor |
| Jellyfin fixes | ship both patches from `transcode/spike/jellyfin-transcode-fixes.patch` (wedge + marker race) and JellyMesh's shared-mode no-wipe, in the jm3 image; each gets a lab regression test | the shim works around them today; the image should not need workarounds |
| Chaos regression | the `bench` binary runs drills A–E (freeze, delete, pre-first-segment kill, 4K HDR, codec switch) plus a network partition. It scores failed requests, minimum buffer and ffprobe frame continuity. Runs weekly as a CronJob against a canary session and on every release in CI. The CronJob gets a Kuma push monitor in `k8s/kuma/monitors.yaml` in the same PR, or `kuma-coverage --check` fails it | the proof has to stay true across Jellyfin and driver upgrades |
| **One encoder per output (multi-replica)** | Before running, the agent takes a per-output-prefix lease: an `O_EXCL` lock file `<md5>.lock` in the scratch dir, heartbeated and expiring after the fence timeout. A second job for a held prefix gets `Duplicate`, and the shim exits 0 without writing; the second Jellyfin replica then serves the first one's segments from disk | with two Jellyfin replicas (JellyMesh stage 3) and no stickiness, replica B has no `TranscodingJob` for A's session. So B starts a second ffmpeg at the next "missing" segment while A's is still writing the same `<md5>N.ts` files, and two encoders on one prefix corrupt segments. This lease makes the sticky cookie optional instead of required |
| Subtitle burn-in | The IR classifies `subtitles=`/`overlay` burn-in. Workers mount Jellyfin's attachments/fonts path read-only (it's on the shared `/config`), or those jobs are routed to the local CPU deliberately | `subtitles=f=/config/...` points at a path workers don't mount. Today such jobs fail on every worker before the first segment and only then fall back to local CPU |
| Fuzzing | `cargo fuzz` on the IR parser; property tests: capacity never exceeded; the shim never exits non-zero before the first segment while any worker remains | the parser is the security and correctness boundary |

## 3. Scheduling: spreading load

1. **Cost model.** Start from the measured static weights (1080p = 1; 4K = 2.3 on the Arc, 2 on the P4). Then learn online: each running job reports its speed, and the agent keeps an EWMA of cost per source class per card. It admits a job only if predicted utilisation stays at or below 85% of the throughput ceiling that holds ≥1.3× realtime. The static weights stay as a hard ceiling, so a bad estimate can never overload a card.

   **Throttling matters here.** Jellyfin pauses ffmpeg once it's about 60 s ahead, so a steady-state session costs about 1.0× realtime. The measured flat-out figures are the **burst** ceiling (a new session, or catch-up after a seek), not the steady state, so the pool holds more sessions than the units suggest. Cost samples are taken only while ffmpeg is unpaused (the same pause tracking the watchdog uses). Otherwise throttled jobs read as slow and the model under-admits forever. The admission ceiling stays sized for concurrent bursts.
2. **Placement score** (shim, per job):
   `score = free_fraction × quality_rank[codec] × locality`
   - `free_fraction` spreads load, which is the default behaviour.
   - `quality_rank` favours the better encoder for the requested codec (calibrated in §4).
   - `locality` slightly prefers a worker on the same node as the media's NFS client cache (small, to measure).
   - Ties: the least recently used worker.
   - Among the top two candidates, pick with probability proportional to score (power-of-two choices). This avoids a stampede when both Jellyfin replicas choose at once.
3. **Priority classes.**
   - `playback` (HLS for a viewer) always wins.
   - `batch` (trickplay, keyframe extraction, and progressive/download transcodes, which have no restart primitive) goes to the pool only when there's headroom, at low priority, and may be preempted. A pool failure -- preempted, a lost worker, or a plain non-zero exit, not just a preemption -- hands the job back to Jellyfin as a retryable failure once a first frame exists; before that the shim reruns locally and Jellyfin never sees a failure. Landed for trickplay only (§10 P2); keyframe extraction and progressive/download transcodes are out of scope for this pass (see §10).
4. **Degrade instead of stall (N+1 policy).** When a card is lost and the survivors are over capacity, re-admitted and new playback jobs get an "emergency" render: fastest preset, GPU chain only. That buys throughput at a small quality cost, so viewers don't stutter. A pool gauge and alert show when the pool is running degraded. When capacity returns, new jobs go back to normal quality automatically.

## 4. Quality

Principle: **for a given bitrate cap (which the client sets), produce the highest perceptual quality this card can while keeping the pool's realtime guarantees.** And make it consistent, so a failover mid-session doesn't visibly change the picture.

1. **Measurement first.**
   - `bench quality` encodes a fixed corpus: 1080p film, 4K DV/HDR10, animation, dark and grainy titles.
   - It runs per card × codec × rate-control mode × preset × bitrate ladder (2, 4, 8, 15, 25 Mbps).
   - It scores VMAF (plus PSNR/SSIM) against the source, scaled to the output resolution. HDR→SDR is scored against a reference tone map.
   - Measurement uses a separate ffmpeg build with libvmaf; whether jellyfin-ffmpeg includes libvmaf is unverified.
   - Output: a calibration table per card that the agent loads.
2. **Rate control, mapped per encoder instead of Jellyfin's libx264 CRF.** Jellyfin sends `-crf`/`-b:v` plus `-maxrate -bufsize`. The agent renders:
   - **QSV (Arc):** VBR or LA-ICQ with `-look_ahead_depth`, `-extbrc 1`, `-adaptive_i/-adaptive_b`, `-b_strategy 1`, preset from the headroom policy.
   - **NVENC (P4, Pascal):** `-rc vbr -tune hq -multipass fullres -spatial-aq 1`, plus `-temporal-aq` / `-b_ref_mode` only if the startup probe shows Pascal supports them. `-preset p5–p7` by headroom.
   - Both honour Jellyfin's maxrate/bufsize exactly; clients depend on those caps. The spike's delivered-bitrate undershoot (about 1.3–1.8 Mbps against an 8 Mbps cap) is the first thing this fixes.
3. **Preset scaling by headroom.** When the pool is idle, use the slowest preset that still gives ≥1.5× realtime at the current load. The calibration table maps presets to cost. The preset is fixed per job at admission, so segment timing is unaffected.
4. **Tone mapping.** Compare `tonemap_vaapi` (Arc, prod today), `tonemap_cuda` bt2390, `tonemap_opencl`, and libplacebo/Vulkan (verify it's in jellyfin-ffmpeg) with the same metrics plus a visual check on the dark and bright reference scenes. Pick the best per card that holds realtime; it may differ by load tier.
5. **Consistency across failover.** Per codec, pick a rate-control setup on each card that hits the same VMAF (±1.5) at each ladder rung, so a failover between cards changes quality imperceptibly.
6. **Outputs.** H.264 + HEVC 8-bit + HEVC 10-bit (MEASURED common). Verify HEVC 10-bit end-to-end through both GPU chains for HDR-capable clients (only 8-bit output has been exercised so far). AV1 is not offered with the current cards (§7).

## 5. Startup latency

The target is first segment p50 < 1.5 s and p95 < 3 s (today 2–9 s).
1. Instrument every phase: shim schedule, TLS, agent admission + ffprobe, ffmpeg probe (`-analyzeduration 200M -probesize 1G` over NFS), GPU init, first segment written, NFS visibility.
2. Cache the source ffprobe in the agent (keyed on path, size and mtime), so a restart or failover of the same title doesn't probe twice.
3. Media read path: measure an NFS readahead bump and FS-Cache (`cachefilesd`) on a node-local SSD, for the first seconds of titles being played. Only on nodes with local NVMe. Measure before adopting.
4. Warm GPU contexts: keep the device-init and CUDA JIT caches hot (the JIT part is done), and pre-warm one decode session per codec at agent startup.
5. Reducing probesize only for containers where it's provably safe, gated on the corpus test. Default: don't.

## 6. Robustness to Jellyfin changes

The whole design leans on the HLS restart primitive (`DynamicHlsController` restarting ffmpeg at the next missing segment) and on the exact command grammar.
- **Command corpus:** collect the ffmpeg command lines from prod `FFmpeg.Transcode-*.log` (all codecs, subtitle burns, audio-only) as golden inputs for the IR parser and renderers. Every new Jellyfin version's commands are replayed in CI; an unparsed shape fails the build.
- **Contract test:** a lab test starts a stock Jellyfin version, kills a job mid-stream and before the first segment, and asserts the restart behaviour. It runs on each Jellyfin bump before prod.

## 7. Capacity and hardware (DECIDED 2026-09-26: the two existing cards only)

- **Pool:** Arc 14 units + P4 6 units, plus the CPU worker as spill (slow, a last resort). No new hardware.
- **N+1 with two unequal cards.** Losing the P4 always fits on the Arc. Losing the Arc at full load leaves about 6 units. Policy: **run the full capacity, and make an Arc loss degrade instead of stall.**
  1. **Survivable load.** Up to the P4's capacity (6 units) plus the CPU spill, any single card loss is fully covered.
  2. **Above that, emergency mode (§3.4).** The P4 switches new and failed-over jobs to its fastest preset (the extra capacity this gives is measured in P4). Overflow goes to the CPU worker. Beyond both, a failed-over session can still stall. That is the accepted residual risk.
  3. **Alert** `TranscodePoolNotSurvivable` when pool load exceeds the survivable level for more than 10 min, so it's visible how often the risk is actually live.
- **AV1 is not offered:** the P4 can't encode it, so no AV1 session could fail over. The old P7 is dropped.
- **The CPU worker runs on the dl380** (24 threads), not the tower, which already carries Frigate. JellyMesh's plan already asks for that.

## 8. Phases

Each phase ends with a measured gate. Nothing reaches viewers before phase 3.

| Phase | Scope | Gate |
|---|---|---|
| **P0 Baselines** | command corpus from prod logs; quality harness + first calibration run; startup-latency breakdown | the calibration table exists; latency broken down per phase |
| **P1 Native parity** | Rust workspace: proto, IR (parser + QSV/NVENC/CPU renderers, goldens), shim, agent (admission, heartbeat/fence, re-run, capabilities probe, per-output lease), sync. **Build pipeline** (the repo has no Rust today): `cargo` → musl static binaries in `ci.yml` (fmt, clippy, test, fuzz smoke) → GHCR images `tcpool-agent` (FROM the jellyfin-ffmpeg base matching Jellyfin) and a shim artifact for JellyMesh's image to copy in, built on the existing self-hosted runner | all 9 spike protocol cases and cluster drills A–E pass on the native binaries in tc-lab; golden corpus 100% parsed; shim cold start < 5 ms |
| **P2 Hardening** | batch/priority classes (decision 3), mTLS via step-ca, allowlist, progress watchdog, drain, gRPC health, headless-Service discovery, DaemonSets by label, metrics + alerts + Kuma, fuzzing | the chaos suite plus a network-partition drill are green; `kubectl drain` of a GPU node during 3 sessions gives 0 failed requests; fuzzing runs clean |
| **P3 Rollout (with JellyMesh #155)** | agents in ns `media` alongside today's Jellyfin; a canary Jellyfin with the native shim for one user; then the JellyMesh GPU-less Deployment | 7 days canary with 0 playback failures attributable to the pool. **Rollback:** the JellyMesh Deployment is GPU-less, so `jellyfin-qsv`/`jellyfin-nvenc` stay defined at replicas 0 through the canary. Rollback = scale one back up (with the placer) and set `encoding.xml` hwaccel back to `qsv` |
| **P4 Scheduling** | online cost model, placement score, priority classes, emergency degrade | a mixed-load soak (≥80% of pool units for 1 h) has no job < 1.0× realtime; an Arc-loss drill at full load shows degradation, not stalls |
| **P5 Quality** | per-encoder rate-control mapping from calibration, preset scaling, tone-map choice, HEVC 10-bit verification | VMAF per rung within ±1.5 across cards; delivered bitrate within 10% of the cap on hard titles; better than today's prod QSV at equal bitrate |
| **P6 Latency** | probe cache, readahead/FS-Cache trial, warm contexts | first segment p50 < 1.5 s, p95 < 3 s |

Order: P0 → P1 → P2 → P3 is the critical path to production. P4–P6 are independent after P3, and P5 can start its measurement work during P1.

## 9. Decisions (Brian, 2026-09-26)

1. **Rust** for the native control plane.
2. **Only the two existing cards.** §7 policy: full capacity, emergency degrade on an Arc loss, a survivability alert, no AV1.
3. **Batch work goes to the pool at low priority** (trickplay, keyframes, progressive transcodes), preemptible by playback (§3.3).
4. **The Python spike never serves real viewers.** The canary starts on the native build after P2.

## 10. Status and remaining work (live checklist)

Updated 2026-09-27. `[x]` done and verified · `[~]` in progress · `[ ]` to do. Owner is the session, unless it's a subagent (sub) or JellyMesh (JM).

### P0 Baselines
- [x] Command corpus: 67 prod QSV + 67 lab software-mode commands (`transcode/corpus/`)
- [~] Quality calibration: VMAF per card/codec/preset/bitrate (sub) → `transcode/calibration/`
- [~] Startup-latency breakdown per phase (sub)

### P1 Native parity
- [x] `tcpool-ir`: byte-for-byte parity with the spike (201 goldens); `render()` adds `temp_file` segments and copy-without-hwaccel
- [x] gRPC protocol, agent, shim, sync in Rust; static musl binaries (agent 3.1 MB, shim 2.1 MB)
- [x] Shim overhead 0.33 ms vs 71 ms for Python (MEASURED)
- [x] Protocol suite 14/14 on native binaries, locally and in CI (`tcpool` job)
- [x] Per-output lease with follow-then-take-over (multi-replica Jellyfin)
- [x] Native binaries serving tc-lab (2026-09-27): playback 0 failed requests; graceful pod delete of the
      serving Arc mid-4K-HDR → agent drained in 0.9 s, Jellyfin resumed at segment 22 on the new pod,
      0 failed requests, lowest buffer 1.3 s
- [x] Lost-before-first-segment re-run on the native shim (SIGKILLed Arc → P4 re-ran from 0, 0 failed)
- [ ] Remaining native drills: freeze (SIGSTOP), network partition, node drain with 3 concurrent sessions
- [~] Container images + GHCR publish workflow (sub C, branch `tcpool-deploy`)

### P2 Hardening
- [x] Progress watchdog (pause-aware) — suite case 10/11
- [x] Graceful drain on SIGTERM (ends after the next segment) — case 12
- [x] Command allowlist (134 real commands pass, 11 attacks rejected) — case 13
- [x] mTLS (pool CA, client certs required, drain on cert rotation, plaintext health port) — case 14
- [x] Metrics: agent :9903, sync :9904; alert rules `transcode/deploy/alerts.yaml` (sub A, merged)
- [~] Fuzzing + property tests + bypass hunt (sub B, branch `tcpool-fuzz`)
- [~] cert-manager certs, DaemonSets per GPU label, headless-Service DNS discovery, PDBs, JellyMesh contract (sub C)
- [x] Batch/priority class (decision 3), trickplay only: routes at `Priority::Batch` behind `TC_BATCH=1` (unset by
      default, so this ships dark). Keyframe extraction needs no pool routing -- Jellyfin never spawns `ffmpeg`
      for it. Progressive/download transcodes are deferred, no restart primitive to build the preemption
      contract on yet (out of scope, A8). `TC_TRICKPLAY_OUTPUT_ROOT` is Jellyfin's `TempDirectory`, not the
      final sprite-sheet directory Jellyfin later reads from -- the two are wired together by a shared volume
      mount, which is P3 work (deploy manifests), not this item. A1 (REVISED): a pool failure before a first
      frame reruns locally (exit 0); a pool failure once a first frame exists exits the shim non-zero with no
      local rerun (a from-scratch rerun can't catch the pool's high-water mark inside Jellyfin's ~20s poll
      window) -- Jellyfin retries it as a normal failed trickplay task. Suite cases 15-19 (proto_test.sh)
- [x] Seek affinity (PLAYBACK only): a fresh `rank()` on every ffmpeg restart (seek, audio/subtitle
      track change, bitrate switch, failover) could hop a session between the Arc (QSV) and the P4
      (NVENC) mid-playback and show a visible quality step, so the shim now pins a session to its
      first worker and keeps it there through ordinary restarts. Key: the HLS output prefix
      `lease::lease_path` already derives its lock file from -- Jellyfin computes it as
      `MD5(MediaPath-UserAgent-DeviceId-PlaySessionId)` and every `DynamicHlsController` restart
      handler carries the same `playSessionId` straight through
      (`Jellyfin.Api/Helpers/StreamingHelpers.cs:377-386`, `DynamicHlsController.cs:229` et al. in
      `~/.cache/jellymesh-vendor/jellyfin-src`), so it is stable **server-side** across any restart
      that carries an existing `playSessionId`, and changes only on a genuinely new session.
      Verified for a seek; whether a client-driven track/bitrate change reuses `PlaySessionId`
      rather than re-invoking `/PlaybackInfo` (which mints a fresh one --
      `MediaInfoHelper.cs:132`) was not independently checked -- no jellyfin-web client source is
      vendored here (see `crates/shim/src/affinity.rs`'s doc comment). Store: `<md5>.worker`, a
      sibling of the lease lock in the shared scratch dir (so every Jellyfin replica sees it),
      written atomically (tmp + rename) with the winning `Worker.name` on every PLAYBACK
      `Accepted`, never for BATCH. Use: `apply_affinity` moves the pinned worker to the front of
      `rank()`'s output when it answered Hello this round and still shows free capacity; `Caps`
      carries no draining flag, so a pinned-but-draining worker is only caught by its
      `Busy{reason:"draining"}` at admission, which the existing per-candidate loop already falls
      through on -- no new field needed. Failover: the pin is cleared before the 255-exit site (a
      worker LOST mid-transcode, or another replica's lease holder gone after the first segment),
      before a non-zero exit from the agent's own stall watchdog or drain (a wedged/hung card, or
      a rolling update -- these reach the shim the same as any other post-first-segment failure,
      not as a dropped connection), and opportunistically by `affinity::sweep_stale` for any file
      past `TC_AFFINITY_TTL_SECS` that a session never revisits, so the restart ranks fresh
      instead of returning to a card that just died or is draining, and a fully-orphaned pin does
      not sit in shared scratch forever; a clean Busy/refusal, or the file's own session simply
      ending cleanly, leaves it alone otherwise. `TC_AFFINITY_TTL_SECS` (default 6h) ages out an
      orphaned file; `TC_AFFINITY=0` disables the feature (default on). Unit tests:
      `crates/shim/src/affinity.rs` (round-trip, overwrite, clear, sweep, TTL, garbage/blank
      content, env parsing) and `crates/shim/src/tests.rs` (`apply_affinity_*`); suite case 20
      (proto_test.sh) exercises the full write-on-Accept / seek-stays-pinned /
      kill-clears-and-fails-over / stall-clears-and-fails-over path against the native binaries.
- [ ] Chaos CronJob + Kuma push monitor (weekly drills against a canary)
- [ ] Drain drill: `kubectl drain` a GPU node during 3 sessions → 0 failed requests

### P3 Rollout (with JellyMesh #155)
- [ ] Agents in ns `media` (TLS on), alongside today's Jellyfin
- [ ] Canary Jellyfin with the native shim for one user, 7 days, 0 pool-attributable failures
- [ ] JellyMesh GPU-less Deployment on the pool (JM); `jellyfin-qsv/nvenc` kept at replicas 0 for rollback
- [ ] Deploy alert rules + Kuma monitors; KB + runbook

### P4 Scheduling
- [x] Resolution-weighted capacity (Arc 14 / 4K 2.3, P4 6 / 4K 2), busy → next worker, free-capacity ranking
- [~] Emergency degrade (fastest preset + emergency capacity when every worker is busy)
- [ ] Online cost model (EWMA of unpaused speed), quality-aware placement score, power-of-two-choices
- [ ] Sticky server failover (future release, Brian 2026-09-27): prod's Traefik failover route sends a
      failed-over viewer back to the primary as soon as it is healthy, so one failover costs two ffmpeg
      restarts. MEASURED 2026-09-27 18:02Z/18:05Z: a Safari remux (video copy, DTS->AAC) restarted at 49:12
      on the fallback and again at 52:48 on the primary, and the viewer reported audio drifting out of sync.
      Pin sessions with a sticky cookie so new sessions prefer the primary but a failed-over one stays put.
      Prototype + drill in jm-lab first. Related: remux restarts can drift A/V (video resumes on a source
      keyframe, audio at the exact second); add an A/V start-PTS check to the restart drills.
- [ ] Trickplay resume from high-water mark: instead of retrying whole (§10 P2), resume a preempted-after-first-
      -frame job with `-ss` + `-start_number` at the pool's last complete frame. Not done in P2 because of four
      unresolved hazards (A1 REVISED): (1) keyframe-only mode (`-skip_frame nokey`, no `setpts`) doesn't land on
      an `N * interval` boundary after a seek, so the resumed sequence can't be spliced onto the original
      numbering; (2) the pool's last `.jpg` may be truncated by the kill, so resuming needs an EOI-byte check to
      decide whether to resume at N or overwrite N-1; (3) `-threads 1` software seek+decode of a 4K source may
      itself exceed Jellyfin's ~20s trickplay window, i.e. the resume could be slower than a full local retry;
      (4) resuming changes the argv the pool renders, which breaks the "exec_real preserves argv exactly"
      invariant every other fallback in this codebase relies on

### P5 Quality
- [x] Baseline measured (P0, calibration/README.md): Arc delivers 16-22% of the cap, VMAF 87.1/87.5 @8M
      (h264/hevc); P4 94.8/93.2; Arc-vs-P4 gap 5.4-7.8 (a failover is visible). Calibrated settings
      measured Arc 93.5/97.6, P4 93.5/96.9, gap +0.04/+0.72
- [ ] **Next session, step 1** (docs/tcpool-next-session.md): calibrated rate control + presets in render()
- [ ] Rate-control mapping per encoder from the calibration (fix the bitrate undershoot)
- [ ] Preset scaling by headroom; tone-map choice per card (vaapi/cuda/opencl/libplacebo by metrics)
- [ ] HEVC 10-bit end-to-end through both GPU chains; VMAF consistency ±1.5 across cards

### P6 Latency
- [x] Probe clamp: Jellyfin's `-probesize 1G` cost 8-12 s over the tower's 1 GbE link; clamped to 50M/5M
      (mapped streams unchanged on all 3 titles) → first segment 0.78-0.88 s (2026-09-27)
- [x] P0 latency baseline: first segment p50 0.8-1.35 s warm (calibration/README.md)
- [ ] Hedged start: if no first segment after ~4 s, start a second attempt elsewhere and keep the first to finish
- [ ] Tower uplink: 1 GbE saturates on a remux probe (enp5s0f1 unused); faster link or bond
- [ ] Readahead/FS-Cache trial, warm contexts; cold-cache latency run still owed

## 11. Jellyfin integration and UI controls

**How it stays seamless.** Jellyfin is not modified. It runs with hardware acceleration
**none**, so it always emits a portable software command line (`libx264`/`libx265`, `scale`,
`tonemapx`). The shim sits at Jellyfin's configured ffmpeg path. Every HLS transcode goes to the
pool. Every other invocation (probes, `-encoders`, subtitle extraction) execs the real local
ffmpeg unconditionally; trickplay does too unless `TC_BATCH=1` and its output directory is under
`TC_TRICKPLAY_OUTPUT_ROOT` (§10 P2, shipped dark), in which case it routes to the pool at
`Priority::Batch` instead. Each agent swaps in its own card's decoder, encoder and GPU filter chain
(`crates/ir/src/lib.rs`, `filters.rs`). Consequences:
- Jellyfin needs no GPU, and its per-device probes (`IsVaapiDevice*`) never matter, because they
  only feed vaapi/qsv command building (EncodingHelper.cs:1051).
- The hardware-specific options in the UI don't apply in `none` mode. The agents decide hardware
  decode, low-power mode and VPP tonemapping per card.
- The prod QSV corpus is only used to test the allowlist. The translator's input is the software dialect.

**Which UI settings the pool honours, overrides, or ignores:**

| Jellyfin setting (Dashboard → Playback → Transcoding) | Pool behaviour | Status |
|---|---|---|
| Hardware acceleration = **none** | The one dialect the IR parses and re-targets to QSV/NVENC/CPU. Setting QSV/NVENC here would hand the agents a dialect they don't translate | pin it: sync alerts on drift (P3) |
| Transcode path | Must be the shared scratch `/transcodes/tc` | pin it: sync alerts on drift (P3) |
| Allow encoding in HEVC / AV1 | Offered only if every configured worker can output it | **fix:** sync currently overwrites the checkbox. Make it `effective = your choice AND pool can`, and show why when it's off (P3) |
| Hardware decoding checkboxes, low-power encoders, VPP tonemap | Not used in `none` mode. Each agent hardware-decodes what its card supports and falls back to CPU decode per job (e.g. no AV1 decode on the P4) | works, by design |
| Enable tone mapping | Jellyfin emits `tonemapx`; the agents run it on the GPU (CPU fallback per job) | works |
| Tone-map algorithm / peak / desat / range | Carried in `tonemapx`. Honoured on the P4 (`tonemap_cuda` takes them). **Dropped on the Arc**: `tonemap_vaapi` only takes primaries/transfer/matrix | **fix (P5):** a non-default algorithm routes the Arc through OpenCL tonemap, or the UI note says the Arc uses a fixed curve |
| Encoder preset, H.264/H.265 CRF | Preset mapped per encoder; CRF → per-encoder rate control from the calibration | P5: QSV `-global_quality` is CQP and ignores the cap (16-22% of cap, calibration/README.md) |
| Throttle transcodes, segment deletion | Pass-through; the agent honours ffmpeg's pause/resume keys | works (suite case 11) |
| Per-user bitrate limit, "allow video transcoding" | Enforced by Jellyfin before the pool is involved | works |
| Thread count | Irrelevant to GPU workers | ignored |

**Visibility and controls: a Jellyfin plugin, "Transcode Pool".** Jellyfin's dashboard shows a
session as "Transcoding (hardware)" and cannot know which card is serving it. A small C# plugin
(Jellyfin's native extension point) adds a dashboard page backed by a JSON `/status` endpoint on
tcpool-sync:
- Read-only first (P3): workers (up/down/draining), capabilities, units used/capacity, emergency
  mode, which worker serves each session (the session id is in the segment path), and which
  codecs are offered and why.
- Controls second (P4): drain/undrain a worker and HEVC/AV1 intent. These go through sync, which
  holds an admin client cert. Agents gain an admin-only `Drain` RPC.
- The Grafana board and Kuma remain the alerting path. The plugin is the operator view, and it
  shows the same numbers.

Checklist (added to §10):
- [ ] sync: intent-vs-effective for Allow HEVC/AV1; pin HardwareAccelerationType=qsv and the transcode path, alerting on drift
- [ ] sync `/status` JSON endpoint (workers, sessions → worker, offers with reasons)
- [ ] Jellyfin plugin, read-only dashboard page (P3), then drain/intent controls (P4)
- [x] Native pool serves the GPU-less tc-lab Jellyfin (hwaccel none): 0 failed requests, first segment 1.09 s (2026-09-27)
- [ ] Arc tonemap: honour the UI algorithm (OpenCL route), or document the fixed curve

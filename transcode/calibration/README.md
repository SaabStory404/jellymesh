# Transcode pool calibration — P0, 2026-09-26/27

First calibration pass for `transcode/docs/PLAN.md` §4 (quality) and §5 (startup
latency). Everything here is **MEASURED** in the `tc-lab` namespace on this cluster; nothing is
inherited from the plan. Raw rows: `2026-09-26-p0.csv` (one row per encode).

## Setup

| | |
|---|---|
| Cards | Arc A380 (`tc-worker-qsv`, gpu-node-arc, `h264_qsv`/`hevc_qsv`), Tesla P4 (`tc-worker-nv`, gpu-node-p4, `h264_nvenc`/`hevc_nvenc`), CPU anchor (`tc-worker-cpu`, gpu-node-arc, 8 threads) |
| ffmpeg | jellyfin-ffmpeg 8.1.2 on the workers (no libvmaf); metrics in a short-lived `tc-p0-vmaf` pod on gpu-node-p4, `lscr.io/linuxserver/ffmpeg:latest` = **ffmpeg 9.0 with libvmaf** (pod deleted after the run) |
| Excerpts | 20 s from t=1200 s of each lab title, video only, output 1920x1080 (480 frames at 24 fps) |
| Caps | video `-maxrate` 3 / 8 / 15 Mbps, `-bufsize` 2x cap, exactly Jellyfin's shape |
| Baseline | `agent.translate()` on the real Jellyfin command line (corpus `lab-sw.tsv`): `-crf 23` (h264) / `-crf 28` (hevc) -> `-global_quality` (QSV) / `-cq` (NVENC), plus `-maxrate`/`-bufsize`, GPU filter chain from `agent.hw_filters()`, `-preset veryfast` |
| Candidates | same GPU filter chain and device init, rate control replaced (below) |
| Metrics | VMAF `vmaf_v0.6.1` + PSNR-Y + float SSIM in one libvmaf pass, distorted=input0, reference=input1, both `setpts=PTS-STARTPTS`, `shortest=1:repeatlast=0` |
| Reference | Jellyfin's own **CPU** chain with `scale=...:flags=lanczos`, x264 `-crf 8 -preset medium` (32–65 Mbps). Sample C uses jellyfin-ffmpeg's `tonemapx=tonemap=bt2390:desat=0:peak=100`, the tone mapper prod Jellyfin emits in software mode. libplacebo was not used: the reference matches what Jellyfin itself would produce, and comparing tone mappers is a separate §4.4 experiment |

**Read the Sample C rows with care.** The Arc encodes tone map with `tonemap_vaapi`, the P4 with
`tonemap_cuda`, and the reference with CPU `tonemapx`. So a Sample C score is encoder loss **plus**
GPU-tonemapper-vs-`tonemapx` mismatch, and the Arc-vs-P4 gap on Sample C is confounded by two
different tone mappers. The two SDR titles are the clean cross-card comparison; Sample C is the
HDR path's own number, not a codec number.

`-analyzeduration 200M -probesize 1G` is **not** used for the quality matrix: it changes startup
only, and fps/realtime come from ffmpeg's own `speed=` counter. Startup probing is measured in
"Startup latency" below with those flags.

## What the drivers actually accept (MEASURED, not from docs)

QSV silently drops options it cannot honour, so the accepted set was read back from `qsvenc`'s
verbose parameter dump rather than from the exit code.

| Card / encoder | requested | honoured | evidence |
|---|---|---|---|
| Arc `h264_qsv` | extbrc, look_ahead_depth 40, adaptive_i, adaptive_b, b_strategy | **extbrc only** | dump: `ExtBRC: ON`, but `AdaptiveI: OFF; AdaptiveB: OFF`, `GopRefDist: 4` unchanged by `b_strategy` |
| Arc `h264_qsv` + `-look_ahead_depth 40` | — | **CRASHES** | ffmpeg dies with **SIGSEGV (exit -11)** in every combination that sets `look_ahead_depth` on h264_qsv (alone, with extbrc, with everything), although the dump shows `LookAheadDepth: 40, AdaptiveI: ON, AdaptiveB: ON, MBBRC: ON` first. Left core dumps in the scratch dir. **The renderer must not emit `look_ahead_depth` for h264_qsv on this driver** |
| Arc `hevc_qsv` | same set | **extbrc + look_ahead_depth 40 + b_strategy** (`GopRefDist: 5`); `AdaptiveI/AdaptiveB: unknown` = silently dropped | dump: `RateControlMethod: VBR, ExtBRC: ON, LookAheadDepth: 40, BRefType: pyramid` |
| P4 `h264_nvenc` | spatial-aq, multipass fullres, temporal-aq, b_ref_mode middle | **all four accepted** (no error, Pascal) | exit 0 |
| P4 `hevc_nvenc` | same | **spatial-aq + multipass fullres**; `temporal-aq` and `b_ref_mode middle` rejected | ffmpeg errors out (Pascal HEVC) |

Candidate rate control as run:
- **QSV:** `-b:v 0.95*cap -maxrate cap -bufsize 2*cap` (VBR confirmed in the dump: `RateControlMethod: VBR`, `TargetKbps 7600 / MaxKbps 8000`) + the honoured options above, presets `veryfast / medium / veryslow`.
- **NVENC:** `-rc vbr -tune hq -b:v 0.95*cap -maxrate cap -bufsize 2*cap` + honoured options, presets `p2 / p5 / p7`.
- **CPU anchor:** `libx264` / `libx265 -preset slow`, same `-b:v/-maxrate/-bufsize`.

## Quality findings

**Coverage (read first).** All 164 encodes in the CSV have delivered bitrate, fps and realtime
factor. **48 of them have VMAF/SSIM/PSNR**: every baseline (36 = 2 cards x 2 codecs x 3 titles x
3 caps), the 8 Mbps candidate rung (h264 on all three titles, HEVC on the 1080p SDR title), the
8 Mbps CPU anchors, and the two tone-map-floor encodes. The 3 and 15 Mbps candidate rungs and the
veryfast/veryslow/p2/p7 presets are **not scored** — libvmaf needs ~45 s of a fully loaded node per
20 s clip here (the dl380 Xeon has no AVX2) and the lab workers were needed back for the native
drills. Their bitrate and throughput rows are complete, so re-scoring is a `runner`-style pass over
the files if they are regenerated.

### 1. The baseline's rate control is the whole problem

| card | baseline mapping | `RateControlMethod` (from qsvenc's verbose dump) | delivered, mean % of cap | range |
|---|---|---|---|---|
| Arc A380 | `-crf 23/28` -> `-global_quality 23/28` + maxrate/bufsize | **CQP** | h264 **22%**, hevc **16%** | 4–60% |
| Tesla P4 | `-crf 23/28` -> `-cq 23/28` + maxrate/bufsize | (NVENC dumps nothing; behaves as a capped quality target) | h264 **85%**, hevc **54%** | 19–102% |

- On the **Arc the baseline is cap-blind**: the identical bitrate comes out at 3, 8 and 15 Mbps
  (e.g. Sample A h264 = 1205 kbps at every cap; Sample B 845 kbps; Sample C 1796 kbps), and
  therefore the identical VMAF (87.12 mean for h264, 87.53 for HEVC at *every* rung). `-global_quality`
  with the iHD driver selects CQP, which has no bitrate target at all — `-maxrate`/`-bufsize` only
  clamp it. This is the plain cause of the plan's "1.3–1.8 Mbps against an 8 Mbps cap".
- On the **P4 the baseline tracks the cap until its quality target is met**, then stops: h264 hits
  98–100% at 3 and 8 Mbps but only 54–65% at 15 Mbps; HEVC only 19–47% at 8 and 15 Mbps.
- Candidate VBR fixes cap adherence: **Arc 96%** of cap (94–97%, never over), **P4 101%** h264 /
  **98%** hevc (96–**105%** — real VBV overshoot above the client's cap, inside the plan's 10%
  tolerance but in the wrong direction for a hard cap).

### 2. VMAF at the 8 Mbps rung (the scored rung)

| card | codec | mode | delivered % of cap | VMAF | dVMAF vs baseline | titles in mean |
|---|---|---|---|---|---|---|
| Arc A380 | h264 | baseline (CQP) | 16% | 87.12 | — | 3 |
| Arc A380 | h264 | **candidate VBR + extbrc, preset medium** | 96% | **93.49** | **+6.37** | 3 |
| Arc A380 | hevc | baseline (CQP) | 11% | 87.53 | — | 3 |
| Arc A380 | hevc | **candidate VBR + extbrc + LAD 40 + b_strategy, medium** | 96% | **97.61** | **+10.08** | 1 (sample-a) |
| Tesla P4 | h264 | baseline (capped `-cq 23`) | 99% | **94.76** | — | 3 |
| Tesla P4 | h264 | candidate VBR + tune hq + multipass + AQ, p5 | 101% | 93.45 | **-1.31** | 3 |
| Tesla P4 | hevc | baseline (capped `-cq 28`) | 42% | 93.21 | — | 3 |
| Tesla P4 | hevc | **candidate VBR, p5** | 99% | **96.89** | **+3.68** | 1 (sample-a) |

The +6 to +10 VMAF on the Arc is a **cap-utilisation** gain, not encoder efficiency: the baseline was
spending 0.8–1.8 Mbps. The only equal-bitrate comparison in the set is the P4's h264 row, and there
the current `-cq` mapping is **1.3 VMAF better than VBR at the same bitrate** — NVENC's quality
target is a good encoder mode; its flaw is that it ignores headroom above its target. For P5 that
suggests a capped-CQ hybrid (`-rc vbr -cq N -maxrate -bufsize`) rather than plain VBR on the P4.

### 3. Arc vs P4 consistency (target ±1.5 VMAF)

| codec | rung | Arc | P4 | gap | within ±1.5? |
|---|---|---|---|---|---|
| h264 | 8M, recommended pair (medium / p5) | 93.49 | 93.45 | **+0.04** | yes |
| hevc | 8M, recommended pair (medium / p5) | 97.61 | 96.89 | **+0.72** | yes (1 title) |
| h264 | 3M / 8M / 15M, today's baseline | 87.12 | 92.53 / 94.76 / 94.87 | **-5.4 / -7.6 / -7.8** | no |
| hevc | 3M / 8M / 15M, today's baseline | 87.53 | 92.96 / 93.21 / 93.20 | **-5.4 / -5.7 / -5.7** | no |

So **today a failover between the cards is a visible quality change (5–8 VMAF); with the candidate
RC the two cards land within 1 VMAF of each other** at the scored rung. The other two rungs are
unscored, so ±1.5 is demonstrated at 8 Mbps only.

### 4. CPU anchor and the HDR tone-map floor

| comparison | VMAF | realtime x |
|---|---|---|
| x264 `-preset slow`, 8 Mbps (3 titles) | **96.69** | 0.59 (single job, 8 threads) |
| x265 `-preset slow`, 8 Mbps (sample-a) | 97.61 | 0.18 |
| best Arc h264 8M / best P4 h264 8M | 93.49 / 93.45 | 11.5 / 4.5 |
| best Arc hevc 8M (sample-a) | 97.61 (equal to x265 slow) | 11.6 |

At h264/8 Mbps both cards are **~3.2 VMAF below x264 `-preset slow`** while running 8–20x faster; at
HEVC/8 Mbps the Arc matches x265 slow on the 1080p SDR title. The CPU worker cannot serve playback
anyway (0.18–0.59x realtime for one job).

Tone-map floor (Sample C, 40 Mbps cap, slowest preset — the residual loss is GPU tone map vs CPU
`tonemapx`): **Arc `tonemap_vaapi` 95.55**, **P4 `tonemap_cuda` 97.81**. The Sample C 8 Mbps
candidates score 93.39 (Arc) and 95.48 (P4) — each about 2.2 below its own card's floor, so the
~2 VMAF Arc/P4 difference on Sample C is the **tone mapper**, not the encoder. That is the §4.4
experiment's starting number: `tonemap_cuda` is closer to Jellyfin's CPU `tonemapx` than
`tonemap_vaapi` is.

### 5. Throughput and preset cost (all 164 rows, one job alone)

| card | codec | realtime x, baseline -> veryfast/medium/veryslow (Arc) or p2/p5/p7 (P4), min over titles at 8M |
|---|---|---|
| Arc A380 | h264 | 12.1 -> 11.1 / 11.5 / 10.0 |
| Arc A380 | hevc | 12.1 -> 11.0 / 11.6 / 9.5 |
| Tesla P4 | h264 | 4.57 -> 4.56 / 4.48 / 4.24 |
| Tesla P4 | hevc | 4.56 -> 4.57 / 4.54 / 3.09 |

Relative cost of the slowest preset (mean over titles and caps): Arc veryslow **1.24x** (h264) /
**1.46x** (hevc) the fastest preset's time; P4 p7 **1.26x** / **1.51x** p2. **The ≥1.5x realtime
filter never binds for a single job** — the worst single-job figure in the whole matrix is P4 HEVC
p7 at 15 Mbps, 2.52x. So the preset choice is a *concurrency* decision (how many sessions fit),
which is P4's measurement, not P0's.

### Recommendation (first pass, to load into the agent's calibration table)

| card | codec | rate control | preset | note |
|---|---|---|---|---|
| Arc A380 | h264 | `-b:v 0.95*cap -maxrate cap -bufsize 2*cap -extbrc 1` | medium | **never emit `-look_ahead_depth` for h264_qsv** (SIGSEGV) |
| Arc A380 | hevc | same + `-look_ahead_depth 40 -b_strategy 1` | medium | `adaptive_i`/`adaptive_b` are silently dropped, so do not bother |
| Tesla P4 | h264 | keep a quality target but make it fill the cap: `-rc vbr -cq 23 -b:v 0.95*cap -maxrate cap -bufsize 2*cap -tune hq -multipass fullres -spatial-aq 1 -temporal-aq 1 -b_ref_mode middle` | p5 | plain VBR lost 1.3 VMAF vs today's `-cq`; the hybrid is the P5 experiment |
| Tesla P4 | hevc | same minus `-temporal-aq`/`-b_ref_mode` (rejected on Pascal) | p5 | |

<!--QUALITY-->


## Startup latency

MEASURED 2026-09-26 23:33–23:36 CDT, 18 sessions (6 per title, so the "p95" column below is the
max of 6; fresh `PlaySessionId`, sequential,
h264 / 8 Mbps / 1080p) driven through the lab Jellyfin NodePort from the workstation. All 18 were
scheduled onto the Arc (`tc-worker-qsv` is first in `TC_WORKERS`, and it was never busy: the agent
admitted each job with only its own weight in use). Client timestamps are workstation-clock; agent
timestamps are kubelet `--timestamps` (ms) on the node; the segment timestamp is its NFS mtime
(TrueNAS clock, measured **13 ms** from the pod clock).

**Trap worth keeping:** an identical playback request is served straight from the segments already
on disk — Jellyfin derives the transcode path's md5 from the playback parameters, so the first run
of this harness measured 0.9 s "startup" with **no ffmpeg at all**. Each session here uses a
slightly different `VideoBitrate` to force a real transcode.

| Phase | Sample A (1080p SDR) | Sample B (1620p SDR) | Sample C (4K DV/HDR10) |
|---|---|---|---|
| Jellyfin PlaybackInfo + master.m3u8 | 0.041 / 0.178 | 0.040 / 0.051 | 0.041 / 0.044 |
| variant playlist | 0.086 / 0.184 | 0.093 / 0.143 | 0.087 / 0.111 |
| Jellyfin pre-ffmpeg + shim schedule + agent admission | 0.151 / 0.316 | 0.152 / 0.187 | 0.164 / 0.216 |
| agent accept -> ffmpeg spawned | 0.001 / 0.002 | 0.001 / 0.001 | 0.001 / 0.001 |
| ffmpeg spawn -> first segment complete on NFS | **0.496 / 0.541** | **0.358 / 0.407** | **0.875 / 0.913** |
| NFS visibility + Jellyfin serve | 0.233 / 0.383 | 0.294 / 0.345 | 0.285 / 0.324 |
| **first segment, request -> last byte** | **0.875 / 1.187** | **0.798 / 0.894** | **1.345 / 1.385** |
| total incl. both playlists | 1.006 / 1.550 | 0.961 / 1.029 | 1.474 / 1.532 |

(p50 / p95 seconds, n=6 per title.)

Sub-phases measured directly on the Arc worker (3 runs each, warm NFS):

| Component | sample-a | sample-b | sample-c |
|---|---|---|---|
| agent admission ffprobe (height, for weighting) | 0.030 | 0.016 | 0.024 |
| ffmpeg input probe, Jellyfin's `-analyzeduration 200M -probesize 1G` | 0.235 | 0.019 | 0.449 |
| QSV device + encoder init (1 frame from lavfi, no media) | 0.10 | 0.10 | 0.10 |
| ffmpeg spawn -> first segment visible (same real job, `-progress` gated at 0.5 s granularity) | 1.29 | 1.11 | 1.49 |

**Reading:** the dominant phase is ffmpeg itself — input probe (0.02–0.45 s) plus QSV init (~0.10 s)
plus encoding the whole first 3 s segment (INFERRED from the mtime pattern: Jellyfin appears to
serve segment 0 only once it is complete; not measured directly).
Next is "NFS visibility + Jellyfin serve" at 0.23–0.29 s, which is the `actimeo=1` /
`lookupcache=positive` revalidation plus the HTTP transfer. The shim + scheduling + admission path
costs 0.15 s total and the agent's own spawn is 1 ms — **the control plane is not the latency**.

These are **warm-cache** numbers: the titles had been read repeatedly by the corpus and drill runs,
so the plan's "2–9 s today" is presumably a cold-cache and/or pre-`actimeo=1` figure. The p50/p95
here (0.8–1.35 s / 0.9–1.4 s) already meet the §5 target (p50 < 1.5 s, p95 < 3 s) in the warm case;
a cold-cache run is still needed before the target can be called met. `tc-shim.log` timestamps have
1 s resolution, so the shim's own share is inside the 0.15 s lump rather than broken out.

Two more limits of this run: every session landed on the Arc (the shim tries `TC_WORKERS` in
order and the Arc was never busy), so **P4 and CPU-worker startup is unmeasured**; and because the
segment's mtime is read after it was served, "ffmpeg -> first segment" is an upper bound and
"NFS visibility + serve" a lower bound — their sum is exact.

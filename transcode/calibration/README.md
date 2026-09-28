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

## P5 applied: calibrated rate control in `render()` (2026-09-27)

`render()` now maps Jellyfin's `-crf N -maxrate cap -bufsize 2cap` per encoder
(`crates/ir/src/lib.rs` `apply_rate_control`; agent `TC_RC=legacy` restores the P1 mapping):

| encoder | rate control | preset |
|---|---|---|
| `h264_qsv` | `-b:v 0.95*cap -extbrc 1`, Jellyfin's `-maxrate`/`-bufsize` kept | `medium` |
| `hevc_qsv` | same + `-look_ahead_depth 40 -b_strategy 1` | `medium` |
| `h264_nvenc` | `-b:v 0.95*cap -rc vbr -tune hq -multipass fullres -b_ref_mode middle` | `p5` |
| `hevc_nvenc` | `-b:v 0.95*cap -rc vbr -tune hq -multipass fullres` | `p5` |
| CPU, stream copy, no `-maxrate`, AV1 | unchanged | |

Two changes from the P0 recommendation above, both MEASURED in this run:
- **No `-cq` hybrid on NVENC.** `-rc vbr -cq 23/28 -b:v 0.95cap` behaves like the old `-cq`: it
  stops at its quality target, delivering **49–60% of the cap at 15M (h264) and 25–55% at 8M/15M
  (hevc)**. Rejected.
- **No `-spatial-aq`/`-temporal-aq` on NVENC.** With AQ the P4 scored 0.7–1.8 VMAF lower at the
  same bitrate on sample-b and missed ±1.5 against the Arc there (h264 3M **+2.66**, hevc 8M/15M
  **+1.93/+1.92**). Without AQ every sample-b rung is within ±1.26.

### Method

Same excerpt, reference and metric as P0 (20 s from t=1200 s, 1920 wide, video only; reference =
Jellyfin's CPU chain with lanczos, x264 `-crf 8 -preset medium`, `tonemapx` for sample-c; libvmaf
`vmaf_v0.6.1` + PSNR-Y + float SSIM, `setpts=PTS-STARTPTS` on both, `shortest=1:repeatlast=0`).
Differences: each encode's argv is the output of the real `render()` (a small CLI over `tcpool_ir`)
applied to Jellyfin's software command from `corpus/lab-sw.tsv` with the rung's `-maxrate`/`-bufsize`,
run with jellyfin-ffmpeg 8.1.2 directly on the lab workers (`tc-worker-qsv`, `tc-worker-nv`; references
on `tc-worker-cpu`), no agent redeploy. Scoring ran on the workstation (ffmpeg with libvmaf, AVX2).
Delivered bitrate = sum of video packet sizes / 20 s (the legacy Arc rows reproduce P0 exactly:
1202.8 kbps sample-a h264). Raw rows: `2026-09-27-p5.csv`.

**Sample-a gap.** Sample-a's lab mount went stale mid-run (the source was removed from the library),
after the Arc rows, the P4 AQ/hybrid/92% rows and the reference were done but before the shipped
no-AQ P4 set ran. So the **P4 sample-a rows for the shipped config are UNMEASURED**; the nearest
measured proxy is the AQ variant (Arc − P4: h264 +1.26/+0.60/+0.33, hevc +1.07/+0.71/+0.52 at
3/8/15M). On sample-b, dropping AQ raised the P4 by 0.8–1.8 VMAF, so the true sample-a gap is
likely smaller, but that is inferred, not measured.

### Cross-card consistency (target ±1.5 VMAF per rung)

Arc = calibrated QSV, P4 = calibrated NVENC as shipped (no AQ). VMAF mean / PSNR-Y dB / SSIM.

| codec | title | cap | Arc % of cap | Arc VMAF / PSNR / SSIM | P4 % of cap | P4 VMAF / PSNR / SSIM | Arc − P4 | ±1.5 |
|---|---|---|---|---|---|---|---|---|
| h264 | sample-b | 3M | 94.1 | 87.29 / 42.27 / 0.9853 | 100.1 | 86.47 / 42.10 / 0.9841 | +0.82 | pass |
| h264 | sample-b | 8M | 94.4 | 89.80 / 43.02 / 0.9891 | 100.7 | 89.71 / 42.98 / 0.9891 | +0.09 | pass |
| h264 | sample-b | 15M | 94.5 | 91.65 / 43.98 / 0.9923 | 101.2 | 91.68 / 43.95 / 0.9927 | −0.03 | pass |
| hevc | sample-b | 3M | 94.1 | 88.55 / 42.61 / 0.9870 | 102.2 | 87.87 / 42.54 / 0.9870 | +0.68 | pass |
| hevc | sample-b | 8M | 94.5 | 91.06 / 43.55 / 0.9913 | 100.4 | 90.05 / 43.40 / 0.9910 | +1.01 | pass |
| hevc | sample-b | 15M | 94.7 | 92.72 / 44.61 / 0.9943 | 100.2 | 91.46 / 44.36 / 0.9937 | +1.26 | pass |
| h264 | sample-a | 3/8/15M | 96.4 / 96.3 / 96.5 | 95.43 / 97.27 / 97.98 | — | unmeasured (see above) | — | open |
| hevc | sample-a | 3/8/15M | 96.3 / 96.6 / 96.5 | 96.16 / 97.61 / 98.10 | — | unmeasured | — | open |
| h264 | sample-c (HDR→SDR) | 3M | 95.8 | 90.94 / 33.72 / 0.9848 | 96.0 | 93.72 / 41.22 / 0.9924 | −2.78 | fail (tone map) |
| h264 | sample-c | 8M | 96.0 | 92.87 / 33.79 / 0.9866 | 98.5 | 95.62 / 41.85 / 0.9942 | −2.75 | fail (tone map) |
| h264 | sample-c | 15M | 96.0 | 93.77 / 33.82 / 0.9876 | 97.9 | 96.61 / 42.44 / 0.9954 | −2.83 | fail (tone map) |
| hevc | sample-c | 3M | 95.1 | 91.97 / 33.55 / 0.9852 | 95.7 | 94.65 / 41.33 / 0.9931 | −2.68 | fail (tone map) |
| hevc | sample-c | 8M | 95.9 | 93.48 / 33.65 / 0.9868 | 93.7 | 95.84 / 41.91 / 0.9946 | −2.36 | fail (tone map) |
| hevc | sample-c | 15M | 95.9 | 94.20 / 33.73 / 0.9878 | 94.3 | 96.48 / 42.42 / 0.9954 | −2.28 | fail (tone map) |

Sample-c's gap is not rate control: at the same delivered bitrate the Arc's PSNR-Y is ~8 dB below
the P4's at every rung, which is the `tonemap_vaapi` vs `tonemap_cuda` difference P0 already
isolated (tone-map floors 95.55 vs 97.81). It is the §4.4 tone-map item's opening number. Sample-c
was scored over 473 paired frames, and two tail frames (469, 471) score 0.0 in every row (identical
offset for every encode, ~0.4 off each mean).

### Delivered bitrate (target: within 10% of the cap)

| card | range over 3 titles x 2 codecs x 3 rungs | over the cap? |
|---|---|---|
| Arc, calibrated | **94.1–96.6%** | never |
| P4, calibrated (no AQ; sample-a from the AQ run) | **93.7–102.4%** | yes, up to +2.4% (h264 3M) |
| Arc, legacy (`-global_quality`, today) | 7.4–22.4% at 8M | — |
| P4, legacy (`-cq`, today) | h264 96–100%, hevc 35–47% at 8M | — |

The P4 overshoot is VBV/multipass behaviour at the low rung, not the target fraction: a 92% target
delivered the identical 3073 kbps at sample-a h264 3M. Inside the ±10% gate, but a 2% overshoot of a
client's cap is the wrong direction; revisit if a client is seen to stall on it.

### Better than today's prod QSV at equal bitrate

Arc calibrated at a cap equal to what the legacy mapping delivered (legacy is cap-blind, so its 8M
row is its only operating point):

| codec | title | legacy kbps / VMAF | calibrated kbps / VMAF | Δ VMAF |
|---|---|---|---|---|
| h264 | sample-a | 1203 / 91.35 | 1208 / 92.18 | **+0.83** |
| hevc | sample-a | 885 / 91.31 | 886 / 92.26 | **+0.94** |
| h264 | sample-b | 844 / 81.39 | 822 / 82.48 | **+1.09** |
| hevc | sample-b | 593 / 82.08 | 578 / 83.56 | **+1.49** |
| h264 | sample-c | 1793 / 88.13 | 1787 / 89.57 | **+1.45** |
| hevc | sample-c | 1185 / 88.69 | 1171 / 89.95 | **+1.26** |

At the same 8M cap the Arc goes from 81.4–91.4 to 89.8–97.6 VMAF; that larger gain is cap
utilisation, the table above is the encoder-efficiency part.

**Trade on the P4:** h264 at 8M gives up ~0.5 VMAF against today's `-cq 23` at equal bitrate
(sample-b 89.71 vs 90.19), in exchange for tracking the cap at 15M and for HEVC, where `-cq 28`
delivered 35–47% of an 8M cap. The CPU worker is unchanged (Jellyfin's own x264 `-crf 23 veryfast`
+ VBV): 1.6–2.2 Mbps at an 8M cap, VMAF 91.25 / 84.28 / 90.62 (a/b/c) — it is a fallback, not a
playback tier (P0: 0.18–0.59x realtime at `slow`).

Not re-measured here: throughput/concurrency at `medium`/`p5` (single-job cost is P0's, INHERITED),
preset scaling, HEVC 10-bit.

## P5.1: bitrate ladder (2026-09-28)

**Why.** Jellyfin's `-maxrate` is the *client's* max bitrate, not a quality choice. MEASURED in prod
(Jellyfin FFmpeg logs, 2026-09-28): real clients send `-maxrate 61599184 -bufsize 123198368`, so P5
targeted ~58 Mbit/s for a 1080p stream (overfills LAN clients; a remote client whose cap is its real
bandwidth gets no headroom). Requests without PlaybackInfo send `-maxrate 0 -bufsize 0`, which P5
left on the legacy quality target (on the Arc: cap-blind CQP).

**What.** `crates/ir/src/ladder.rs` + `apply_rate_control`:
- target `-b:v` = `pct x min(cap, rung)`; `pct` = **95 QSV, 90 NVENC** (below); Jellyfin's
  `-maxrate`/`-bufsize` are kept as the ceiling.
- `-maxrate 0` / no `-maxrate`: `-maxrate rung -bufsize 2 x rung` + the same target, i.e. exactly a
  calibrated request capped at the rung. `TC_RC=legacy` restores the P1 mapping for all of it.
- The output size comes from Jellyfin's `scale=` (width bound `min(max(iw,ih*a),W)`, dual bound,
  `-1:H`, fixed) resolved against the source size/rate from the agent's admission ffprobe (the same
  single call that sets the job weight, now `width,height,avg_frame_rate`). Class = 16:9-equivalent
  height (`max(h, w*9/16)`, so scope 1920x800 is 1080p). No probe: the scale bound (an upper bound);
  nothing at all: the top rung.

| class | h264 rung | hevc rung (also 10-bit) | basis |
|---|---|---|---|
| 360p | 1.5M | 1.2M | INHERITED: 1080p x (pixels ratio)^0.75 |
| 480p | 2.5M | 2.0M | INHERITED, same |
| 720p | 4.5M | 3.6M | INHERITED, same; lab-checked below |
| **1080p** | **8M** | **6.4M** | **MEASURED** (below) |
| 1440p | 12M | 9.6M | INHERITED |
| 2160p | 22M | 17.6M | INHERITED |
| > 32 fps | x 1.5 | x 1.5 | INHERITED |

- **1080p h264 = 8M** (P5 rows, Arc): 3->8M buys +1.84 / +2.51 VMAF (sample-a / sample-b), 8->15M
  +0.71 / +1.85 for almost twice the bits: 0.37-0.50 VMAF per Mbit below 8M, 0.10-0.26 above.
- **hevc = 0.8 x h264**: the hevc bitrate matching each h264 8M score (interpolated between the hevc 3M
  and 8M rows) is 0.86 / 0.69 of the h264 bitrate on the Arc and 0.89 / 0.76 on the P4 (sample-a /
  sample-b), mean 0.80. The lab run confirms it: hevc at its rung scores 90.41 (Arc) / 89.45 (P4)
  against h264 at its rung 89.80 / 89.53.

### Lab check (MEASURED 2026-09-28, `2026-09-28-p51.csv`)

Same excerpt/reference/metric as P5 (20 s from t=1200 s, lanczos CPU reference per output size,
libvmaf `vmaf_v0.6.1`). argv = real `render()` with the probe's source hint, run with jellyfin-ffmpeg
8.1.2 on `tc-worker-qsv` / `tc-worker-nv`; sample-b = 3840x1620 23.976 fps SDR (so 1080p output is
1920x810), sample-c = 4K DV/HDR10 (tone-map-confounded, not in the ±1.5 claim). Sample-a is still
gone from the lab.

**LAN-like cap 60M no longer overfills; remote-like 4M stays under the cap** (sample-b, kbps / % of
`-maxrate` / VMAF):

| codec | out | cap | Arc delivered | Arc VMAF | P4 delivered | P4 VMAF | Arc − P4 | ±1.5 |
|---|---|---|---|---|---|---|---|---|
| h264 | 1080p | 60M, P5 (no ladder) | 55811 / 93.0% | 95.45 | 51551 / 85.9% | 95.13 | +0.32 | pass |
| h264 | 1080p | 60M | **7560** / 12.6% | 89.81 | **7874** / 13.1% | 89.43 | +0.38 | pass |
| h264 | 1080p | 4M | 3767 / **94.2%** | 88.08 | 3865 / **96.6%** | 87.29 | +0.79 | pass |
| h264 | 1080p | 0 (none) | 7555 / 94.4% | 89.80 | 7858 / 98.2% | 89.53 | +0.27 | pass |
| hevc | 1080p | 60M, P5 (no ladder) | 55822 / 93.0% | 95.88 | 51149 / 85.2% | 94.47 | +1.41 | pass |
| hevc | 1080p | 60M | **6034** / 10.1% | 90.42 | **6210** / 10.3% | 89.47 | +0.95 | pass |
| hevc | 1080p | 4M | 3766 / **94.1%** | 89.27 | 3938 / **98.4%** | 88.48 | +0.79 | pass |
| hevc | 1080p | 0 (none) | 6032 / 94.3% | 90.41 | 6162 / 96.3% | 89.45 | +0.96 | pass |
| h264 | 720p | 60M | **4256** / 7.1% | 92.66 | **4389** / 7.3% | 92.32 | +0.34 | pass |
| h264 | 720p | 4M | 3796 / **94.9%** | 92.46 | 3894 / **97.3%** | 91.97 | +0.49 | pass |
| h264 | 720p | 0 (none) | 4257 / 94.6% | 92.65 | 4384 / 97.4% | 92.32 | +0.33 | pass |
| h264 | 1080p sample-c | 60M / 4M / 0 | 7682 / 3843 (96.1%) / 7681 | 93.42 / 92.14 / 93.40 | 7495 / 3740 (93.5%) / 7485 | 96.00 / 94.87 / 96.01 | −2.6 to −2.7 | tone map (known) |

- **Every 4M row is under the cap on both cards** (Arc 94.1-96.1%, P4 93.5-98.4%). The ladder does
  not touch this case (95%/90% of the cap, as P5 on the Arc).
- **The cost of the ladder at 60M is real and deliberate:** 1080p h264 drops from 95.45 to 89.81 VMAF
  (Arc) for 7.4x fewer bits; this 4K-web source is the hardest title in the set (sample-a was 97.3 at
  8M in P5). If LAN quality matters more than bandwidth, raise the 1080p rung; the table is one
  constant.
- **NVENC target 90%, not 95% (MEASURED, P4 only).** The P4 delivers 7-9% above its `-b:v` (8183 kbps
  for 7600, 6492 for 6080, 4587 for 4275 in this run). At 95% a binding 4M cap came out at **99.7-101.3%**
  (h264 1080p 3989, hevc 4054, h264 720p 4030 kbps); at 90% it is 96.6-98.4%, and the P4 now delivers
  within 4% of the Arc at every rung. An 85% variant (3691 / 3701 kbps at 720p h264 / 1080p hevc,
  VMAF 91.86 / 88.36) cost 0.1 VMAF for no cap benefit and is not shipped. The 95% rows are in the CSV
  as `nv95` (delivered only). This supersedes the NVENC `0.95*cap` in the P5 table above.

**`-maxrate 0`: ladder VBR instead of the legacy quality target** (sample-b, kbps / VMAF):

| codec | out | Arc legacy | Arc P5.1 | P4 legacy | P4 P5.1 | Arc − P4 legacy / P5.1 |
|---|---|---|---|---|---|---|
| h264 | 1080p | 2571 / 86.71 | 7555 / 89.80 (+3.09) | 8027 / 90.19 | 7858 / 89.53 (−0.66) | **−3.48** / +0.27 |
| hevc | 1080p | 576 / 83.10 | 6032 / 90.41 (+7.31) | 2832 / 87.84 | 6162 / 89.45 (+1.61) | **−4.74** / +0.96 |
| h264 | 720p | 1428 / 90.33 | 4257 / 92.65 (+2.32) | 3602 / 92.38 | 4384 / 92.32 (−0.06) | **−2.05** / +0.33 |

Legacy under-delivers on the Arc (576 kbps for 1080p hevc) and puts the cards 2-5 VMAF apart; the
ladder puts both at the rung and within 1 VMAF. The P4 gives up up to 0.66 VMAF at 1080p h264 against
its own `-cq 23` at equal bits (the same trade P5 measured at 8M).


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

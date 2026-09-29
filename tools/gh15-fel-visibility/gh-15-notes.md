# Issue #15: DV7 FEL enhancement-layer visibility on bright content

## What the issue claims vs what was found

Issue text: "One dark test scene showed base layer vs full BL+FEL reconstruction at
VMAF 99.88 / PSNR 60.8 dB (invisible)... repeat the same measurement (vs-nlq
reconstruction as reference) on 3-4 bright DV7 FEL titles."

MEASURED this session: no artifact for that prior measurement exists anywhere
retrievable -- not in `mcp__homelab__kb_search` (terms: vs-nlq, FEL reconstruction, DV7,
dovi_tool, "VMAF 99.88"), not in `jellymesh-handoff/HANDOFF.md`, `SHORTCUTS.md`,
`BUGHUNT-STATUS.md`, not in `jellymesh/transcode/docs/PLAN.md` or `docs/BUGHUNT.md`.
`SHORTCUTS.md` row 161 explicitly lists "DV7 playback via dovi_tool one-file trial" as
"not started (open decision)" as of the 2026-09-27 handoff. The only dovi_tool work done
was profile 7->8.1 *conversion* (BL-only, EL discarded via `-m 2 convert --discard`),
not a BL vs BL+FEL VMAF/PSNR comparison. No vs-nlq (quietvoid/vs-nlq VapourSynth plugin)
install or invocation was found anywhere in this repo or the handoff.

Conclusion: the 99.88/60.8 figures in the issue are not reproducible from any artifact
this session could find. Either they came from a session/host not captured in the KB or
this repo, or they were asserted without a durable record. Flagged to Brian
(needs_brian), not blocking -- a new, documented measurement was run instead.

## Method actually used (Track A: direct FEL residual, not vs-nlq reconstruction)

Reconstructing the vs-nlq (NLQ inverse-quantization) output the "correct" way needs the
quietvoid/vs-nlq VapourSynth plugin, which was not present anywhere in this repo/handoff
and was not attempted from scratch this session (time-boxed decision, see advisor
guidance in the session transcript: don't spend more than one image-build iteration on
an unproven toolchain when a direct measurement answers the same question).

Instead: for each clip, `dovi_tool demux` splits the HEVC bitstream into base-layer (BL)
and enhancement-layer (EL) sub-streams directly (this is the same demux dovi_tool's own
NLQ reconstruction path uses internally before applying the NLQ curve). We decode BL and
EL luma planes and compute the residual `EL - BL` directly, in two forms:

  - full-frame residual RMS -> approximate PSNR (20*log10(65535/RMS), 16-bit)
  - residual restricted to pixels where BL luma exceeds a threshold (highlight mask)

This is NOT the same number as an actual NLQ-reconstructed-vs-BL VMAF comparison -- the
NLQ curve applies a monotonic (usually non-linear) transform to the EL residual before
it's added back to BL, so magnitudes will differ from a true vs-nlq run. But it directly
answers the visibility question: if EL carries near-zero residual energy in the highlight
mask, no NLQ curve can conjure detail that isn't in the substream, so this is a legitimate
(if approximate) proxy, clearly labeled as such in every JSON result
(`"method": "fel_residual_direct (Track A -- ...)"`, `--threshold` state, mask coverage).

## Titles

Chosen from `jellymesh-handoff/dv7-census.tsv` (76 rows with `el_type == "7 (FEL)"`,
46 with `"7 (MEL)"` -- MEL carries a zero residual per issue's own framing, excluded).
Selected 4 bright/highlight-heavy FEL titles that exist on tank
(MEASURED via `tn_exec` `ls` on `/mnt/tank/data/media/movies/`):

  - Top Gun: Maverick (2022) -- jet cockpit glare, sun-drenched flight sequences
  - WALL-E (2008) -- bright space/reflective-surface animation
  - Iron Man (2008) -- arc reactor, explosions, desert daylight
  - Raiders of the Lost Ark (1981) -- desert daylight scenes (already hostPath-mounted
    in tc-lab from a prior session, avoided adding a new mount)

Bright ~40s window per title picked programmatically via `find_bright.sh`
(ffmpeg signalstats YAVG sampled every 5 min across the middle 80% of runtime, at
640px scale to keep the scan itself cheap) rather than picked from memory.

## Resource footprint

- Clip cut: `-c copy`, no re-encode, ~500MB-1.5GB per 40s clip at typical 4K remux
  bitrates (50-80 Mbps).
- BL/EL demux: comparable size split across two files.
- Raw gray16 decode: 4K luma-only 16-bit ~33.2 MB/frame *before* considering EL may be a
  different (often lower) resolution; at 24fps for 40s that's up to ~32GB per
  layer if EL matches BL resolution -- large. Written to `$SCRATCH/gh15/<slug>/` (PVC
  `tc-scratch-quota`, 200Gi, 194Gi free MEASURED via pod exec `df -h /transcodes`
  before the run) and deleted by `analyze.sh` after each title's JSON result is written.
  Nothing touches `/data/media` (all title mounts are `readOnly: true`).

## Job

`tc-lab` namespace, `gh15-fel-visibility` Job, node-pinned to `k3s-dl380` (only node
with the `/data/media` hostPath backing, per KB fact
`2026-09-27-dv7-fel-to-dv8-1-conversion-trial-data-media-nfs-mount-confi`), scripts via
ConfigMap `gh15-fel-scripts`.

Image ended up being the jm6 jellyfin image (`ghcr.io/saabstory404/jellymesh-jellyfin:
12.1-jm6@sha256:9075a9540f3086b19fa756c91936401aa61b5c20cac8020bc3a0b16b59f3ae24`,
already cached on k3s-dl380), NOT `debian:bookworm-slim` + apt as first planned --
MEASURED apt-get inside a fresh debian-slim pod on this node stalled repeatedly (3
attempts, see KB fact `2026-09-28-k3s-dl380-fresh-debian-bookworm-slim-pod-s-apt-get-
stalls-on`) while wget/curl from the jm6 image to the same hosts (GitHub, deb.debian.org)
worked instantly. The jm6 image already has ffmpeg/ffprobe at
`/usr/lib/jellyfin-ffmpeg/` and python3; only dovi_tool 2.3.4 needed fetching (wget from
the GitHub release, no apt).

Also switched from `geq`/`blend=all_expr` (ffmpeg's per-pixel *expression interpreters*)
to `lutyuv`/`blend=all_mode=multiply` (LUT + native-C blend) after the expression-based
version was MEASURED to take minutes per pass and would have blown the Job's time
budget -- see KB fact `2026-09-28-jellyfin-ffmpeg-per-frame-signalstats-only-surfaces-
via-ffpr...`. `lutyuv` produced byte-identical output to `geq` on the same clip
(verified) at roughly 2 orders of magnitude less wall time.

Job ran end to end in ~10 minutes for all 4 titles (started 2026-09-29 01:11:18Z,
Succeeded by ~01:21Z) once the image and filter fixes landed -- three earlier attempts
failed or stalled (apt stall x3, one `el - bl` correctness bug caught by advisor review
before it produced a real result, one `geq`/`blend=all_expr` performance dead end).

## Results

Per-title results in this directory (`iron-man.json`, `raiders.json`,
`top-gun-maverick.json`, `walle.json`) and aggregated in `results.json`. All values are
mean |residual| in 10-bit units (0-1023 scale) and as a fraction of full range;
`highlight_vs_full_ratio` = highlight-masked mean residual ÷ full-frame mean residual
(>1 means the FEL residual is proportionally *stronger* in bright regions than the
frame average; <1 means weaker).

| Title | Clip window | Full-frame mean\|residual\| | Highlight mean\|residual\| | Highlight coverage | Ratio |
|---|---|---|---|---|---|
| Iron Man (2008) | t=2555s, 40s (970 frames) | 10.51 / 1023 (1.03%) | 17.02 / 1023 (1.66%) | 2.56% | **1.618** |
| Raiders of the Lost Ark (1981) | t=2491s, 40s (966 frames) | 8.96 / 1023 (0.88%) | 5.05 / 1023 (0.49%) | 1.33% | **0.563** |
| Top Gun: Maverick (2022) | t=5881s, 40s (973 frames), picked via `find_bright.sh` peak (YAVG=346.2) | 1.86 / 1023 (0.18%) | 1.64 / 1023 (0.16%) | 0.89% | **0.885** |
| WALL-E (2008) | t=1790s, 40s (978 frames), picked via `find_bright.sh` peak (YAVG=391.9) | 0.76 / 1023 (0.07%) | 1.06 / 1023 (0.10%) | 0.05% | **1.393** |

Highlight threshold: BL luma > 600/1023 (tv-range normalized ~0.61, roughly the
upper-third of the available range) on all four titles, chosen once and held constant
for comparability. `nlq_offset` came back 512/1023 (the exact 10-bit midpoint) on
every title checked -- consistent, expected for LinearDeadzone NLQ encoding.

### Reading these numbers

All four ratios are close to 1 (0.56x to 1.62x) -- none show an order-of-magnitude jump
in residual energy in highlights vs the rest of the frame. Two titles (Iron Man, WALL-E)
show a real increase (residual is 1.4-1.6x stronger in bright regions than average);
two (Raiders, Top Gun: Maverick) show residual *no higher or even lower* in highlights.
The absolute magnitudes are small everywhere: even the largest full-frame mean
(Iron Man, 1.03% of the 10-bit range) is a small fraction of the total range, and
WALL-E's highlight coverage on this particular bright clip was tiny (0.05% of pixels),
making its ratio less statistically solid than the other three (based on far fewer
highlight pixels).

This is a residual-energy proxy, not a perceptual visibility measurement (no vs-nlq
NLQ-curve reconstruction, no VMAF/PSNR against a true BL+FEL frame -- see the method
section above). It answers "does the coded EL residual carry more signal in bright
regions" -- moderately yes on 2/4 titles, not on the other 2 -- rather than "would a
viewer perceive FEL's contribution as more visible in highlights."

### Recommendation

The evidence here does not show FEL residual concentrating strongly and consistently
in highlights across titles -- results are mixed (2 of 4 up, 2 of 4 flat-or-down) and
all magnitudes are small (residual mean is under 2% of the full 10-bit range even in
the strongest case, Iron Man). Combined with the prior (unverifiable) dark-scene report
of VMAF 99.88 / PSNR 60.8 (effectively invisible), nothing in this session's
measurements makes a strong case that server-side FEL reconstruction would produce a
visible improvement on bright content specifically. Recommend NOT prioritizing
server-side FEL reconstruction based on this evidence, but do not treat this as a
closed question -- the method here is a residual-energy proxy, not the vs-nlq
reconstruction + VMAF the issue actually asked for, and that harder measurement was
not attempted this session (no working vs-nlq toolchain found or built; would need a
VapourSurece + quietvoid/vs-nlq plugin build, out of scope for the time spent here).
`close_issue=false`: the numbers are suggestive, not conclusive, and the issue's own
requested method (vs-nlq reconstruction + VMAF/PSNR) was not run.

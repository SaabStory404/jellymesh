#!/usr/bin/env bash
# jellymesh issue #15: compute FEL enhancement-layer residual energy vs base-layer
# highlight regions, entirely via ffmpeg/ffprobe filters (no numpy -- MEASURED not
# available in the jm6 image this Job runs in, and installing it via apt was
# unreliable on this node's egress this session; see docs/engineering/gh15-fel-visibility.md).
#
# CORRECTED method: residual = EL sample - nlq_offset (NOT el - bl; see docs/engineering/gh15-fel-visibility.md
# "correction" section -- the first version of this pipeline had that bug, caught by
# advisor review before the real Job ran). nlq_offset/deadzone come from
# `dovi_tool export -d all` on the clip's RPU, frame 0, in native 10-bit units.
#
# LUT-based (not geq/blend=all_expr): `geq`/`blend=all_expr` are per-pixel expression
# interpreters -- MEASURED unusably slow (one 970-frame 1920x1080 pass took minutes and
# was still the bottleneck that would have blown the Job's time budget). `lutyuv`
# evaluates its expression once per the 1024 possible 10-bit input values and then does
# table lookups at decode speed -- MEASURED equivalent output to geq (exact match to
# printed precision on the same clip) and ~18s for a full 970-frame pass vs. several
# minutes+ incomplete for geq. `blend=all_mode=multiply` (native C, not all_expr) does
# the masked-residual combination.
set -euo pipefail

FFPROBE=/usr/lib/jellyfin-ffmpeg/ffprobe
BL="$1"          # BL.hevc, full resolution
EL="$2"          # EL.hevc, typically half resolution
EL_W="$3"
EL_H="$4"
NLQ_OFFSET="$5"        # 10-bit, e.g. 512
DEADZONE="$6"          # 10-bit, e.g. 0 (reported, not currently used in the stats below)
HI_THRESH="$7"         # 10-bit BL luma highlight-mask threshold (tv range code value)
OUT_JSON="$8"
TITLE="$9"

mean() {
  awk '{s+=$1; n++} END {if (n>0) printf "%.6f", s/n; else print 0}'
}

echo "== full-frame mean |residual| ==" >&2
full_mean_abs=$("$FFPROBE" -hide_banner -f lavfi \
  -i "movie=$EL,lutyuv=y='abs(val-$NLQ_OFFSET)',signalstats" \
  -show_entries frame_tags=lavfi.signalstats.YAVG -of csv=p=0 2>/dev/null \
  | cut -d, -f1 | mean)

echo "== highlight mask coverage (fraction of pixels with BL luma > $HI_THRESH) ==" >&2
# lutyuv output is 0 or 1023 (not 0/1) so the LUT is well-behaved across the full
# 10-bit domain; YAVG on that is directly coverage_frac * 1023, so divide by 1023.
coverage_x1023=$("$FFPROBE" -hide_banner -f lavfi \
  -i "movie=$BL,scale=$EL_W:$EL_H,lutyuv=y='if(gt(val\,$HI_THRESH)\,1023\,0)',signalstats" \
  -show_entries frame_tags=lavfi.signalstats.YAVG -of csv=p=0 2>/dev/null \
  | cut -d, -f1 | mean)
coverage=$(awk -v x="$coverage_x1023" 'BEGIN{printf "%.6f", x/1023}')

echo "== masked mean |residual| * coverage (needs dividing by coverage) ==" >&2
# blend all_mode=multiply computes A*B/1023 per pixel (8/10-bit normalized multiply);
# with mask in {0,1023} and residual in [0,1023], this yields exactly `residual` where
# mask=1023 and 0 elsewhere -- same masked-sum-then-divide-by-coverage approach as before.
masked_mean_abs_x_cov=$("$FFPROBE" -hide_banner -f lavfi \
  -i "movie=$BL,scale=$EL_W:$EL_H,lutyuv=y='if(gt(val\,$HI_THRESH)\,1023\,0)'[m];movie=$EL,lutyuv=y='abs(val-$NLQ_OFFSET)'[r];[m][r]blend=all_mode=multiply,signalstats" \
  -show_entries frame_tags=lavfi.signalstats.YAVG -of csv=p=0 2>/dev/null \
  | cut -d, -f1 | mean)

python3 - "$full_mean_abs" "$coverage" "$masked_mean_abs_x_cov" \
  "$NLQ_OFFSET" "$DEADZONE" "$HI_THRESH" "$TITLE" "$OUT_JSON" <<'PYEOF'
import json
import sys

full_mean_abs = float(sys.argv[1])
coverage_frac = float(sys.argv[2])
masked_mean_abs_x_cov = float(sys.argv[3])
nlq_offset = int(sys.argv[4])
deadzone = int(sys.argv[5])
hi_thresh = int(sys.argv[6])
title = sys.argv[7]
out_path = sys.argv[8]

masked_mean_abs = (masked_mean_abs_x_cov / coverage_frac) if coverage_frac > 0 else None

# tv-range 10-bit code -> approximate PQ-normalized luminance -> nits (BT.2100 PQ EOTF
# approximation is complex; this is a linear code-to-nits approximation good enough to
# report "roughly how bright" the threshold is, not a colorimetrically exact value):
# limited range 10-bit: PQ = (code - 64) / 876, then nits ~= 10000 * PQ^(1/0.1593) is the
# full PQ EOTF -- too complex for a one-line report; instead report the tv-range-adjusted
# normalized code value, which is what actually determines "highlight" here.
hi_thresh_tv_normalized = round((hi_thresh - 64) / 876.0, 4) if hi_thresh >= 64 else 0.0

result = {
    "title": title,
    "method": "fel_nlq_residual_ffmpeg_lutyuv (residual = EL - nlq_offset via ffprobe/lutyuv/"
              "signalstats filtergraphs, 10-bit native tv-range scale; mean|residual| only; "
              "NOT a full vs-nlq curve reconstruction; see docs/engineering/gh15-fel-visibility.md)",
    "nlq_offset_10bit": nlq_offset,
    "deadzone_threshold_10bit": deadzone,
    "highlight_threshold_10bit_code": hi_thresh,
    "highlight_threshold_tv_range_normalized": hi_thresh_tv_normalized,
    "highlight_mask_coverage_pct": round(coverage_frac * 100, 3),
    "full_frame_residual_mean_abs_10bit": round(full_mean_abs, 4),
    "full_frame_residual_mean_abs_normalized": round(full_mean_abs / 1023.0, 6),
    "highlight_residual_mean_abs_10bit": round(masked_mean_abs, 4) if masked_mean_abs is not None else None,
    "highlight_residual_mean_abs_normalized": round(masked_mean_abs / 1023.0, 6) if masked_mean_abs is not None else None,
    "highlight_vs_full_ratio": round(masked_mean_abs / full_mean_abs, 3) if (masked_mean_abs is not None and full_mean_abs > 0) else None,
}

with open(out_path, "w") as f:
    json.dump(result, f, indent=2)

print(json.dumps(result, indent=2), file=sys.stderr)
PYEOF

#!/usr/bin/env bash
# Scan a source title for a bright ~40s window using ffprobe/signalstats YAVG
# (10-bit native scale via the `movie=` filter's `seek_point` option + `-read_intervals`
# -- MEASURED this combination seeks without a full decode from start, unlike
# `-read_intervals` alone which errored "Could not seek" on a bare movie= source with
# no seek_point. See gh-15-notes.md), sampled every 5 minutes across the middle 80% of
# the runtime, then prints "start_seconds duration_seconds" for the highest-YAVG
# window found.
set -euo pipefail
FFPROBE=/usr/lib/jellyfin-ffmpeg/ffprobe
SRC="$1"

DUR_TOTAL=$("$FFPROBE" -v error -show_entries format=duration -of csv=p=0 "$SRC")
DUR_TOTAL=${DUR_TOTAL%.*}
SCAN_START=$(( DUR_TOTAL / 10 ))
SCAN_END=$(( DUR_TOTAL * 9 / 10 ))

BEST_START=0
BEST_YAVG=-1

for s in $(seq "$SCAN_START" 300 "$SCAN_END"); do
  yavg=$("$FFPROBE" -v error -f lavfi \
    -i "movie='$SRC':seek_point=$s,scale=640:-1,signalstats" \
    -read_intervals '%+#10' \
    -show_entries frame_tags=lavfi.signalstats.YAVG -of csv=p=0 2>/dev/null \
    | cut -d, -f1 | awk '{s+=$1; n++} END {if (n>0) print s/n; else print -1}')
  if [ -z "$yavg" ]; then yavg=-1; fi
  echo "t=$s YAVG=$yavg" >&2
  if awk -v a="$yavg" -v b="$BEST_YAVG" 'BEGIN{exit !(a>b)}'; then
    BEST_YAVG="$yavg"
    BEST_START="$s"
  fi
done

echo "$BEST_START 40"

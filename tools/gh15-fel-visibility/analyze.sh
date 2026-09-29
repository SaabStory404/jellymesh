#!/usr/bin/env bash
# jellymesh issue #15: DV7 FEL enhancement-layer visibility on bright content.
#
# For each title:
#   1. Cut a bright ~40s clip with ffmpeg -c copy (no re-encode).
#   2. Demux the clip's BL and EL HEVC streams with dovi_tool.
#   3. Extract + export the RPU to get nlq_offset/deadzone (frame 0).
#   4. Run residual.sh (ffprobe/signalstats filtergraphs, no numpy) to get
#      full-frame and highlight-masked mean |residual|.
#
# All intermediates go to $SCRATCH/gh15/<slug>/ and are deleted at the end of the run.
set -euo pipefail

FFMPEG=/usr/lib/jellyfin-ffmpeg/ffmpeg
FFPROBE=/usr/lib/jellyfin-ffmpeg/ffprobe
DOVI_TOOL=/usr/local/bin/dovi_tool

SCRATCH="${SCRATCH:-/transcodes/gh15}"
RESULTS="$SCRATCH/results"
mkdir -p "$RESULTS"

TITLE_SLUG="$1"
SRC="$2"
START="$3"
DUR="$4"
HI_THRESH="${5:-700}"   # 10-bit BL luma highlight threshold (~0.68 of full range)

WORK="$SCRATCH/$TITLE_SLUG"
rm -rf "$WORK"
mkdir -p "$WORK"
cd "$WORK"

echo "== $TITLE_SLUG: cutting ${DUR}s clip at ${START}s =="
"$FFMPEG" -hide_banner -loglevel warning -y \
  -ss "$START" -i "$SRC" -t "$DUR" \
  -map 0:v:0 -c copy -bsf:v hevc_mp4toannexb -f hevc clip.hevc
ls -l clip.hevc

echo "== $TITLE_SLUG: demuxing BL/EL =="
"$DOVI_TOOL" demux -i clip.hevc -b BL.hevc -e EL.hevc
ls -l BL.hevc EL.hevc

echo "== $TITLE_SLUG: extracting + exporting RPU for nlq_offset/deadzone =="
"$DOVI_TOOL" extract-rpu -i clip.hevc -o RPU.bin
"$DOVI_TOOL" info -s -i RPU.bin | tee rpu-summary.txt
"$DOVI_TOOL" export -i RPU.bin -d all=rpu.json

read -r NLQ_OFFSET DEADZONE <<< "$(python3 -c "
import json
d = json.load(open('rpu.json'))
f0 = d[0]
nlq = f0['rpu_data_mapping']['nlq']
print(nlq['nlq_offset'][0], nlq['linear_deadzone_threshold'][0])
")"
echo "NLQ_OFFSET=$NLQ_OFFSET DEADZONE=$DEADZONE"

EL_WH=$("$FFPROBE" -v error -select_streams v:0 -show_entries stream=width,height -of csv=p=0:s=x EL.hevc)
EL_W="${EL_WH%x*}"
EL_H="${EL_WH#*x}"
echo "EL resolution: $EL_W x $EL_H"

echo "== $TITLE_SLUG: residual analysis =="
bash /work/residual.sh BL.hevc EL.hevc "$EL_W" "$EL_H" "$NLQ_OFFSET" "$DEADZONE" "$HI_THRESH" \
  "$RESULTS/$TITLE_SLUG.json" "$TITLE_SLUG"

cat "$RESULTS/$TITLE_SLUG.json"
cp rpu-summary.txt "$RESULTS/$TITLE_SLUG.rpu-summary.txt"

echo "== $TITLE_SLUG: cleaning up intermediates =="
cd /
rm -rf "$WORK"

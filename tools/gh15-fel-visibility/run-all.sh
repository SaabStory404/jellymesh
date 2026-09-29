#!/usr/bin/env bash
# Driver for the gh15-fel-visibility Job. Runs in the jm6 jellyfin image (has
# ffmpeg/ffprobe at /usr/lib/jellyfin-ffmpeg/, avoids apt entirely -- MEASURED this
# node's egress stalls specifically inside a fresh debian:bookworm-slim container's
# apt-get, while wget/curl from the jm6 image and plain host egress both work fine;
# see gh-15-notes.md). Fetches dovi_tool via wget (GitHub egress confirmed working).
#
# Bright-window timestamps for iron-man and raiders are hardcoded (MEASURED this
# session via find_bright.sh: iron-man t=2555, raiders t=2491) to save the ~15-20 min
# each full bright-scan cost earlier this session; top-gun-maverick and walle are
# scanned fresh (5-min stride, same as before).
set -euo pipefail

echo "== fetching dovi_tool 2.3.4 static binary =="
mkdir -p /usr/local/bin
wget -q --timeout=30 -O /tmp/dovi_tool.tar.gz \
  https://github.com/quietvoid/dovi_tool/releases/download/2.3.4/dovi_tool-2.3.4-x86_64-unknown-linux-musl.tar.gz
tar -xzf /tmp/dovi_tool.tar.gz -C /usr/local/bin
chmod +x /usr/local/bin/dovi_tool
/usr/local/bin/dovi_tool --version

declare -A TITLES=(
  ["top-gun-maverick"]="/media/Top Gun - Maverick (2022)/Top.Gun.Maverick.2022.UHD.BluRay.2160p.TrueHD.Atmos.7.1.DV.HEVC.REMUX-FraMeSToR-AsRequested.mkv"
  ["walle"]="/media/WALL-E (2008)/WALL·E (2008) [Remux-2160p][DV HDR10Plus][TrueHD Atmos 7.1][HEVC]-LACTATO.mkv"
  ["iron-man"]="/media/Iron Man (2008)/Iron.Man.2008.UHD.BluRay.2160p.TrueHD.Atmos.7.1.DV.HEVC.HYBRID.REMUX-FraMeSToR-xpost.mkv"
  ["raiders"]="/media/Raiders of the Lost Ark (1981)/Raiders of the Lost Ark (1981) [Remux-2160p][DV HDR10][TrueHD Atmos 7.1][HEVC]-LEOVOORHEES.mkv"
)
declare -A KNOWN_START=(
  ["iron-man"]="2555"
  ["raiders"]="2491"
)

mkdir -p /transcodes/gh15/results

for slug in "${!TITLES[@]}"; do
  src="${TITLES[$slug]}"
  echo "############################################"
  echo "## $slug"
  echo "## $src"
  echo "############################################"
  if [ ! -f "$src" ]; then
    echo "MISSING: $src" | tee "/transcodes/gh15/results/$slug.MISSING.txt"
    continue
  fi

  if [ -n "${KNOWN_START[$slug]:-}" ]; then
    start="${KNOWN_START[$slug]}"
    dur=40
    echo "-- using known bright window from prior scan: start=${start}s --"
  else
    echo "-- scanning for bright window --"
    read -r start dur < <(bash /work/find_bright.sh "$src" | tail -1)
    echo "chosen window: start=${start}s dur=${dur}s"
  fi

  bash /work/analyze.sh "$slug" "$src" "$start" "$dur" 600 \
    || echo "FAILED: $slug" | tee "/transcodes/gh15/results/$slug.FAILED.txt"
done

echo "== all results =="
ls -la /transcodes/gh15/results/
for f in /transcodes/gh15/results/*.json; do
  [ -f "$f" ] || continue
  echo "--- $f ---"
  cat "$f"
done

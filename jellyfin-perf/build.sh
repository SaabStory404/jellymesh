#!/usr/bin/env bash
# Build Jellyfin v12.1 + jellyfin-12.1-perf.patch + the bughunt/ series (applied in numeric order)
# and collect the assemblies they change into $JF_OVERLAY (default ~/.cache/jellymesh-vendor/jellyfin-perf),
# for JG_OVERLAY in the lab. BUGHUNT=0 builds the perf patch alone.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
SRC=${JF_SRC:-$HOME/.cache/jellymesh-vendor/jellyfin-src}
OUT=${JF_OVERLAY:-$HOME/.cache/jellymesh-vendor/jellyfin-perf}
if [ ! -d "$SRC/.git" ]; then
  git clone --filter=blob:none https://github.com/jellyfin/jellyfin.git "$SRC"
fi
git -C "$SRC" fetch -q --tags origin
git -C "$SRC" checkout -q -f v12.1
# Drop files created by a previous run's patches (bin/obj are ignored, so the build cache survives).
git -C "$SRC" clean -fdq
git -C "$SRC" apply "$HERE/jellyfin-12.1-perf.patch"
if [ "${BUGHUNT:-1}" != 0 ]; then
  for p in "$HERE"/bughunt/[0-9][0-9]-*.patch; do
    git -C "$SRC" apply "$p"
  done
fi
dotnet build "$SRC/Jellyfin.Server/Jellyfin.Server.csproj" -c Release
mkdir -p "$OUT"
for a in Emby.Server.Implementations Jellyfin.Server.Implementations MediaBrowser.Controller MediaBrowser.MediaEncoding MediaBrowser.Model Jellyfin.Api jellyfin; do
  cp "$SRC/Jellyfin.Server/bin/Release/net10.0/$a.dll" "$OUT/"
done
echo "overlay in $OUT"

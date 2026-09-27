#!/usr/bin/env bash
# Build Jellyfin v12.1 + jellyfin-12.1-perf.patch and collect the assemblies it changes into
# $JF_OVERLAY (default ~/.cache/jellymesh-vendor/jellyfin-perf), for JG_OVERLAY in the lab.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
SRC=${JF_SRC:-$HOME/.cache/jellymesh-vendor/jellyfin-src}
OUT=${JF_OVERLAY:-$HOME/.cache/jellymesh-vendor/jellyfin-perf}
if [ ! -d "$SRC/.git" ]; then
  git clone --filter=blob:none https://github.com/jellyfin/jellyfin.git "$SRC"
fi
git -C "$SRC" fetch -q --tags origin
git -C "$SRC" checkout -q -f v12.1
git -C "$SRC" apply "$HERE/jellyfin-12.1-perf.patch"
dotnet build "$SRC/Jellyfin.Server/Jellyfin.Server.csproj" -c Release
mkdir -p "$OUT"
for a in Emby.Server.Implementations Jellyfin.Server.Implementations MediaBrowser.Controller; do
  cp "$SRC/Jellyfin.Server/bin/Release/net10.0/$a.dll" "$OUT/"
done
echo "overlay in $OUT"

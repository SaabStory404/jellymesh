#!/bin/sh
# initContainer: install/refresh the JellyMesh plugins into the shared /config (Jellyfin loads
# plugins only from $JELLYFIN_DATA_DIR/plugins). Same image version = same plugin version.
#
# jm7: never rewrite a DLL in place. The other replica may be running with that DLL loaded
# (mapped); overwriting its bytes crashed a live Jellyfin with BadImageFormatException "Bad IL
# range" (MEASURED, podman lab, jm6 process under a jm7 Galera DLL copy). Identical files are
# skipped; changed ones are written next to the target and renamed over it (new inode).
set -eu
DEST=${JELLYFIN_DATA_DIR:-/config/data}/plugins
for p in /opt/jellymesh/plugins/*/; do
  name=$(basename "$p")
  mkdir -p "$DEST/$name"
  for f in "$p"*.dll; do
    d="$DEST/$name/$(basename "$f")"
    if [ -f "$d" ] && cmp -s "$f" "$d"; then
      continue
    fi
    cp -f "$f" "$d.jmnew"
    mv -f "$d.jmnew" "$d"
    echo "jellymesh: updated $name/$(basename "$f")"
  done
  echo "jellymesh: installed $name"
done

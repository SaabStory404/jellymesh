#!/usr/bin/env bash
# Jellyfin 12.1 nodes on the Galera lab cluster, using a prepared real-library config.
#   jf-galera.sh up <name> <host-port> <galera-nodes>   e.g. up a 18501 gl-db1
#     <galera-nodes> may be a list (gl-db2,gl-db3,gl-db1): MySqlConnector connects to the first
#     reachable one (LoadBalance=FailOver) and moves on when it dies.
#   jf-galera.sh reload <name> <host-port>              copy a rebuilt plugin in and restart
#   jf-galera.sh down
# Env: JG_SRC  prepared config dir (scheduled tasks emptied, lab API key) — default ~/.cache/dbsidecar/s
#      JG_PLUGIN  built provider output dir
#      JG_OVERLAY dir of rebuilt Jellyfin server DLLs (a patched fork) mounted over the image's
#      JG_MESH    built JellyMesh plugin dir: installed, node joins the Redis mesh (JG_REDIS, default
#                 gl-redis:6379); JG_RC=1 also turns on the Redis response cache
set -euo pipefail
LAB=${JG_LAB:-$HOME/.cache/galera-lab}
SRC=${JG_SRC:-$HOME/.cache/dbsidecar/s}
PLUGIN=${JG_PLUGIN:-}
IMG=${JG_IMG:-ghcr.io/hotio/jellyfin:release-12.1}

case "${1:-}" in
  up)
    name=$2; port=$3; db=$4; dir="$LAB/jf-$name"
    if [ ! -d "$dir" ]; then
      podman unshare cp -a "$SRC" "$dir"
      podman unshare rm -rf "$dir/data/plugins/PostgreSQL Database Provider_3.0.2.0" "$dir/data/data/jellyfin.db" "$dir/database.xml"
      if [ -n "${JG_SQLITE:-}" ]; then
        # SQLite node for comparisons: a private copy of that database, stock provider.
        podman unshare cp "$JG_SQLITE" "$dir/data/data/jellyfin.db"
        podman unshare chown -R 1000:1000 "$dir"
      fi
    fi
    if [ ! -f "$dir/database.xml" ] && [ -z "${JG_SQLITE:-}" ]; then
      : "${PLUGIN:?JG_PLUGIN: built Galera provider output dir}"
      plug="$dir/data/plugins/JellyMesh Galera_1.0.0.0"
      podman unshare mkdir -p "$plug"
      # Everything the provider ships, minus assemblies the Jellyfin server already loads.
      for f in "$PLUGIN"/*.dll; do
        case "$(basename "$f")" in Microsoft.EntityFrameworkCore*|Jellyfin.*Implementations*) [ "$(basename "$f")" = Jellyfin.Database.Providers.Galera.dll ] || continue ;; esac
        podman unshare cp "$f" "$plug/"
      done
      cat > "$LAB/.database.xml" <<EOF
<?xml version="1.0" encoding="utf-8"?>
<DatabaseConfigurationOptions xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">
  <DatabaseType>PLUGIN_PROVIDER</DatabaseType>
  <LockingBehavior>NoLock</LockingBehavior>
  <CustomProviderOptions>
    <PluginName>JellyMesh Galera</PluginName>
    <PluginAssembly>Jellyfin.Database.Providers.Galera.dll</PluginAssembly>
    <ConnectionString>Server=$db;Database=jellyfin;Uid=jellyfin;Pwd=jellyfin;SslMode=Disabled;AllowPublicKeyRetrieval=true;LoadBalance=FailOver</ConnectionString>
  </CustomProviderOptions>
</DatabaseConfigurationOptions>
EOF
      podman unshare cp "$LAB/.database.xml" "$dir/database.xml"
      podman unshare chown -R 1000:1000 "$dir"
    fi
    mesh_env=()
    if [ -n "${JG_MESH:-}" ]; then
      mplug="$dir/data/plugins/JellyMesh_2.0.0.0"
      podman unshare mkdir -p "$mplug"
      for f in "$JG_MESH"/JellyMesh.dll "$JG_MESH"/StackExchange.Redis.dll "$JG_MESH"/Pipelines.Sockets.Unofficial.dll "$(dirname "$0")/../../mesh/meta.json"; do
        podman unshare cp "$f" "$mplug/"
      done
      podman unshare chown -R 1000:1000 "$mplug"
      mesh_env=(-e JELLYMESH_REDIS="${JG_REDIS:-gl-redis:6379}" -e JELLYMESH_SHARED_DB=1 -e JELLYMESH_RESPONSE_CACHE="${JG_RC:-0}")
    fi
    # JG_SHARED=1: patched fork's JELLYFIN_SHARED_DB (no per-node caches; coherent without a plugin)
    [ "${JG_SHARED:-0}" = 1 ] && mesh_env+=(-e JELLYFIN_SHARED_DB=1)
    overlay=()
    if [ -n "${JG_OVERLAY:-}" ]; then
      for f in "$JG_OVERLAY"/*.dll; do overlay+=(-v "$f:/usr/lib/jellyfin/bin/$(basename "$f"):ro,z"); done
    fi
    podman run -d --name "jg-$name" --network gl -p "$port:8096" -e PUID=1000 -e PGID=1000 \
      -e JELLYMESH_NODE="$name" "${mesh_env[@]}" -v "$dir:/config:Z" "${overlay[@]}" "$IMG" >/dev/null
    timeout 900 bash -c "until curl -sf -H 'Authorization: MediaBrowser Token=\"jmlabkey0000000000000000000000001\"' localhost:$port/Users >/dev/null; do sleep 3; done"
    ;;
  reload)  # reload <name> <host-port>: copy a rebuilt plugin in, restart, wait until the API answers
    name=$2; port=$3; plug="$LAB/jf-$name/data/plugins/JellyMesh Galera_1.0.0.0"
    for f in "$PLUGIN"/*.dll; do
      case "$(basename "$f")" in Microsoft.EntityFrameworkCore*|Jellyfin.*Implementations*) [ "$(basename "$f")" = Jellyfin.Database.Providers.Galera.dll ] || continue ;; esac
      podman unshare cp "$f" "$plug/"
    done
    podman restart "jg-$name" >/dev/null
    sleep 3
    timeout 900 bash -c "until curl -sf -H 'Authorization: MediaBrowser Token=\"jmlabkey0000000000000000000000001\"' localhost:$port/Users >/dev/null; do sleep 2; done"
    ;;
  down)
    podman rm -f $(podman ps -a --format '{{.Names}}' | grep '^jg-' || true) >/dev/null 2>&1 || true
    ;;
  *) echo "usage: $0 up <name> <port> <galera-node> | down" >&2; exit 2 ;;
esac

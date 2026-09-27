#!/usr/bin/env bash
# Galera lab: Percona XtraDB Cluster 8.4 nodes on a podman network.
#   galera-lab.sh boot            first node (bootstraps the cluster) -> host port 13306
#   galera-lab.sh join <n>        node n (2, 3, ...) joins via node 1  -> host port 1330<n>
#   galera-lab.sh db              create database jellyfin (utf8mb4_bin) + user
#   galera-lab.sh status          wsrep cluster size / state per node
#   galera-lab.sh down            remove nodes AND their data volumes
set -euo pipefail
IMG=docker.io/percona/percona-xtradb-cluster:8.4
ROOTPW=${GL_ROOTPW:-labroot}
NET=gl
CERTS=${GL_CERTS:-$HOME/.cache/galera-lab/certs}
# Cluster traffic stays encrypted (PXC 8 default): every node shares one CA + server cert, which
# the xtrabackup state transfer requires even with encryption off (MEASURED: joiner aborts with
# "Could not find a CA file").
# PERMISSIVE: EF Core takes its migration lock with GET_LOCK, which ENFORCING rejects on PXC
# (named locks are node-local in Galera). Migrations run from one node in any case.
ARGS=(--innodb-buffer-pool-size=768M --max-connections=500)

certs() {
  [ -f "$CERTS/ca.pem" ] || {
    mkdir -p "$CERTS" && cd "$CERTS"
    openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj "/CN=jellymesh-lab-ca" -keyout ca-key.pem -out ca.pem 2>/dev/null
    openssl req -newkey rsa:2048 -nodes -subj "/CN=galera-node" -keyout server-key.pem -out server-req.pem 2>/dev/null
    openssl x509 -req -in server-req.pem -days 3650 -CA ca.pem -CAkey ca-key.pem -set_serial 01 -out server-cert.pem 2>/dev/null
    chmod 644 ./*.pem   # lab: readable by the container's mysql user
    cd - >/dev/null
  }
  # The SST script reads TLS settings from config files (my_print_defaults), not command-line
  # flags, so they live in a mounted cnf (MEASURED: flags alone -> "Could not find a CA file").
  cat > "$CERTS/../jellymesh.cnf" <<'EOF'
[mysqld]
pxc_strict_mode=PERMISSIVE
# Jellyfin's grids DISTINCT over whole BaseItems rows (multi-KB Data JSON); at the 16M default
# those temp tables spill to disk (MEASURED: Audio grid COUNT 2 disk tmp tables, -60 ms at 256M).
tmp_table_size=256M
ssl-ca=/certs/ca.pem
ssl-cert=/certs/server-cert.pem
ssl-key=/certs/server-key.pem
[sst]
ssl-ca=/certs/ca.pem
ssl-cert=/certs/server-cert.pem
ssl-key=/certs/server-key.pem
EOF
}

node() {  # n [extra env...]
  local n=$1; shift
  certs
  podman run -d --name "gl-db$n" --network "$NET" -p "1330$n:3306" \
    -e MYSQL_ROOT_PASSWORD="$ROOTPW" -e CLUSTER_NAME=jellymesh "$@" \
    -v "gl-db$n-data:/var/lib/mysql" -v "$CERTS:/certs:ro,z" \
    -v "$CERTS/../jellymesh.cnf:/etc/my.cnf.d/jellymesh.cnf:ro,z" "$IMG" "${ARGS[@]}" >/dev/null
}

ready() {  # n [boot]
  # The bootstrap node's entrypoint first runs a temporary init server (Galera on, so wsrep_ready
  # is ON), then restarts. Joining or writing during that window crashed the donor (MEASURED), so
  # wait for the entrypoint to finish. Joiners get their data by SST and never print that line.
  if [ "${2:-}" = boot ]; then
    timeout 600 bash -c "until podman logs gl-db$1 2>&1 | grep -q 'MySQL init process done'; do sleep 2; done"
  fi
  timeout 600 bash -c "until podman exec gl-db$1 mysql -uroot -p$ROOTPW -Nse \"SHOW STATUS LIKE 'wsrep_local_state_comment'\" 2>/dev/null | grep -q Synced; do sleep 2; done"
}

case "${1:-}" in
  boot)
    podman network exists "$NET" || podman network create "$NET" >/dev/null
    node 1
    ready 1 boot
    ;;
  join)
    node "$2" -e CLUSTER_JOIN=gl-db1
    ready "$2"
    ;;
  rejoin)
    # A node that died (even the one that bootstrapped) must come back as a joiner: restarted with
    # its bootstrap settings it refuses with "It may not be safe to bootstrap the cluster from this
    # node" (MEASURED). Keeps its data volume, so the gap is an incremental transfer (IST).
    live=$(podman ps --format '{{.Names}}' | grep '^gl-db' | grep -vx "gl-db$2" | head -1)
    podman rm -f "gl-db$2" >/dev/null
    node "$2" -e CLUSTER_JOIN="$live"
    ready "$2"
    ;;
  db)
    podman exec gl-db1 mysql -uroot -p"$ROOTPW" -e "
      CREATE DATABASE IF NOT EXISTS jellyfin CHARACTER SET utf8mb4 COLLATE utf8mb4_bin;
      CREATE USER IF NOT EXISTS 'jellyfin'@'%' IDENTIFIED BY 'jellyfin';
      GRANT ALL ON jellyfin.* TO 'jellyfin'@'%';"
    ;;
  status)
    for c in $(podman ps --format '{{.Names}}' | grep '^gl-db'); do
      printf '%s ' "$c"
      podman exec "$c" mysql -uroot -p"$ROOTPW" -Nse "SHOW STATUS WHERE Variable_name IN ('wsrep_cluster_size','wsrep_local_state_comment','wsrep_ready')" 2>/dev/null | tr '\n\t' '  '
      echo
    done
    ;;
  down)
    for c in $(podman ps -a --format '{{.Names}}' | grep '^gl-db' || true); do podman rm -f -v "$c" >/dev/null; podman volume rm -f "$c-data" >/dev/null 2>&1 || true; done
    ;;
  *) echo "usage: $0 boot|join <n>|db|status|down" >&2; exit 2 ;;
esac

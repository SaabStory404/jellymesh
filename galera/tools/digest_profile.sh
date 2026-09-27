#!/usr/bin/env bash
# Per-statement profile of one Jellyfin API call against a Galera lab node, from
# performance_schema digests: how many statements, and how much time the server spent on them.
#   digest_profile.sh <jellyfin-url> <path?query> [runs] [galera-container]
set -euo pipefail
URL=$1; CALL=$2; RUNS=${3:-10}; DB=${4:-gl-db1}
TOKEN=${JM_TOKEN:-jmlabkey0000000000000000000000001}
my() { podman exec "$DB" mysql -uroot -p"${GL_ROOTPW:-labroot}" -N -e "$1" 2>/dev/null; }

curl -sf -o /dev/null -H "Authorization: MediaBrowser Token=\"$TOKEN\"" "$URL$CALL"   # warm
my "TRUNCATE performance_schema.events_statements_summary_by_digest"
start=$(date +%s%N)
for _ in $(seq "$RUNS"); do
  curl -sf -o /dev/null -H "Authorization: MediaBrowser Token=\"$TOKEN\"" "$URL$CALL"
done
wall=$(( ($(date +%s%N) - start) / 1000000 / RUNS ))
echo "wall per call: ${wall} ms"
my "SELECT CONCAT('statements per call: ', ROUND(SUM(COUNT_STAR)/$RUNS,1), '   server ms per call: ', ROUND(SUM(SUM_TIMER_WAIT)/1e9/$RUNS,1))
    FROM performance_schema.events_statements_summary_by_digest WHERE SCHEMA_NAME='jellyfin'"
echo "top digests (calls/run, server ms/run, text):"
my "SELECT ROUND(COUNT_STAR/$RUNS,1), ROUND(SUM_TIMER_WAIT/1e9/$RUNS,1), LEFT(REPLACE(DIGEST_TEXT, '\n', ' '), 150)
    FROM performance_schema.events_statements_summary_by_digest WHERE SCHEMA_NAME='jellyfin'
    ORDER BY SUM_TIMER_WAIT DESC LIMIT 8"

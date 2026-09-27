#!/bin/sh
# tcpool-entry: SIGTERM reaches the child, and PID 1 is the wrapper, not the agent.
set -eu
HERE=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# Stub "agent": traps TERM, records that it got it, exits 17 like a drain would.
cat > "$tmp/agent" <<'STUB'
#!/bin/sh
trap 'echo drained > "$TC_TEST_OUT"; exit 17' TERM
while :; do sleep 0.1; done
STUB
chmod +x "$tmp/agent"

TC_TEST_OUT="$tmp/got" TC_AGENT_BIN="$tmp/agent" "$HERE/tcpool-entry" &
wrapper=$!
i=0
while [ ! -d "/proc/$wrapper/task" ] && [ "$i" -lt 50 ]; do i=$((i + 1)); done
sleep 1
kill -TERM "$wrapper"
code=0
wait "$wrapper" || code=$?

[ "$(cat "$tmp/got" 2>/dev/null)" = drained ] || { echo "FAIL: child never saw SIGTERM"; exit 1; }
[ "$code" -eq 17 ] || { echo "FAIL: wrapper exited $code, want the child's 17"; exit 1; }
echo "ok: SIGTERM relayed to the agent, child exit status preserved ($code)"

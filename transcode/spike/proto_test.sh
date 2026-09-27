#!/usr/bin/env bash
# Local test of the shim<->agent protocol: normal stop, frozen worker (shim must give up after
# TC_DEAD_AFTER), frozen shim (agent must fence its ffmpeg after TC_FENCE_AFTER). Needs ffmpeg.
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
# Which implementation to test: the Python spike (default) or the native binaries, e.g.
#   AGENT="$PWD/transcode/target/debug/tcpool-agent" SHIM="$PWD/transcode/target/debug/tcpool-shim" bash proto_test.sh
AGENT=${AGENT:-python3 $HERE/agent.py}
SHIM=${SHIM:-python3 $HERE/shim.py}
# native shim: local fallback runs this, and it logs here
export TC_FFMPEG_REAL=${TC_FFMPEG_REAL:-ffmpeg}
T=${TC_PROTO_DIR:-$(mktemp -d)}
mkdir -p "$T/out"
# native agent allowlist: this suite reads and writes under $T
export TC_INPUT_ROOTS="$T" TC_OUTPUT_ROOT="$T"
ffmpeg -hide_banner -loglevel error -y -f lavfi -i testsrc2=duration=60:size=640x360:rate=24 \
  -c:v libx264 -preset ultrafast "$T/src.mkv"

export TC_WORKERS=cpu=127.0.0.1:19901 TC_SHIM_LOG="$T/shim.log"
TC_KIND=cpu TC_NAME=cpu TC_FFMPEG=ffmpeg TC_LOG="$T/agent.log" TC_PORT=19901 $AGENT >> "$T/agent.log" 2>&1 &
AG=$!
ARGS=(-re -i "$T/src.mkv" -c:v libx264 -preset ultrafast -force_key_frames "expr:gte(t,n_forced*3)" -f hls -hls_time 3 -hls_list_size 0
      -hls_segment_filename "$T/out/s%d.ts" "$T/out/p.m3u8")
enc() { pgrep -f "ffmpeg -re -i $T/src.mkv"; }
# agents probe their encoders before listening
listening() { until grep -q -E "listening on (0.0.0.0)?:$1" "$T/agent.log" 2>/dev/null; do sleep 0.2; done; }
listening 19901

echo "== 1: normal run, Jellyfin-style 'q' after 4 s"
( sleep 4; printf q ) | timeout 20 $SHIM "${ARGS[@]}" 2>/dev/null
echo "exit=$? segments=$(ls "$T/out" | grep -c ts)"

echo "== 2: freeze the worker (agent + ffmpeg): shim should exit 255 after ~6 s"
rm -f "$T"/out/*
sleep 30 | $SHIM "${ARGS[@]}" 2>/dev/null &
SH=$!
# freeze only once segment 0 exists (mid-stream): before it, a lost worker is re-run, not exited
until [ -e "$T/out/s0.ts" ]; do sleep 0.2; done
kill -STOP "$AG" $(enc)
t0=$SECONDS
while kill -0 "$SH" 2>/dev/null; do sleep 0.2; done
echo "shim gone after $((SECONDS - t0)) s"
kill -CONT "$AG" $(enc) 2>/dev/null
sleep 2
enc >/dev/null && echo "thawed ffmpeg STILL RUNNING (zombie writer)" || echo "thawed ffmpeg gone"

echo "== 3: freeze the shim: agent should kill ffmpeg after ~3 s"
rm -f "$T"/out/*
sleep 30 | $SHIM "${ARGS[@]}" 2>/dev/null &
SH=$!
sleep 3
kill -STOP "$SH"
sleep 5
enc >/dev/null && echo "ffmpeg STILL RUNNING" || echo "ffmpeg fenced"
kill -9 "$SH"

echo "== 4: first worker frozen before connect: shim should skip it (no hello) and use the second"
TC_KIND=cpu TC_NAME=cpu2 TC_FFMPEG=ffmpeg TC_LOG="$T/agent.log" TC_PORT=19902 $AGENT >> "$T/agent.log" 2>&1 &
AG2=$!
listening 19902
kill -STOP "$AG"
rm -f "$T"/out/*
t0=$SECONDS
( sleep 4; printf q ) | TC_WORKERS=cpu=127.0.0.1:19901,cpu2=127.0.0.1:19902 timeout 20 $SHIM "${ARGS[@]}" 2>/dev/null
echo "exit=$? in $((SECONDS - t0)) s segments=$(ls "$T/out" | grep -c ts)"
kill -CONT "$AG"

echo "== 5: first worker killed before the first segment: shim should re-run on the second, not exit"
rm -f "$T"/out/*
( sleep 6; printf q ) | TC_WORKERS=cpu=127.0.0.1:19901,cpu2=127.0.0.1:19902 timeout 20 $SHIM "${ARGS[@]}" 2>/dev/null &
SH=$!
sleep 0.7
kill -9 "$AG" $(enc)
wait "$SH"
echo "exit=$? segments=$(ls "$T/out" | grep -c ts)"
echo "== 6: first worker's ffmpeg exits 1 before any segment: shim should re-run on the second"
# passes the startup probe (-f null), fails every HLS job
printf '#!/bin/sh\ncase "$*" in *.m3u8*) exit 1;; esac\nexec ffmpeg "$@"\n' > "$T/ff-fail"
chmod +x "$T/ff-fail"
TC_KIND=cpu TC_NAME=bad TC_FFMPEG="$T/ff-fail" TC_LOG="$T/agent.log" TC_PORT=19903 $AGENT >> "$T/agent.log" 2>&1 &
AG3=$!
listening 19903
rm -f "$T"/out/*
( sleep 4; printf q ) | TC_WORKERS=bad=127.0.0.1:19903,cpu2=127.0.0.1:19902 timeout 20 $SHIM "${ARGS[@]}" 2>/dev/null
echo "exit=$? segments=$(ls "$T/out" | grep -c ts)"
kill "$AG3"

echo "== 7: first worker cannot output HEVC: an HEVC job must skip it"
TC_KIND=cpu TC_NAME=h264only TC_OUTPUTS=h264 TC_FFMPEG=ffmpeg TC_LOG="$T/agent.log" TC_PORT=19904 $AGENT >> "$T/agent.log" 2>&1 &
AG4=$!
listening 19904
rm -f "$T"/out/*
HEVC=(-re -i "$T/src.mkv" -c:v libx265 -preset ultrafast -f hls -hls_time 3 -hls_list_size 0
      -hls_segment_filename "$T/out/s%d.ts" "$T/out/p.m3u8")
( sleep 4; printf q ) | TC_WORKERS=h264only=127.0.0.1:19904,cpu2=127.0.0.1:19902 timeout 20 $SHIM "${HEVC[@]}" 2>/dev/null
echo "exit=$? segments=$(ls "$T/out" | grep -c ts) worker=$(grep -o 'transcode -> worker [a-z0-9]*' "$T/shim.log" | tail -1)"
kill "$AG4"

echo "== 8: worker at TC_MAX_JOBS=1 with a job running: a second job must go to the next worker"
TC_KIND=cpu TC_NAME=one TC_MAX_JOBS=1 TC_FFMPEG=ffmpeg TC_LOG="$T/agent.log" TC_PORT=19905 $AGENT >> "$T/agent.log" 2>&1 &
AG5=$!
listening 19905
mkdir -p "$T/out2"
ARGS2=(-re -i "$T/src.mkv" -c:v libx264 -preset ultrafast -f hls -hls_time 3 -hls_list_size 0
       -hls_segment_filename "$T/out2/s%d.ts" "$T/out2/p.m3u8")
( sleep 8; printf q ) | TC_WORKERS=one=127.0.0.1:19905 timeout 20 $SHIM "${ARGS2[@]}" 2>/dev/null &
SH=$!
sleep 1.5
rm -f "$T"/out/*
( sleep 4; printf q ) | TC_WORKERS=one=127.0.0.1:19905,cpu2=127.0.0.1:19902 timeout 20 $SHIM "${ARGS[@]}" 2>/dev/null
echo "exit=$? segments=$(ls "$T/out" | grep -c ts) worker=$(grep -o 'transcode -> worker [a-z0-9]*' "$T/shim.log" | tail -1)"
wait "$SH"
kill "$AG5"

echo "== 9: weighted capacity: 3-unit worker running a 4K job (3 units) must refuse a second job"
ffmpeg -hide_banner -loglevel error -y -f lavfi -i testsrc2=duration=30:size=3840x2160:rate=24 \
  -c:v libx264 -preset ultrafast "$T/src4k.mkv"
TC_KIND=cpu TC_NAME=weighted TC_CAPACITY=3 TC_FFMPEG=ffmpeg TC_LOG="$T/agent.log" TC_PORT=19906 $AGENT >> "$T/agent.log" 2>&1 &
AG6=$!
listening 19906
mkdir -p "$T/out4k"
ARGS4K=(-re -i "$T/src4k.mkv" -vf scale=-2:360 -c:v libx264 -preset ultrafast -f hls -hls_time 3 -hls_list_size 0
        -hls_segment_filename "$T/out4k/s%d.ts" "$T/out4k/p.m3u8")
( sleep 8; printf q ) | TC_WORKERS=weighted=127.0.0.1:19906 timeout 20 $SHIM "${ARGS4K[@]}" 2>/dev/null &
SH=$!
sleep 2
rm -f "$T"/out/*
( sleep 4; printf q ) | TC_WORKERS=weighted=127.0.0.1:19906,cpu2=127.0.0.1:19902 timeout 20 $SHIM "${ARGS[@]}" 2>/dev/null
echo "exit=$? worker=$(grep -o 'transcode -> worker [a-z0-9]*' "$T/shim.log" | tail -1)"
grep -o "accepted a [0-9.]*-unit job\|refused a [0-9.]*-unit job" "$T/agent.log" | tail -2 | tr '\n' ' '; echo
wait "$SH"
kill "$AG6" "$AG2"
TC_KIND=cpu TC_NAME=cpu TC_FFMPEG=ffmpeg TC_LOG="$T/agent.log" TC_PORT=19901 $AGENT >> "$T/agent.log" 2>&1 &
AG=$!

case "$AGENT" in *python*) NATIVE= ;; *) NATIVE=1 ;; esac
if [ -n "$NATIVE" ]; then
  # native-only: progress watchdog and graceful drain
  TC_KIND=cpu TC_NAME=w TC_STALL_AFTER=4 TC_FFMPEG=ffmpeg TC_PORT=19907 $AGENT >> "$T/agent.log" 2>&1 &
  AG7=$!
  listening 19907
  export TC_WORKERS=w=127.0.0.1:19907

  echo "== 10: ffmpeg hangs under a healthy agent: the watchdog must end it (~4 s), shim exits non-zero"
  rm -f "$T"/out/*
  sleep 30 | $SHIM "${ARGS[@]}" 2>/dev/null &
  SH=$!
  until [ -e "$T/out/s0.ts" ]; do sleep 0.2; done
  kill -STOP $(enc)
  t0=$SECONDS
  while kill -0 "$SH" 2>/dev/null; do sleep 0.2; done; dt=$((SECONDS - t0)); wait "$SH"; code=$?
  echo "stalled: shim exit=$code after $dt s"
  kill -CONT $(enc) 2>/dev/null
  sleep 1
  enc >/dev/null && echo "stalled ffmpeg STILL RUNNING" || echo "stalled ffmpeg gone"

  echo "== 11: Jellyfin pauses ffmpeg (p): no progress while paused is not a stall"
  rm -f "$T"/out/*
  ( until [ -e "$T/out/s0.ts" ]; do sleep 0.2; done; printf p; kill -STOP $(enc); sleep 8; kill -CONT $(enc); printf u; sleep 3; printf q ) \
    | timeout 30 $SHIM "${ARGS[@]}" 2>/dev/null
  echo "paused: exit=$?"

  echo "== 12: SIGTERM the agent mid-stream: it drains after the next segment, then exits"
  rm -f "$T"/out/*
  sleep 30 | $SHIM "${ARGS[@]}" 2>/dev/null &
  SH=$!
  until [ -e "$T/out/s0.ts" ]; do sleep 0.2; done
  t0=$SECONDS
  kill -TERM "$AG7"
  while kill -0 "$SH" 2>/dev/null; do sleep 0.2; done; dt=$((SECONDS - t0)); wait "$SH"; code=$?
  while kill -0 "$AG7" 2>/dev/null; do sleep 0.2; done
  segs=$(ls "$T/out" | grep -c '\.ts$')
  echo "drained: shim exit=$code after $dt s, agent $(grep -o "drained in [0-9.]*s" "$T/agent.log" | tail -1) complete_segments=$segs tmp_left=$(ls "$T/out" | grep -c tmp)"

  echo "== 13: a job reading outside the allowed roots must be refused by the agent"
  TC_KIND=cpu TC_NAME=strict TC_FFMPEG=ffmpeg TC_PORT=19908 TC_INPUT_ROOTS=/nonexistent $AGENT >> "$T/agent.log" 2>&1 &
  AG8=$!
  listening 19908
  rm -f "$T"/out/*
  ( sleep 4; printf q ) | TC_WORKERS=strict=127.0.0.1:19908 timeout 20 $SHIM "${ARGS[@]}" 2>/dev/null
  echo "refused: exit=$? agent_refusals=$(grep -c 'REFUSED a job' "$T/agent.log") fallback=$(grep -c 'running LOCALLY' "$T/shim.log")"
  kill "$AG8"

  echo "== 14: mTLS: a client with a pool certificate runs; one without is refused at the handshake"
  P=$T/pki; mkdir -p "$P"
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 1 -subj /CN=tcpool-ca \
    -keyout "$P/ca.key" -out "$P/ca.crt" 2>/dev/null
  for who in agent client; do
    san="DNS:tcpool-agent"; [ $who = client ] && san="DNS:tcpool-client"
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj /CN=tcpool-$who \
      -keyout "$P/$who.key" -out "$P/$who.csr" 2>/dev/null
    printf "subjectAltName=%s\nextendedKeyUsage=%s\n" "$san" "$([ $who = agent ] && echo serverAuth || echo clientAuth)" > "$P/$who.ext"
    openssl x509 -req -in "$P/$who.csr" -CA "$P/ca.crt" -CAkey "$P/ca.key" -CAcreateserial -days 1 \
      -extfile "$P/$who.ext" -out "$P/$who.crt" 2>/dev/null
  done
  TC_KIND=cpu TC_NAME=tls TC_FFMPEG=ffmpeg TC_PORT=19909 TC_HEALTH_PORT=19910 TC_TLS_REQUIRED=1 \
    TC_TLS_CERT="$P/agent.crt" TC_TLS_KEY="$P/agent.key" TC_TLS_CA="$P/ca.crt" $AGENT >> "$T/agent.log" 2>&1 &
  AG9=$!
  listening 19909
  rm -f "$T"/out/*
  ( sleep 4; printf q ) | TC_WORKERS=tls=127.0.0.1:19909 TC_TLS_CERT="$P/client.crt" TC_TLS_KEY="$P/client.key" \
    TC_TLS_CA="$P/ca.crt" timeout 20 $SHIM "${ARGS[@]}" 2>/dev/null
  ok=$?; ranok=$(grep -c 'transcode -> worker tls' "$T/shim.log")
  before=$(grep -c 'running LOCALLY' "$T/shim.log")
  ( sleep 4; printf q ) | TC_WORKERS=tls=127.0.0.1:19909 timeout 20 $SHIM "${ARGS[@]}" 2>/dev/null
  after=$(grep -c 'running LOCALLY' "$T/shim.log")
  echo "mtls: with-cert exit=$ok ran_on_agent=$ranok; without-cert fell_back=$((after - before))"
  kill "$AG9"
fi

sleep 1
kill "$AG"
grep -hv "start:\|transcode ->" "$T/agent.log" "$T/shim.log" | cut -c1-180

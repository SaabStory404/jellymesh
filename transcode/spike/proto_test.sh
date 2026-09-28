#!/usr/bin/env bash
# Local test of the shim<->agent protocol: normal stop, frozen worker (shim must give up after
# TC_DEAD_AFTER), frozen shim (agent must fence its ffmpeg after TC_FENCE_AFTER), plus (native-only)
# the BATCH (trickplay) admission/preemption path. Needs ffmpeg.
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

# Cases 1-19 share "$T/out/p.m3u8" (and a few other paths) across unrelated jobs -- a fixture
# artifact of this suite reusing output dirs, not a real Jellyfin session -- so seek affinity
# (which keys on exactly that output prefix) is off here and scoped on only for case 20, where
# it's the feature under test, to keep every case's worker-selection assertion exactly as it was.
export TC_WORKERS=cpu=127.0.0.1:19901 TC_SHIM_LOG="$T/shim.log" TC_AFFINITY=0
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
grep -o "accepted a [0-9.]*-unit [a-z]* job\|refused a [0-9.]*-unit [a-z]* job" "$T/agent.log" | tail -2 | tr '\n' ' '; echo
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

  # BATCH (trickplay): TC_BATCH=1 routes a mjpeg/%0Nd.jpg command to the pool at Priority::Batch,
  # under TC_TRICKPLAY_OUTPUT_ROOT only; playback preempts a running batch job. Worker names are
  # "batchw"/"batchslow" (never "cpu2", which the fixed-count grep above already owns) and every
  # new case's echo is prefixed "batch:"/"preempt-early:"/"preempt-late:" so none of it begins
  # with "exit=0" at column 0.
  ffmpeg -hide_banner -loglevel error -y -f lavfi -i testsrc2=duration=10:size=320x180:rate=24 \
    -c:v libx264 -preset ultrafast "$T/srcbatch.mkv"
  ffmpeg -hide_banner -loglevel error -y -f lavfi -i testsrc2=duration=6:size=320x180:rate=24 \
    -c:v libx264 -preset ultrafast "$T/srcpreempt.mkv"
  mkdir -p "$T/trickplay"
  TC_KIND=cpu TC_NAME=batchw TC_CAPACITY=1 TC_BATCH_HEADROOM=0 TC_FFMPEG=ffmpeg TC_PORT=19911 \
    TC_TRICKPLAY_OUTPUT_ROOT="$T/trickplay" $AGENT >> "$T/agent.log" 2>&1 &
  AGB=$!
  listening 19911
  tpargs() { # $1 = source, $2 = output dir
    printf '%s\n' -loglevel error -threads 1 -i "$1" -map 0:0 -an -sn -vf fps=1,scale=160:-2 \
      -threads 1 -c:v mjpeg -qscale:v 4 -vsync 0 -f image2 "$2/%08d.jpg"
  }

  # "same number of jpgs as local ffmpeg" is checked as an explicit match=yes/no against a real
  # local run, not a hardcoded frame count -- the exact count is an ffmpeg-version artifact (fps
  # filter EOF flushing), not the property under test.
  match() { [ "$1" = "$2" ] && [ "$1" -gt 0 ] && echo yes || echo no; }

  echo "== 15: TC_BATCH=1 trickplay job under the output root runs on the agent"
  mkdir -p "$T/trickplay/a" "$T/trickplay/a-local"
  mapfile -t TPARGS < <(tpargs "$T/srcbatch.mkv" "$T/trickplay/a")
  TC_BATCH=1 TC_TRICKPLAY_OUTPUT_ROOT="$T/trickplay" TC_WORKERS=batchw=127.0.0.1:19911 \
    timeout 20 $SHIM "${TPARGS[@]}" < /dev/null 2>/dev/null
  code=$?
  pool_frames=$(ls "$T/trickplay/a" | grep -c jpg)
  mapfile -t TPARGS_LOCAL < <(tpargs "$T/srcbatch.mkv" "$T/trickplay/a-local")
  ffmpeg "${TPARGS_LOCAL[@]}" 2>/dev/null
  local_frames=$(ls "$T/trickplay/a-local" | grep -c jpg)
  # cases 16/18/19 reuse this source+filter unmodified (routed to exec_real, never rendered), so
  # their local-fallback frame count is expected to match this same reference.
  EXPECT_FRAMES=$local_frames
  echo "batch: on-pool exit=$code frames_match=$(match "$pool_frames" "$local_frames") worker=$(grep -o 'transcode -> worker [a-z0-9]*' "$T/shim.log" | tail -1) accepted=$(grep -c 'accepted a [0-9.]*-unit batch job' "$T/agent.log")"

  echo "== 16: trickplay output outside TC_TRICKPLAY_OUTPUT_ROOT runs locally, agent never sees it"
  mkdir -p "$T/outside-trickplay"
  mapfile -t TPARGS_OUT < <(tpargs "$T/srcbatch.mkv" "$T/outside-trickplay")
  before=$(grep -c 'transcode -> worker batchw' "$T/shim.log")
  TC_BATCH=1 TC_TRICKPLAY_OUTPUT_ROOT="$T/trickplay" TC_WORKERS=batchw=127.0.0.1:19911 \
    $SHIM "${TPARGS_OUT[@]}" < /dev/null 2>/dev/null
  code=$?
  after=$(grep -c 'transcode -> worker batchw' "$T/shim.log")
  frames=$(ls "$T/outside-trickplay" | grep -c jpg)
  echo "batch: outside-root exit=$code frames_match=$(match "$frames" "$EXPECT_FRAMES") pool_attempts=$((after - before))"

  # local-run canary: TC_FFMPEG_REAL points here so we can prove, by line count, whether the
  # shim's own local fallback (exec_real) ever ran ffmpeg -- not by racing pgrep against a
  # process that may already have exited.
  printf '#!/bin/sh\necho "$@" >> "%s/local-runs"\nexec ffmpeg "$@"\n' "$T" > "$T/ff-canary"
  chmod +x "$T/ff-canary"
  local_run_count() { wc -l < "$T/local-runs" 2>/dev/null || echo 0; }

  echo "== 17a: a playback job preempts a batch job BEFORE its first frame; it reruns locally"
  # A slow-starting worker: every ffmpeg invocation (startup probe, this job) sleeps 5s before
  # exec-ing the real binary, so a preemption fired the instant admission is confirmed always
  # lands well before any frame could exist -- deterministic, not a timing race.
  # Only the actual trickplay job's own invocation sleeps (matched on its output dir), not the
  # agent's startup capability probes or anything else that happens to share this binary.
  printf '#!/bin/sh\ncase "$*" in */trickplay/early/*) sleep 5;; esac\nexec ffmpeg "$@"\n' > "$T/ff-slow"
  chmod +x "$T/ff-slow"
  # The agent derives its ffprobe path from ffmpeg's own directory (no separate override), so
  # a same-directory ffprobe shim keeps the playback job's admission weight probe working
  # exactly like every other worker's, instead of failing closed to the unknown-height default.
  printf '#!/bin/sh\nexec ffprobe "$@"\n' > "$T/ffprobe"
  chmod +x "$T/ffprobe"
  TC_KIND=cpu TC_NAME=batchslow TC_CAPACITY=1 TC_BATCH_HEADROOM=0 TC_FFMPEG="$T/ff-slow" TC_PORT=19914 \
    TC_TRICKPLAY_OUTPUT_ROOT="$T/trickplay" $AGENT >> "$T/agent.log" 2>&1 &
  AGS=$!
  listening 19914
  mkdir -p "$T/trickplay/early" "$T/trickplay/early-local"
  mapfile -t TPARGS_EARLY < <(tpargs "$T/srcpreempt.mkv" "$T/trickplay/early")
  TPARGS_EARLY_RE=(-re "${TPARGS_EARLY[@]}")
  mapfile -t TPARGS_EARLY_LOCAL < <(tpargs "$T/srcpreempt.mkv" "$T/trickplay/early-local")
  ffmpeg "${TPARGS_EARLY_LOCAL[@]}" 2>/dev/null
  ref_frames_early=$(ls "$T/trickplay/early-local" | grep -c jpg)
  accepted_before=$(grep -c 'accepted a [0-9.]*-unit batch job' "$T/agent.log")
  preempted_before=$(grep -c 'preempted by a playback admission' "$T/agent.log")
  local_before=$(local_run_count)
  TC_BATCH=1 TC_TRICKPLAY_OUTPUT_ROOT="$T/trickplay" TC_WORKERS=batchslow=127.0.0.1:19914 \
    TC_FFMPEG_REAL="$T/ff-canary" timeout 20 $SHIM "${TPARGS_EARLY_RE[@]}" < /dev/null 2>/dev/null &
  SHE=$!
  # Wait only for admission (Accepted), never for a frame: the whole point of this case is that
  # none can exist yet.
  until [ "$(grep -c 'accepted a [0-9.]*-unit batch job' "$T/agent.log")" -gt "$accepted_before" ]; do sleep 0.1; done
  early_frames_at_trigger=$(ls "$T/trickplay/early" | grep -c jpg)
  rm -f "$T"/out/*
  ( sleep 4; printf q ) | TC_WORKERS=batchslow=127.0.0.1:19914 timeout 20 $SHIM "${ARGS[@]}" 2>/dev/null
  pbcode_early=$?
  wait "$SHE"; batchcode_early=$?
  early_frames=$(ls "$T/trickplay/early" | grep -c jpg)
  preempted_after=$(grep -c 'preempted by a playback admission' "$T/agent.log")
  local_after=$(local_run_count)
  echo "preempt-early: playback_exit=$pbcode_early frames_at_trigger=$early_frames_at_trigger batch_shim_exit=$batchcode_early frames_match=$(match "$early_frames" "$ref_frames_early") local_runs=$((local_after - local_before)) agent_preempted=$((preempted_after - preempted_before))"
  kill "$AGS"

  echo "== 17b: a playback job preempts a batch job AFTER its first frame; no local rerun"
  mkdir -p "$T/trickplay/late" "$T/trickplay/late-local"
  # Long enough that the preempt (triggered the instant frame 1 exists) can never lose the race
  # to the source's own EOF.
  ffmpeg -hide_banner -loglevel error -y -f lavfi -i testsrc2=duration=20:size=320x180:rate=24 \
    -c:v libx264 -preset ultrafast "$T/srclatepreempt.mkv"
  mapfile -t TPARGS_LATE < <(tpargs "$T/srclatepreempt.mkv" "$T/trickplay/late")
  TPARGS_LATE_RE=(-re "${TPARGS_LATE[@]}")
  mapfile -t TPARGS_LATE_LOCAL < <(tpargs "$T/srclatepreempt.mkv" "$T/trickplay/late-local")
  ffmpeg "${TPARGS_LATE_LOCAL[@]}" 2>/dev/null
  ref_frames_late=$(ls "$T/trickplay/late-local" | grep -c jpg)
  preempted_before=$(grep -c 'preempted by a playback admission' "$T/agent.log")
  local_before=$(local_run_count)
  TC_BATCH=1 TC_TRICKPLAY_OUTPUT_ROOT="$T/trickplay" TC_WORKERS=batchw=127.0.0.1:19911 \
    TC_FFMPEG_REAL="$T/ff-canary" timeout 20 $SHIM "${TPARGS_LATE_RE[@]}" < /dev/null 2>/dev/null &
  SHL=$!
  until [ -e "$T/trickplay/late/00000001.jpg" ]; do sleep 0.2; done
  rm -f "$T"/out/*
  ( sleep 4; printf q ) | TC_WORKERS=batchw=127.0.0.1:19911 timeout 20 $SHIM "${ARGS[@]}" 2>/dev/null
  pbcode_late=$?
  wait "$SHL"; batchcode_late=$?
  # No further writer should touch the output once the shim has exited: give any (wrongly
  # spawned) rerun a beat, then confirm the count is unchanged and short of the full set.
  late_frames_at_exit=$(ls "$T/trickplay/late" | grep -c jpg)
  sleep 1
  late_frames_after_wait=$(ls "$T/trickplay/late" | grep -c jpg)
  partial="no"
  [ "$late_frames_at_exit" -gt 0 ] && [ "$late_frames_at_exit" -lt "$ref_frames_late" ] && partial="yes"
  stable="no"
  [ "$late_frames_after_wait" -eq "$late_frames_at_exit" ] && stable="yes"
  preempted_after=$(grep -c 'preempted by a playback admission' "$T/agent.log")
  local_after=$(local_run_count)
  no_rerun_logged=$(grep -c "exiting $batchcode_late without a local rerun" "$T/shim.log")
  echo "preempt-late: playback_exit=$pbcode_late batch_shim_exit=$batchcode_late partial=$partial stable=$stable local_runs=$((local_after - local_before)) agent_preempted=$((preempted_after - preempted_before)) no_rerun_logged=$no_rerun_logged"

  echo "== 17c: a batch job's worker (agent + ffmpeg) is lost outright AFTER its first frame; no local rerun"
  # 17b only covers the preempted path through batch_pool_failure(); a plain Attempt::Lost (agent
  # process gone, not a preemption) after a frame exists is otherwise covered only by the shim's
  # own unit tests, never against the real gRPC stream -- mirrors case 2's kill-the-agent shape.
  mkdir -p "$T/trickplay/lost" "$T/trickplay/lost-local"
  mapfile -t TPARGS_LOST < <(tpargs "$T/srclatepreempt.mkv" "$T/trickplay/lost")
  TPARGS_LOST_RE=(-re "${TPARGS_LOST[@]}")
  mapfile -t TPARGS_LOST_LOCAL < <(tpargs "$T/srclatepreempt.mkv" "$T/trickplay/lost-local")
  ffmpeg "${TPARGS_LOST_LOCAL[@]}" 2>/dev/null
  ref_frames_lost=$(ls "$T/trickplay/lost-local" | grep -c jpg)
  TC_KIND=cpu TC_NAME=batchlost TC_CAPACITY=1 TC_BATCH_HEADROOM=0 TC_FFMPEG=ffmpeg TC_PORT=19917 \
    TC_TRICKPLAY_OUTPUT_ROOT="$T/trickplay" $AGENT >> "$T/agent.log" 2>&1 &
  AGL=$!
  listening 19917
  # Anchored on the source + this case's own output dir, like encaff()/encfiller() above.
  encl() { pgrep -f "ffmpeg .*-i $T/srclatepreempt.mkv.*trickplay/lost/%08d"; }
  local_before=$(local_run_count)
  TC_BATCH=1 TC_TRICKPLAY_OUTPUT_ROOT="$T/trickplay" TC_WORKERS=batchlost=127.0.0.1:19917 \
    TC_FFMPEG_REAL="$T/ff-canary" timeout 20 $SHIM "${TPARGS_LOST_RE[@]}" < /dev/null 2>/dev/null &
  SHLO=$!
  until [ -e "$T/trickplay/lost/00000001.jpg" ]; do sleep 0.2; done
  # Both in one kill (like case 20c's KILLAG+encaff), not two sequential ones: a separate
  # ffmpeg-then-agent kill leaves a window where the agent can still notice the dead child and
  # report a clean-ish Exit before it is itself killed, turning this into the Exited(c,false)
  # path (already covered by finding #1's regression) instead of the Attempt::Lost this case
  # exists to cover.
  kill -9 "$AGL" $(encl) 2>/dev/null
  wait "$SHLO"; lostcode=$?
  lost_frames_at_exit=$(ls "$T/trickplay/lost" | grep -c jpg)
  sleep 1
  lost_frames_after_wait=$(ls "$T/trickplay/lost" | grep -c jpg)
  partial="no"
  [ "$lost_frames_at_exit" -gt 0 ] && [ "$lost_frames_at_exit" -lt "$ref_frames_lost" ] && partial="yes"
  stable="no"
  [ "$lost_frames_after_wait" -eq "$lost_frames_at_exit" ] && stable="yes"
  local_after=$(local_run_count)
  no_rerun_logged_lost=$(grep -c "was lost.*without a local rerun" "$T/shim.log")
  echo "batch: lost-after-frame exit=$lostcode partial=$partial stable=$stable local_runs=$((local_after - local_before)) no_rerun_logged=$no_rerun_logged_lost"

  echo "== 18: TC_ACCEPT_BATCH=0 and a headroom-full worker both fall back to a local run"
  TC_KIND=cpu TC_NAME=batchoff TC_ACCEPT_BATCH=0 TC_TRICKPLAY_OUTPUT_ROOT="$T/trickplay" \
    TC_FFMPEG=ffmpeg TC_PORT=19912 $AGENT >> "$T/agent.log" 2>&1 &
  AGBO=$!
  listening 19912
  TC_KIND=cpu TC_NAME=batchfull TC_CAPACITY=1 TC_BATCH_HEADROOM=1 TC_TRICKPLAY_OUTPUT_ROOT="$T/trickplay" \
    TC_FFMPEG=ffmpeg TC_PORT=19913 $AGENT >> "$T/agent.log" 2>&1 &
  AGBF=$!
  listening 19913
  mkdir -p "$T/trickplay/d-off" "$T/trickplay/d-full"
  mapfile -t TPARGS_OFF < <(tpargs "$T/srcbatch.mkv" "$T/trickplay/d-off")
  mapfile -t TPARGS_FULL < <(tpargs "$T/srcbatch.mkv" "$T/trickplay/d-full")
  TC_BATCH=1 TC_TRICKPLAY_OUTPUT_ROOT="$T/trickplay" TC_WORKERS=batchoff=127.0.0.1:19912 \
    $SHIM "${TPARGS_OFF[@]}" < /dev/null 2>/dev/null
  offcode=$?
  TC_BATCH=1 TC_TRICKPLAY_OUTPUT_ROOT="$T/trickplay" TC_WORKERS=batchfull=127.0.0.1:19913 \
    $SHIM "${TPARGS_FULL[@]}" < /dev/null 2>/dev/null
  fullcode=$?
  off_frames=$(ls "$T/trickplay/d-off" | grep -c jpg)
  full_frames=$(ls "$T/trickplay/d-full" | grep -c jpg)
  echo "batch: accept-off exit=$offcode frames_match=$(match "$off_frames" "$EXPECT_FRAMES") reason=$(grep -c 'busy (batch-disabled): refusing a batch job' "$T/agent.log")"
  echo "batch: headroom-full exit=$fullcode frames_match=$(match "$full_frames" "$EXPECT_FRAMES") reason=$(grep -c 'busy (headroom): refused a [0-9.]*-unit batch job' "$T/agent.log")"
  kill "$AGBO" "$AGBF"

  echo "== 19: TC_BATCH unset (default): trickplay never contacts the pool"
  mkdir -p "$T/trickplay/e"
  mapfile -t TPARGS_GATE < <(tpargs "$T/srcbatch.mkv" "$T/trickplay/e")
  before=$(grep -c 'transcode request' "$T/shim.log")
  TC_TRICKPLAY_OUTPUT_ROOT="$T/trickplay" TC_WORKERS=batchw=127.0.0.1:19911 \
    $SHIM "${TPARGS_GATE[@]}" < /dev/null 2>/dev/null
  code=$?
  after=$(grep -c 'transcode request' "$T/shim.log")
  frames=$(ls "$T/trickplay/e" | grep -c jpg)
  echo "batch: gate-off exit=$code frames_match=$(match "$frames" "$EXPECT_FRAMES") pool_attempts=$((after - before))"
  kill "$AGB"

  # Seek affinity (native-only: the python spike does not implement it). Two equal cpu workers,
  # "affa" listed first so an idle tie-break picks it deterministically for the very first start;
  # every case's echo is prefixed "affinity:" so none of it begins with "exit=0" at column 0.
  echo "== 20: seek affinity pins a PLAYBACK session across a restart, and clears on worker loss"
  TC_KIND=cpu TC_NAME=affa TC_CAPACITY=4 TC_STALL_AFTER=4 TC_FFMPEG=ffmpeg TC_LOG="$T/agent.log" TC_PORT=19915 $AGENT >> "$T/agent.log" 2>&1 &
  AGAFFA=$!
  listening 19915
  TC_KIND=cpu TC_NAME=affb TC_CAPACITY=4 TC_STALL_AFTER=4 TC_FFMPEG=ffmpeg TC_LOG="$T/agent.log" TC_PORT=19916 $AGENT >> "$T/agent.log" 2>&1 &
  AGAFFB=$!
  listening 19916
  export TC_WORKERS=affa=127.0.0.1:19915,affb=127.0.0.1:19916 TC_AFFINITY=1
  mkdir -p "$T/outaff" "$T/outfiller"
  PINFILE="$T/outaff/paff.worker" # <md5>.worker sibling of the paff.tcpool.lock lease file
  ARGS_AFF=(-re -i "$T/src.mkv" -c:v libx264 -preset ultrafast -f hls -hls_time 3 -hls_list_size 0
            -hls_segment_filename "$T/outaff/s%d.ts" "$T/outaff/paff.m3u8")
  ARGS_AFF_SEEK=(-re -i "$T/src.mkv" -c:v libx264 -preset ultrafast -f hls -hls_time 3 -hls_list_size 0 -start_number 5
                 -hls_segment_filename "$T/outaff/s%d.ts" "$T/outaff/paff.m3u8")
  FILLER=(-re -i "$T/src.mkv" -c:v libx264 -preset ultrafast -f hls -hls_time 3 -hls_list_size 0
          -hls_segment_filename "$T/outfiller/s%d.ts" "$T/outfiller/f.m3u8")
  # Anchored on "ffmpeg -re -i" like enc() above: an unanchored pgrep on just the output pattern
  # also matches $SHIM's own argv (it's handed the same args), which would SIGKILL the shim being
  # tested instead of (or as well as) the ffmpeg doing the encoding.
  encaff() { pgrep -f "ffmpeg -re -i $T/src.mkv.*outaff/s%d"; }
  encfiller() { pgrep -f "ffmpeg -re -i $T/src.mkv.*outfiller/s%d"; }

  echo "== 20a: the initial start picks and pins one worker"
  ( sleep 4; printf q ) | timeout 20 $SHIM "${ARGS_AFF[@]}" 2>/dev/null
  code1=$?
  worker1=$(grep -o 'transcode -> worker [a-z0-9]*' "$T/shim.log" | tail -1 | awk '{print $NF}')
  pin1=$(cat "$PINFILE" 2>/dev/null || echo absent)
  echo "affinity: start exit=$code1 worker=$worker1 pinned=$pin1"

  echo "== 20b: a seek stays on the pinned worker even though the other is now freer"
  # Load the pinned worker to 75% free with an unrelated job; the other worker stays 100% free, so
  # rank() alone would now prefer it -- the seek must still land on the pin.
  sleep 30 | TC_WORKERS=$worker1=127.0.0.1:$([ "$worker1" = affa ] && echo 19915 || echo 19916) \
    $SHIM "${FILLER[@]}" 2>/dev/null &
  FILL=$!
  until [ -e "$T/outfiller/s0.ts" ]; do sleep 0.2; done
  rm -f "$T"/outaff/s*.ts
  ( sleep 4; printf q ) | timeout 20 $SHIM "${ARGS_AFF_SEEK[@]}" 2>/dev/null
  code2=$?
  worker2=$(grep -o 'transcode -> worker [a-z0-9]*' "$T/shim.log" | tail -1 | awk '{print $NF}')
  pin2=$(cat "$PINFILE" 2>/dev/null || echo absent)
  same="no"; [ "$worker1" = "$worker2" ] && same="yes"
  echo "affinity: seek exit=$code2 worker=$worker2 pinned=$pin2 same_worker=$same"
  kill "$FILL" 2>/dev/null
  kill $(encfiller) 2>/dev/null

  echo "== 20c: killing the pinned worker mid-stream exits 255 and clears the pin"
  rm -f "$T"/outaff/s*.ts
  sleep 30 | timeout 20 $SHIM "${ARGS_AFF[@]}" 2>/dev/null &
  SHAFF=$!
  until [ -e "$T/outaff/s0.ts" ]; do sleep 0.2; done
  if [ "$worker2" = affa ]; then KILLAG="$AGAFFA"; else KILLAG="$AGAFFB"; fi
  kill -9 "$KILLAG" $(encaff)
  wait "$SHAFF"; code3=$?
  pin3=$(cat "$PINFILE" 2>/dev/null || echo absent)
  echo "affinity: kill exit=$code3 pinned=$pin3"

  echo "== 20d: the restart after the kill lands on the survivor and re-pins to it"
  rm -f "$T"/outaff/s*.ts
  ( sleep 4; printf q ) | timeout 20 $SHIM "${ARGS_AFF[@]}" 2>/dev/null
  code4=$?
  worker4=$(grep -o 'transcode -> worker [a-z0-9]*' "$T/shim.log" | tail -1 | awk '{print $NF}')
  pin4=$(cat "$PINFILE" 2>/dev/null || echo absent)
  echo "affinity: restart exit=$code4 worker=$worker4 pinned=$pin4"

  echo "== 20e: a stall (agent watchdog kills ffmpeg, agent itself stays up) also clears the pin"
  # Regression for the gap where only the Lost (dropped-connection) branch cleared the pin: a
  # watchdog kill (stall/drain) reaches the shim as Attempt::Exited(c, false), not Lost, and used
  # to leave the file behind pinning a restart right back to a card whose job just got SIGKILLed.
  rm -f "$T"/outaff/s*.ts
  # timeout 28, not 20: ARGS_AFF (unlike ARGS) carries no -force_key_frames, so s0.ts can take
  # ~11-16s to land on a loaded host before the ~4s watchdog (TC_STALL_AFTER=4 on affa/affb) even
  # starts counting; 20 was observed to race the outer timeout instead of the watchdog. The
  # "sleep 30" stdin pipe already covers this (case 11 uses the same timeout 30 for the same
  # slow-first-segment reason).
  sleep 30 | timeout 28 $SHIM "${ARGS_AFF[@]}" 2>/dev/null &
  SHAFF2=$!
  until [ -e "$T/outaff/s0.ts" ]; do sleep 0.2; done
  kill -STOP $(encaff) # freeze ffmpeg's own progress; the agent stays healthy and does the killing
  t0=$SECONDS
  while kill -0 "$SHAFF2" 2>/dev/null; do sleep 0.2; done
  dt=$((SECONDS - t0))
  wait "$SHAFF2"; code5=$?
  pin5=$(cat "$PINFILE" 2>/dev/null || echo absent)
  echo "affinity: stall exit=$code5 pinned=$pin5 after=${dt}s"
  kill -CONT $(encaff) 2>/dev/null # safety net; the watchdog's SIGKILL should already be gone

  kill "$AGAFFA" "$AGAFFB" 2>/dev/null

  # 21: shared transcode dir (docs/SHARED-TRANSCODE.md). A shim that carries JELLYMESH_KEEPALIVE
  # (patched Jellyfin in shared mode) makes its job detachable: the agent keeps ffmpeg running
  # when the shim dies with its replica, heartbeats the lease itself, honours a takeover from
  # another replica's shim (a seek there), and ends + cleans up once the keepalive goes stale.
  STEM=0123456789abcdef0123456789abcdef
  STEM2=fedcba9876543210fedcba9876543210
  mkdir -p "$T/outsh/.jellymesh-alive"
  KA="$T/outsh/.jellymesh-alive/sess1"
  KA2="$T/outsh/.jellymesh-alive/sess2"
  echo playing > "$KA"; echo playing > "$KA2"
  TC_KIND=cpu TC_NAME=shared TC_FFMPEG=ffmpeg TC_LOG="$T/agent.log" TC_PORT=19921 TC_ORPHAN_IDLE_SECS=4 \
    $AGENT >> "$T/agent.log" 2>&1 &
  AGSH=$!
  listening 19921
  SH_ARGS=(-re -i "$T/src.mkv" -c:v libx264 -preset ultrafast -force_key_frames "expr:gte(t,n_forced*3)" -f hls
           -hls_time 3 -hls_list_size 0 -hls_segment_filename "$T/outsh/${STEM}%d.ts" "$T/outsh/${STEM}.m3u8")
  SH_SEEK=(-re -i "$T/src.mkv" -c:v libx264 -preset ultrafast -force_key_frames "expr:gte(t,n_forced*3)" -f hls
           -hls_time 3 -hls_list_size 0 -start_number 12 -hls_segment_filename "$T/outsh/${STEM}%d.ts" "$T/outsh/${STEM}.m3u8")
  SH_ARGS2=(-re -i "$T/src.mkv" -c:v libx264 -preset ultrafast -force_key_frames "expr:gte(t,n_forced*3)" -f hls
           -hls_time 3 -hls_list_size 0 -hls_segment_filename "$T/outsh/${STEM2}%d.ts" "$T/outsh/${STEM2}.m3u8")
  encsh() { pgrep -f "^ffmpeg .*src.mkv.*outsh/$1"; }
  segs() { ls "$T/outsh" | grep -c "^$1[0-9]*\.ts$"; }
  lease_age() { echo $(( $(date +%s) - $(stat -c %Y "$T/outsh/$1.tcpool.lock" 2>/dev/null || echo 0) )); }
  ( while :; do touch "$KA"; sleep 1; done ) &
  TOUCH=$!

  echo "== 21a: the shim dies mid-stream; its detachable job keeps the same ffmpeg writing"
  sleep 60 | TC_WORKERS=shared=127.0.0.1:19921 JELLYMESH_KEEPALIVE="$KA" $SHIM "${SH_ARGS[@]}" 2>/dev/null &
  SHA=$!
  until [ -e "$T/outsh/${STEM}1.ts" ]; do sleep 0.2; done
  pid1=$(encsh "$STEM")
  # no `wait`: bash would wait for the whole `sleep 60 | shim` pipeline job
  kill -9 "$SHA"
  sleep 7
  pid2=$(encsh "$STEM"); n1=$(segs "$STEM"); sleep 4; n2=$(segs "$STEM")
  same=no; [ -n "$pid1" ] && [ "$pid1" = "$pid2" ] && same=yes
  grow=no; [ "$n2" -gt "$n1" ] && grow=yes
  fresh=no; [ "$(lease_age "$STEM")" -le 3 ] && fresh=yes
  echo "shared: detached same_ffmpeg=$same growing=$grow lease_fresh=$fresh (pids $pid1/$pid2 segs $n1->$n2 lease_age $(lease_age "$STEM")s)"

  echo "== 21b: another replica's shim seeks the same output: takes it over from the orphan"
  ( sleep 6; printf q ) | TC_WORKERS=shared=127.0.0.1:19921 JELLYMESH_KEEPALIVE="$KA" timeout 40 $SHIM "${SH_SEEK[@]}" 2>/dev/null
  code=$?
  old=gone; kill -0 "$pid1" 2>/dev/null && old=running
  seek=no; [ -e "$T/outsh/${STEM}12.ts" ] && seek=yes
  took=$(grep -c "took output .*${STEM}.m3u8 over from another replica" "$T/shim.log")
  echo "shared: takeover exit=$code old_ffmpeg=$old seek_segment=$seek took_over_logged=$took"
  kill "$TOUCH" 2>/dev/null

  echo "== 21c: attached holder taken over: the old shim exits 0 and leaves the new writer's files"
  rm -f "$T/outsh/${STEM}"*
  ( while :; do touch "$KA"; sleep 1; done ) &
  TOUCH=$!
  sleep 60 | TC_WORKERS=shared=127.0.0.1:19921 JELLYMESH_KEEPALIVE="$KA" timeout 50 $SHIM "${SH_ARGS[@]}" 2>/dev/null &
  SHA=$!
  until [ -e "$T/outsh/${STEM}0.ts" ]; do sleep 0.2; done
  ( sleep 6; printf q ) | TC_WORKERS=shared=127.0.0.1:19921 JELLYMESH_KEEPALIVE="$KA" timeout 40 $SHIM "${SH_SEEK[@]}" 2>/dev/null &
  SHB=$!
  wait "$SHA"; codea=$?
  wait "$SHB"; codeb=$?
  kept=no; [ -e "$T/outsh/${STEM}12.ts" ] && kept=yes
  echo "shared: attached-takeover old_exit=$codea new_exit=$codeb seek_segment=$kept"
  kill "$TOUCH" 2>/dev/null

  echo "== 21d: detached and nobody touches the keepalive: ffmpeg ends, outputs are removed"
  sleep 60 | TC_WORKERS=shared=127.0.0.1:19921 JELLYMESH_KEEPALIVE="$KA2" $SHIM "${SH_ARGS2[@]}" 2>/dev/null &
  SHC=$!
  until [ -e "$T/outsh/${STEM2}0.ts" ]; do sleep 0.2; done
  kill -9 "$SHC"
  t0=$SECONDS
  while encsh "$STEM2" >/dev/null && [ $((SECONDS - t0)) -lt 20 ]; do sleep 0.5; done
  gone=no; encsh "$STEM2" >/dev/null || gone=yes
  sleep 1
  left=$(ls "$T/outsh" | grep -c "^$STEM2")
  ka=present; [ -e "$KA2" ] || ka=removed
  echo "shared: orphan-expired ffmpeg_gone=$gone after=$((SECONDS - t0))s files_left=$left keepalive=$ka"
  echo "== 21e: a throttler-paused job whose shim dies is resumed (u) by the agent"
  # Stock ffmpeg has no p/u keys (case 11 fakes the pause with SIGSTOP), so this checks the key
  # stream itself: a wrapper records what the agent writes to ffmpeg's stdin, then runs ffmpeg.
  mkdir -p "$T/keys"
  printf '#!/bin/sh\nexec 3<&0\n( cat <&3 > "%s/keys/$$" ) &\nexec ffmpeg "$@" < /dev/null\n' "$T" > "$T/ff-keys"
  chmod +x "$T/ff-keys"
  TC_KIND=cpu TC_NAME=keys TC_FFMPEG="$T/ff-keys" TC_LOG="$T/agent.log" TC_PORT=19922 TC_ORPHAN_IDLE_SECS=4 \
    $AGENT >> "$T/agent.log" 2>&1 &
  AGK=$!
  listening 19922
  rm -f "$T/outsh/${STEM}"*
  ( while :; do touch "$KA"; sleep 1; done ) &
  TOUCH=$!
  ( until [ -e "$T/outsh/${STEM}0.ts" ]; do sleep 0.2; done; printf p; sleep 60 ) \
    | TC_WORKERS=keys=127.0.0.1:19922 JELLYMESH_KEEPALIVE="$KA" $SHIM "${SH_ARGS[@]}" 2>/dev/null &
  SHK=$!
  until [ -e "$T/outsh/${STEM}0.ts" ]; do sleep 0.2; done
  sleep 2
  kill -9 "$SHK"
  sleep 3
  resumed=$(grep -l '^pu$' "$T"/keys/* 2>/dev/null | wc -l)
  kill "$TOUCH" 2>/dev/null
  echo "shared: paused-detach keys_pu=$resumed detach_logged=$(grep -c 'detaching: ffmpeg keeps writing' "$T/agent.log")"
  kill "$AGK" 2>/dev/null

  echo "== 21f: a detached job throttles itself against the viewer's position (<keepalive>.seg)"
  # The wrapper turns the agent's p/u keys into SIGSTOP/SIGCONT (stock ffmpeg has no p/u; the
  # jellyfin-ffmpeg the agents run does), so the segment lead is real, not just the key stream.
  # Limits scaled down: pause above 6 s (2 segments) ahead, resume below 4 s, position stale 5 s.
  mkdir -p "$T/keys2"
  printf '#!/bin/bash\nexec 3<&0\n( while IFS= read -r -n1 k <&3; do printf %%s "$k" >> "%s/keys2/$$"; case "$k" in p) kill -STOP $$;; u) kill -CONT $$;; esac; done ) &\nexec ffmpeg "$@" < /dev/null\n' "$T" > "$T/ff-stop"
  chmod +x "$T/ff-stop"
  TC_KIND=cpu TC_NAME=thr TC_FFMPEG="$T/ff-stop" TC_LOG="$T/agent.log" TC_PORT=19923 TC_ORPHAN_IDLE_SECS=4 \
    TC_ORPHAN_LEAD_MAX_SECS=6 TC_ORPHAN_LEAD_RESUME_SECS=4 TC_ORPHAN_POS_STALE_SECS=5 \
    $AGENT >> "$T/agent.log" 2>&1 &
  AGT=$!
  listening 19923
  rm -f "$T/outsh/${STEM}"*
  SEGF="$KA.seg"
  echo 0 > "$SEGF"
  newest() { grep -v '^#' "$T/outsh/${STEM}.m3u8" 2>/dev/null | tail -1 | sed -E "s/^${STEM}([0-9]+)\.ts$/\1/"; }
  keys2() { cat "$T"/keys2/* 2>/dev/null; }
  ( while :; do touch "$KA"; sleep 1; done ) &
  TOUCH=$!
  ( while :; do touch "$SEGF"; sleep 1; done ) &
  SEGT=$!
  sleep 60 | TC_WORKERS=thr=127.0.0.1:19923 JELLYMESH_KEEPALIVE="$KA" $SHIM "${SH_ARGS[@]}" 2>/dev/null &
  SHT=$!
  until [ -e "$T/outsh/${STEM}0.ts" ]; do sleep 0.2; done
  kill -9 "$SHT"
  # viewer parked at segment 0: the job pauses once the output is > 2 segments ahead
  t0=$SECONDS
  until [ "$(keys2)" = p ] || [ $((SECONDS - t0)) -gt 30 ]; do sleep 0.5; done
  sleep 1; n1=$(newest); sleep 6; n2=$(newest)
  bounded=no; [ -n "$n1" ] && [ "$n1" = "$n2" ] && [ "$n2" -le 4 ] && bounded=yes
  # the viewer catches up: resume, then pause again once > 2 segments ahead of it
  kill "$SEGT"; echo "$n2" > "$SEGF"
  ( while :; do touch "$SEGF"; sleep 1; done ) &
  SEGT=$!
  t0=$SECONDS
  until [ "$(keys2)" = pup ] || [ $((SECONDS - t0)) -gt 30 ]; do sleep 0.5; done
  sleep 1; n3=$(newest)
  repaused=no; [ "$(keys2)" = pup ] && [ "$n3" -gt "$n2" ] && [ $((n3 - n2)) -le 4 ] && repaused=yes
  # position goes stale while the keepalive still says playing: unknown -> unthrottled (u)
  kill "$SEGT"
  t0=$SECONDS
  until [ "$(keys2)" = pupu ] || [ $((SECONDS - t0)) -gt 20 ]; do sleep 0.5; done
  stale=$((SECONDS - t0))
  sleep 4; n4=$(newest)
  runs=no; [ -n "$n4" ] && [ "$n4" -gt "$n3" ] && runs=yes
  gauge=$(grep -c "resuming ffmpeg (newest segment [0-9]*, viewer at $n2, lead unknown)" "$T/agent.log")
  # viewer gone: expires and removes the sidecar with the keepalive
  kill "$TOUCH"
  t0=$SECONDS
  while [ -e "$SEGF" ] && [ $((SECONDS - t0)) -lt 20 ]; do sleep 0.5; done
  seg_left=present; [ -e "$SEGF" ] || seg_left=removed
  echo "shared: throttle keys=$(keys2) bounded=$bounded repaused=$repaused stale_resume_after=${stale}s runs_after=$runs sidecar=$seg_left (newest $n1/$n2 -> $n3 -> $n4, stale_logged=$gauge)"
  kill "$AGT" 2>/dev/null
  kill "$AGSH" 2>/dev/null
fi

sleep 1
kill "$AG"
grep -hv "start:\|transcode ->" "$T/agent.log" "$T/shim.log" | cut -c1-180

#!/bin/bash
# Profile round 8 (2026-11-07): IDN Voice under load. The Voice server (the same core as
# both Site Managers', 2 worker threads) in one process, guests in another
# (examples/voice_bench.rs), over the WebSocket path, recording on. For each scenario:
# the server's CPU (% of one core) over the run, its memory, what arrived and how late.
#   round8.sh [SECONDS]   (default 20)
set -u
S=$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)
IMPL=$(cd "$S/../../../.." && pwd)
SECS=${1:-20}
B=$IMPL/target/release/examples/voice_bench
OUT=$IMPL/../build-kit/profile/round8
mkdir -p "$OUT"
cargo build --release -q -p idn-voice --example voice_bench --manifest-path "$IMPL/Cargo.toml" || exit 1
PORT=17999 DIR=$OUT/server
cpu() { awk '{print $14+$15}' /proc/$1/stat; }
rss() { awk '/VmRSS/{print $2}' /proc/$1/status; }
run() { # NAME CHANNEL LISTENERS TALKERS
  rm -rf "$DIR"
  "$B" server $PORT "$DIR" > "$OUT/server.log" 2>&1 & local srv=$!
  for _ in $(seq 50); do grep -q ready "$OUT/server.log" && break; sleep 0.1; done
  local hz; hz=$(getconf CLK_TCK)
  local c0 t0; c0=$(cpu $srv); t0=$(date +%s.%N)
  local line; line=$("$B" load $PORT "$2" "$3" "$4" "$SECS" 2> "$OUT/$1.load.log")
  local c1 t1; c1=$(cpu $srv); t1=$(date +%s.%N)
  local mem; mem=$(rss $srv)
  local rec; rec=$(du -sk "$DIR" | cut -f1)
  kill -TERM $srv; wait $srv 2>/dev/null
  local pc; pc=$(awk -v a=$c0 -v b=$c1 -v h=$hz -v s=$t0 -v e=$t1 'BEGIN{printf "%.1f", (b-a)*100/h/(e-s)}')
  echo "$1: server_cpu=${pc}% rss_kb=$mem disk_kb=$rec $line $(cat "$OUT/$1.load.log" | tr '\n' ' ')" | tee -a "$OUT/results.txt"
}
: > "$OUT/results.txt"
run dispatch-35   1 35 1
run members-200   1 199 1
run four-talkers  1 46 4
run half-35       2 35 1
run at-rest-200   1 200 0

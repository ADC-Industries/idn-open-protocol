#!/bin/bash
# P1 (2026-11-03): adapt/1 with a CPU receiver of a 2160p30 switcher program over the
# network. Per second: the receiver's decode time, the sender's quality and skips, the
# receiver's late and dropped pictures. p1-adapt.sh [gpu|cpu] [SECONDS] [RUNS]
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../../common.sh"; source "$S/../lib.sh"
set -u
W=$(workdir prof5)
G=$B/examples/gpu_recv
H=$(hostname | tr a-z A-Z)
enc=${1:-gpu}; secs=${2:-40}; runs=${3:-1}
export IDN_NODE=off IDN_CLOCK=off IDN_MACHINE=off
[ $enc = cpu ] && export IDN_GPU_SHQ=0
for run in $(seq $runs); do
  export IDN_STATE_DIR=$W/state-$enc; rm -rf $IDN_STATE_DIR; mkdir -p $IDN_STATE_DIR
  P=()
  $B/idn send --no-node --name ACam --size 3840x2160 --fps 30/1 --seconds $((secs + 30)) > $W/cam.log 2>&1 & P+=($!)
  $B/idn-switcher --no-node --format 2160p30 --port 7986 --name ASw > $W/sw-$enc-$run.log 2>&1 & P+=($!)
  until curl -s http://127.0.0.1:7986/api/v1/state >/dev/null 2>&1; do sleep 0.5; done; sleep 2
  api="curl -s -X POST -H X-IDN-Switcher:1 http://127.0.0.1:7986/api/v1"
  $api/add_input -d "{\"network\":\"$H (ACam)\",\"label\":\"Cam\"}" >/dev/null
  $api/add_input -d '{"title":{"layers":[{"type":"box","rect":[0.05,0.78,0.5,0.14],"colour":"#000000b0","radius":0.1},{"type":"text","rect":[0.07,0.8,0.46,0.1],"text":"Pastor Okafor","size":0.05,"colour":"#ffffff"}]},"label":"T"}' >/dev/null
  on_bus http://127.0.0.1:7986/api/v1 program 1; bug_on http://127.0.0.1:7986/api/v1 2
  sleep 3
  $G "$H (ASw)" $secs --cpu > $W/recv-$enc-$run.log 2> $W/recv-$enc-$run.err & g=$!
  if [ -n "${PERF:-}" ]; then sleep 12; { $S/../gpu-sample.sh $g; $S/../gpu-procs.sh; } > $W/sample-$enc-$run.txt & perf stat -e task-clock,instructions,cycles -p $g -- sleep 6 2> $W/stat-$enc-$run.txt; fi
  if [ -n "${SWCPU:-}" ]; then sleep 10; c0=$(awk '{print $14+$15}' /proc/${P[1]}/stat); sleep 8; c1=$(awk '{print $14+$15}' /proc/${P[1]}/stat); echo "switcher $(( (c1 - c0) * 1000 / 100 / 8 )) ms/s" > $W/swcpu-$enc-$run.txt; python3 $S/../switcher_state.py >> $W/swcpu-$enc-$run.txt; fi
  wait $g
  for ((i=${#P[@]}-1; i>=0; i--)); do kill -TERM ${P[i]} 2>/dev/null; done; wait 2>/dev/null
  echo "== $enc run $run"; awk 'NR%4==0' $W/recv-$enc-$run.log
done

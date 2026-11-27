#!/bin/bash
# Profiling round 4: after the reorganization and the bug round.
# Round 3's scenarios again (round3.sh all, its baselines from round2.sh), and D2: a desk
# with 8 stereo sources on this machine's clock, lanes filtering and passing through.
#   round4.sh round3 | through | all
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../../common.sh"
set -u
W=$(workdir prof4)
OUT=$W/out; export OUT; mkdir -p "$OUT"
source "$S/../lib.sh"
H=$(hostname | tr a-z A-Z)
PIDS=()
cleanup() { local i; for ((i=${#PIDS[@]}-1; i>=0; i--)); do kill -TERM ${PIDS[i]} 2>/dev/null; done; for p in "${PIDS[@]}"; do wait $p 2>/dev/null; done; PIDS=(); }
trap cleanup EXIT

# The desk's lanes from 8 stereo sources (16 channels routed by the Node), untimed (no
# site clock): with IDN_LANE_THROUGH off, then on. Passing through starts after 30 s
# of calm, so each is measured from 45 s.
through() {
  for mode in off on; do
    rm -rf $W/state-th-$mode $W/th.sock; mkdir -p $W/state-th-$mode; export IDN_STATE_DIR=$W/state-th-$mode
    export IDN_NODE=$W/th.sock IDN_CLOCK=off IDN_LANE_THROUGH=$mode
    $B/idn-node run --user --state $W/state-th-$mode/node --endpoint $W/th.sock --panel-port 7995 > $W/th-node.log 2>&1 & PIDS+=($!)
    sleep 2
    for i in 1 2 3 4 5 6 7 8; do
      $B/idn send --name P4Tone$i --size 160x90 --static --seconds 600 > $W/th-tone$i.log 2>&1 & PIDS+=($!)
    done
    $B/idn mixer --channels 16 --port 7983 > $W/th-mixer-$mode.log 2>&1 & PIDS+=($!); local mix=$!
    sleep 4
    for i in 1 2 3 4 5 6 7 8; do
      $B/idn-node route Mixer $((2*i-1)) "$H (P4Tone$i)" 1 >/dev/null
      $B/idn-node route Mixer $((2*i)) "$H (P4Tone$i)" 2 >/dev/null
    done
    measure through-$mode-mixer $mix 45 10
    curl -s http://127.0.0.1:7983/api/v1/state | python3 -c '
import json,sys
l=json.load(sys.stdin).get("lanes",[])
print("lanes %d, through %d, drift %s, fixed %s, ended %s" % (len(l), sum(1 for x in l if x.get("through")),
  [round(x.get("calm_drift_ppm",0),1) for x in l], [x.get("through_fixed_ms") for x in l], sorted({x.get("through_ended") for x in l})))' | sed "s/^/through-$mode: /" | tee -a $OUT/results.tsv
    cleanup
  done
  unset IDN_NODE IDN_CLOCK IDN_LANE_THROUGH
}

round3() { $S/round3.sh all; cp $K/prof3/out/results.tsv $OUT/round3-scenarios.tsv; }

case ${1:-all} in
  round3) round3 ;;
  through) through ;;
  all) through; round3 ;;
esac

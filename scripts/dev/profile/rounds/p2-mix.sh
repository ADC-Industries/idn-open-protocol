#!/bin/bash
# P2 (2026-11-03): a 2160p30 switcher's vertical mix (1080x1920, of a virtual shot)
# received over the network, its samples read back (IDN_GPU_SHQ=0) or encoded on the GPU.
# p2-mix.sh [cpu|gpu]
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../../common.sh"; source "$S/../lib.sh"
set -u
W=$(workdir prof5)
G=$B/examples/gpu_recv
H=$(hostname | tr a-z A-Z)
enc=${1:-cpu}
export IDN_NODE=off IDN_CLOCK=off IDN_MACHINE=off
[ $enc = cpu ] && export IDN_GPU_SHQ=0
export IDN_STATE_DIR=$W/state-mix; rm -rf $IDN_STATE_DIR; mkdir -p $IDN_STATE_DIR
P=()
$B/idn send --no-node --name MCam --size 3840x2160 --fps 30/1 --seconds 60 > $W/mcam.log 2>&1 & P+=($!)
$B/idn-switcher --no-node --format 2160p30 --port 7986 --name MSw > $W/msw-$enc.log 2>&1 & P+=($!)
until curl -s http://127.0.0.1:7986/api/v1/state >/dev/null 2>&1; do sleep 0.5; done; sleep 2
api="curl -s -X POST -H X-IDN-Switcher:1 http://127.0.0.1:7986/api/v1"
$api/add_input -d "{\"network\":\"$H (MCam)\",\"label\":\"Cam\"}" >/dev/null
$api/add_input -d '{"virtual":{"input":1,"view":{"x":0.4,"y":0.5,"zoom":1.5}},"label":"Shot"}' >/dev/null
on_bus http://127.0.0.1:7986/api/v1 program 2
$api/add_mix -d '{"name":"Vertical","format":"1080x1920"}' >/dev/null
sleep 3
name="$H (Vertical)"
echo "mix source: $name"
$G "$name" 15 --cpu > $W/mrecv-$enc.log 2> $W/mrecv-$enc.err & g=$!
sleep 6; perf stat -e task-clock,instructions,cycles -p $g -- sleep 5 2> $W/mstat-$enc.txt; python3 $S/../switcher_state.py | tail -2
wait $g
for ((i=${#P[@]}-1; i>=0; i--)); do kill -TERM ${P[i]} 2>/dev/null; done; wait 2>/dev/null
tail -3 $W/mrecv-$enc.log; grep "average" $W/mrecv-$enc.err

#!/bin/bash
# The grainy 4K30 file as SpeedHQ; recv_gpu in cpu and gpu mode in turn: its CPU and
# pictures over 10 s, and its page faults. Stopped by PID.
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../../common.sh"
F=$K/media/grain4k30.mp4
OUT=$K/prof
port=7400
for mode in cpu gpu; do
  port=$((port+1))
  $B/idn-codec play $F --port $port --loop --speedhq --seconds 22 --no-mdns --no-node > $OUT/play.log 2>&1 &
  pp=$!
  sleep 2
  $B/examples/recv_gpu 127.0.0.1:$port $mode 16 > $OUT/rg_$mode.log 2>&1 &
  rp=$!
  sleep 4
  perf stat -e page-faults,task-clock -p $rp -- sleep 10 2>&1 | grep -E "page-faults|task-clock" | sed "s/^/  $mode: /"
  wait $rp; wait $pp
  cat $OUT/rg_$mode.log | sed "s/^/  $mode: /"
done

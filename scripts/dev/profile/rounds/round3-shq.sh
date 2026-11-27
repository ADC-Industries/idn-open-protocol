#!/bin/bash
# A 2160p30 switcher's composed program to a receiver over the network: SpeedHQ on the CPU, then the GPU.
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../../common.sh"
W=$(workdir prof3); H=$(hostname | tr a-z A-Z)
source "$S/../lib.sh"; OUT=$W/out
export IDN_NODE=off IDN_CLOCK=off IDN_MACHINE=off
for how in cpu gpu; do
  export IDN_STATE_DIR=$W/state-shq-$how; rm -rf $IDN_STATE_DIR; mkdir -p $IDN_STATE_DIR
  [ $how = cpu ] && export IDN_GPU_SHQ=0
  P=()
  $B/idn send --no-node --name SCam-$how --size 3840x2160 --fps 30/1 --seconds 120 > /dev/null 2>&1 & P+=($!)
  $B/idn-switcher --no-node --format 2160p30 --port 7989 --name SSw-$how > $W/shq-sw-$how.log 2>&1 & P+=($!); sw=$!
  until curl -s http://127.0.0.1:7989/api/v1/state >/dev/null 2>&1; do sleep 0.5; done; sleep 2
  api="curl -s -X POST -H X-IDN-Switcher:1 http://127.0.0.1:7989/api/v1"
  $api/add_input -d "{\"network\":\"$H (SCam-$how)\",\"label\":\"Cam\"}" >/dev/null
  $api/add_input -d '{"title":{"layers":[{"type":"box","rect":[0.05,0.78,0.5,0.14],"colour":"#000000b0","radius":0.1},{"type":"text","rect":[0.07,0.8,0.46,0.1],"text":"Pastor Okafor","size":0.05,"colour":"#ffffff"}]},"label":"T"}' >/dev/null
  on_bus http://127.0.0.1:7989/api/v1 program 1; bug_on http://127.0.0.1:7989/api/v1 2
  sleep 2
  $B/idn recv "$H (SSw-$how)" --no-node --seconds 40 --max-frames 1 --transport quic --out $IDN_STATE_DIR/rx > $W/shq-recv-$how.log 2>&1 & P+=($!); r=$!
  measure shq-$how-switcher $sw 6 10
  wait $r
  for p in "${P[@]}"; do kill $p 2>/dev/null; done; wait 2>/dev/null
  unset IDN_GPU_SHQ
  grep -E "video messages decoded" $W/shq-recv-$how.log
  grep -iE "SpeedHQ encoded on the GPU" $W/shq-sw-$how.log | head -1
done

#!/bin/bash
# A 2160p30 switcher recording its composed program: from the GPU, then read back (IDN_GPU_ENCODE=0).
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../../common.sh"
W=$(workdir prof3); H=$(hostname | tr a-z A-Z)
source "$S/../lib.sh"; OUT=$W/out
export IDN_NODE=off IDN_CLOCK=off
for how in gpu readback; do
  export IDN_STATE_DIR=$W/state-rec-$how; rm -rf $IDN_STATE_DIR; mkdir -p $IDN_STATE_DIR
  [ $how = readback ] && export IDN_GPU_ENCODE=0
  P=()
  $B/idn send --no-node --name RCam-$how --size 3840x2160 --fps 30/1 --seconds 120 > /dev/null 2>&1 & P+=($!)
  $B/idn-switcher --no-node --format 2160p30 --port 7988 --name RSw-$how > $W/rec-sw-$how.log 2>&1 & P+=($!); sw=$!
  until curl -s http://127.0.0.1:7988/api/v1/state >/dev/null 2>&1; do sleep 0.5; done; sleep 2
  api="curl -s -X POST -H X-IDN-Switcher:1 http://127.0.0.1:7988/api/v1"
  $api/add_input -d "{\"network\":\"$H (RCam-$how)\",\"label\":\"Cam\"}" >/dev/null
  $api/add_input -d '{"title":{"layers":[{"type":"box","rect":[0.05,0.78,0.5,0.14],"colour":"#000000b0","radius":0.1},{"type":"text","rect":[0.07,0.8,0.46,0.1],"text":"Pastor Okafor","size":0.05,"colour":"#ffffff"}]},"label":"T"}' >/dev/null
  on_bus http://127.0.0.1:7988/api/v1 program 1; bug_on http://127.0.0.1:7988/api/v1 2
  sleep 2
  rm -f $W/rec-$how.mp4
  $api/stream_add -d "{\"file\":\"$W/rec-$how.mp4\"}" >/dev/null
  measure rec-$how-switcher $sw 6 10
  $api/stream_remove -d '{"id":1}' >/dev/null; sleep 3
  for p in "${P[@]}"; do kill $p 2>/dev/null; done; wait 2>/dev/null
  unset IDN_GPU_ENCODE
  echo "$how: $(ffprobe -v error -count_frames -select_streams v:0 -show_entries stream=nb_read_frames,width,height -of csv=p=0 $W/rec-$how.mp4)"
  grep -E "encoder|read back|drawn" $W/rec-sw-$how.log | head -3
done

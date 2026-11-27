#!/bin/bash
# Profiling round 6: after the encoding choices (processor, format),
# the CPU encoders, the GPU ways into the encoders (VA-API, Vulkan Video, NVENC through
# CUDA) and the shared encoders taking pictures from the GPU. Each process measured with
# lib.sh; rows into out/results.tsv.
#   round6.sh encoders | cpu_encoders | switcher_h264 | node_panel | service | all
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../../common.sh"
set -u
W=$(workdir prof6)
OUT=$W/out; export OUT; mkdir -p "$OUT"
source "$S/../lib.sh"
H=$(hostname | tr a-z A-Z)
# The FFmpeg the Linux package carries (Kvazaar, SVT-AV1, OpenH264 are in it).
FF=${IDN_FFMPEG_LIB:-$ROOT/../build-kit/linux-ffmpeg/ffmpeg-n8.1.3-9-g29e619e767-linux64-lgpl-shared-8.1/lib}
PIDS=()
cleanup() { local i; for ((i=${#PIDS[@]}-1; i>=0; i--)); do kill -TERM ${PIDS[i]} 2>/dev/null; done; for p in "${PIDS[@]}"; do wait $p 2>/dev/null; done; PIDS=(); }
trap cleanup EXIT
api() { curl -s -X POST -H 'X-IDN-Switcher: 1' "http://127.0.0.1:7990/api/v1/$1" -d "$2" >/dev/null; }
# A Node for the scenario, its video settings as given (KEY=VALUE ...).
node() {
  rm -rf "$W/state" "$W/n.sock"; mkdir -p "$W/state"
  export IDN_NODE=$W/n.sock IDN_CLOCK=off IDN_STATE_DIR=$W/state
  "$B/idn-node" run --user --state "$W/state/node" --endpoint "$W/n.sock" --panel-port 7996 > "$W/node.log" 2>&1 & PIDS+=($!)
  NODE=$!
  sleep 2
  [ $# -gt 0 ] && "$B/idn-node" set "$@" >/dev/null
}

# Each GPU encoder here, a 1080p picture on the GPU 120 times: straight from the GPU and
# read back first.
encoders() {
  LD_LIBRARY_PATH=$FF IDN_NODE=off "$B/idn-codec" capture encoders 2>/dev/null | sed 's/^/encoders: /' | tee -a "$OUT/results.tsv"
  LD_LIBRARY_PATH=$FF IDN_NODE=off "$B/idn-codec" capture encoders --bench 2>/dev/null | grep -v "^h264 encoder\|^hevc encoder\|^av1 encoder" | sed 's/^/bench: /' | tee -a "$OUT/results.tsv"
}

# A 1080p30 test camera with an encoder, sending each format to one receiver of
# compressed video: the sender's CPU, by format and processor.
cpu_encoders() {
  local fmt proc
  for proc in cpu gpu; do
    for fmt in speedhq h264 hevc av1; do
      node video-processor=$proc video-format=$fmt
      LD_LIBRARY_PATH=$FF "$B/idn-codec" send --name P6Cam --size 1920x1080 --fps 30 --seconds 120 > "$W/cam-$proc-$fmt.log" 2>&1 & PIDS+=($!); local cam=$!
      sleep 3
      LD_LIBRARY_PATH=$FF "$B/idn-codec" recv "$H (P6Cam)" --max-size 1920x1080 --seconds 60 > "$W/recv-$proc-$fmt.log" 2>&1 & PIDS+=($!)
      measure "enc-$proc-$fmt" $cam 6 10
      echo "enc-$proc-$fmt receiver: $(grep -v '^Svt' "$W/recv-$proc-$fmt.log" | tail -1 | cut -c1-110)" | tee -a "$OUT/results.tsv"
      cleanup
    done
  done
}

# The switcher with three 1080p30 cameras, its program forced to H.264 for a receiver over
# the network: from the GPU (the shared encoder takes its pictures there) and read back
# (IDN_GPU_ENCODE=0).
switcher_h264() {
  local way
  for way in gpu readback; do
    node video-format=h264
    local c
    for c in 1 2 3; do
      "$B/idn" send --name P6Cam$c --size 1920x1080 --fps 30/1 --seconds 300 > "$W/sw-cam$c.log" 2>&1 & PIDS+=($!)
    done
    if [ $way = readback ]; then export IDN_GPU_ENCODE=0; fi
    LD_LIBRARY_PATH=$FF "$B/idn-switcher" --format 1080p30 --port 7990 --name P6Sw > "$W/sw-$way.log" 2>&1 & PIDS+=($!); local sw=$!
    unset IDN_GPU_ENCODE
    until curl -s http://127.0.0.1:7990/api/v1/state >/dev/null 2>&1; do sleep 0.5; done; sleep 2
    for c in 1 2 3; do api add_input "{\"network\":\"$H (P6Cam$c)\",\"label\":\"Cam $c\"}"; done
    on_bus http://127.0.0.1:7990/api/v1 program 1
    LD_LIBRARY_PATH=$FF "$B/idn-codec" recv "$H (P6Sw)" --max-size 1920x1080 --seconds 60 > "$W/sw-recv-$way.log" 2>&1 & PIDS+=($!)
    measure "sw-h264-$way" $sw 8 10
    echo "sw-h264-$way receiver: $(tail -1 "$W/sw-recv-$way.log" | cut -c1-100)" | tee -a "$OUT/results.tsv"
    echo "sw-h264-$way log: $(grep -m2 -i 'encoder\|read back' "$W/sw-$way.log" | tr '\n' ' ' | cut -c1-160)" | tee -a "$OUT/results.tsv"
    cleanup
  done
}

# The Node at rest, then with its panel open (the new frame's status every 2 s).
node_panel() {
  node
  measure node-rest $NODE 3 10
  local url tok
  url=$("$B/idn-node" open 2>&1 | grep -o 'http[^ ]*' | head -1)
  tok=${url#*t=}
  # The panel's session: its token gives a cookie, then the status as the page asks it.
  curl -s -c "$W/cj" "$url" >/dev/null
  ( while :; do curl -s -b "$W/cj" http://127.0.0.1:7996/api/v1/status >/dev/null; sleep 2; done ) & PIDS+=($!)
  measure node-panel $NODE 3 10
  cleanup
}

case ${1:-all} in
  encoders) encoders ;;
  cpu_encoders) cpu_encoders ;;
  switcher_h264) switcher_h264 ;;
  node_panel) node_panel ;;
  service) "$S/round5.sh" service ;;
  all) encoders; cpu_encoders; switcher_h264; node_panel; "$S/round5.sh" service ;;
esac

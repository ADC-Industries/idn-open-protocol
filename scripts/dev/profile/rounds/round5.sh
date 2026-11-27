#!/bin/bash
# Profiling round 5: a typical church service at 1080p30, everything
# running at once on this machine, each process measured with lib.sh, then the machine's
# total CPU and the GPU's load. The IDN Node; three 1080p30 cameras; the switcher with
# them, a slide and a lower third, recording to a file and streaming over RTMP (ffmpeg
# takes it); a projector receiving the program over the network (decoding on the CPU);
# the desk with eight sources routed to it on the Node; the presentation screen (a
# virtual X screen with a moving window) captured.
#   round5.sh service
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../../common.sh"
set -u
W=$(workdir prof5)
OUT=$W/out; export OUT; mkdir -p "$OUT"
source "$S/../lib.sh"
H=$(hostname | tr a-z A-Z)
PIDS=()
cleanup() { local i; for ((i=${#PIDS[@]}-1; i>=0; i--)); do kill -TERM ${PIDS[i]} 2>/dev/null; done; for p in "${PIDS[@]}"; do wait $p 2>/dev/null; done; PIDS=(); }
trap cleanup EXIT
api() { curl -s -X POST -H 'X-IDN-Switcher: 1' "http://127.0.0.1:7990/api/v1/$1" -d "$2" >/dev/null; }
# The machine's CPU over N seconds, in cores, from /proc/stat.
host_cpu() {
  local a b
  a=$(awk '/^cpu /{print $2+$3+$4+$6+$7+$8, $2+$3+$4+$5+$6+$7+$8}' /proc/stat); sleep "$1"
  b=$(awk '/^cpu /{print $2+$3+$4+$6+$7+$8, $2+$3+$4+$5+$6+$7+$8}' /proc/stat)
  echo "$a $b" | awk -v n=$(nproc) '{printf "%.2f cores busy of %d", ($3-$1)/($4-$2)*n, n}'
}
gpu_busy() {
  local i s=0
  for i in $(seq 20); do s=$((s + $(cat /sys/class/drm/card0/device/gpu_busy_percent))); sleep 0.25; done
  echo "$((s / 20))%"
}
# Starts the service (SVC_SECS seconds of camera, default 900); its processes' PIDs in
# SVC_SW, SVC_CAM (one camera), SVC_PROJ, SVC_MIX, SVC_NODE, SVC_CAP (round7.sh soak too).
service_start() {
  local secs=${SVC_SECS:-900}
  rm -rf "$W/state-svc" "$W/svc.sock"; mkdir -p "$W/state-svc"; export IDN_STATE_DIR=$W/state-svc
  export IDN_NODE=$W/svc.sock IDN_CLOCK=off
  "$B/idn-node" run --user --state "$W/state-svc/node" --endpoint "$W/svc.sock" --panel-port 7996 > "$W/svc-node.log" 2>&1 & PIDS+=($!); local node=$!
  sleep 2
  local c
  for c in 1 2 3; do
    "$B/idn" send --name P5Cam$c --size 1920x1080 --fps 30/1 --seconds $secs > "$W/svc-cam$c.log" 2>&1 & PIDS+=($!)
  done
  local cam=${PIDS[-1]}
  for c in 1 2 3 4 5 6 7 8; do
    "$B/idn" send --name P5Mic$c --size 160x90 --static --seconds $secs > "$W/svc-mic$c.log" 2>&1 & PIDS+=($!)
  done
  # The RTMP server the stream goes to.
  ffmpeg -nostdin -loglevel error -listen 1 -f flv -i rtmp://127.0.0.1:19350/live/service -c copy -f null - > "$W/svc-rtmp.log" 2>&1 & PIDS+=($!)
  "$B/idn-switcher" --format 1080p30 --port 7990 --name P5Sw > "$W/svc-sw.log" 2>&1 & PIDS+=($!); local sw=$!
  "$B/idn" mixer --channels 16 --port 7991 > "$W/svc-mixer.log" 2>&1 & PIDS+=($!); local mix=$!
  until curl -s http://127.0.0.1:7990/api/v1/state >/dev/null 2>&1; do sleep 0.5; done; sleep 2
  for c in 1 2 3; do api add_input "{\"network\":\"$H (P5Cam$c)\",\"label\":\"Cam $c\"}"; done
  api add_input "{\"file\":\"$K/vmove/wide.png\",\"label\":\"Slide\"}"
  api add_input '{"title":{"layers":[{"type":"box","rect":[0.05,0.78,0.5,0.14],"colour":"#000000b0","radius":0.1},{"type":"text","rect":[0.07,0.8,0.46,0.1],"text":"Pastor Okafor","size":0.05,"colour":"#ffffff"}]},"label":"Lower third"}'
  on_bus http://127.0.0.1:7990/api/v1 program 1; on_bus http://127.0.0.1:7990/api/v1 preview 2
  bug_on http://127.0.0.1:7990/api/v1 5
  api stream_add "{\"file\":\"$W/svc-rec.mp4\"}"
  api stream_add '{"url":"rtmp://127.0.0.1:19350/live/service"}'
  for c in 1 2 3 4 5 6 7 8; do
    "$B/idn-node" route Mixer $((2*c-1)) "$H (P5Mic$c)" 1 >/dev/null
    "$B/idn-node" route Mixer $((2*c)) "$H (P5Mic$c)" 2 >/dev/null
  done
  # The presentation screen: a virtual X screen with a moving window on it, captured.
  Xvfb :95 -screen 0 1920x1080x24 > /dev/null 2>&1 & PIDS+=($!)
  sleep 1
  "$B/idn" send --no-node --name P5Move --size 1280x720 --fps 30/1 --seconds $secs > "$W/svc-move.log" 2>&1 & PIDS+=($!)
  sleep 2
  DISPLAY=:95 "$B/idn-display" "$H (P5Move)" --window > "$W/svc-movewin.log" 2>&1 & PIDS+=($!)
  sleep 2
  "$B/idn-codec" capture --screen :95 --name P5Screen --fps 30 > "$W/svc-cap.log" 2>&1 & PIDS+=($!); local cap=$!
  # The projector: the program over the network, decoded on the CPU.
  IDN_MACHINE=off "$B/examples/gpu_recv" "$H (P5Sw)" $secs --cpu > "$W/svc-projector.log" 2>&1 & PIDS+=($!); local proj=$!
  sleep 10
  SVC_SW=$sw SVC_CAM=$cam SVC_PROJ=$proj SVC_MIX=$mix SVC_NODE=$node SVC_CAP=$cap
}
service() {
  service_start
  local sw=$SVC_SW cam=$SVC_CAM proj=$SVC_PROJ mix=$SVC_MIX node=$SVC_NODE cap=$SVC_CAP
  echo "service: machine $(host_cpu 10), GPU busy $(gpu_busy)" | tee -a "$OUT/results.tsv"
  measure svc-switcher $sw 0 10
  measure svc-camera $cam 0 8
  measure svc-projector $proj 0 8
  measure svc-desk $mix 0 8
  measure svc-node $node 0 8
  measure svc-capture $cap 0 8
  curl -s http://127.0.0.1:7990/api/v1/state | python3 -c "import json,sys;s=json.load(sys.stdin);print('svc frame_ms',s.get('frame_ms'),'worst',s.get('frame_ms_worst_10s'),'late',s.get('late_frames'),'parts',s.get('frame_parts_ms'))" | tee -a "$OUT/results.tsv"
  echo "svc projector: $(tail -1 "$W/svc-projector.log" | cut -c1-90)" | tee -a "$OUT/results.tsv"
  cleanup
  ffprobe -v error -show_entries stream=codec_type,avg_frame_rate -of compact "$W/svc-rec.mp4" | tr '\n' ' ' | sed 's/^/svc recording: /' | tee -a "$OUT/results.tsv"; echo
  rm -f "$W/svc-rec.mp4"
  unset IDN_NODE IDN_CLOCK
}
# Run, not sourced (round7.sh sources it for service_start).
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  case ${1:-service} in
    service) service ;;
  esac
fi

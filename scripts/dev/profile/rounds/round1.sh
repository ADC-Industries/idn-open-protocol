#!/bin/bash
# The whole stack's costs, scenario by scenario: CPU, page faults/s, RSS, and perf CPU and
# page-fault profiles (out/LABEL.cpu.txt, out/LABEL.pf.txt; rows in out/results.tsv).
# Usage: round1.sh [SCENARIO ...] (default: all). B: binaries (default the release build).
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../../common.sh"
set -u
. "$S/../lib.sh"
AV=$K/tmp/av
H=$(hostname | tr a-z A-Z)
export IDN_NODE=off
PIDS=()
bg() { "$@" > "$OUT/log.$(basename $1).$$.$RANDOM" 2>&1 & PIDS+=($!); LAST=$!; }
stop_all() { for p in "${PIDS[@]}"; do kill $p 2>/dev/null; done; for p in "${PIDS[@]}"; do wait $p 2>/dev/null; done; PIDS=(); sleep 1; }

# Sending SpeedHQ (the encoder), one receiver.
s_send() { local size=$1
  bg $B/idn-send --no-node --no-mdns --port 7781 --size $size --fps 60/1 --fourcc SHQ2 --seconds 36; local S=$LAST
  sleep 1; bg $B/idn-recv 127.0.0.1:7781 --no-node --seconds 34
  measure send-shq2-$size $S; stop_all; }
# Receiving SpeedHQ (the decoder).
s_recv() { local size=$1 fcc=$2 extra=${3:-}
  bg $B/idn-send --no-node --no-mdns --port 7781 --size $size --fps 60/1 --fourcc $fcc --seconds 36
  sleep 1; bg $B/idn-recv 127.0.0.1:7781 --no-node --seconds 34 $extra; local R=$LAST
  measure recv-$(echo $fcc | tr A-Z a-z)-$size${extra:+-low} $R; stop_all; }
# Receiving H.264 (an NDI|HX-style stream of the 4K test file): idn-recv saves the
# compressed stream without decoding it, so this is the network side only (decoding is
# FFmpeg's, in the programs that show it: OBS, the switcher).
s_recv_hx() {
  bg $B/idn-send --no-node --no-mdns --port 7781 --hx $ROOT/../handoff/lab4h/uhd.h264 --seconds 36
  sleep 1; bg $B/idn-recv 127.0.0.1:7781 --no-node --seconds 34; local R=$LAST
  measure recv-h264-uhd-passthrough $R; stop_all; }
# The media player sending a 1080p30 file with sound.
s_player() {
  bg $B/idn-codec play $AV/fb20.mp4 --name ProfPlay --loop --port 7782; local P=$LAST
  sleep 1; bg $B/idn-recv 127.0.0.1:7782 --no-node --seconds 34
  measure player-1080p30 $P; stop_all; }
# The switcher: four 1080p30 SpeedHQ inputs, a cut to one, its program received and
# recorded. $1: "" (GPU) or --cpu.
s_switcher() { local mode=${1:-}
  for i in 1 2 3 4; do bg $B/idn-send --no-node --no-mdns --port 779$i --size 1920x1080 --fps 30/1 --fourcc SHQ2 --name In$i --seconds ${SWSECS:-40}; done
  bg $B/idn-switcher --format 1080p30 --port 27971 --no-node $mode; local W=$LAST
  sleep 4
  api() { curl -s -H "X-IDN-Switcher: 1" -H "Content-Type: application/json" -X POST "http://127.0.0.1:27971/api/v1/$1" -d "$2"; }
  for i in 1 2 3 4; do api add_input "{\"network\": \"127.0.0.1:779$i\"}" >/dev/null; done
  sleep 2; on_bus http://127.0.0.1:27971/api/v1 program 1; on_bus http://127.0.0.1:27971/api/v1 preview 2; api audio '{"source": "program"}' >/dev/null
  rm -f $OUT/sw.mkv; api stream_add "{\"file\": \"$OUT/sw.mkv\"}" >/dev/null
  bg $B/idn-recv "$H (Switcher)" --no-node --seconds 36
  measure switcher-4x1080p30${mode:+-cpu} $W 6 ${SWWIN:-10}; stop_all; }
# Sound: a card's input sent, and received to a card (the ALSA loopback).
s_audio() {
  bg $B/idn audio send --device "hw:CARD=Loopback,DEV=1" --name ProfSnd --port 7783 --no-mdns; local S=$LAST
  sleep 1; bg $B/idn audio recv 127.0.0.1:7783 --device "hw:CARD=Loopback,DEV=0"; local R=$LAST
  measure audio-send $S; measure audio-recv $R 0; stop_all; }
# Idle: the Node with nothing connected.
s_idle() {
  rm -rf $OUT/node; bg env IDN_CLOCK=off $B/idn-node run --user --state $OUT/node --endpoint $OUT/node.sock --panel-port 0 --lan-panel-port 0; local N=$LAST
  measure node-idle $N 3 10; stop_all; }

ALL="send2160 send1080 recv2160 recv1080 recv1080a recvlow recvhx player switcher switchercpu audio idle"
for s in ${@:-$ALL}; do
  case $s in
    send2160) s_send 3840x2160;; send1080) s_send 1920x1080;;
    recv2160) s_recv 3840x2160 SHQ2;; recv1080) s_recv 1920x1080 SHQ2;; recv1080a) s_recv 1920x1080 SHQ7;;
    recvlow) s_recv 3840x2160 SHQ2 --low;; recvodd) s_recv 1918x1080 SHQ2;; recvodd4k) s_recv 3838x2160 SHQ2;; recvhx) s_recv_hx;; player) s_player;;
    switcher) s_switcher;; switchercpu) s_switcher --cpu;; audio) s_audio;; idle) s_idle;;
    *) echo "unknown scenario $s";;
  esac
done

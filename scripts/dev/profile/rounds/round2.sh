#!/bin/bash
# Profiling round 2: church scenarios, each process measured with
# lib.sh (CPU %, page faults/s, RSS, perf profiles into out/LABEL.*).
#   round2.sh sw1080 | sw2160 | mixer | all
# Isolated state (IDN_STATE_DIR), no Node, test sources from `idn send`.
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../../common.sh"
set -u
W=$(workdir prof2)
B=${B_OVERRIDE:-$B}
OUT=${OUT:-$W/out}; export OUT; mkdir -p "$OUT"
source "$S/../lib.sh"
H=$(hostname | tr a-z A-Z)
export IDN_NODE=off
PIDS=()
# The programs under xvfb-run are added after it: stopped first, last added first.
cleanup() { local i; for ((i=${#PIDS[@]}-1; i>=0; i--)); do kill -TERM ${PIDS[i]} 2>/dev/null; done; for p in "${PIDS[@]}"; do wait $p 2>/dev/null; done; PIDS=(); }
trap cleanup EXIT
api() { curl -s -X POST http://127.0.0.1:7983/api/v1/$1 -H 'X-IDN-Switcher: 1' -d "$2" >/dev/null; }
state() { curl -s http://127.0.0.1:7983/api/v1/state; }

# A service: two cameras, a slide (a picture), a lower third as a global layer, the program
# recorded; at 2160p also a virtual shot on program and a vertical mix.
switcher() {
  local tag=$1 size=$2 fmt=$3
  rm -rf $W/state-$tag; mkdir -p $W/state-$tag; export IDN_STATE_DIR=$W/state-$tag
  $B/idn send --no-node --name P2Cam1-$tag --size $size --fps 30/1 --seconds 600 > $W/cam1.log 2>&1 & PIDS+=($!); local cam=$!
  $B/idn send --no-node --name P2Cam2-$tag --size $size --fps 30/1 --seconds 600 > $W/cam2.log 2>&1 & PIDS+=($!)
  $B/idn-switcher --no-node --format $fmt --port 7983 --name P2Sw > $W/sw-$tag.log 2>&1 & PIDS+=($!); local sw=$!
  until state >/dev/null 2>&1; do sleep 0.5; done; sleep 2
  api add_input "{\"network\":\"$H (P2Cam1-$tag)\",\"label\":\"Cam 1\"}"
  api add_input "{\"network\":\"$H (P2Cam2-$tag)\",\"label\":\"Cam 2\"}"
  api add_input "{\"file\":\"$K/vmove/wide.png\",\"label\":\"Slide\"}"
  api add_input '{"title":{"layers":[{"type":"box","rect":[0.05,0.78,0.5,0.14],"colour":"#000000b0","radius":0.1},{"type":"text","rect":[0.07,0.8,0.46,0.1],"text":"Pastor Okafor","size":0.05,"colour":"#ffffff"}]},"label":"Lower third"}'
  on_bus http://127.0.0.1:7983/api/v1 program 1; on_bus http://127.0.0.1:7983/api/v1 preview 2
  bug_on http://127.0.0.1:7983/api/v1 4
  if [ $tag = sw2160 ]; then
    if [ -z "${NOVIRT:-}" ]; then
      api add_input '{"virtual":{"input":1,"view":{"x":0.4,"y":0.5,"zoom":1.5}},"label":"Shot"}'
      on_bus http://127.0.0.1:7983/api/v1 program 5
    fi
    [ -z "${NOMIX:-}" ] && api add_mix '{"name":"Vertical","format":"1080x1920"}'
  fi
  [ -z "${NOREC:-}" ] && api stream_add "{\"file\":\"$W/rec-$tag.mp4\"}"
  if [ -n "${RSSWATCH:-}" ]; then for i in $(seq 9); do sleep 10; echo "rss $((i*10)) s: $(awk '/VmRSS/{print int($2/1024)}' /proc/$sw/status) MB" | tee -a $OUT/results.tsv; done; fi
  measure $tag-switcher $sw 8 10
  [ -n "${PERF:-}" ] && perf record -F 499 ${PERF_CG:--g} -p $sw -o $W/$tag.perf -- sleep 8 > /dev/null 2>&1
  measure $tag-camsend $cam 1 5
  state | python3 -c "import json,sys;s=json.load(sys.stdin);print('$tag frame_ms',s.get('frame_ms'),'worst',s.get('frame_ms_worst_10s'),'late',s.get('late_frames'),'gpu_ms',s.get('gpu_ms'),'parts',s.get('frame_parts_ms'),'compositor',s.get('compositor'))" | tee -a $OUT/results.tsv
  cleanup; [ -n "${KEEPREC:-}" ] || rm -f $W/rec-$tag.mp4
}

# The desk: its own benchmark (32 busy channels, no network), profiled.
mixer() {
  ( cd $ROOT && cargo run -q --release -p idn-mixer --example desk_bench -- 20 > $OUT/desk_bench.txt 2>&1 ) & local runner=$!
  sleep 6
  local pid=$(pgrep -n -f "examples/desk_bench")
  [ -n "$pid" ] && measure mixer-desk $pid 1 8
  wait $runner; cat $OUT/desk_bench.txt | tee -a $OUT/results.tsv
}

# A projector: idn-display in a window of a virtual X screen (no desktop here: decode,
# conversion and upload are real, the screen's own presentation isn't).
display() {
  rm -rf $W/state-disp; mkdir -p $W/state-disp; export IDN_STATE_DIR=$W/state-disp
  $B/idn send --no-node --name P2Prog --size 1920x1080 --fps 30/1 --seconds 600 > $W/prog.log 2>&1 & PIDS+=($!)
  sleep 2
  xvfb-run -a -s "-screen 0 1920x1080x24" $B/idn-display "$H (P2Prog)" --window > $W/disp.log 2>&1 & PIDS+=($!)
  sleep 4
  local pid=$(pgrep -n -x idn-display); [ -n "$pid" ] && PIDS+=($pid)
  [ -n "$pid" ] && measure display1080 $pid 4 10
  cleanup
}

# Sound through the Node's routing into a live desk and out again: four test sources
# (2 ch each, tiny static pictures), the mixer's 8 channels routed from them.
routing() {
  rm -rf $W/state-rt $W/node.sock; mkdir -p $W/state-rt; export IDN_STATE_DIR=$W/state-rt
  export IDN_NODE=$W/node.sock IDN_CLOCK=off
  $B/idn-node run --user --state $W/state-rt/node --endpoint $W/node.sock --panel-port 7992 > $W/node.log 2>&1 & PIDS+=($!); local node=$!
  sleep 2
  for i in 1 2 3 4; do
    $B/idn send --name P2Tone$i --size 160x90 --static --seconds 600 > $W/tone$i.log 2>&1 & PIDS+=($!)
  done
  $B/idn mixer --channels 8 --port 7984 > $W/mixer.log 2>&1 & PIDS+=($!); local mix=$!
  sleep 4
  for i in 1 2 3 4; do
    $B/idn-node route Mixer $((2*i-1)) "$H (P2Tone$i)" 1 >/dev/null
    $B/idn-node route Mixer $((2*i)) "$H (P2Tone$i)" 2 >/dev/null
  done
  measure routing-mixer $mix 6 10
  measure routing-node $node 1 5
  cleanup; export IDN_NODE=off; unset IDN_CLOCK
}

# Listening on phones: one channel from a test source, 15 listeners (a headless browser,
# WHEP, sound only), the Opus encoding once and the fan-out.
listen() {
  rm -rf $W/state-ln; mkdir -p $W/state-ln; export IDN_STATE_DIR=$W/state-ln
  $B/idn send --no-node --name P2Voice --size 160x90 --static --seconds 600 > $W/voice.log 2>&1 & PIDS+=($!)
  sleep 2
  $B/idn-codec listen "English=$H (P2Voice)" --port 7986 --no-node > $W/listen.log 2>&1 & PIDS+=($!); local ln=$!
  sleep 3
  local shell=$(ls -d ~/.cache/ms-playwright/chromium_headless_shell-*/*/chrome-headless-shell | head -1)
  $shell --headless --no-sandbox --disable-web-security --user-data-dir=$W/chrome --autoplay-policy=no-user-gesture-required \
    --remote-debugging-port=9333 "file://$W/crowd.html?n=15" > $W/chrome.log 2>&1 & PIDS+=($!)
  sleep 8
  curl -s http://127.0.0.1:7986/api/v1/state | tee -a $OUT/results.tsv; echo | tee -a $OUT/results.tsv
  measure listen15 $ln 2 10
  cleanup
}

# Sound cards (ALSA's loopback, card "Loopback"): a test source played into it by
# `idn audio recv`, captured back by `idn audio send` and sent on: both card paths.
cards() {
  rm -rf $W/state-cards; mkdir -p $W/state-cards; export IDN_STATE_DIR=$W/state-cards
  $B/idn send --no-node --name P2Band --size 160x90 --static --seconds 600 > $W/band.log 2>&1 & PIDS+=($!)
  sleep 2
  $B/idn audio recv "$H (P2Band)" --no-node --device "hw:CARD=Loopback,DEV=0" --seconds 600 > $W/arecv.log 2>&1 & PIDS+=($!); local rcv=$!
  $B/idn audio send --no-node --name P2Card --device "hw:CARD=Loopback,DEV=1" --seconds 600 > $W/asend.log 2>&1 & PIDS+=($!); local snd=$!
  sleep 4
  measure card-recv $rcv 4 10
  measure card-send $snd 1 10
  grep -v "^ALSA" $W/arecv.log | tail -1 | tee -a $OUT/results.tsv
  grep -v "^ALSA" $W/asend.log | tail -1 | tee -a $OUT/results.tsv
  cleanup
}

# The site's Discovery Server and Site Manager with ten sources registered with it.
sitemgr() {
  rm -rf $W/state-sm; mkdir -p $W/state-sm/trust; export IDN_STATE_DIR=$W/state-sm
  $B/idn discovery-server --port 5967 --trust-dir $W/state-sm/trust --passphrase p2 --panel-port 29202 --no-announce < /dev/null > $W/sm.log 2>&1 & PIDS+=($!); local sm=$!
  sleep 2
  for i in $(seq 10); do
    $B/idn send --no-node --name P2Src$i --size 160x90 --static --server 127.0.0.1:5967 --no-mdns --seconds 600 > $W/src$i.log 2>&1 & PIDS+=($!)
  done
  sleep 4
  measure sitemgr $sm 4 10
  cleanup
}

# IDN Data: three feeds (CSV, JSON, RSS-less XML) read every second, sent as sources.
data() {
  rm -rf $W/state-data; mkdir -p $W/state-data; export IDN_STATE_DIR=$W/state-data
  printf 'name,score\nA,1\nB,2\nC,3\n' > $W/state-data/t.csv
  printf '[{"name":"A","n":1},{"name":"B","n":2}]' > $W/state-data/t.json
  printf '<rows><row><name>A</name></row><row><name>B</name></row></rows>' > $W/state-data/t.xml
  $B/idn data --no-node --port 7976 --state-dir $W/state-data > $W/data.log 2>&1 & PIDS+=($!); local d=$!
  until curl -s http://127.0.0.1:7976/api/v1/state >/dev/null; do sleep 0.5; done
  for f in csv json xml; do
    curl -s -X POST http://127.0.0.1:7976/api/v1/feed_add -H 'X-IDN-Data: 1' -d "{\"name\":\"P2$f\",\"from\":\"$W/state-data/t.$f\",\"format\":\"auto\",\"every\":1}" >> $W/data.log
  done
  measure data $d 4 10
  cleanup
}

# OMT both ways: a 1080p30 camera sent as OMT (VMX), and that taken back as IDN.
omt() {
  rm -rf $W/state-omt; mkdir -p $W/state-omt; export IDN_STATE_DIR=$W/state-omt
  $B/idn send --no-node --name P2OmtCam --size 1920x1080 --fps 30/1 --seconds 600 > $W/omtcam.log 2>&1 & PIDS+=($!)
  sleep 2
  $B/idn omt send "$H (P2OmtCam)" --name P2Omt --no-node --seconds 600 > $W/omtsend.log 2>&1 & PIDS+=($!); local s=$!
  sleep 3
  local addr=$($B/idn omt list 2>/dev/null | grep -o 'omt://[^ ]*' | head -1)
  $B/idn omt recv "${addr:-$H (P2Omt)}" --name P2OmtBack --no-node --seconds 600 > $W/omtrecv.log 2>&1 & PIDS+=($!); local r=$!
  measure omt-send $s 4 10
  measure omt-recv $r 1 10
  tail -n 2 $W/omtsend.log $W/omtrecv.log | tee -a $OUT/results.tsv
  cleanup
}

# Captions: speech from a file, looped, captioned by base.en on the GPU.
captions() {
  rm -rf $W/state-cap; mkdir -p $W/state-cap; export IDN_STATE_DIR=$W/state-cap
  $B/idn codec play $K/cap-music/speech-jfk.wav --loop --name P2Speech --no-node > $W/speech.log 2>&1 & PIDS+=($!)
  sleep 2
  $B/idn captions "$H (P2Speech)" --model base.en --models-dir $K/models --no-node > $W/cap.log 2>&1 & PIDS+=($!); local c=$!
  sleep 8
  measure captions $c 4 10
  grep -c "idn-captions:" $W/cap.log | sed 's/^/caption lines: /' | tee -a $OUT/results.tsv
  cleanup
}

# AES67 both ways: a source's sound as a stream (multicast, TTL 1), and that taken back.
aes67() {
  rm -rf $W/state-aes; mkdir -p $W/state-aes; export IDN_STATE_DIR=$W/state-aes
  $B/idn send --no-node --name P2AesSrc --size 160x90 --static --seconds 600 > $W/aessrc.log 2>&1 & PIDS+=($!)
  sleep 2
  $B/idn audio aes67 send "$H (P2AesSrc)" --name P2Aes --address 239.69.83.67 --ttl 1 --no-node --seconds 600 > $W/aessend.log 2>&1 & PIDS+=($!); local s=$!
  sleep 3
  $B/idn audio aes67 recv P2Aes --name P2AesBack --no-node --seconds 600 > $W/aesrecv.log 2>&1 & PIDS+=($!); local r=$!
  measure aes67-send $s 4 10
  measure aes67-recv $r 1 10
  tail -n 2 $W/aessend.log $W/aesrecv.log | tee -a $OUT/results.tsv
  cleanup
}

case ${1:-all} in
  sw1080) switcher sw1080 1920x1080 1080p30 ;;
  sw2160) switcher sw2160 3840x2160 2160p30 ;;
  mixer) mixer ;;
  display) display ;;
  routing) routing ;;
  listen) listen ;;
  cards) cards ;;
  sitemgr) sitemgr ;;
  data) data ;;
  omt) omt ;;
  captions) captions ;;
  aes67) aes67 ;;
  sweep) sitemgr; data; omt; captions; aes67 ;;
  all) switcher sw1080 1920x1080 1080p30; switcher sw2160 3840x2160 2160p30; mixer; display; routing; listen; cards ;;
esac

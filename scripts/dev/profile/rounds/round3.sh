#!/bin/bash
# Profiling round 3: what came since round 2 (GPU sharing, game mode,
# IDN Remote's previews, the Node at rest, encryption's cost), and round 2's baselines
# again after the review's fixes. Each process measured with lib.sh.
#   round3.sh nodeidle | tls | gpushare | site | baseline | all
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../../common.sh"
set -u
W=$(workdir prof3)
OUT=$W/out; export OUT; mkdir -p "$OUT"
source "$S/../lib.sh"
H=$(hostname | tr a-z A-Z)
export IDN_NODE=off IDN_CLOCK=off
PIDS=()
# The programs under xvfb-run are added after it: stopped first, last added first.
cleanup() { local i; for ((i=${#PIDS[@]}-1; i>=0; i--)); do kill -TERM ${PIDS[i]} 2>/dev/null; done; for p in "${PIDS[@]}"; do wait $p 2>/dev/null; done; PIDS=(); }
trap cleanup EXIT
fresh() { rm -rf $W/state-$1; mkdir -p $W/state-$1; export IDN_STATE_DIR=$W/state-$1; }

# The Node with nothing to do, alone and then a member of a site (passphrase join).
nodeidle() {
  fresh idle
  $B/idn-node run --user --state $W/state-idle/node --endpoint $W/n.sock --panel-port 7993 > $W/node.log 2>&1 & PIDS+=($!); local node=$!
  measure node-idle $node 5 20
  $B/idn discovery-server --port 5968 --trust-dir $W/state-idle/trust --passphrase p3 --panel-port 7968 --no-announce < /dev/null > $W/ds.log 2>&1 & PIDS+=($!); local ds=$!
  IDN_NODE=$W/n.sock $B/idn-node set server=127.0.0.1:5968 passphrase=p3 > /dev/null
  measure node-in-site $node 8 20
  measure site-manager-idle $ds 1 10
  cleanup
}

# 1080p30 over the network on this machine (shared memory off): plain, then encrypted
# with a passphrase (TLS control, secure QUIC media: the same as linked computers').
tls() {
  export IDN_MACHINE=off
  for mode in plain secure; do
    fresh tls-$mode
    # plain: NDI's QUIC, unencrypted; secure: the IDN link with a passphrase (TLS, as
    # linked computers' sessions are).
    local pp=(); [ $mode = secure ] && pp=(--passphrase p3)
    local tr=(); [ $mode = plain ] && tr=(--transport quic)
    $B/idn send --no-node --name P3Tls-$mode --size 1920x1080 --fps 30/1 --seconds 600 "${pp[@]}" > $W/tls-send.log 2>&1 & PIDS+=($!); local s=$!
    sleep 2
    $B/idn recv "$H (P3Tls-$mode)" --no-node --seconds 600 --max-frames 1 --out $W/state-tls-$mode/rx "${pp[@]}" "${tr[@]}" > $W/tls-recv-$mode.log 2>&1 & PIDS+=($!); local r=$!
    measure tls-$mode-send $s 6 10
    measure tls-$mode-recv $r 1 10
    cleanup
  done
  unset IDN_MACHINE
}

# A 2160p30 switcher's composed program (a camera with a lower third on air) received on
# this machine three ways: GPU to GPU (gpu/1), through shared memory into this memory, and
# over the network. examples/gpu_recv receives with no window (Xvfb has no DRI3: a window
# there draws with lavapipe, on no GPU the switcher has).
gpushare() {
  local G=$B/examples/gpu_recv
  for how in ${HOWS:-gpu memory network}; do
    fresh gs-$how
    [ $how = network ] && export IDN_MACHINE=off
    $B/idn send --no-node --name P3Cam-$how --size 3840x2160 --fps 30/1 --seconds 600 > $W/gs-cam.log 2>&1 & PIDS+=($!)
    $B/idn-switcher --no-node --format 2160p30 --port 7985 --name P3Sw-$how > $W/gs-sw-$how.log 2>&1 & PIDS+=($!); local sw=$!
    until curl -s http://127.0.0.1:7985/api/v1/state >/dev/null 2>&1; do sleep 0.5; done; sleep 2
    local api="curl -s -X POST -H X-IDN-Switcher:1 http://127.0.0.1:7985/api/v1"
    $api/add_input -d "{\"network\":\"$H (P3Cam-$how)\",\"label\":\"Cam\"}" >/dev/null
    $api/add_input -d '{"title":{"layers":[{"type":"box","rect":[0.05,0.78,0.5,0.14],"colour":"#000000b0","radius":0.1},{"type":"text","rect":[0.07,0.8,0.46,0.1],"text":"Pastor Okafor","size":0.05,"colour":"#ffffff"}]},"label":"Lower third"}' >/dev/null
    on_bus http://127.0.0.1:7985/api/v1 program 1; bug_on http://127.0.0.1:7985/api/v1 2
    sleep 3
    local cpu=(); [ $how = memory ] && cpu=(--cpu)
    $G "$H (P3Sw-$how)" 600 "${cpu[@]}" > $W/gs-recv-$how.log 2>&1 & PIDS+=($!); local r=$!
    measure gs-$how-switcher $sw 6 10
    measure gs-$how-recv $r 1 10
    echo "$how: $(tail -1 $W/gs-recv-$how.log)" >> $OUT/results.tsv
    cleanup; unset IDN_MACHINE
  done
}

# A site on this machine: the Site Manager, a Node joined by passphrase, the screen (a
# virtual X screen with a moving test window) captured for remote control, IDN Remote
# with its preview of it, then a game session (idn-display --control --game).
site() {
  fresh site
  $B/idn discovery-server --port 5969 --trust-dir $W/state-site/trust --passphrase p3 --panel-port 7969 --no-announce < /dev/null > $W/site-ds.log 2>&1 & PIDS+=($!)
  $B/idn-node run --user --state $W/state-site/node --endpoint $W/s.sock --panel-port 7994 > $W/site-node.log 2>&1 & PIDS+=($!); local node=$!
  sleep 2
  export IDN_NODE=$W/s.sock
  $B/idn-node set server=127.0.0.1:5969 passphrase=p3 > /dev/null
  sleep 6
  Xvfb :93 -screen 0 1920x1080x24 > /dev/null 2>&1 & PIDS+=($!)
  sleep 1
  # Something moving on the screen: the switcher's test page isn't needed, a display of
  # a test source in a window will do.
  $B/idn send --name P3Move --size 1280x720 --fps 30/1 --seconds 600 > $W/site-move.log 2>&1 & PIDS+=($!)
  sleep 2
  DISPLAY=:93 $B/idn-display "$H (P3Move)" --window > $W/site-movewin.log 2>&1 & PIDS+=($!)
  sleep 3
  $B/idn-codec capture --screen :93 --name P3Screen --fps 30 --control > $W/site-cap.log 2>&1 & PIDS+=($!); local cap=$!
  sleep 4
  measure site-capture30 $cap 2 10
  xvfb-run -a -s "-screen 0 1600x900x24" $B/idn-remote > $W/site-remote.log 2>&1 & PIDS+=($!)
  sleep 8
  local rem=$(pgrep -n -x idn-remote); [ -n "$rem" ] && PIDS+=($rem)
  [ -n "$rem" ] && measure site-remote-previews $rem 2 12
  measure site-capture-with-preview $cap 1 10
  kill -TERM $cap; wait $cap 2>/dev/null
  $B/idn-codec capture --screen :93 --name P3Game --game --control > $W/site-game.log 2>&1 & PIDS+=($!); local game=$!
  sleep 3
  xvfb-run -a -s "-screen 0 1920x1080x24" $B/idn-display "$H (P3Game)" --control --game --window > $W/site-gamewin.log 2>&1 & PIDS+=($!)
  sleep 6
  local gd=$(pgrep -n -f "idn-display .*P3Game"); [ -n "$gd" ] && PIDS+=($gd)
  measure site-game-capture $game 1 10
  [ -n "$gd" ] && measure site-game-display $gd 1 10
  measure site-node-busy $node 1 10
  grep -E "not sent|takes keyboard|fps|ms" $W/site-gamewin.log | tail -3 >> $OUT/results.tsv
  cleanup; export IDN_NODE=off
}

# Round 2's main baselines again (switcher 1080p and 2160p, desk, projector).
baseline() {
  ( OUT=$OUT $S/round2.sh sw1080; OUT=$OUT $S/round2.sh sw2160; OUT=$OUT $S/round2.sh mixer; OUT=$OUT $S/round2.sh display )
}

case ${1:-all} in
  nodeidle) nodeidle ;;
  tls) tls ;;
  gpushare) gpushare ;;
  site) site ;;
  baseline) baseline ;;
  all) nodeidle; tls; gpushare; site; baseline ;;
esac

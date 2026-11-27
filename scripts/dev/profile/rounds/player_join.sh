#!/bin/bash
# player_join.sh MODE: a pass-through receiver watching; then a SpeedHQ receiver joins:
# seconds until it has 30 pictures, and the player's CPU before and after.
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../../common.sh"
MODE=$1; F=$K/media/t4k60.mp4
D=$(mktemp -d $K/tmp/pjoin.XXXX); cd $D
IDN_NODE=off $B/idn-codec play $F --idle $MODE --decoder gpu --loop --no-node --name PJ > play.log 2>&1 < /dev/null & P=$!
sleep 3
PORT=$(ss -ltnp | grep "pid=$P," | grep -v ":5960 " | head -1 | awk '{print $4}' | sed 's/.*://')
IDN_NODE=off timeout 40 $B/idn-codec recv 127.0.0.1:$PORT --no-node --codecs hevc --seconds 30 > recv.log 2>&1 & R=$!
sleep 6
t() { awk '{print $14+$15}' /proc/$P/stat; }
a=$(t); sleep 4; b=$(t)
s0=$(date +%s.%N)
IDN_NODE=off timeout 30 $B/idn-recv 127.0.0.1:$PORT --no-node --seconds 15 > shq.log 2>&1 & Q=$!
# Until its first picture line, then the CPU while it watches.
until grep -q "video:" shq.log 2>/dev/null; do sleep 0.05; done
s1=$(date +%s.%N)
sleep 2; c=$(t); sleep 6; d=$(t)
wait $Q
echo "$MODE: before $(( (b-a) * 100 / $(getconf CLK_TCK) / 4 ))%, with a SpeedHQ receiver $(( (d-c) * 100 / $(getconf CLK_TCK) / 6 ))% of a core; its first picture after $(python3 -c "print(round($s1 - $s0, 2))") s; $(grep -o 'frames: [0-9]*[^,]*' shq.log | head -1)"
kill $P $R 2>/dev/null; wait $P $R 2>/dev/null
cd $K/tmp && rm -rf "$D"

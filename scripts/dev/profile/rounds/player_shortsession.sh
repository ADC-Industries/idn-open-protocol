#!/bin/bash
# player_shortsession.sh: a 4K60 HEVC player (GPU decoding, warm); a pass-through receiver
# connects for 0.4 s and leaves (before the file's keyframe), then one stays: the player's CPU.
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../../common.sh"
F=$K/media/t4k60.mp4
D=$(mktemp -d $K/tmp/pshort.XXXX); cd $D
IDN_NODE=off $B/idn-codec play $F --decoder gpu --loop --no-node --name PS > play.log 2>&1 < /dev/null & P=$!
sleep 3
PORT=$(ss -ltnp | grep "pid=$P," | grep -v ":5960 " | head -1 | awk '{print $4}' | sed 's/.*://')
for i in 1 2 3; do IDN_NODE=off timeout -s INT ${T:-0.4} $B/idn-codec recv 127.0.0.1:$PORT --no-node --allow > short$i.log 2>&1; sleep 0.3; done
IDN_NODE=off timeout 30 $B/idn-codec recv 127.0.0.1:$PORT --no-node --allow --seconds 16 > recv.log 2>&1 & R=$!
sleep 6
t() { awk '{print $14+$15}' /proc/$P/stat; }
a=$(t); sleep 6; b=$(t)
echo "after short sessions: player $(( (b-a) * 100 / $(getconf CLK_TCK) / 6 ))% of a core"
wait $R; grep -h "pictures:" recv.log
kill $P 2>/dev/null; wait $P 2>/dev/null
tr "\r" "\n" < play.log | grep -a "receiver\|pictures needed\|pictures not" | grep -v Playing | head; cd $K/tmp && rm -rf "$D"

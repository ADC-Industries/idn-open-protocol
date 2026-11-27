#!/bin/bash
# player_idle.sh MODE [FILE]: a file played (GPU decoding) into one pass-through
# receiver; the player's CPU over 10 s and what the receiver got.
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../../common.sh"
MODE=$1; F=${2:-$K/media/t4k60.mp4}
D=$(mktemp -d $K/tmp/pidle.XXXX); cd $D
IDN_NODE=off $B/idn-codec play $F --idle $MODE --decoder gpu --loop --no-node --name PI > play.log 2>&1 < /dev/null & P=$!
sleep 3
PORT=$(ss -ltnp | grep "pid=$P," | grep -v ":5960 " | head -1 | awk '{print $4}' | sed 's/.*://')
IDN_NODE=off timeout 30 $B/idn-codec recv 127.0.0.1:$PORT --no-node --codecs hevc --seconds 20 > recv.log 2>&1 & R=$!
sleep 6
t() { awk '{print $14+$15}' /proc/$P/stat; }
a=$(t); sleep 10; b=$(t)
wait $R
echo "$MODE: player $(( (b-a) * 100 / $(getconf CLK_TCK) / 10 ))% of a core; $(grep -o 'pictures: .*' recv.log)"
grep -o "passed [0-9]* encoded [0-9]*" play.log | tail -1
kill $P; wait $P 2>/dev/null
cd $K/tmp && rm -rf "$D"

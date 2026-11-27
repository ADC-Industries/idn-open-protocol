#!/bin/bash
# player_auto.sh FILE: the player with the automatic decoder choice and a SpeedHQ receiver
# (pictures needed) for 25 s: the decoder it settles on and its CPU in the last 8 s.
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../../common.sh"
F=$1; B=$B
D=$(mktemp -d $K/tmp/pauto.XXXX); cd $D
IDN_NODE=off $B/idn-codec play $F --loop --no-node --name PA $DEC > play.log 2>&1 < /dev/null & P=$!
sleep 3
PORT=$(ss -ltnp | grep "pid=$P," | grep -v ":5960 " | head -1 | awk '{print $4}' | sed 's/.*://')
IDN_NODE=off timeout 40 $B/idn-recv 127.0.0.1:$PORT --no-node --seconds 25 > recv.log 2>&1 & Q=$!
sleep 15
t() { awk '{print $14+$15}' /proc/$P/stat; }
a=$(t); sleep 8; b=$(t)
wait $Q
echo "$(basename $F): player $(( (b-a) * 100 / $(getconf CLK_TCK) / 8 ))% of a core; $(grep -o 'frames: [0-9]*[^,]*' recv.log | tail -1)"
tr '\r' '\n' < play.log | grep -ao "player: .*" | head -5
tr '\r' '\n' < play.log | grep -a "^Playing" | tail -1 | grep -o "decoder [^,]*\|shifted [^,]*"
kill $P 2>/dev/null; wait $P 2>/dev/null
cd $K/tmp && rm -rf "$D"

#!/bin/bash
# player_still.sh FILE: idn codec play of a picture file, an idn-recv (SpeedHQ) for 20 s:
# pictures received and the player's CPU.
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../../common.sh"
F=$1; B=$B
D=$(mktemp -d $K/tmp/pstill.XXXX); cd $D
IDN_NODE=off $B/idn-codec play $F --loop --no-node --name PS > play.log 2>&1 < /dev/null & P=$!
sleep 3
PORT=$(ss -ltnp | grep "pid=$P," | grep -v ":5960 " | head -1 | awk '{print $4}' | sed 's/.*://')
IDN_NODE=off timeout 30 $B/idn-recv 127.0.0.1:$PORT --no-node --seconds 20 > recv.log 2>&1 & Q=$!
sleep 5
t() { awk '{print $14+$15}' /proc/$P/stat; }
a=$(t); sleep 10; b=$(t)
wait $Q
echo "player $(( (b-a) * 100 / $(getconf CLK_TCK) / 10 ))% of a core; $(grep -o 'frames: [0-9]*[^,]*' recv.log | tail -1)"
tail -3 recv.log; tail -3 play.log
kill $P 2>/dev/null; wait $P 2>/dev/null
cd $K/tmp && rm -rf "$D"

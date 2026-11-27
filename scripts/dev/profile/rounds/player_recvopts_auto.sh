#!/bin/bash
# player_recvopts.sh MODE RECV-OPTS...: a 4K60 HEVC player (GPU decoding, --idle MODE) and
# one `idn codec recv` with the given options: the player's CPU over 6 s.
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../../common.sh"
MODE=$1; shift; F=$K/media/${FILE:-t4k60.mp4}
D=$(mktemp -d $K/tmp/precv.XXXX); cd $D
IDN_NODE=off $B/idn-codec play $F --idle $MODE --loop --no-node --name PR > play.log 2>&1 < /dev/null & P=$!
sleep 3
PORT=$(ss -ltnp | grep "pid=$P," | grep -v ":5960 " | head -1 | awk '{print $4}' | sed 's/.*://')
IDN_NODE=off timeout 30 $B/idn-codec recv 127.0.0.1:$PORT --no-node --seconds 15 "$@" > recv.log 2>&1 & R=$!
sleep 6
t() { awk '{print $14+$15}' /proc/$P/stat; }
a=$(t); sleep 6; b=$(t)
echo "$MODE $*: player $(( (b-a) * 100 / $(getconf CLK_TCK) / 6 ))% of a core"
wait $R; grep -h "pictures:\|by format" recv.log | head -2; grep -o "passed [0-9]* encoded [0-9]*" play.log | tail -1; grep -a "receiver" play.log | tr '\r' '\n' | grep receiver | grep -v Playing | head -3
kill $P 2>/dev/null; wait $P 2>/dev/null
cd $K/tmp && rm -rf "$D"

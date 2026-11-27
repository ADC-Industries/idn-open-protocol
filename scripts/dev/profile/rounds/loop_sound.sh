#!/bin/bash
# loop_sound.sh BIN_DIR [FILE]: idn codec play FILE --loop and one `idn codec recv` (AAC
# passed through with H.264) for 65 s: sound received against the pictures' time.
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../../common.sh"
B=$1; F=${2:-$K/media/fb20.mp4}
D=$(mktemp -d $K/tmp/lsnd.XXXX); cd $D
IDN_NODE=off $B/idn-codec play $F --loop --no-node --name LS > play.log 2>&1 < /dev/null & P=$!
sleep 2
PORT=$(ss -ltnp | grep "pid=$P," | grep -v ":5960 " | head -1 | awk '{print $4}' | sed 's/.*://')
IDN_NODE=off $B/idn-codec recv 127.0.0.1:$PORT --no-node --seconds 65 > recv.log 2>&1
grep -E "pictures:|audio:|sound received" recv.log
kill $P; wait $P 2>/dev/null
cd $K/tmp && rm -rf "$D"

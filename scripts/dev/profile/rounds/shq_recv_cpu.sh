#!/bin/bash
# Receiver CPU for an idn-send → idn-recv SpeedHQ pair on this host.
#   shq_recv_cpu.sh SIZE [SECONDS]   e.g. 3840x2160 15
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../../common.sh"
D=$(mktemp -d $K/tmp/shqm.XXXX); cd $D
IDN_NODE=off $B/idn-send --name SHQM --size ${1:-3840x2160} --fps 60 --no-node --seconds 90 > send.log 2>&1 < /dev/null & S=$!
sleep 3
P=$(ss -ltnp | grep "pid=$S," | grep -v ":5960 " | head -1 | awk '{print $4}' | sed 's/.*://')
IDN_NODE=off $B/idn-recv 127.0.0.1:$P --no-node --seconds 80 > recv.log 2>&1 < /dev/null & R=$!
sleep 5
t() { awk '{print $14+$15}' /proc/$R/stat; }
a=$(t); sleep ${2:-15}; b=$(t)
echo "idn-recv ${1:-3840x2160}: $(( (b-a) * 100 / $(getconf CLK_TCK) / ${2:-15} ))% of a core"
grep -c "video:" recv.log >/dev/null
kill $R $S 2>/dev/null; wait $R $S 2>/dev/null; cd /; rm -rf $D

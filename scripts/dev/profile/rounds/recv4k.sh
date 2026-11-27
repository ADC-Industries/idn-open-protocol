#!/bin/bash
# A 4K30 file played as SpeedHQ to one idn-recv (decoding, nothing written): the
# receiver's CPU over SECS seconds, and optionally a perf profile of it. Stopped by PID.
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../../common.sh"
F=${F:-$K/media/t4k30.mp4}
SECS=${SECS:-10}; PORT=${PORT:-7300}; OUT=$K/prof
$B/idn-codec play $F --port $PORT --loop --speedhq --seconds $((SECS+8)) --no-mdns --no-node > $OUT/play.log 2>&1 &
pp=$!
sleep 2
$B/idn-recv 127.0.0.1:$PORT --seconds $((SECS+4)) --no-node $RECV_ARGS > $OUT/recv.log 2>&1 &
rp=$!
sleep 3
t0=$(awk '{print $14+$15}' /proc/$rp/stat); s0=$(date +%s%N)
if [ -n "$PERF" ]; then perf record -F 999 -g -p $rp -o $OUT/perf.data -- sleep $SECS 2>/dev/null >/dev/null; else sleep $SECS; fi
t1=$(awk '{print $14+$15}' /proc/$rp/stat); s1=$(date +%s%N)
tck=$(getconf CLK_TCK)
echo "receiver CPU: $(( (t1-t0)*100000/tck / ((s1-s0)/1000000) ))% of a core (100 = one core) over ${SECS}s"
wait $rp; wait $pp
grep -E "frames|pictures|video via|decode" $OUT/recv.log | tail -4

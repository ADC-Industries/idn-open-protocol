#!/bin/bash
# link_lossy.sh CC LOSS [CAP|-] [DELAY_MS one way]: as link_sound.sh (auto sound), the link through a UDP relay
# dropping LOSS (0-1) of packets each way (CAP: Mbit/s policer), both link ends with
# IDN_LINK_CC=CC (cubic, bbr, newreno): the receivers' pictures/s and longest gaps.
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../../common.sh"
SOUND=auto; LIMIT=40; export IDN_LINK_CC=$1; LOSS=$2; CAP=${3:--}; DELAY=${4:-0}
W=$(mktemp -d $K/tmp/llossy.XXXX)
export IDN_PASSPHRASE="St Mark" IDN_NODE=off
PIDS=()
python3 $S/../lossy.py 27970 27965 $LOSS $CAP $DELAY & PIDS+=($!)
(sleep 70 | exec $B/idn discovery-server --port 25959 --trust-dir "$W/auth" --no-panel --no-announce) > "$W/ds-main.log" 2>&1 &
sleep 0.5; PIDS+=($(pgrep -n -f "discovery-server --port 25959"))
sleep 1
$B/idn recv 127.0.0.1:1 --trust-dir "$W/sm" --server 127.0.0.1:25959 --no-node --seconds 3 > /dev/null 2>&1
$B/idn recv 127.0.0.1:1 --trust-dir "$W/rm" --server 127.0.0.1:25959 --no-node --seconds 3 > /dev/null 2>&1
$B/idn-codec link serve --port 27965 --discovery-server 127.0.0.1:25959 --trust-dir "$W/gw" --name "MAIN-GW (Site link)" --no-node --seconds 60 --sound $SOUND --limit $LIMIT > "$W/gw.log" 2>&1 & PIDS+=($!)
sleep 2
$B/idn-codec link connect 127.0.0.1:27970 --campus-server 25969 --discovery-server 127.0.0.1:25969 --trust-dir "$W/rl" --name "CAMPUS (Site link)" --no-node --seconds 55 --sound $SOUND --limit $LIMIT > "$W/rl.log" 2>&1 & PIDS+=($!)
sleep 3
$B/idn recv 127.0.0.1:1 --trust-dir "$W/sc" --server 127.0.0.1:25969 --no-node --seconds 4 > /dev/null 2>&1
$B/idn recv 127.0.0.1:1 --trust-dir "$W/rc" --server 127.0.0.1:25969 --no-node --seconds 4 > /dev/null 2>&1
env -u IDN_PASSPHRASE $B/idn send --name "Cam Main" --server 127.0.0.1:25959 --trust-dir "$W/sm" --no-mdns --no-node --seconds 45 > "$W/send-main.log" 2>&1 & PIDS+=($!)
env -u IDN_PASSPHRASE $B/idn send --name "Cam Campus" --server 127.0.0.1:25969 --trust-dir "$W/sc" --no-mdns --no-node --seconds 45 > "$W/send-campus.log" 2>&1 & PIDS+=($!)
sleep 10
H=$(hostname | tr a-z A-Z)
env -u IDN_PASSPHRASE $B/idn recv "$H (Cam Main)" --server 127.0.0.1:25969 --trust-dir "$W/rc" --no-node --seconds 12 > "$W/recv-at-campus.log" 2>&1 & R1=$!
env -u IDN_PASSPHRASE $B/idn recv "$H (Cam Campus)" --server 127.0.0.1:25959 --trust-dir "$W/rm" --no-node --seconds 12 > "$W/recv-at-main.log" 2>&1 & R2=$!
wait $R1 $R2
kill ${PIDS[@]} 2>/dev/null; sleep 1
echo "== cc $IDN_LINK_CC, loss $LOSS, cap $CAP, delay $DELAY ms each way:"; grep -h "Mbit/s" "$W/gw.log" | tail -n 1; grep -h "video at" "$W/gw.log" "$W/rl.log" | tail -2
for f in recv-at-campus recv-at-main; do echo "  $f: $(grep -h "video messages decoded" "$W/$f.log")"; done
echo "$W"

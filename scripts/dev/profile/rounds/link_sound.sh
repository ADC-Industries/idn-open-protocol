#!/bin/bash
# link_sound.sh SOUND [LIMIT]: two sites on one machine linked (main: authority DS 25959 +
# gateway UDP 27965; campus: joins through the link, DS 25969), both link ends with
# --sound SOUND (--limit LIMIT): a camera at each received at the other; the receivers'
# sound and pictures and the link's own lines.
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../../common.sh"
SOUND=$1; LIMIT=${2:-40}
W=$(mktemp -d $K/tmp/lsound.XXXX)
export IDN_PASSPHRASE="St Mark" IDN_NODE=off
PIDS=()
(sleep 70 | exec $B/idn discovery-server --port 25959 --trust-dir "$W/auth" --no-panel --no-announce) > "$W/ds-main.log" 2>&1 &
sleep 0.5; PIDS+=($(pgrep -n -f "discovery-server --port 25959"))
sleep 1
$B/idn recv 127.0.0.1:1 --trust-dir "$W/sm" --server 127.0.0.1:25959 --no-node --seconds 3 > /dev/null 2>&1
$B/idn recv 127.0.0.1:1 --trust-dir "$W/rm" --server 127.0.0.1:25959 --no-node --seconds 3 > /dev/null 2>&1
$B/idn-codec link serve --port 27965 --discovery-server 127.0.0.1:25959 --trust-dir "$W/gw" --name "MAIN-GW (Site link)" --no-node --seconds 60 --sound $SOUND --limit $LIMIT > "$W/gw.log" 2>&1 & PIDS+=($!)
sleep 2
$B/idn-codec link connect 127.0.0.1:27965 --campus-server 25969 --discovery-server 127.0.0.1:25969 --trust-dir "$W/rl" --name "CAMPUS (Site link)" --no-node --seconds 55 --sound $SOUND --limit $LIMIT > "$W/rl.log" 2>&1 & PIDS+=($!)
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
echo "== $SOUND, limit $LIMIT; link lines:"; grep -h "Opus\|sound" "$W/gw.log" "$W/rl.log" | sort | uniq -c | head; grep -h "Mbit/s" "$W/gw.log" | tail -n 1
for f in recv-at-campus recv-at-main; do echo "== $f"; grep -E "video messages decoded|audio messages decoded|audio|level|peak" "$W/$f.log" | head -6; done
echo "$W"

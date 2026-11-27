#!/bin/bash
# Profiling round 7 (after the wider bug round): what starting costs (the
# encoder probe, each program's start and what it keeps of the GPU), 4K, a 30-minute
# service, programs at rest (session agent, session helper, Site Managers with many machines and
# panels), IDN Industrial under MQTT and SQL load, and the mixer at 64 channels. Rows
# into build-kit/prof7/out/results.tsv.
#   round7.sh startup | uhd | soak [MINUTES] | idle | industrial | mixer64 | all
S=$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd); source "$S/../../common.sh"
set -u
W=$(workdir prof7)
OUT=$W/out; export OUT; mkdir -p "$OUT"
source "$S/../lib.sh"
H=$(hostname | tr a-z A-Z)
FF=${IDN_FFMPEG_LIB:-$ROOT/../build-kit/linux-ffmpeg/ffmpeg-n8.1.3-9-g29e619e767-linux64-lgpl-shared-8.1/lib}
export LD_LIBRARY_PATH=$FF
PIDS=()
cleanup() { local i; for ((i=${#PIDS[@]}-1; i>=0; i--)); do kill -TERM ${PIDS[i]} 2>/dev/null; done; for p in "${PIDS[@]}"; do wait $p 2>/dev/null; done; PIDS=(); }
trap cleanup EXIT
api() { curl -s -X POST -H 'X-IDN-Switcher: 1' "http://127.0.0.1:${2:-7990}/api/v1/$1" -d "${3:-{\}}" >/dev/null; }
sw_api() { curl -s -X POST -H 'X-IDN-Switcher: 1' "http://127.0.0.1:7990/api/v1/$1" -d "$2" >/dev/null; }
up() { local t=$((SECONDS + ${2:-30})); until curl -sk "$1" >/dev/null 2>&1; do [ $SECONDS -gt $t ] && return 1; sleep 0.2; done; }
startup_py() { python3 "$S/../startup.py" "$@"; }
host_cpu() {
  local a b
  a=$(awk '/^cpu /{print $2+$3+$4+$6+$7+$8, $2+$3+$4+$5+$6+$7+$8}' /proc/stat); sleep "$1"
  b=$(awk '/^cpu /{print $2+$3+$4+$6+$7+$8, $2+$3+$4+$5+$6+$7+$8}' /proc/stat)
  echo "$a $b" | awk -v n=$(nproc) '{printf "%.2f cores busy of %d", ($3-$1)/($4-$2)*n, n}'
}
gpu_busy() {
  local i s=0
  for i in $(seq 20); do s=$((s + $(cat /sys/class/drm/card0/device/gpu_busy_percent))); sleep 0.25; done
  echo "$((s / 20))%"
}
# A fresh state folder and Node for a scenario.
node() {
  rm -rf "$W/state" "$W/n.sock"; mkdir -p "$W/state"
  export IDN_NODE=$W/n.sock IDN_CLOCK=off IDN_STATE_DIR=$W/state
  "$B/idn-node" run --user --state "$W/state/node" --endpoint "$W/n.sock" --panel-port 7996 > "$W/node.log" 2>&1 & PIDS+=($!)
  NODE=$!
  local t=$((SECONDS + 10)); until [ -S "$W/n.sock" ] || [ $SECONDS -gt $t ]; do sleep 0.1; done
}

# What starting costs. The encoder probe alone (each GPU and CPU encoder opened and fed a
# picture), with and without FFmpeg's Vulkan device; each program from its start to
# ready; and what it holds afterwards (S2-3: the probe's devices kept for the process's
# life), the switcher and a SpeedHQ camera with and without the Vulkan device.
startup() {
  local i
  for i in 1 2 3; do IDN_NODE=off startup_py probe-$i exit -- "$B/idn-codec" probe; done
  for i in 1 2 3; do IDN_NODE=off IDN_VULKAN_ENCODE=off startup_py probe-novulkan-$i exit -- "$B/idn-codec" probe; done
  rm -rf "$W/st"; mkdir -p "$W/st"
  startup_py start-node "$W/st/n.sock" -- env IDN_STATE_DIR=$W/st IDN_CLOCK=off "$B/idn-node" run --user --state "$W/st/node" --endpoint "$W/st/n.sock" --panel-port 7996
  node
  startup_py start-switcher http://127.0.0.1:7990/api/v1/state 5 -- "$B/idn-switcher" --format 1080p30 --port 7990 --name P7Sw
  IDN_VULKAN_ENCODE=off startup_py start-switcher-novulkan http://127.0.0.1:7990/api/v1/state 5 -- "$B/idn-switcher" --format 1080p30 --port 7990 --name P7Sw
  startup_py start-mixer http://127.0.0.1:7991/ 5 -- "$B/idn" mixer --channels 32 --port 7991
  # A SpeedHQ camera (no encoder used): what the probe leaves it holding.
  "$B/idn-node" set video-format=speedhq >/dev/null
  local v cam
  for v in on off; do
    if [ $v = off ]; then export IDN_VULKAN_ENCODE=off; fi
    "$B/idn-codec" send --name P7Cam --size 1920x1080 --fps 30 --seconds 30 > "$W/cam-$v.log" 2>&1 & cam=$!
    sleep 6
    startup_py --pid $cam camera-speedhq-vulkan-$v
    measure camera-speedhq-vulkan-$v $cam 0 5
    kill $cam 2>/dev/null; wait $cam 2>/dev/null
    unset IDN_VULKAN_ENCODE
  done
  cleanup
  # The Site Managers.
  rm -rf "$W/sm"; mkdir -p "$W/sm"
  startup_py start-site-manager https://127.0.0.1:29202/ 5 -- "$B/idn-site-manager" --trust-dir "$W/sm/trust" --port 15960 --panel-port 29202 --no-announce
  rm -rf "$W/ism"; mkdir -p "$W/ism"
  startup_py start-site-manager-industrial http://127.0.0.1:18080/healthz 5 -- env IDN_DATA=$W/ism IDN_HTTP=127.0.0.1:18080 IDN_QUIC=127.0.0.1:17966 IDN_MQTT=127.0.0.1:11883 IDN_PUBLIC_URLS=idn://127.0.0.1:17966 IDN_API_KEY=prof7-api-key-0123456789 "$B/idn-site-manager-industrial"
  rm -rf "$W/inode"; mkdir -p "$W/inode"
  startup_py start-node-industrial http://127.0.0.1:17968/status 5 -- "$B/idn-industrial" run --state "$W/inode" --listen 127.0.0.1:17968
}

# 4K: a 2160p test camera to one receiver, SpeedHQ at 30 and 60 fps, H.264 and HEVC from
# the GPU at 30; then the switcher at 2160p30 with two 2160p30 cameras, its program to a
# receiver. Sender, receiver and switcher measured; the receivers' pictures a second.
uhd() {
  local fps fmt cam rx
  for case in speedhq:30 speedhq:60 h264:30 hevc:30; do
    fmt=${case%:*} fps=${case#*:}
    node; "$B/idn-node" set video-format=$fmt video-processor=gpu >/dev/null
    "$B/idn-codec" send --name P7Uhd --size 3840x2160 --fps $fps --seconds 300 > "$W/uhd-$fmt-$fps.log" 2>&1 & PIDS+=($!); cam=$!
    sleep 3
    "$B/idn-codec" recv "$H (P7Uhd)" --max-size 3840x2160 --seconds 70 > "$W/uhd-recv-$fmt-$fps.log" 2>&1 & PIDS+=($!); rx=$!
    measure uhd-recv-$fmt-$fps $rx 6 8
    measure uhd-send-$fmt-$fps $cam 0 10
    wait $rx 2>/dev/null
    echo "uhd-$fmt-$fps receiver: $(grep -v '^Svt' "$W/uhd-recv-$fmt-$fps.log" | grep -E "pictures:|latency" | tr '\n' ' ' | cut -c1-200)" | tee -a "$OUT/results.tsv"
    cleanup
  done
  node; "$B/idn-node" set video-format=speedhq >/dev/null
  local c
  for c in 1 2; do
    "$B/idn" send --name P7UhdCam$c --size 3840x2160 --fps 30/1 --seconds 300 > "$W/uhdsw-cam$c.log" 2>&1 & PIDS+=($!)
  done
  "$B/idn-switcher" --format 2160p30 --port 7990 --name P7UhdSw > "$W/uhdsw.log" 2>&1 & PIDS+=($!); local sw=$!
  up http://127.0.0.1:7990/api/v1/state; sleep 2
  for c in 1 2; do sw_api add_input "{\"network\":\"$H (P7UhdCam$c)\",\"label\":\"Cam $c\"}"; done
  on_bus http://127.0.0.1:7990/api/v1 program 1; on_bus http://127.0.0.1:7990/api/v1 preview 2
  "$B/idn-codec" recv "$H (P7UhdSw)" --max-size 3840x2160 --seconds 70 > "$W/uhdsw-recv.log" 2>&1 & PIDS+=($!); rx=$!
  measure uhd-switcher $sw 8 10
  measure uhd-switcher-recv $rx 0 8
  curl -s http://127.0.0.1:7990/api/v1/state | python3 -c "import json,sys;s=json.load(sys.stdin);print('uhd-switcher frame_ms',s.get('frame_ms'),'worst',s.get('frame_ms_worst_10s'),'late',s.get('late_frames'))" | tee -a "$OUT/results.tsv"
  wait $rx 2>/dev/null
  echo "uhd-switcher receiver: $(grep -v '^Svt' "$W/uhdsw-recv.log" | grep -E "pictures:|latency" | tr '\n' ' ' | cut -c1-200)" | tee -a "$OUT/results.tsv"
  echo "uhd machine: $(host_cpu 5), GPU $(gpu_busy)" | tee -a "$OUT/results.tsv"
  cleanup
}

# A service held for MINUTES (default 30): round 5's whole 1080p30 service (Node, three
# cameras, switcher with slide and lower third recording and streaming, a projector,
# the desk with eight sources, a captured screen), sampled every 30 s (soak.py): memory,
# threads, descriptors and CPU over time, the switcher's frames; then the recording checked.
soak() {
  local min=${1:-30} w=$W o=$OUT
  source "$S/round5.sh"
  W=$w OUT=$o
  SVC_SECS=$((min * 60 + 300)) service_start
  echo "soak: started $(date +%T), $min min; machine $(host_cpu 5)" | tee -a "$OUT/results.tsv"
  python3 "$S/../soak.py" "$OUT/soak.tsv" "$min" 30 switcher=$SVC_SW camera=$SVC_CAM projector=$SVC_PROJ \
    desk=$SVC_MIX node=$SVC_NODE capture=$SVC_CAP --state http://127.0.0.1:7990/api/v1/state
  echo "soak: end machine $(host_cpu 5), GPU $(gpu_busy)" | tee -a "$OUT/results.tsv"
  curl -s http://127.0.0.1:7990/api/v1/state | python3 -c "import json,sys;s=json.load(sys.stdin);print('soak switcher frame_ms',s.get('frame_ms'),'worst',s.get('frame_ms_worst_10s'),'late',s.get('late_frames'))" | tee -a "$OUT/results.tsv"
  cleanup
  sleep 2
  ffprobe -v error -show_entries format=duration:stream=codec_type,avg_frame_rate,nb_frames -of compact "$W/svc-rec.mp4" 2>&1 | tr '\n' ' ' | sed 's/^/soak recording: /' | tee -a "$OUT/results.tsv"; echo
  echo "soak recording errors: $(ffmpeg -v error -i "$W/svc-rec.mp4" -f null - 2>&1 | grep -vc 'non monotonically')" | tee -a "$OUT/results.tsv"
  echo "soak projector: $(tail -1 "$W/svc-projector.log" | cut -c1-120)" | tee -a "$OUT/results.tsv"
  rm -f "$W/svc-rec.mp4"
}

KEY=prof7-api-key-0123456789
# An IDN Site Manager | Industrial for the scenario (HTTP 18080, QUIC 17966, MQTT 11883).
ism_start() {
  rm -rf "$W/ism"; mkdir -p "$W/ism"
  IDN_DATA=$W/ism IDN_HTTP=127.0.0.1:18080 IDN_QUIC=127.0.0.1:17966 IDN_MQTT=127.0.0.1:11883 \
    IDN_PUBLIC_URLS=idn://127.0.0.1:17966 IDN_API_KEY=$KEY "$B/idn-site-manager-industrial" > "$W/ism.log" 2>&1 & PIDS+=($!)
  ISM=$!
  up http://127.0.0.1:18080/healthz
}
ism_api() { curl -s -X "$1" -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' "http://127.0.0.1:18080$2" ${3:+-d "$3"}; }
# The machines the Industrial Site Manager has, and how many are online.
ism_count() {
  ism_api GET /api/machines | python3 -c 'import json,sys
j=json.load(sys.stdin); m=j if isinstance(j,list) else j.get("machines",[])
print(len(m), sum(1 for x in m if x.get("online")))' 2>/dev/null
}
# N machines (idn-industrial run), each its own state and local API (18101…), enrolled.
ism_machines() {
  local n=$1 i tok
  MACHINES=()
  for i in $(seq $n); do
    rm -rf "$W/im$i"; mkdir -p "$W/im$i"
    "$B/idn-industrial" run --state "$W/im$i" --listen 127.0.0.1:$((18100 + i)) > "$W/im$i.log" 2>&1 & PIDS+=($!); MACHINES+=($!)
  done
  for i in $(seq $n); do
    up http://127.0.0.1:$((18100 + i))/status 20
    tok=$(ism_api POST /api/tokens "{\"name\":\"line-$i\",\"tags\":{\"plant\":\"P7\"},\"hours\":1}" | python3 -c 'import json,sys;print(json.load(sys.stdin)["token"])')
    "$B/idn-industrial" enrol "$tok" --state "$W/im$i" --listen 127.0.0.1:$((18100 + i)) > /dev/null 2>&1
  done
  local t=$((SECONDS + 60))
  until [ "$(ism_count | cut -d' ' -f2)" = "$n" ] || [ $SECONDS -gt $t ]; do sleep 1; done
  echo "industrial: machines, online: $(ism_count)" | tee -a "$OUT/results.tsv"
}

# Programs at rest: the session agent (idn-session; the tray it replaced is gone) and the
# Industrial session helper on a virtual screen; the IDN Site Manager with ten machines, then with five panels open (each its status every
# 2 s, reports and audit every 10 s, as the page asks); the Industrial Site Manager with
# twenty machines, then with five operators refreshing the machine list every 5 s.
idle() {
  Xvfb :96 -screen 0 1920x1080x24 > /dev/null 2>&1 & PIDS+=($!)
  sleep 1
  node
  DISPLAY=:96 dbus-run-session -- "$B/idn-session" > "$W/session.log" 2>&1 & PIDS+=($!)
  sleep 5
  local agent; agent=$(pgrep -n -x idn-session)
  [ -n "$agent" ] && measure idle-session $agent 3 10
  rm -rf "$W/hnode"; mkdir -p "$W/hnode"
  "$B/idn-industrial" run --state "$W/hnode" --listen 127.0.0.1:17969 > "$W/hnode.log" 2>&1 & PIDS+=($!); local hn=$!
  up http://127.0.0.1:17969/status
  measure idle-node-industrial $hn 2 10
  DISPLAY=:96 "$B/idn-helper-industrial" --state "$W/hnode" --node http://127.0.0.1:17969 > "$W/helper.log" 2>&1 & PIDS+=($!); local hp=$!
  measure idle-helper $hp 5 10
  measure idle-node-industrial-with-helper $hn 0 10
  cleanup
  # The IDN Site Manager, ten machines.
  rm -rf "$W/sm"; mkdir -p "$W/sm"; echo "prof7-site-password" > "$W/sm/pw"
  "$B/idn-site-manager" --trust-dir "$W/sm/trust" --port 15960 --panel-port 29202 --passphrase prof7pass \
    --panel-password-file "$W/sm/pw" --no-announce > "$W/sm.log" 2>&1 & PIDS+=($!); local sm=$!
  up https://127.0.0.1:29202/
  local i
  for i in $(seq 10); do
    rm -rf "$W/m$i"; mkdir -p "$W/m$i"
    IDN_STATE_DIR=$W/m$i IDN_CLOCK=off "$B/idn-node" run --user --state "$W/m$i/node" --endpoint "$W/m$i.sock" --panel-port $((7100 + i)) > "$W/m$i.log" 2>&1 & PIDS+=($!)
  done
  sleep 2
  for i in $(seq 10); do
    IDN_NODE=$W/m$i.sock "$B/idn-node" set name=P7M$i server=127.0.0.1:15960 passphrase=prof7pass auto-site=false > /dev/null 2>&1
  done
  sleep 15
  curl -sk -c "$W/sm/cj" -H 'X-IDN-Node: 1' -H 'Content-Type: application/json' -d '{"password":"prof7-site-password"}' https://127.0.0.1:29202/api/v1/login > /dev/null
  curl -sk -b "$W/sm/cj" https://127.0.0.1:29202/api/v1/status > "$W/sm-status.json"
  echo "site manager status: $(wc -c < "$W/sm-status.json") bytes, $(grep -o 'P7M[0-9]*' "$W/sm-status.json" | sort -u | wc -l) of the 10 machines named" | tee -a "$OUT/results.tsv"
  measure idle-site-manager-10 $sm 0 10
  measure idle-site-node $(pgrep -n -f "$W/m1/node") 0 10
  for i in $(seq 5); do
    curl -sk -c "$W/sm/cj$i" -H 'X-IDN-Node: 1' -H 'Content-Type: application/json' -d '{"password":"prof7-site-password"}' https://127.0.0.1:29202/api/v1/login > /dev/null
    ( n=0; while :; do curl -sk -b "$W/sm/cj$i" https://127.0.0.1:29202/api/v1/status > /dev/null
        if [ $((n % 5)) = 0 ]; then curl -sk -b "$W/sm/cj$i" https://127.0.0.1:29202/api/v1/reports > /dev/null; curl -sk -b "$W/sm/cj$i" 'https://127.0.0.1:29202/api/v1/audit?n=200' > /dev/null; fi
        n=$((n + 1)); sleep 2; done ) & PIDS+=($!)
  done
  measure idle-site-manager-10-panels5 $sm 4 10
  cleanup
  # The Industrial Site Manager, twenty machines.
  ism_start
  ism_machines 20
  measure idle-ism-20 $ISM 5 10
  measure idle-industrial-machine ${MACHINES[0]} 0 10
  for i in $(seq 5); do
    ( while :; do ism_api GET /api/machines > /dev/null; sleep 5; done ) & PIDS+=($!)
  done
  measure idle-ism-20-ops5 $ISM 3 10
  cleanup
}

# IDN Industrial under load: twenty machines each sending 25 messages a second (500 a
# second, about 300 bytes each) through their local API, an application subscribed over
# MQTT taking them; then one machine's SQL module reading 1,000 rows from SQLite (ODBC)
# every second as a snapshot (IND_ODBC_BIN: an idn-industrial built with the odbc feature).
industrial() {
  ism_start
  ism_machines 20
  ism_api POST /api/mqtt-users '{"username":"app","password":"prof7-app-password"}' > /dev/null
  "$B/examples/mqtt_watch" 127.0.0.1 11883 app prof7-app-password 'idn/+/m/#' 100 > "$W/watch.txt" 2>&1 & PIDS+=($!)
  sleep 2
  local dirs="" i
  for i in $(seq 20); do dirs+=" $W/im$i:$((18100 + i))"; done
  python3 "$S/../industrial_load.py" 25 60 $dirs > "$W/load.txt" 2>&1 & PIDS+=($!); local ld=$!
  measure ind-load-ism $ISM 10 10
  measure ind-load-machine ${MACHINES[0]} 0 10
  wait $ld
  sleep 5
  echo "ind-load: $(tail -1 "$W/load.txt"); the application got $(grep -c '^idn/' "$W/watch.txt") messages" | tee -a "$OUT/results.tsv"
  cleanup
  local drv=$ROOT/../build-kit/odbc/root/usr/lib/x86_64-linux-gnu/odbc/libsqlite3odbc.so
  local odbc_lib=$ROOT/../build-kit/odbc/root/usr/lib/x86_64-linux-gnu
  python3 "$S/../industrial_load.py" --make-db "$W/loads.db" 1000
  ism_start
  rm -rf "$W/sq"; mkdir -p "$W/sq/modules"
  printf '%s\n' '[sql]' 'zone = "America/New_York"' '[[sql.connection]]' 'name = "batch"' \
    "connect = \"Driver=$drv;Database=$W/loads.db;\"" 'dialect = "generic"' \
    '[[sql.query]]' 'name = "loads"' 'connection = "batch"' 'kind = "snapshot"' 'type = "scada.loads"' \
    'sql = "SELECT id, modified, truck, tons, note FROM loads"' 'every_s = 1' > "$W/sq/modules/local.toml"
  LD_LIBRARY_PATH=$odbc_lib:$FF "${IND_ODBC_BIN:-$B/idn-industrial}" run --state "$W/sq" --listen 127.0.0.1:18200 > "$W/sq.log" 2>&1 & PIDS+=($!); local sq=$!
  up http://127.0.0.1:18200/status
  local tok; tok=$(ism_api POST /api/tokens '{"name":"sql","hours":1}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["token"])')
  "$B/idn-industrial" enrol "$tok" --state "$W/sq" --listen 127.0.0.1:18200 > /dev/null 2>&1
  sleep 8
  measure ind-sql-machine $sq 2 10
  local wk; wk=$(pgrep -n -f "idn-industrial sql-worker")
  [ -n "$wk" ] && measure ind-sql-worker $wk 0 10
  measure ind-sql-ism $ISM 0 10
  echo "ind-sql: $(grep -i 'sql' "$W/sq.log" | tail -2 | tr '\n' ' ' | cut -c1-200)" | tee -a "$OUT/results.tsv"
  cleanup
}

# The mixer at 64 channels: 32 stereo sources routed to its 64 inputs (each a sender of
# sound with a still picture), its default desk; then desk_bench (32 and 64 channels, busy
# and default, the block's time).
mixer64() {
  node
  local i
  for i in $(seq 32); do
    "$B/idn" send --name P7Src$i --size 160x90 --static --seconds 600 > "$W/src$i.log" 2>&1 & PIDS+=($!)
  done
  local src=${PIDS[-1]}
  "$B/idn" mixer --channels 64 --inputs 64 --port 7991 > "$W/mix64.log" 2>&1 & PIDS+=($!); local mix=$!
  up http://127.0.0.1:7991/
  sleep 3
  for i in $(seq 32); do
    "$B/idn-node" route Mixer $((2 * i - 1)) "$H (P7Src$i)" 1 > /dev/null
    "$B/idn-node" route Mixer $((2 * i)) "$H (P7Src$i)" 2 > /dev/null
  done
  sleep 8
  THREADS=1 measure mixer64-desk $mix 2 10
  head -6 "$OUT/mixer64-desk.threads.txt" | sed 's/^/mixer64 thread /' >> "$OUT/results.tsv"
  measure mixer64-a-source $src 0 8
  echo "mixer64 machine: $(host_cpu 5)" | tee -a "$OUT/results.tsv"
  cleanup
  "$B/examples/desk_bench" 20 | sed 's/^/desk_bench: /' | tee -a "$OUT/results.tsv"
}

# Run, not sourced (sourced: the functions only).
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
case ${1:-all} in
  startup) startup ;;
  uhd) uhd ;;
  soak) soak "${2:-30}" ;;
  idle) idle ;;
  industrial) industrial ;;
  mixer64) mixer64 ;;
  all) startup; uhd; idle; industrial; mixer64; soak 30 ;;
esac
fi

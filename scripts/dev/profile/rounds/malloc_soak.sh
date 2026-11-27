#!/bin/bash
# glibc's allocator in a long-running IDN program (idn_common::memory, 2026-11-26): the
# switcher with a 1080p30 network camera, a title and its program recorded (FFmpeg's
# encoder allocating large buffers), MINUTES each with the tuning on and off
# (IDN_MALLOC_TUNE). soak.py samples the switcher's memory every 30 s; the summary gives
# its own (anonymous) memory's change after a warm-up.
#   malloc_soak.sh [MINUTES] [MODES]   (default 30, "on off")
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../../common.sh"
W=$(workdir malloc-soak); H=$(hostname | tr a-z A-Z)
source "$S/../lib.sh"
MIN=${1:-30}
MODES=${2:-on off}
export IDN_NODE=off IDN_CLOCK=off
for mode in $MODES; do
  export IDN_MALLOC_TUNE=$mode
  export IDN_STATE_DIR=$W/state-$mode; rm -rf "$IDN_STATE_DIR"; mkdir -p "$IDN_STATE_DIR"
  P=()
  "$B/idn" send --no-node --name MCam-$mode --size 1920x1080 --fps 30/1 \
    --seconds $((MIN * 60 + 120)) > "$W/send-$mode.log" 2>&1 & P+=($!)
  "$B/idn-switcher" --no-node --format 1080p30 --port 7989 --name MSw-$mode \
    > "$W/sw-$mode.log" 2>&1 & P+=($!); sw=$!
  until curl -s http://127.0.0.1:7989/api/v1/state >/dev/null 2>&1; do sleep 0.5; done; sleep 2
  api="curl -s -X POST -H X-IDN-Switcher:1 http://127.0.0.1:7989/api/v1"
  $api/add_input -d "{\"network\":\"$H (MCam-$mode)\",\"label\":\"Cam\"}" >/dev/null
  $api/add_input -d '{"title":{"layers":[{"type":"box","rect":[0.05,0.78,0.5,0.14],"colour":"#000000b0","radius":0.1},{"type":"text","rect":[0.07,0.8,0.46,0.1],"text":"Soak","size":0.05,"colour":"#ffffff"}]},"label":"T"}' >/dev/null
  on_bus http://127.0.0.1:7989/api/v1 program 1; bug_on http://127.0.0.1:7989/api/v1 2
  sleep 2
  rm -f "$W/rec-$mode.mp4"
  $api/stream_add -d "{\"file\":\"$W/rec-$mode.mp4\"}" >/dev/null
  python3 "$S/../soak.py" "$W/soak-$mode.tsv" "$MIN" 30 "switcher-$mode=$sw" \
    --state http://127.0.0.1:7989/api/v1/state
  $api/stream_remove -d '{"id":1}' >/dev/null; sleep 3
  for p in "${P[@]}"; do kill "$p" 2>/dev/null; done; wait 2>/dev/null
done
# Own memory's slope after the first quarter (warm-up), MB an hour, per mode.
for mode in $MODES; do
  awk -F'\t' -v m="switcher-$mode" -v min="$MIN" '
    $2 == m { t[n] = $1; o[n] = $5; n++ }
    END {
      if (n < 4) { print m ": too few samples"; exit }
      s = int(n / 4); dt = (t[n-1] - t[s]) / 3600
      printf "%s: own %d -> %d MB (from %.1f min), slope %+.0f MB/h\n", m, o[s], o[n-1], (t[s]-t[0])/60, (o[n-1]-o[s])/(dt>0?dt:1)
    }' "$W/soak-$mode.tsv" | tee -a "$W/results.txt"
done

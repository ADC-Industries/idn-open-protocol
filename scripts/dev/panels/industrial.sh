#!/bin/bash
# The Industrial settings forms (§I.8.4) in a headless Chromium: the Node | Industrial's
# panel (industrial-node.mjs: a whole file through the form and back, typing, saving,
# restart notice, the pushed view, secrets, the backup) and the Site Manager |
# Industrial's Modules page (industrial-sm.mjs: form, push, edit, TOML mode, actions
# refused). Debug builds of idn-industrial and idn-site-manager-industrial; needs chromium
# and node.
#   scripts/dev/panels/industrial.sh
set -u
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd)
IMPL=$(cd "$S/../../.." && pwd)
B=${IDN_BIN:-$IMPL/target/debug}
W=$(mktemp -d)
PIDS=()
stop() { for p in "${PIDS[@]}"; do kill -TERM $p 2>/dev/null; wait $p 2>/dev/null; done; rm -rf "$W"; }
trap stop EXIT
mkdir -p "$W/node" "$W/sm"
"$B/idn-industrial" run --state "$W/node" --listen 127.0.0.1:17968 > "$W/node.log" 2>&1 & PIDS+=($!)
IDN_DATA=$W/sm IDN_HTTP=127.0.0.1:18081 IDN_QUIC=off IDN_MQTT=127.0.0.1:11884 IDN_API_KEY=smoke-key-1234567890 \
  IDN_ADMIN_PASSWORD=smoke-password1 "$B/idn-site-manager-industrial" > "$W/sm.log" 2>&1 & PIDS+=($!)
chromium --headless=new --no-sandbox --remote-debugging-port=9353 --user-data-dir="$W/chromium" about:blank > /dev/null 2>&1 & PIDS+=($!)
sleep 3
failed=0
node "$S/industrial-node.mjs" 9353 http://127.0.0.1:17968/ "$(cat "$W/node/api.token")" || failed=1
node "$S/industrial-sm.mjs" 9353 http://127.0.0.1:18081/ smoke-password1 || failed=1
exit $failed

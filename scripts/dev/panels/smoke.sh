#!/bin/bash
# The Node panel's smoke test (IDN Media's only web panel: the switcher's, the desk's, the
# Site Manager's and IDN Data's went to the embedding application): a Node started on its own
# (no site, state in the kit), its panel opened in a headless Chromium (cdp.mjs: every
# button clicked, any script error fails it), then stopped. Release programs (`IDN_BIN`);
# needs chromium and node.
#   smoke.sh
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../common.sh"
set -u
W=$(workdir panels)
export IDN_NODE=off IDN_CLOCK=off
PIDS=()
stop() { local i; for ((i=${#PIDS[@]}-1; i>=0; i--)); do kill -TERM ${PIDS[i]} 2>/dev/null; done; for p in "${PIDS[@]}"; do wait $p 2>/dev/null; done; PIDS=(); }
trap stop EXIT
fresh() { rm -rf "$W/$1"; mkdir -p "$W/$1"; export IDN_STATE_DIR=$W/$1; }
waitfor() { local i; for i in $(seq 60); do curl -sk -o /dev/null "$1" && return 0; sleep 0.25; done; return 1; }
# A stand-in browser: the address a program opens (with its one-time token) written down.
mkdir -p "$W/bin"
printf '#!/bin/sh\necho "$1" > "%s/opened"\n' "$W" > "$W/bin/xdg-open"
chmod +x "$W/bin/xdg-open"
chromium --headless=new --no-sandbox --ignore-certificate-errors --remote-debugging-port=9340 \
  --user-data-dir="$W/chromium" about:blank > "$W/chromium.log" 2>&1 & PIDS+=($!)
sleep 3
failed=0
check() { # NAME URL [PASSWORD]
  local out
  if out=$(node "$S/cdp.mjs" 9340 "$2" ${3:+"$3"} 2>&1); then
    echo "PASS $1: $out"
  else
    echo "FAIL $1: $out"
    failed=1
  fi
}
node_() {
  fresh node
  local sock=$K/smoke-node.sock
  rm -f "$sock"
  "$B/idn-node" run --user --state "$W/node/st" --endpoint "$sock" --panel-port 7942 > "$W/node.log" 2>&1 & PIDS+=($!)
  waitfor http://127.0.0.1:7942/ || return
  local url; url=$(IDN_NODE=$sock PATH="$W/bin:$PATH" "$B/idn-node" open 2>/dev/null | grep -o 'http://[^ ]*' | head -1)
  check node "$url"
}
node_
exit $failed

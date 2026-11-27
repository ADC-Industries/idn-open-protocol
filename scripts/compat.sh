#!/bin/bash
# The compatibility check (G10, spec/extensions/idn-node.md §N.23): this build against an
# older one, both ways — media (a source and a receiver) and the site (a Node and the Site
# Manager). Every release works with the previous two; run it against each before a release:
#
#   scripts/compat.sh OLD_DIR [NEW_DIR]     (NEW_DIR default target/release, else target/debug)
#
# OLD_DIR holds an older build's programs (handoff/rv6, handoff/rv7). Prints PASS or FAIL per
# check; exits 1 if any failed.
set -uo pipefail
OLD="$(cd "${1:?usage: scripts/compat.sh OLD_DIR [NEW_DIR]}" && pwd)"
IMPL="$(cd "$(dirname "$0")/.." && pwd)"
NEW="${2:-$IMPL/target/release}"
[ -x "$NEW/idn-node" ] || NEW="$IMPL/target/debug"
NEW="$(cd "$NEW" && pwd)"
W="$(mktemp -d)"
# The programs started, by PID, in a file: the checks run in subshells.
cleanup() { [ -f "$W/pids" ] && kill $(cat "$W/pids") 2>/dev/null; sleep 1; rm -rf "$W"; }
trap cleanup EXIT
export IDN_NODE=off IDN_CLOCK=off
HOST="$(hostname | tr a-z A-Z)"
FAILED=0
result() { if [ "$2" = ok ]; then echo "PASS  $1"; else echo "FAIL  $1 ($2)"; FAILED=1; fi; }

# Media: SRC's idn-send to DST's idn-recv, a few seconds of pictures and sound.
media() {
  local name="Compat $RANDOM" src="$1" dst="$2"
  "$src/idn-send" --name "$name" --no-node --size 640x360 --fps 30/1 --seconds 10 >"$W/send.log" 2>&1 &
  echo $! >>"$W/pids"
  sleep 2
  local got
  got="$(cd "$W" && timeout 30 "$dst/idn-recv" "$HOST ($name)" --seconds 5 --no-node 2>&1 | awk '/x video via/ {print $1; exit}')"
  if [ "${got:-0}" -gt 100 ]; then echo ok; else echo "${got:-0} pictures in 5 s"; fi
}
result "old source -> new receiver" "$(media "$OLD" "$NEW")"
result "new source -> old receiver" "$(media "$NEW" "$OLD")"

# The site: NODE_BIN's Node joins SM_BIN's Site Manager, is approved, reports, and takes a
# setting from it.
site() {
  local sm="$1" node="$2" port=$((20000 + RANDOM % 20000)) d="$W/site$RANDOM"
  mkdir -p "$d/st"
  echo compatpass >"$d/pw"
  "$sm/idn-discovery-server" --port "$port" --trust-dir "$d/trust" --panel-port $((port + 1)) \
    --panel-password-file "$d/pw" --no-announce </dev/null >"$d/ds.log" 2>&1 &
  echo $! >>"$W/pids"
  echo "<idn_node name=\"COMPAT-$RANDOM\" auto_site=\"false\"/>" >"$d/st/node.xml"
  "$node/idn-node" run --user --state "$d/st" --endpoint "$d/n.sock" --panel-port 0 >"$d/node.log" 2>&1 &
  echo $! >>"$W/pids"
  sleep 3
  local C=(-sk -b "$d/cj" -c "$d/cj") U="https://127.0.0.1:$((port + 1))/api/v1"
  curl "${C[@]}" -H 'X-IDN-Node: 1' -H 'Content-Type: application/json' -X POST "$U/login" -d '{"password":"compatpass"}' >/dev/null
  curl "${C[@]}" -H 'X-IDN-Node: 1' -H 'Content-Type: application/json' -X POST "$U/pairing" -d '{"seconds":"600"}' >/dev/null
  IDN_NODE="$d/n.sock" "$node/idn-node" set "server=127.0.0.1:$port" >/dev/null 2>&1
  local id=""
  for _ in $(seq 20); do
    id="$(curl "${C[@]}" "$U/status" | python3 -c 'import json,sys; p=json.load(sys.stdin)["pending"]; print(p[0]["id"] if p else "")' 2>/dev/null)"
    [ -n "$id" ] && break
    sleep 1
  done
  [ -n "$id" ] || { echo "no join request"; return; }
  curl "${C[@]}" -H 'X-IDN-Node: 1' -H 'Content-Type: application/json' -X POST "$U/approve" -d "{\"id\":\"$id\"}" >/dev/null
  local fp=""
  for _ in $(seq 30); do
    fp="$(curl "${C[@]}" "$U/status" | python3 -c 'import json,sys; m=[x for x in json.load(sys.stdin)["machines"] if x["online"] and x["node"]]; print(m[0]["fingerprint"] if m else "")' 2>/dev/null)"
    [ -n "$fp" ] && break
    sleep 1
  done
  [ -n "$fp" ] || { echo "approved, but no report"; return; }
  curl "${C[@]}" -H 'X-IDN-Node: 1' -H 'Content-Type: application/json' -X POST "$U/node" -d "{\"fingerprint\":\"$fp\",\"name\":\"COMPAT-RENAMED\"}" >/dev/null
  for _ in $(seq 20); do
    grep -q 'name="COMPAT-RENAMED"' "$d/st/node.xml" 2>/dev/null && { echo ok; return; }
    sleep 1
  done
  echo "a setting from the Site Manager didn't arrive"
}
result "old Node -> new Site Manager" "$(site "$NEW" "$OLD")"
result "new Node -> old Site Manager" "$(site "$OLD" "$NEW")"
exit $FAILED

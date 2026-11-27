# Measuring one process for the profiling harness (sourced by the round scripts, after
# ../common.sh).
# measure LABEL PID [WARM_S] [WINDOW_S]: CPU %, page faults/s, RSS start -> end and, of
# the end's, the process's own memory (anonymous: the rest is mapped files, such as the
# program's own pages, shared and given back under pressure; the one `idn` program
# reads 15 MB of its 62 MB that way at start, 2026-11-03); perf
# CPU and page-fault profiles of the window into out/LABEL.*; a row into results.tsv.
OUT=${OUT:-$K/prof/out}
mkdir -p "$OUT"
HZ=$(getconf CLK_TCK)
st() { awk '{print $10, $14+$15}' /proc/$1/stat 2>/dev/null; }
rss() { awk '/VmRSS/{print int($2/1024)}' /proc/$1/status 2>/dev/null; }
anon() { awk '/^Anonymous:/{print int($2/1024)}' /proc/$1/smaps_rollup 2>/dev/null; }
# Each thread: "TID NAME TICKS" (names with spaces joined by _).
threads() {
  for t in /proc/$1/task/*; do
    local tid=${t##*/} name
    name=$(tr ' ' '_' < $t/comm 2>/dev/null) || continue
    awk -v id=$tid -v n="$name" '{sub(/^.*\) /,""); print id, n, $12+$13}' $t/stat 2>/dev/null
  done
}
measure() {
  local label=$1 pid=$2 warm=${3:-5} win=${4:-8}
  sleep "$warm"
  [ -d /proc/$pid ] || { echo "$label: process gone"; return 1; }
  local f0 t0 f1 t1 r0 r1
  # The counters first, alone (a profiler attached costs CPU of its own), then the
  # profiles over a second window: CPU by frame pointers, page faults (every 50th) by
  # DWARF unwinding, which gets through libc's memmove/memset to the caller.
  read f0 t0 < <(st $pid); r0=$(rss $pid)
  local th0; th0=$(threads $pid)
  sleep "$win"
  read f1 t1 < <(st $pid); r1=$(rss $pid); local a1; a1=$(anon $pid)
  local th1; th1=$(threads $pid)
  # DWARF=1: CPU call chains through libc too (memmove's callers), at a lower rate.
  if [ -n "${DWARF:-}" ]; then
    perf record -q -F 199 --call-graph dwarf,16384 -p $pid -o "$OUT/$label.cpu.data" -- sleep 4 >/dev/null 2>&1 &
  else
    perf record -q -F 499 -g -p $pid -o "$OUT/$label.cpu.data" -- sleep 5 >/dev/null 2>&1 &
  fi
  local p1=$!
  perf record -q -e page-faults -c 50 --call-graph dwarf,16384 -p $pid -o "$OUT/$label.pf.data" -- sleep 3 >/dev/null 2>&1 &
  local p2=$!
  wait $p1 $p2
  local cpu=$(( (t1-t0)*100/HZ/win )) pf=$(( (f1-f0)/win ))
  printf "%s\t%d%%\t%d\t%d -> %d MB (own %s MB)\n" "$label" $cpu $pf "$r0" "$r1" "${a1:-?}" | tee -a "$OUT/results.tsv"
  # Each thread's share (by name, summed), % of a core: THREADS=1 prints it.
  join <(echo "$th0" | sort) <(echo "$th1" | sort) \
    | awk -v hz=$HZ -v w=$win '{d[$2]+=($5-$3)} END {for (n in d) if (d[n]>0) printf "%s %.1f\n", n, d[n]*100/hz/w}' \
    | sort -k2 -rn > "$OUT/$label.threads.txt"
  [ -n "${THREADS:-}" ] && head -8 "$OUT/$label.threads.txt" | sed 's/^/  /;s/$/%/'
  true
  # Top self-time symbols, and where the page faults happen (children included).
  perf report -i "$OUT/$label.cpu.data" --no-children --sort dso,symbol --stdio 2>/dev/null \
    | grep -E '^ +[0-9.]+%' | head -15 | sed 's/ \+/ /g' > "$OUT/$label.cpu.txt"
  perf report -i "$OUT/$label.pf.data" --children --sort symbol --stdio 2>/dev/null \
    | grep -E '^ +[0-9.]+%' | grep -v '0x[0-9a-f]\{16\}' | head -25 | sed 's/ \+/ /g' > "$OUT/$label.pf.txt"
}

# The switcher since its scene model: only scenes go on program and
# preview, and the overlays are global layers. URL: http://127.0.0.1:PORT/api/v1.
sw_post() { curl -s -X POST -H 'X-IDN-Switcher: 1' -H 'Content-Type: application/json' "$1/$2" -d "$3"; }
# scene_of URL INPUT: a scene holding input INPUT full frame; prints its id.
scene_of() {
  local id
  id=$(sw_post "$1" add_input "{\"scene\":true,\"label\":\"Scene $2\"}" |
    python3 -c 'import sys,json; print(json.load(sys.stdin).get("id",""))')
  sw_post "$1" scene_items "{\"id\":$id,\"items\":[{\"source\":$2}]}" >/dev/null
  echo "$id"
}
# on_bus URL program|preview INPUT: input INPUT on that bus through a scene of its own.
on_bus() { sw_post "$1" "$2" "{\"id\":$(scene_of "$1" "$3")}" >/dev/null; }
# bug_on URL INPUT: input INPUT full frame over every scene (what an overlay was).
bug_on() {
  sw_post "$1" global_layer "{\"layer\":{\"source\":$2,\"name\":\"Bug $2\",\"fade\":0},\"add_to_scenes\":true}" >/dev/null
}

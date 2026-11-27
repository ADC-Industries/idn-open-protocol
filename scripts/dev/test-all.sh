#!/bin/bash
# The full test run, debug and release: cargo-nextest (every test in a process of its own,
# the programs side by side; .config/nextest.toml) and the doc tests (`cargo test --doc`,
# which nextest doesn't run). Builds first, one profile at a time (the build is what needs
# the memory and the CPU), then runs.
#   test-all.sh [seq|par]   seq (default, reliable): debug's tests, then release's; par: both at
#   once (half the time; a doubled load still upsets a test now and then)
# Logs and the summary: build-kit/test-all/ (summary.txt; per-test times in
# target/nextest/default/junit.xml, the last profile run). Exit 0 only when all passed.
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/common.sh"
set -u
# One heavy build or test run on this host at a time, shared with the other projects'
# (since 2026-11-20): wait for the lock, hold it to the end (freed if the
# run dies). /tmp is RAM here: the lock file goes at reboot, as it should.
exec 9>/tmp/heavy-build.lock
if ! flock -n 9; then
  echo "test-all: waiting for another heavy build (/tmp/heavy-build.lock)" >&2
  flock 9
fi
mode=${1:-seq}
out=$(workdir test-all)
cap=$S/capped.sh
cd "$ROOT"
command -v cargo-nextest >/dev/null || {
  echo "no cargo-nextest (build-kit/nextest: the prebuilt one from get.nexte.st, linked into ~/.cargo/bin)" >&2
  exit 2
}
: > "$out/summary.txt"
# The tests' state in a folder of their own, not the host's (~/.local/state: a desk test
# made device folders beside the real idn-aoip's; bug run N8-8).
export IDN_STATE_DIR=$out/state
rm -rf "$IDN_STATE_DIR"; mkdir -p "$IDN_STATE_DIR"
say() { echo "$*" | tee -a "$out/summary.txt"; }
start=$(date +%s)

# Build: the test programs of both profiles (nextest's run then only checks they're there).
# Gently (a full rebuild pegged the host): TA_JOBS jobs (default 4, 4 at most) at
# low priority. Each test program's link runs its own threads too, so 12 jobs drove the load
# to 80 on 16 cores beside the host's own services.
jobs=${TA_JOBS:-4}
# No incremental caches for these full builds (they write gigabytes a full rebuild never
# reads again: the disk, not the processor, was what stalled the host's services).
export CARGO_INCREMENTAL=0
for p in debug release; do
  flag=(); [ "$p" = release ] && flag=(--release)
  t=$(date +%s)
  if ! nice -n 10 "$cap" ta-build-$p 20G "$jobs" test --workspace --no-run "${flag[@]}" > "$out/build-$p.log" 2>&1; then
    say "$p: the build failed (build-$p.log)"; grep -E "^error" "$out/build-$p.log" | head -20 | tee -a "$out/summary.txt"
    exit 1
  fi
  say "$p: built in $(( $(date +%s) - t )) s"
done

run() { # profile
  local p=$1 flag=() t rc=0
  [ "$p" = release ] && flag=(--release)
  t=$(date +%s)
  # (No CPU ceiling while tests run: the real-time audio tests would be throttled into
  # false failures. Compiling is capped, capped.sh's CAP_CPU.)
  CAP_CPU=1600% "$cap" ta-$p 16G 12 nextest run --workspace --no-fail-fast "${flag[@]}" > "$out/nextest-$p.log" 2>&1 || rc=1
  nice -n 10 "$cap" ta-doc-$p 16G "$jobs" test --workspace --doc "${flag[@]}" > "$out/doc-$p.log" 2>&1 || rc=1
  {
    echo "== $p ($(( $(date +%s) - t )) s)"
    grep -E "^ +Summary " "$out/nextest-$p.log" || echo "no nextest summary (nextest-$p.log)"
    grep -E "^ +(FAIL|TIMEOUT|SIGSEGV|SIGABRT|LEAK)" "$out/nextest-$p.log" | sort -u
    grep -E "^ +SLOW" "$out/nextest-$p.log" | sort -u | head -10
    grep "^test result" "$out/doc-$p.log" | awk '{p+=$4; f+=$6} END {print "  doc tests: passed", p+0, "failed", f+0}'
    grep -E "^test .* FAILED" "$out/doc-$p.log"
  } > "$out/part-$p.txt"
  return $rc
}

rc=0
if [ "$mode" = par ]; then
  run debug & d=$!
  run release & r=$!
  wait $d || rc=1
  wait $r || rc=1
else
  run debug || rc=1
  run release || rc=1
fi
cat "$out/part-debug.txt" "$out/part-release.txt" | tee -a "$out/summary.txt"
say "all: $(( $(date +%s) - start )) s, $([ $rc = 0 ] && echo passed || echo FAILED)"
exit $rc

#!/bin/bash
# Profile round 10 (2026-11-08): after the shared units (Worker, time limits, cli::Args,
# split files), the lock sweep (lock_ok / try_lock_ok everywhere) and the unsafe audit's
# fixes. Round 9's set again, to compare like with like (lanes fixed and adaptive, the
# switcher's compositor SDR and HDR, the HDR CPU paths, the desk), plus the send and
# receive path the lock sweep touched most (one 1080p30 output's CPU; several outputs
# with receivers under load). Run with the host otherwise idle.
#   round10.sh
set -u
S=$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)
IMPL=$(cd "$S/../../../.." && pwd)
OUT=$IMPL/../build-kit/profile/round10
mkdir -p "$OUT"
cd "$IMPL" || exit 1
: > "$OUT/results.txt"
say() { echo "$*" | tee -a "$OUT/results.txt"; }
cargo build --release -q -p idn-open-protocol --example bench_lanes --example bench_output \
  --example bench_load || exit 1
for mode in fixed adaptive; do
  say "lanes 1x12ch $mode: $(target/release/examples/bench_lanes 12 20 $mode 1 | tail -1)"
  say "lanes 32x2ch $mode: $(target/release/examples/bench_lanes 2 20 $mode 32 | tail -1)"
done
cargo build --release -q -p idn-switcher --features gpu --example compose_bench || exit 1
target/release/examples/compose_bench 2>&1 | grep -E "GPU|hdr" | tee -a "$OUT/results.txt"
for t in scale_p216_speed untransform_speed; do
  cargo test --release -q -p idn-ffmpeg --lib "$t" -- --ignored --nocapture 2>&1 | grep -E "ms" | tee -a "$OUT/results.txt"
done
cargo build --release -q -p idn-mixer --example desk_bench || exit 1
target/release/examples/desk_bench 2>&1 | tail -8 | tee -a "$OUT/results.txt"
say "output 1080p30 (quic):"
TRANSPORT=quic target/release/examples/bench_output 15 2>&1 | tail -3 | tee -a "$OUT/results.txt"
say "4 outputs + receivers, 2 hog threads (quic):"
TRANSPORT=quic target/release/examples/bench_load 4 15 2 2>&1 | tail -6 | tee -a "$OUT/results.txt"

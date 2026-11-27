#!/bin/bash
# Runs cargo in a memory-capped user scope (no swap), logging memory and CPU temperature
# once a second. An overrun kills the build inside the scope instead of freezing the machine.
#   capped.sh NAME MAXMEM JOBS cargo-args...
# Logs: build-kit/logs/NAME.mem (on disk: /tmp is RAM on this host and is wiped at boot).
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/common.sh"
set -u
name=$1 max=$2 jobs=$3
# idn-release (YubiKeys over PC/SC) links libpcsclite: the kit's pcsc/ has the .so link
# that a fresh build needs (as package.sh sets it).
[ -z "${PCSC_LIB_DIR:-}" ] && [ -d "$K/pcsc" ] && export PCSC_LIB_DIR="$K/pcsc"
shift 3
dir=$K/logs
mkdir -p "$dir"
log=$dir/$name.mem
: > "$log"
hw=$(dirname "$(grep -l k10temp /sys/class/hwmon/hwmon*/name)")
(
  while sleep 1; do
    cg=$(systemctl --user show -p ControlGroup --value "$name.scope" 2>/dev/null)
    [ -n "$cg" ] && [ -d "/sys/fs/cgroup$cg" ] || continue
    d=/sys/fs/cgroup$cg
    echo "$(date +%T) cur=$(( $(cat $d/memory.current) / 1048576 ))M peak=$(( $(cat $d/memory.peak) / 1048576 ))M anon=$(awk '/^anon /{print int($2/1048576)}' $d/memory.stat)M avail=$(awk '/MemAvailable/{print int($2/1024)}' /proc/meminfo)M cpu=$(( $(cat $hw/temp1_input) / 1000 ))C procs=$(wc -l < $d/cgroup.procs) oom_kills=$(awk '/oom_kill /{print $2}' $d/memory.events)" >> "$log"
  done
) &
sampler=$!
# A hard CPU ceiling for everything in the scope (compilers, their threads, linkers):
# CAP_CPU, default 400% (four cores of the host's 16; builds at 4 jobs still
# drove the load to 65, each compile and link running threads of its own).
CARGO_BUILD_JOBS=$jobs systemd-run --user --scope --unit="$name" -q \
  -p MemoryMax="$max" -p MemorySwapMax=0 -p CPUQuota="${CAP_CPU:-400%}" cargo "$@"
rc=$?
kill $sampler 2>/dev/null
echo "cargo exit $rc"
echo "highest anonymous memory:"; sed 's/.*anon=\([0-9]*\)M.*/\1 &/' "$log" | sort -n | tail -1 | cut -d' ' -f2-
exit $rc

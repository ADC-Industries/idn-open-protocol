#!/bin/bash
# gpu-sample.sh PID: over 5 s, every 100 ms, the GPU's busy percentage, its shader clock, its
# memory clock and the mean clock of all cores; prints the means and the share of samples
# with the GPU at 100%.
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../common.sh"
D=/sys/class/drm/card0/device
for i in $(seq 50); do
  s=$(grep '\*' $D/pp_dpm_sclk | head -1 | grep -o '[0-9]*Mhz' | tr -d Mhz)
  m=$(grep '\*' $D/pp_dpm_mclk | head -1 | grep -o '[0-9]*Mhz' | tr -d Mhz)
  echo "$(cat $D/gpu_busy_percent) $(awk '/MHz/{s+=$4;n++}END{printf "%.0f", s/n}' /proc/cpuinfo) ${s:-0} ${m:-0}"
  sleep 0.1
done | awk '{g+=$1; c+=$2; s+=$3; m+=$4; if ($1>=99) f++; n++} END {printf "gpu busy %.0f%% (at 100%%: %.0f%% of samples), gpu %.0f MHz, memory %.0f MHz, cores %.0f MHz\n", g/n, 100*f/n, s/n, m/n, c/n}'

#!/bin/bash
# gpu-procs.sh: each process's GPU (gfx + compute engine) time over one second, in ms.
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../common.sh"
snap() { for f in /proc/[0-9]*/fdinfo/*; do
  awk -v f="$f" '/drm-client-id/{c=$2} /drm-engine-(gfx|compute)/{t+=$2} END{if (c!="") print f, c, t+0}' "$f" 2>/dev/null
done | awk '{split($1,a,"/"); print a[3], $2, $3}' | sort -u -k1,2; }
a=$(snap); sleep 1; b=$(snap)
join <(echo "$a" | awk '{print $1"/"$2, $3}' | sort) <(echo "$b" | awk '{print $1"/"$2, $3}' | sort) |
  awk '{split($1,p,"/"); d=($3-$2)/1e6; if (d>0) {cmd="cat /proc/"p[1]"/comm"; cmd | getline n; close(cmd); printf "%s %s %.0f ms\n", p[1], n, d}}'

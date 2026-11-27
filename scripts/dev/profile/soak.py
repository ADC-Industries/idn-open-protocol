#!/usr/bin/env python3
"""A soak's samples: soak.py OUT.tsv MINUTES EVERY_S LABEL=PID... [--state URL]

Every EVERY_S seconds for MINUTES, each process's CPU (% of a core over the interval),
RSS and own (anonymous) memory in MB, threads and open descriptors, as TSV rows (time,
label, cpu, rss, own, threads, fds); with --state, the switcher's frame time, worst of 10 s
and late frames (label `frames`). At the end a summary per process: first and last
sample, the change in own memory, the CPU's mean and highest, printed and appended to
$OUT/results.tsv. A process that ends is reported once and dropped."""

import json
import os
import sys
import time
import urllib.request

HZ = os.sysconf("SC_CLK_TCK")


def sample(pid):
    try:
        with open(f"/proc/{pid}/stat") as f:
            s = f.read().rsplit(")", 1)[1].split()
        t = int(s[11]) + int(s[12])
        rss = threads = own = 0
        with open(f"/proc/{pid}/status") as f:
            for l in f:
                if l.startswith("VmRSS:"):
                    rss = int(l.split()[1]) // 1024
                elif l.startswith("Threads:"):
                    threads = int(l.split()[1])
        with open(f"/proc/{pid}/smaps_rollup") as f:
            for l in f:
                if l.startswith("Anonymous:"):
                    own = int(l.split()[1]) // 1024
        fds = len(os.listdir(f"/proc/{pid}/fd"))
        return t, rss, own, threads, fds
    except OSError:
        return None


def main():
    a = sys.argv[1:]
    state = None
    if "--state" in a:
        i = a.index("--state")
        state = a[i + 1]
        del a[i:i + 2]
    out, minutes, every = a[0], float(a[1]), float(a[2])
    procs = dict(x.split("=", 1) for x in a[3:])
    prev = {k: sample(v) for k, v in procs.items()}
    rows = {k: [] for k in procs}
    t0 = time.monotonic()
    with open(out, "w") as f:
        f.write("t_s\tlabel\tcpu%\trss_MB\town_MB\tthreads\tfds\n")
        while time.monotonic() - t0 < minutes * 60:
            time.sleep(every)
            now = round(time.monotonic() - t0)
            for k, pid in list(procs.items()):
                s = sample(pid)
                if s is None:
                    f.write(f"{now}\t{k}\tended\n")
                    print(f"soak: {k} ended at {now} s", flush=True)
                    del procs[k]
                    continue
                cpu = (s[0] - prev[k][0]) * 100 / HZ / every
                prev[k] = s
                rows[k].append((now, cpu) + s[1:])
                f.write(f"{now}\t{k}\t{cpu:.1f}\t{s[1]}\t{s[2]}\t{s[3]}\t{s[4]}\n")
            if state:
                try:
                    j = json.load(urllib.request.urlopen(state, timeout=2))
                    f.write(f"{now}\tframes\t{j.get('frame_ms')}\t{j.get('frame_ms_worst_10s')}"
                            f"\t{j.get('late_frames')}\n")
                except Exception as e:
                    f.write(f"{now}\tframes\terror {e}\n")
            f.flush()
    summary = []
    for k, r in rows.items():
        if not r:
            continue
        first, last = r[0], r[-1]
        cpus = [x[1] for x in r]
        summary.append(
            f"soak {k}\tcpu mean {sum(cpus) / len(cpus):.1f}% max {max(cpus):.1f}%"
            f"\town {first[3]} -> {last[3]} MB ({last[3] - first[3]:+d})"
            f"\tRSS {first[2]} -> {last[2]} MB\tthreads {first[4]} -> {last[4]}"
            f"\tfds {first[5]} -> {last[5]}\t{len(r)} samples")
    text = "\n".join(summary)
    print(text)
    o = os.environ.get("OUT")
    if o:
        with open(os.path.join(o, "results.tsv"), "a") as f:
            f.write(text + "\n")


if __name__ == "__main__":
    main()

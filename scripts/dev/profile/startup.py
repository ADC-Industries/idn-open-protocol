#!/usr/bin/env python3
"""A program's start: startup.py LABEL READY [HOLD_S] -- CMD...

Starts CMD and waits until READY: an http(s):// URL that answers (any status), a path
that exists, or `exit` (the command runs to its end). Prints one row (also appended to
$OUT/results.tsv): the time to ready, the CPU used by then, and HOLD_S seconds later
(default 3) the process's memory (RSS, its own anonymous), threads, CPU at rest over those
seconds, and what it holds of the GPU: render-node descriptors and the DRM clients'
memory (fdinfo drm-memory-*/drm-total-*, summed per client). The process is then stopped
(TERM, then KILL)."""

import os
import signal
import ssl
import subprocess
import sys
import time
import urllib.request

HZ = os.sysconf("SC_CLK_TCK")


def ticks(pid):
    try:
        with open(f"/proc/{pid}/stat") as f:
            s = f.read().rsplit(")", 1)[1].split()
        return int(s[11]) + int(s[12])
    except OSError:
        return None


def status(pid, key):
    try:
        with open(f"/proc/{pid}/status") as f:
            for l in f:
                if l.startswith(key + ":"):
                    return int(l.split()[1])
    except OSError:
        pass
    return 0


def anon_mb(pid):
    try:
        with open(f"/proc/{pid}/smaps_rollup") as f:
            for l in f:
                if l.startswith("Anonymous:"):
                    return int(l.split()[1]) // 1024
    except OSError:
        pass
    return 0


def gpu(pid):
    """(render/nvidia descriptors, {memory kind: KiB} summed over DRM clients)."""
    fds, clients = 0, {}
    try:
        names = os.listdir(f"/proc/{pid}/fd")
    except OSError:
        return 0, {}
    for n in names:
        try:
            t = os.readlink(f"/proc/{pid}/fd/{n}")
        except OSError:
            continue
        if t.startswith("/dev/dri/") or t.startswith("/dev/nvidia"):
            fds += 1
        try:
            with open(f"/proc/{pid}/fdinfo/{n}") as f:
                info = f.read()
        except OSError:
            continue
        cid, mem = None, {}
        for l in info.splitlines():
            k, _, v = l.partition(":")
            v = v.split()
            if k == "drm-client-id" and v:
                cid = v[0]
            elif (k.startswith("drm-memory-") or k.startswith("drm-total-")) and v:
                kib = int(v[0]) * {"KiB": 1, "MiB": 1024, "GiB": 1 << 20}.get(
                    v[1] if len(v) > 1 else "", 1 / 1024)
                mem[k.split("-", 2)[2]] = kib
        if cid is not None:
            clients[cid] = mem
    total = {}
    for mem in clients.values():
        for k, v in mem.items():
            total[k] = total.get(k, 0) + v
    return fds, total


def ready_now(ready):
    if ready.startswith("http"):
        try:
            ctx = ssl.create_default_context()
            ctx.check_hostname = False
            ctx.verify_mode = ssl.CERT_NONE
            urllib.request.urlopen(ready, timeout=0.5, context=ctx)
            return True
        except urllib.error.HTTPError:
            return True
        except Exception:
            return False
    return os.path.exists(ready)


def held(pid):
    """Memory, threads and the GPU a running process holds, as row fields."""
    fds, mem = gpu(pid)
    memtxt = " ".join(f"{k} {v / 1024:.0f}MB" for k, v in sorted(mem.items()) if v) or "-"
    return (f"RSS {status(pid, 'VmRSS') // 1024} MB (own {anon_mb(pid)})"
            f"\tthreads {status(pid, 'Threads')}\tGPU fds {fds}\tGPU mem {memtxt}")


def emit(row):
    print(row)
    out = os.environ.get("OUT")
    if out:
        with open(os.path.join(out, "results.tsv"), "a") as f:
            f.write(row + "\n")


def main():
    a = sys.argv[1:]
    # startup.py --pid PID LABEL: what a running process holds.
    if a[:1] == ["--pid"]:
        emit(f"{a[2]}\t{held(int(a[1]))}")
        return
    if "--" not in a or len(a) < 3:
        sys.exit(__doc__)
    i = a.index("--")
    label, ready = a[0], a[1]
    hold = float(a[2]) if i > 2 else 3.0
    cmd = a[i + 1:]
    t0 = time.monotonic()
    p = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                         start_new_session=True)
    if ready == "exit":
        _, st, ru = os.wait4(p.pid, 0)
        wall = (time.monotonic() - t0) * 1000
        row = (f"{label}\tready {wall:.0f} ms\tcpu {(ru.ru_utime + ru.ru_stime) * 1000:.0f} ms"
               f"\tmax RSS {ru.ru_maxrss // 1024} MB\texit {os.waitstatus_to_exitcode(st)}")
    else:
        while not ready_now(ready):
            if p.poll() is not None:
                print(f"{label}\tended before ready ({p.returncode})")
                return
            if time.monotonic() - t0 > 60:
                print(f"{label}\tnot ready in 60 s")
                p.kill()
                return
            time.sleep(0.02)
        wall = (time.monotonic() - t0) * 1000
        c0 = ticks(p.pid) or 0
        time.sleep(hold)
        c1 = ticks(p.pid) or c0
        row = (f"{label}\tready {wall:.0f} ms\tcpu by then {c0 * 1000 // HZ} ms"
               f"\tat rest {(c1 - c0) * 100 / HZ / hold:.1f}%\t{held(p.pid)}")
        try:
            os.killpg(p.pid, signal.SIGTERM)
            p.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(p.pid, signal.SIGKILL)
            p.wait()
        except ProcessLookupError:
            pass
    emit(row)


if __name__ == "__main__":
    main()

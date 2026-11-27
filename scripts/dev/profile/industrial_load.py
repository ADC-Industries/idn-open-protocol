#!/usr/bin/env python3
"""IDN Industrial load for round7.sh.

  industrial_load.py RATE SECONDS STATE_DIR:PORT...
      each machine (its local API on PORT, its token in STATE_DIR/api.token) sent RATE
      messages a second (`batch.ticket`, about 300 bytes) for SECONDS, one connection each;
      the last line: how many were taken, errors, the API's answer time.
  industrial_load.py --make-db FILE ROWS
      a SQLite database with a `loads` table of ROWS rows."""

import http.client
import json
import random
import sqlite3
import sys
import threading
import time


def make_db(path, rows):
    c = sqlite3.connect(path)
    c.execute("DROP TABLE IF EXISTS loads")
    c.execute("CREATE TABLE loads (id INTEGER, modified TEXT, truck TEXT, tons REAL, note TEXT)")
    c.executemany(
        "INSERT INTO loads VALUES (?,?,?,?,?)",
        [(i, "2026-11-07 06:%02d:00" % (i % 60), "T%d" % (i % 40),
          round(random.uniform(5, 12), 2), None) for i in range(rows)])
    c.commit()


def machine(state, port, rate, seconds, results):
    token = open(f"{state}/api.token").read().strip()
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    head = {"Authorization": f"Bearer {token}", "Content-Type": "application/json",
            "Host": f"127.0.0.1:{port}"}
    ok = err = 0
    times = []
    start = time.monotonic()
    n = 0
    while time.monotonic() - start < seconds:
        body = json.dumps({"type": "batch.ticket", "payload": {
            "ticket": n, "plant": "P7", "truck": f"T{n % 40}", "mix": "C30/37",
            "tons": round(random.uniform(5, 12), 2), "water_l": 412.5, "cement_kg": 1630,
            "aggregate_kg": 9120, "admix_ml": 870, "operator": "night shift",
            "started": "2026-11-07T06:10:00Z", "ended": "2026-11-07T06:14:12Z"}})
        t0 = time.monotonic()
        try:
            conn.request("POST", "/messages", body, head)
            r = conn.getresponse()
            r.read()
            if r.status == 200:
                ok += 1
            else:
                err += 1
        except Exception:
            err += 1
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
        times.append((time.monotonic() - t0) * 1000)
        n += 1
        next_at = start + n / rate
        time.sleep(max(0.0, next_at - time.monotonic()))
    results.append((ok, err, times))


def main():
    a = sys.argv[1:]
    if a[:1] == ["--make-db"]:
        make_db(a[1], int(a[2]))
        return
    rate, seconds = float(a[0]), float(a[1])
    results = []
    threads = []
    for spec in a[2:]:
        state, port = spec.rsplit(":", 1)
        t = threading.Thread(target=machine, args=(state, int(port), rate, seconds, results))
        t.start()
        threads.append(t)
    for t in threads:
        t.join()
    ok = sum(r[0] for r in results)
    err = sum(r[1] for r in results)
    times = sorted(x for r in results for x in r[2])
    med = times[len(times) // 2] if times else 0
    p99 = times[len(times) * 99 // 100] if times else 0
    print(f"taken {ok} in {seconds:.0f} s ({ok / seconds:.0f}/s) by {len(results)} machines,"
          f" errors {err}, local API answer median {med:.1f} ms p99 {p99:.1f} ms")


if __name__ == "__main__":
    main()

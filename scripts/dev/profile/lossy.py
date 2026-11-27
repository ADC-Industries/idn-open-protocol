# A UDP relay between a client and 127.0.0.1:TARGET that drops a share of packets both ways.
#   lossy.py LISTEN TARGET DROP [CAP_MBPS|-] [DELAY_MS one way]
import socket, select, random, sys, time, heapq
listen, target, drop = int(sys.argv[1]), int(sys.argv[2]), float(sys.argv[3])
# Optional cap, Mbit/s each way: a policer (over the budget: dropped), as a slow line.
cap = float(sys.argv[4]) * 1e6 / 8 if len(sys.argv) > 4 and sys.argv[4] != "-" else None
delay = float(sys.argv[5]) / 1000 if len(sys.argv) > 5 else 0.0
budget = {True: 0.0, False: 0.0}; last = {True: time.time(), False: time.time()}
def allowed(way, n):
    if cap is None: return True
    now = time.time(); budget[way] = min(cap * 0.05, budget[way] + (now - last[way]) * cap); last[way] = now
    if budget[way] >= n: budget[way] -= n; return True
    return False
front = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); front.bind(("127.0.0.1", listen))
back = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); back.bind(("127.0.0.1", 0))
client = None
held = []; seq = 0  # (due, seq, to_back, data)
while True:
    timeout = max(0.0, held[0][0] - time.time()) if held else None
    r, _, _ = select.select([front, back], [], [], timeout)
    for s in r:
        data, addr = s.recvfrom(65536)
        if random.random() < drop or not allowed(s is front, len(data)):
            continue
        if s is front:
            client = addr
        seq += 1
        heapq.heappush(held, (time.time() + delay, seq, s is front, data))
    now = time.time()
    while held and held[0][0] <= now:
        _, _, to_back, data = heapq.heappop(held)
        if to_back:
            back.sendto(data, ("127.0.0.1", target))
        elif client:
            front.sendto(data, client)

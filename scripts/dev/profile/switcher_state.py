# switcher_state.py: the switcher's frame timing, five samples a second apart, then the worst.
import json, time, urllib.request
def find(d, k):
    if isinstance(d, dict):
        if k in d:
            return d[k]
        for v in d.values():
            r = find(v, k)
            if r is not None:
                return r
    return None
for i in range(5):
    d = json.load(urllib.request.urlopen("http://127.0.0.1:7986/api/v1/state"))
    print("frame %.1f ms  parts %s  gpu %s" % (find(d, "frame_ms"), find(d, "frame_parts_ms"), find(d, "gpu_ms")))
    time.sleep(1)
print("worst 10 s %.1f ms, late frames %s" % (find(d, "frame_ms_worst_10s"), find(d, "late_frames")))

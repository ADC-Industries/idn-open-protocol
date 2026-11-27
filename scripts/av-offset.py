#!/usr/bin/env python3
# Sound against picture in a recorded file (lab items 299/300): for a flash/beep clip (a
# white frame and a beep starting together each second), each beep's start minus the
# nearest flash, in ms, by the file's own times (ffmpeg's signalstats for the frames'
# brightness with their pts, silencedetect for the beeps' starts).
#
#   scripts/av-offset.py FILE
import subprocess, sys, re
f = sys.argv[1]
v = subprocess.run(["ffmpeg","-v","info","-i",f,"-vf","signalstats,metadata=print:key=lavfi.signalstats.YAVG","-an","-f","null","-"],capture_output=True,text=True).stderr
flashes=[]; t=None; last=False
for line in v.splitlines():
    m=re.search(r"pts_time:([0-9.]+)",line)
    if m: t=float(m.group(1))
    m=re.search(r"YAVG=([0-9.]+)",line)
    if m and t is not None:
        b=float(m.group(1))>128
        if b and not last: flashes.append(t)
        last=b
a = subprocess.run(["ffmpeg","-v","info","-i",f,"-af","silencedetect=n=-30dB:d=0.05","-vn","-f","null","-"],capture_output=True,text=True).stderr
beeps=[float(x) for x in re.findall(r"silence_end: ([0-9.]+)",a)]
d=[]
for b in beeps:
    c=min(flashes,key=lambda x:abs(x-b)) if flashes else None
    if c is not None and abs(b-c)<0.3: d.append((b-c)*1000)
d.sort()
print(f"{len(flashes)} flashes, {len(beeps)} beeps, beep-flash ms: median {d[len(d)//2] if d else float('nan'):+.1f}  all {[round(x,1) for x in d]}")

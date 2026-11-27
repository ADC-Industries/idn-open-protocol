#!/bin/bash
# A network receiver joining a switcher whose program stays on the GPU for a GPU receiver.
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd); source "$S/../../common.sh"; source "$S/../lib.sh"
W=$(workdir prof3); H=$(hostname | tr a-z A-Z)
G=$B/examples/gpu_recv
export IDN_NODE=off IDN_CLOCK=off IDN_STATE_DIR=$W/state-join; rm -rf $W/state-join; mkdir -p $W/state-join
P=()
$B/idn send --no-node --name JCam --size 1920x1080 --fps 30/1 --seconds 120 > /dev/null 2>&1 & P+=($!)
$B/idn-switcher --no-node --format 1080p30 --port 7987 --name JSw > $W/join-sw.log 2>&1 & P+=($!)
until curl -s http://127.0.0.1:7987/api/v1/state >/dev/null 2>&1; do sleep 0.5; done; sleep 2
api="curl -s -X POST -H X-IDN-Switcher:1 http://127.0.0.1:7987/api/v1"
$api/add_input -d "{\"network\":\"$H (JCam)\",\"label\":\"Cam\"}" >/dev/null
$api/add_input -d '{"title":{"layers":[{"type":"text","rect":[0.07,0.8,0.46,0.1],"text":"Join","size":0.05,"colour":"#ffffff"}]},"label":"T"}' >/dev/null
on_bus http://127.0.0.1:7987/api/v1 program 1; bug_on http://127.0.0.1:7987/api/v1 2
sleep 2
$G "$H (JSw)" 30 > $W/join-gpu.log 2>&1 & P+=($!)
sleep 6
IDN_MACHINE=off timeout 15 $B/idn recv "$H (JSw)" --no-node --seconds 8 --max-frames 1 --out $W/state-join/rx > $W/join-net.log 2>&1
grep -E "video messages decoded" $W/join-net.log
# A recording started now: pictures in the file.
rm -f $W/join-rec.mp4
$api/stream_add -d "{\"file\":\"$W/join-rec.mp4\"}" >/dev/null
sleep 6
$api/stream_remove -d '{"id":1}' >/dev/null; curl -s http://127.0.0.1:7987/api/v1/state | python3 -c "import json,sys; s=json.load(sys.stdin); print('streams', s.get('streams'))" | cut -c1-300
sleep 3
ffprobe -v error -count_frames -select_streams v:0 -show_entries stream=nb_read_frames,width,height -of csv=p=0 $W/join-rec.mp4
tail -2 $W/join-gpu.log
for p in "${P[@]}"; do kill $p 2>/dev/null; done; wait 2>/dev/null

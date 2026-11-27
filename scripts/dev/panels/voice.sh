#!/bin/bash
# IDN Voice in a real browser (voice.mjs), over WebRTC and then over the WebSocket: the
# Industrial Site Manager (debug build, or IDN_BIN) with Voice, then the IDN Site Manager's
# (HTTPS, its own port), headless Chromium with a fake microphone. Exit 1 if any fails.
set -u
S=$(cd "$(dirname "$(readlink -f "$0")")" && pwd)
IMPL=$(cd "$S/../../.." && pwd)
B=${IDN_BIN:-$IMPL/target/debug}
W=$(mktemp -d)
PIDS=()
stop() { for p in "${PIDS[@]}"; do kill -TERM $p 2>/dev/null; wait $p 2>/dev/null; done; PIDS=(); }
trap 'stop; rm -rf "$W"' EXIT
failed=0
for mode in rtc ws; do
  rm -rf "$W/sm"; mkdir -p "$W/sm"
  udp=0.0.0.0:17967; [ $mode = ws ] && udp=off
  IDN_DATA=$W/sm IDN_HTTP=127.0.0.1:18082 IDN_QUIC=off IDN_MQTT=127.0.0.1:11885 IDN_API_KEY=voice-key-1234567890 \
    IDN_ADMIN_PASSWORD=voice-password1 IDN_VOICE_UDP=$udp "$B/idn-site-manager-industrial" > "$W/sm.log" 2>&1 & PIDS+=($!)
  chromium --headless=new --no-sandbox --use-fake-device-for-media-stream --use-fake-ui-for-media-stream \
    --autoplay-policy=no-user-gesture-required --remote-debugging-port=9354 --user-data-dir="$W/chromium-$mode" \
    about:blank > /dev/null 2>&1 & PIDS+=($!)
  sleep 3
  node "$S/voice.mjs" 9354 http://127.0.0.1:18082/ voice-password1 $mode || { failed=1; tail -5 "$W/sm.log"; }
  stop
done
# The IDN Site Manager (Media): HTTPS on its panel's port plus one, the site password as
# the administrator's.
for mode in rtc ws; do
  rm -rf "$W/msm"; mkdir -p "$W/msm"; echo "voice-password1" > "$W/msm/pw"
  udp=0.0.0.0:17967; [ $mode = ws ] && udp=off
  IDN_VOICE_UDP=$udp "$B/idn-site-manager" --trust-dir "$W/msm/trust" --port 15961 --panel-port 17961 \
    --panel-password-file "$W/msm/pw" --no-announce > "$W/msm.log" 2>&1 & PIDS+=($!)
  chromium --headless=new --no-sandbox --ignore-certificate-errors --use-fake-device-for-media-stream \
    --use-fake-ui-for-media-stream --autoplay-policy=no-user-gesture-required --remote-debugging-port=9354 \
    --user-data-dir="$W/chromium-m$mode" about:blank > /dev/null 2>&1 & PIDS+=($!)
  sleep 3
  node "$S/voice.mjs" 9354 https://127.0.0.1:17962/ voice-password1 $mode || { failed=1; tail -5 "$W/msm.log"; }
  stop
done
exit $failed

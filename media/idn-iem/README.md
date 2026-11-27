# idn-iem: in-ear monitoring for phones

The desk's IEM flow (spec/extensions/idn-iem.md) and the **phone mixing library** that
takes it: a musician's phone receives the desk's IEM channels (one Opus stream each,
5 ms frames), mixes them itself by the musician's mix (saved on the desk, followed live
both ways) and plays them; push-to-talk goes back to the desk as one of its inputs.

- `wire`, `codec`: the packets and the Opus coding (the desk and the phones share them).
- `receiver` (feature `receiver`, on by default): the phone mixing library.
- `ffi`: its C ABI, `include/idn_iem.h`, for Kotlin (JNI) and Swift.
- `play` (feature `play`): the mix on the system's sound card and talkback from its
  microphone with cpal (Linux, Windows, macOS); `idn iem listen` uses it.

MIT OR Apache-2.0, © ADC Industries. Not published to crates.io.

## Embedding it in a phone app

### Build

The library is Rust with one C library, libopus 1.6, built from its bundled source
with CMake by `opusic-c` (BSD-3-Clause). TLS is rustls with ring (no OpenSSL), the push
channel tungstenite. Nothing else is native.

| Target | Rust target | Crate type | Notes |
|---|---|---|---|
| Android arm64 | `aarch64-linux-android` | `cdylib` (`libidn_iem.so`) | the NDK's clang and CMake toolchain (`cargo ndk -t arm64-v8a`, or `CC_aarch64_linux_android`, `AR_…`, `ANDROID_NDK_HOME` set); minSdk 24+ |
| Android x86-64 (emulator) | `x86_64-linux-android` | `cdylib` | as above |
| iOS | `aarch64-apple-ios` | `staticlib` (`libidn_iem.a`) | Xcode's clang; wrap in an XCFramework with the simulator's `aarch64-apple-ios-sim` |

The crate type is chosen at build time, so IDN's own builds stay plain Rust libraries:

```sh
# Android (with cargo-ndk):
cargo ndk -t arm64-v8a rustc -p idn-iem --lib --release --crate-type cdylib
# iOS:
cargo rustc -p idn-iem --lib --release --target aarch64-apple-ios --crate-type staticlib
```

(Not yet built for Android or iOS in IDN's lab: the host has neither toolchain. The
library is checked for Linux and Windows, and linked from C as a `staticlib` on Linux.)

Without the `play` feature (the default) there is no audio-output dependency: the app
plays the mix with its own low-latency output and pulls it with `idn_iem_render`.

### The C ABI in short

```c
IdnIemReceiver *rx = idn_iem_connect(
    "{\"desk\": \"192.168.1.20\", \"token\": \"idn1...\", \"transport\": \"auto\", \"talk\": true}",
    site_ca_der, site_ca_len, &code, err, sizeof err);   // HTTPS, the site CA pinned

// The audio callback (AAudio/Oboe, AVAudioEngine's source node): 48 kHz stereo float.
idn_iem_render(rx, buffer, frames);

// The UI.
char *mix = idn_iem_mix_json(rx);        // {"id","name","master_db","channels":{"3":{"level_db",...}}}
char *chans = idn_iem_channels_json(rx); // [{"id":3,"name":"Keys L","pair":4,"side":"L"},...]
idn_iem_string_free(mix); idn_iem_string_free(chans);
idn_iem_set_level(rx, 3, -6.0f);         // a fader (a pair by either side)
idn_iem_set_pan(rx, 3, 0.25f);           // a pair: its balance
idn_iem_set_mute(rx, 5, 1); idn_iem_set_solo(rx, 2, 1); idn_iem_set_master(rx, -3.0f);
idn_iem_set_event_callback(rx, on_event, ctx);   // {"type":"mix_changed"}, ...

// Push-to-talk: the microphone at its own rate and channels.
idn_iem_talk_start(rx);
idn_iem_talk_feed(rx, mic, frames, 48000, 1);
idn_iem_talk_stop(rx);

idn_iem_free(rx);
```

Results are `IDN_IEM_OK` (0) or a negative code (`include/idn_iem.h`): the refusals a
musician sees are `IDN_IEM_TOKEN_NEEDED`, `IDN_IEM_TOKEN_REFUSED` (expired, revoked),
`IDN_IEM_NOT_ALLOWED`, `IDN_IEM_FULL` (the desk's 32 unicast phones), `IDN_IEM_TLS`
(another certificate), `IDN_IEM_NO_MIX`.

**Kotlin:** a small JNI shim (`external fun connect(config: String, ca: ByteArray?):
Long` …) over these functions; render from an Oboe callback into a `FloatArray`
(or a direct `ByteBuffer`). **Swift:** import the header through a module map;
`idn_iem_render` from an `AVAudioSourceNode`'s render block.

### Connecting

- **Where:** the desk's address (`HOST`; port 29215 over HTTPS by default when a
  certificate is given, 29214 plain on the desk's own machine). The UDP port and the
  multicast group come from the desk's description (`GET /api/v1/iem`).
- **Trust:** on a desk whose machine is in a site (the usual case), pin the **site CA**
  certificate (DER or PEM; from the Site Manager, `GET /api/v1/site_ca`, no login): the
  desk's network port presents the certificate the site signed for its machine, with the
  CA after it (spec idn-extensions.md §E.10.11). The name in it isn't checked against a
  pinned CA (phones reach desks by address; the machine's LAN addresses are in it
  anyway). On a desk not in a site, pin the desk's own self-signed certificate, or give
  its SHA-256 fingerprint (`"ca_sha256"`) as the desk prints it; once that machine joins
  a site, its port presents the site's certificate instead, and only the site CA works.
- **Token:** a site token from the Site Manager, `iem:<mix>` for a musician (its own mix
  only: the library learns which from the desk). `control` and `read` tokens need
  `"mix"`. Renew it before it expires with `idn_iem_set_token`.
- **Transport:** `auto` (multicast when the desk sends it and audio arrives within
  `multicast_wait_ms`, 1.5 s; unicast otherwise, or if multicast stops), `unicast`, or
  `multicast`; `idn_iem_set_transport` changes it live. Multicast on a phone needs:
  Android a `WifiManager.MulticastLock` held while listening and the
  `CHANGE_WIFI_MULTICAST_STATE` permission; iOS the
  `com.apple.developer.networking.multicast` entitlement (Apple grants it on request).
  Without those, use `unicast` (or leave `auto` to fall back).
- **Talk:** `"talk": true` asks for it; a token that may not talk still listens (an
  event `talk_refused`, `idn_iem_talk_start` → `IDN_IEM_NOT_ALLOWED`).

### Threads

The library runs two threads of its own (the UDP session and audio; the push channel)
and a third while on multicast. `idn_iem_render` is for the audio thread: it decodes and
mixes there (one Opus decoder per channel in the mix: 24 channels took 2.3 % of one
core of the lab's x86 mini PC, `the_cost_of_24_channels`; expect a few % of a phone's big
core), allocates nothing of its own in steady state and holds a lock only to take the
next frame. Everything else may be called from any thread. The event callback runs on the
library's threads: hand the JSON to the UI thread.

### Latency budget

| Part | ms |
|---|---|
| The desk: its block, conversion to 48 kHz at other rates, a 5 ms frame, Opus's 2.5 ms look-ahead (`latency.desk_ms`) | ~10 |
| Wi-Fi (multicast on some access points waits for the beacon: use unicast there) | 2–10 |
| The jitter buffer: starts at 15 ms, grows on late frames, shrinks back after 10 s calm (10–100) | ~10–25 |
| Decoding and mixing | < 1 |
| The phone's sound output (AAudio/Oboe low-latency, AVAudioEngine) | 5–20 |
| **In all** (the design accepts +20–40 ms over the desk) | ~30–55 |

Measured over loopback in the tests: from the desk's tap to the frame starting to play
18–22 ms (the buffer settled at its 15 ms target). `idn_iem_status_json` reports
`buffer_ms`, `target_ms`, `rtt_ms`, `loss_pct`, and `latency_ms` (the desk's part, half
the round trip, the buffer and the output latency the app gives with
`idn_iem_set_output_latency_ms`).

### The jitter buffer

Frames are played by their number. A frame missing when later ones are there is lost
and concealed (Opus PLC); nothing there at all means the network is late: one frame is
concealed *without* moving on, so the buffer grows and keeps that much more from then
on. When a frame more than needed was always waiting for a second (the desk's clock
running ahead of the phone's), or three for 200 ms (after a burst), one frame is taken
out, crossfaded into the next. Playing starts with the newest frames; a stop of 200 ms
starts afresh.

### Following the mix

The desk's push channel carries every change of the mix (the sound person's at the
desk, the musician's on another device); the library applies them live. The phone's own
changes are heard at once and sent as partial `iem_mix` commands, at most every 15 ms a
control, each with its per-control sequence number; while one is on its way, the desk's
older value for that control doesn't pull the fader back. A change the desk refuses
comes back as an `error` event (`"error": "desk"`) and the desk's value returns.

## Trying it on a computer

```sh
idn iem listen --desk 192.168.1.20 --ca site-ca.der --token - --talk   # token from stdin
idn iem listen --desk 127.0.0.1 --mix drums --unicast                  # the desk's own machine
```

It plays the mix on the default sound card (`--device NAME`, or `--no-sound`), prints the
status every second, and with `--talk` Enter starts and stops talking.

# idn-voice-client: IDN Voice for handset apps

The client side of IDN Voice (spec/extensions/idn-voice.md §V.13–§V.14; the protocol in
docs/VOICE-CLIENT.md) as a library, so an app is a Voice client without writing the
protocol again. Made for a fleet app on Android, on the Inrico B01 (radios in concrete
trucks that change networks all day); any native app can use it.

- **Connects** over QUIC on IDN's link first (ALPN `idn-voice/1`, connection migration,
  the link's certificate pinned), the WebSocket over TLS (TCP) when QUIC doesn't
  connect or the network blocks UDP; while on the WebSocket it tries QUIC again and moves
  the session over when it connects.
- **Keeps the session** across dropped links: it reconnects at once (after a connection
  that lived 5 s; sooner than that counts as a failed try), then backs off (1, 2, 4…
  10 s), always resuming with the newest token and `have` (the 256 talkers heard most
  recently), so nothing sent meanwhile is lost (the server's store-and-forward window).
  A link that goes silent without a word is given up after `hello`'s `idle_timeout_s`.
- **Talks through dead zones**: a driver who talks while the link is down loses nothing
  (up to `hold_mic_s`, 30 s): see "A talk through a dead zone" below.
- **Sound**: the app pushes microphone PCM and pulls speaker PCM; the library cuts 20 ms
  frames, codes Opus (mono, 24 kbit/s, in-band FEC), sends only while talking, keeps a
  jitter buffer per talker, decodes, conceals losses and mixes, at 48 kHz mono.
- **Everything else** (channels, talk, messages, SOS, calls, drivers) as commands and
  events, in the protocol's own JSON.

MIT OR Apache-2.0, © ADC Industries. Not published to crates.io.

## Threads

The library runs a small tokio runtime of its own: **two worker threads** (the session,
its connection's reader and writer, QUIC), and at most two more briefly for name
lookups. Nothing runs on the app's threads but the calls themselves.

- Every function may be called from any thread, together.
- `pull_speaker` is for the audio output thread: it decodes and mixes there (one Opus
  decoder per talker heard), holds a lock only briefly, and never waits for the network.
- `push_mic` is for the recording thread: it codes there (about 1 ms of a small core per
  20 ms) and queues the packet; it never waits for the network.
- **Events** come on the library's threads, one at a time. Hand them to the app's own
  thread; don't block in the callback. The callback may call the library (also `close`).
- `close` signs off (`bye`), stops the threads, and returns once no event will come.

## Rust

```rust
use idn_voice_client::{Client, Config, Event};

let mut cfg = Config::from_json(&enrolment_answer_with_the_credential)?;
let client = Client::connect(cfg, |e| match e {
    Event::Hello { resumed, hello } => { /* channels, me, joined */ }
    Event::Talking { channel, talkers } => { /* who talks */ }
    Event::Reconnecting { .. } | Event::Connected(_) => { /* link state */ }
    _ => {}
})?;
client.join(3, None);
client.talk_start(3u32);            // the PTT button down
client.push_mic_i16(&pcm, 48_000)?; // as the microphone gives it
client.talk_stop();                 // up
client.pull_speaker_i16(&mut out);  // in the audio output callback
client.sos(Some((53.3498, -6.2603)), None);
client.close();
```

Commands: `join`, `leave`, `talk_start`/`talk_stop`, `message`, `sos`, `call`, `answer`,
`hangup`, `send_json` (anything else in VOICE-CLIENT.md §4.3), `set_channel_volume`,
`network_changed`. State: `transport`, `resume_state`, `stats`.

## The C ABI

`include/idn_voice_client.h` (hand-written to match `src/ffi.rs`):

```c
IdnVoiceClient *idn_voice_connect(const char *config_json, IdnVoiceEventCallback callback,
                                  void *user_data);
int32_t idn_voice_send_json(const IdnVoiceClient *client, const char *json);
int32_t idn_voice_talk(const IdnVoiceClient *client, uint32_t channel, int32_t on);
int32_t idn_voice_push_mic_i16(const IdnVoiceClient *client, const int16_t *pcm,
                               size_t samples, uint32_t rate);
int32_t idn_voice_pull_speaker_i16(const IdnVoiceClient *client, int16_t *out,
                                   size_t samples);
int32_t idn_voice_network_changed(const IdnVoiceClient *client);
char *idn_voice_resume_state(const IdnVoiceClient *client);
void idn_voice_close(IdnVoiceClient *client);
void idn_voice_free_string(char *s);
```

Results are `IDN_VOICE_OK` (0) or negative: `IDN_VOICE_BAD_ARGUMENT`,
`IDN_VOICE_BAD_JSON`, `IDN_VOICE_BAD_RATE`, `IDN_VOICE_PANIC`, `IDN_VOICE_CODEC` (the
Opus encoder failed). No call unwinds into C.
Events are JSON, `{"event": NAME, …}`: `connected` (`transport`), `reconnecting`
(`attempt`, `why`), `hello` (`resumed`, `hello`), `driver`, `joined`, `left`, `talking`
(`channel`, `talkers`), `talk` (`on`, `why`; `why` `busy` when half duplex refused the
start, with the server's `busy` after it as `other`), `message`, `sos`, `error` (`what`,
`error`), `closed` (`why`: the session ended for good, e.g. revoked), `other` (any other
server message, whole). The string is valid during the callback only.

## JNI (the app's wrapper)

The JNI side is the app's: a small shim over the C ABI. A sketch:

```kotlin
object Voice {
    init { System.loadLibrary("idn_voice_client") }
    external fun connect(configJson: String, listener: Listener): Long   // 0: refused
    external fun sendJson(h: Long, json: String): Int
    external fun talk(h: Long, channel: Int, on: Boolean): Int
    external fun pushMic(h: Long, pcm: ShortArray, n: Int, rate: Int): Int
    external fun pullSpeaker(h: Long, out: ShortArray, n: Int): Int
    external fun networkChanged(h: Long): Int
    external fun close(h: Long)
    interface Listener { fun onEvent(json: String) }
}
```

```c
// connect: keep a global ref to the listener; the callback attaches its thread.
static JavaVM *vm;
static void on_event(void *ud, const char *json) {
    JNIEnv *env;
    (*vm)->AttachCurrentThreadAsDaemon(vm, (void **)&env, NULL);  // library threads
    jobject listener = (jobject)ud;                                // a NewGlobalRef
    jstring s = (*env)->NewStringUTF(env, json);
    (*env)->CallVoidMethod(env, listener, on_event_method, s);    // post to the UI thread
    (*env)->DeleteLocalRef(env, s);
}
JNIEXPORT jlong JNICALL Java_..._connect(JNIEnv *env, jobject self, jstring cfg, jobject l) {
    const char *c = (*env)->GetStringUTFChars(env, cfg, NULL);
    jobject g = (*env)->NewGlobalRef(env, l);
    IdnVoiceClient *h = idn_voice_connect(c, on_event, g);
    (*env)->ReleaseStringUTFChars(env, cfg, c);
    if (!h) (*env)->DeleteGlobalRef(env, g);
    return (jlong)(intptr_t)h;
}
// pullSpeaker: GetShortArrayElements / ReleaseShortArrayElements (or a direct ByteBuffer
// and GetDirectBufferAddress) around idn_voice_pull_speaker_i16, from the Oboe/AAudio
// callback. close: idn_voice_close(h), then DeleteGlobalRef the listener.
```

- `AttachCurrentThreadAsDaemon` once per library thread (attaching again is cheap); the
  library's threads are named `idn-voice-client`.
- Android audio: record with `AudioRecord` (or Oboe) at 48000 Hz mono 16-bit
  (`VOICE_COMMUNICATION` source for echo cancellation); play with Oboe/AAudio or
  `AudioTrack` at 48000 Hz mono, pulling 10–20 ms a callback.
- Call `networkChanged` from `ConnectivityManager.NetworkCallback` (`onAvailable`,
  `onLost`, `onCapabilitiesChanged` of the default network).
- Save `idn_voice_resume_state` when the app goes to the background; give it as
  `resume`/`have` in the next configuration so a restart within `resume_s` (120 s) keeps
  the session.

## Building for Android

Pure Rust beside one C library, libopus 1.6, built from its bundled source with CMake by
`opusic-c`; TLS is rustls with ring (C and assembly from the NDK's clang). An app's
developers build it on their side with the NDK:

The crate is a Rust library (`rlib`), as idn-iem's: the C library is chosen at build
time with `cargo rustc --crate-type`, so IDN's own builds link no `.so` or `.a` they
don't use. The C ABI's functions are `#[unsafe(no_mangle)] extern "C"`, so a `cdylib`
exports them.

```sh
rustup target add aarch64-linux-android armv7-linux-androideabi
# Android (with cargo-ndk: the NDK's clang, ar and CMake toolchain), one ABI a run:
cargo ndk -t arm64-v8a rustc -p idn-voice-client --lib --release --crate-type cdylib
cargo ndk -t armeabi-v7a rustc -p idn-voice-client --lib --release --crate-type cdylib
# → target/<triple>/release/libidn_voice_client.so, into app/src/main/jniLibs/<abi>/
# or by hand: the target triple, and the NDK's compilers for the C parts
export ANDROID_NDK_HOME=/path/to/ndk
export CC_aarch64_linux_android=$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android24-clang
export AR_aarch64_linux_android=$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-ar
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=$CC_aarch64_linux_android
cargo rustc -p idn-voice-client --lib --release --target aarch64-linux-android --crate-type cdylib
# A static library for C elsewhere (iOS, a desktop program):
cargo rustc -p idn-voice-client --lib --release --crate-type staticlib
```

minSdk 24 or later. Which ABI the B01 needs (`arm64-v8a` or `armeabi-v7a`)
follows its processor; on 32-bit ARM the TLS cipher is ChaCha20 (much quicker there).
Not built for Android in IDN's lab (no NDK here): it builds and is tested on Linux, and
uses nothing Linux-only (no desktop audio, no `/proc`, no Linux socket options).

## Configuration

```json
{"credential": "idnv1.h4kq2x7ma.n3…",
 "link": ["192.168.1.5:29203"], "link_cert_sha256": "3F:1A:…",
 "urls": ["https://192.168.1.5:29203/voice", "https://voice.example.com/voice"],
 "cert_sha256": "3F:1A:…",
 "resume": "q3…", "have": "17:204,22:9",
 "app": "Fleet Driver", "version": "2.4.0",
 "transport": "auto", "link_connect_ms": 3000, "link_retry_s": 60,
 "link_keepalive_ms": 5000, "ws_connect_ms": 10000, "hold_mic_s": 30,
 "fec_loss_pct": 10}
```

The enrolment or provisioning answer's fields keep their names: pass it with the
credential. `urls` may be base addresses (`https://…/voice`) or WebSocket ones
(`wss://…/voice/ws`); plain `http://` only where the site allows plain handsets.
Fingerprints: 64 hex digits, colons optional. `hold_mic_s` (0–300, default 30): how much
of a talk is held while the link is down (below). `fec_loss_pct` (0–50, default 10): the
loss the microphone's Opus plans its in-band FEC for; each packet then carries a coarse
copy of the one before, so a frame the server dropped for a slow listener is rebuilt
from the next (out of the same 24 kbit/s). Opus adds FEC only from about 10 %; 0 turns
it off.

## A talk through a dead zone

A truck drives into a dead zone while its driver talks. Nothing of the talk is lost:

- **Held**: while the link is down the microphone's packets are kept, in order with the
  talk's start and end (a driver who lets go meanwhile ends the talk after them), up to
  `hold_mic_s` (30 s); past it the oldest go, counted (`Stats::mic_dropped`;
  `mic_held` is what waits). With `hold_mic_s` 0 nothing is held while the link is down.
- **Confirmed**: a cellular link can die without a word, and what was written into it
  then never arrives. So every packet is kept after it is sent until a `pong` confirms
  it (the server handles a connection's messages in order: a `pong` means everything
  sent before its `ping` arrived); while anything is unconfirmed the library pings every
  second. After a drop, what wasn't confirmed is sent again: at worst about a second
  is heard twice, never lost.
- **After the resume**: the server ended the talk when the link went ("link lost", or
  "no sound" after `talk_silence_s`), so the library starts it again before the held
  packets: each packet belongs to the talk it was made in, and that talk's own `talk` on
  goes once before its first packet (a talk with no packet before its end isn't started
  again). Then they go in order before the live microphone, at 4× real time (one every
  5 ms). The server forwards packets as they come (no pacing, no limit but a packet's
  size), and has no mark for late ones: they are an ordinary continuing talk. **What
  listeners hear**: the held speech in full, late, as a burst: their jitter buffer takes
  it at once, plays it at normal speed and catches up (skipping silence, up to 1.25×);
  IDN's web client plays it as it comes. A recording keeps the packets as they arrived
  (the held part closer together).
- **A dead zone the connection survives** (QUIC keeps its connection through a few
  seconds of nothing): the server ends the talk after `talk_silence_s` ("no sound") and
  drops the packets that come after for want of a talk. On that `talking` off the
  library sends again everything of the talk not yet confirmed, the talk started first:
  the same as after a reconnect (at most about a second heard twice).
- **The app keeps recording**: the `talk` event with `on: false` and `why` `link lost`
  or `no sound` is the server's view; the library starts the talk again itself while
  the app holds it. Keep the microphone running while the button is held; stop only on
  the app's own release or another `why` (`time limit`, `taken`, `busy`…).
- **Which talk the server ended**: `talking` names none, so the library numbers the
  talks it starts and matches the answers in order. An end of an older talk (a time
  limit that comes late) drops only what waits of that one, never a newer talk the app
  started meanwhile. If the floor was taken (half duplex, `busy`) or the talk refused,
  its waiting packets are dropped and counted.
- **Other commands** (join, message, SOS, answer…) go in the same order and are kept
  until confirmed too, and sent again after a drop: none is lost to a silent drop or a
  move to QUIC. One sent in the second before a silent drop may arrive twice (an SOS
  twice is one alert: the server merges presses within `sos_merge_s`; a text message
  may show twice).

## Transports and trust

- **QUIC** (the link, VOICE-CLIENT.md §12): tried first, at the configuration's `link`
  addresses, then where `hello.link.quic` says (its `port` on each server host, its
  `addrs`); not where the site offers none (`quic: null`). The listener's leaf
  certificate (Ed25519) must hash to `link_cert_sha256`, or, when none is configured, to
  `hello.link.quic.cert_sha256`; no authority is ever asked. What `hello` says of the
  link (addresses and pin) is learnt only over a pinned connection: QUIC, or a WebSocket
  whose certificate matched `cert_sha256` (not over plain HTTP or a public authority's
  certificate, where anyone in the path could say it). No client
  certificate is needed (a throwaway Ed25519 one is shown if a listener asks); the first
  frame signs in: `{"t": "auth", "credential", "resume", "have", "app", "version"}`.
  One bidirectional stream; frames are a u32 little-endian length, a kind byte (0 JSON,
  1 sound) and the payload (64 KiB at most). An `auth` refused for the credential ends
  the client; any other refusal (QUIC off) means the WebSocket.
- **WebSocket**: `Authorization: Bearer CREDENTIAL`, `?resume=&have=` to come back. The
  server's certificate must hash to `cert_sha256`; one that doesn't (or no pin) is
  checked against public authorities (a tunnel's certificate), as VOICE-CLIENT.md §3.2
  says.
- **Fallback and moving over**: QUIC that doesn't connect in `link_connect_ms` (3 s) →
  the WebSocket; on the WebSocket QUIC is tried every `link_retry_s` (60 s) and the
  session moves over (resumed) when it connects. `network_changed` moves QUIC to a new
  socket (connection migration: no resume) and gives a WebSocket on the old network up at
  once.
- A credential refused (401, or `bye` with a revoked handset) ends the client: `closed`,
  no more tries. A 403 (e.g. "handsets only over HTTPS") tries the next address.

## Settings it follows

From `hello`'s `link` block (the configuration's values until the first `hello`):
`ping_s` (pings), `idle_timeout_s` (a silent link is dead; QUIC's idle timeout too),
`link_connect_ms`, `link_retry_s`, `link_keepalive_ms`, `quic` (where the link is, or
off), and the jitter
buffer's `jitter_start_ms`/`jitter_max_ms` (each channel's own winning). Fixed in the
library: Opus at 24 kbit/s with FEC; the buffer grows by a stall's length (100 ms at
most a stall), shrinks 4 ms a second while steady and halfway back to its start after a
talk, jumps a gap that a reliable link will never fill (one concealment, from the next
frame's FEC), takes a replay counting from 0 again as a new stream, compares `seq` as
serial numbers (it may wrap), and catches up on a backlog by skipping silence and
playing at up to 1.25×. At most 32 talkers' streams are decoded at once (the one quiet
longest goes).

## Tests

`cargo nextest run -p idn-voice-client`: framing, the jitter buffer, the configuration,
Opus, the C ABI; against the real Voice server in-process over the WebSocket (two
handsets: one talks, the other hears it, level, pitch and delay checked; a link cut and
resumed with `have`, the missed sound played; a talker whose link is cut talking on,
heard in full after its resume; a link gone silent noticed at the idle timeout and
resumed; revoked and unknown credentials); the outbox (held, confirmed, sent again
after a drop or a quiet end, one start per talk, the right talk restarted or ended,
bounded); against
a QUIC server of the tests' own (the pin, a client certificate when asked, sound both
ways, a wrong pin refused before the credential goes). `--features quic-server-tests`
adds Voice's own QUIC listener (`tests/all/quic_server.rs`: QUIC and WebSocket handsets
hearing each other, a network change kept by connection migration, the link learnt
only from a pinned server and used when configured, a move after QUIC first failed).

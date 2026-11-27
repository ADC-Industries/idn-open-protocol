# IDN Voice: the client protocol

For developers of Voice clients: a handset app (such as an Android app for the Inrico
B01), a console, a program. Everything here is what the server (`core/idn-voice`:
`http.rs`, `http/handsets.rs`, `ws.rs`, `link.rs`, `session.rs`, `hub.rs`, `rtc.rs`)
does; the design is spec/extensions/idn-voice.md (§V.8 the protocol, §V.13 handsets and
drivers, §V.14 Voice over IDN's link).

**Handsets: IDN's link (QUIC, §12) first, the WebSocket (§4) as the fallback.** Both
carry the same messages and the same session; QUIC keeps the connection when the phone
moves between Wi-Fi and cellular.

**Native apps: use the client library.** `core/idn-voice-client` is this protocol as a
library (Rust, with a C ABI for JNI and Swift: `include/idn_voice_client.h`): QUIC first,
the WebSocket as the fallback and the move back to QUIC, resume with `have`, Opus coding
(20 ms, 24 kbit/s, FEC), a jitter buffer per talker, concealment and mixing, every
timing from `hello`, and talk through dead zones: while the link is down the microphone's
packets are held (`hold_mic_s`, 30 s) and kept after sending until a `pong` confirms
them; after the resume the talk is started again and they go first, in order, at 4× real
time (listeners hear the held speech in full, late, as a burst their jitter buffers
catch up on; at worst about a second twice after a silent drop). The same when the server
ends a talk with "no sound" through a dead zone the QUIC connection survives; the app
keeps recording through `talk` off "link lost"/"no sound" while the button is held. The
app's other commands are kept until confirmed and sent again after a drop too. What
`hello` says of the link is believed only over a pinned connection. The app gives it the
enrolment answer and the credential, pushes
microphone PCM and pulls speaker PCM (48 kHz mono), and gets events as JSON. It is the
recommended way for every native client (an Android app links it through JNI);
its README covers the threads, the C API and building for Android. What follows is for
clients that speak the protocol themselves; the library does all of it.

## 1. Where the server is

| Edition | Base URL (`BASE` below) | WebSocket |
|---|---|---|
| IDN Site Manager (Media) | `https://HOST:29203/voice` (the panel's port + 1; its self-signed certificate) | `wss://HOST:29203/voice/ws` |
| IDN Site Manager \| Industrial | `https://HOST:29203/voice` (`IDN_VOICE_HTTPS`; its own self-signed certificate) on the plant network; `https://NAME/voice` through its tunnel; `http://HOST:8080/voice` for browsers only | `wss://…/voice/ws` |

IDN's link (QUIC, §12) is on UDP **29203** in both editions (the Voice HTTPS port's
number; Industrial: `IDN_VOICE_HTTPS`'s address). Through Industrial's tunnel there is
no UDP: there the WebSocket is the way. WebRTC
(optional, §8) is on UDP 29207. A handset is told its addresses at enrolment (§3: `urls`,
the one it enrolled through first; `link`, the QUIC ones) and in `hello` (`link.quic`).

Errors over HTTP are `{"error": "why"}` with 400 (a bad request), 401 (sign in, or a
credential that isn't one), 403 (not allowed), 404 (no such thing) or 429 (too many
tries; wait a minute). Every request that changes something needs the header
`X-IDN-Voice: 1` or an `Authorization` header (a browser page of another site can send
neither).

## 2. Signing in

- **A handset**: `Authorization: Bearer idnv1.<handset id>.<secret>` on the WebSocket
  upgrade and on HTTP, **over HTTPS only**. Never put it in a URL. Over plain HTTP (IDN
  Industrial's port 8080) it is refused with 403 "handsets only over HTTPS", unless it
  came through the site's tunnel or the site turned `plain_handsets` on. A handset is
  never an administrator: every administrators' route answers it 401.
- **Who a handset is**: its **key**, `driver:<driver id>` (a driver assigned) or
  `handset:<handset id>`, given in `hello.me.key` and presence. Its name is only shown:
  two drivers may share one, and one may be named like a site user without being them.
  Call or message a handset by its key; a site user by their name.
- **A site user**: the Site Manager's session cookie, or Voice's own login
  (`POST BASE/api/login {"user", "password"}` → cookie `idn_voice`, 12 hours; five wrong
  passwords a minute per address).
- **A program**: Industrial's API key as `Authorization: Bearer KEY` (an administrator,
  named `api`).
- **A guest**: `GET BASE/ws?guest=NAME` (Guest and password channels only).

`GET BASE/api/me` → a handset: `{"name", "admin": false, "dispatch", "handset": {"id",
"name", "driver"}, "cert_sha256"}`; a user `{"name", "admin", "dispatch"}`; nobody
`{"name": null}`.

## 3. Enrolment and the site's certificate

**The code.** An administrator (or the dispatch system) makes a handset and gets a one-time code, its
link and the link's QR code:

```
POST BASE/api/handsets            {"name": "Truck 12", "url": "https://192.168.1.5:29203/voice"}
→ {"handset": {…}, "code": "K7MPQ-2XHWD", "expires_ms": 1764230400000,
   "link": "https://192.168.1.5:29203/voice/api/enrol?code=K7MPQ2XHWD&fp=3F1A…(64 hex)&link=192.168.1.5:29203&link_fp=9C04…(64 hex)",
   "qr": "<svg …>"}
```

`url` (optional) is the address to put in the link; else the first of the server's
configured addresses (`IDN_VOICE_URLS`), else the one the request came by. `fp` is left
out where the server has no certificate of its own (§3.2). `link` (`HOST:PORT`,
comma-separated) and `link_fp` (64 hex digits) are where IDN's link is and its
certificate's SHA-256 (§12), left out where it isn't offered. The code: 10 characters by
default (the site's `code_length`, 10–16) from `23456789ABCDEFGHJKLMNPQRSTUVWXYZ`,
valid 24 h (`code_hours`), once. `POST BASE/api/handsets/{id or name}/code` (body optional:
`{"url"}`) makes a new one for an existing handset; enrolling with it replaces the
handset's credential.

**The app enrols** (no sign-in; from the QR code: everything before `/api/enrol` is
`BASE`):

```
POST BASE/api/enrol   {"code": "k7mpq-2xhwd", "device": "Inrico B01 / Android 9"}
→ 200 {"handset": {"id": "h4kq2x7ma", "name": "Truck 12"},
       "credential": "idnv1.h4kq2x7ma.n3…",
       "urls": ["https://192.168.1.5:29203/voice", "https://voice.example.com/voice"],
       "cert_sha256": "3F:1A:…",
       "link": ["192.168.1.5:29203", "voice.example.com:29203"],
       "link_cert_sha256": "9C:04:…" }
→ 403 {"error": "wrong or expired code"}
→ 429 {"error": "too many tries; wait a minute"}   (5 wrong a minute per address, IPv6 by /64; 60 for the whole site)
→ 403 {"error": "handsets only over HTTPS …"}       (plain HTTP)
```

The code is typed in any case, with or without the dash. Keep the credential in the
device's protected storage (Android Keystore-backed); it is shown once. An earlier
credential of that handset stops working and its sessions end.

**Provisioned by the dispatch system** (no code): `POST BASE/api/handsets {"name": "Truck 12",
"issue": "credential"}` (the API key) → `{"handset": {"id", "name"}, "credential",
"urls", "cert_sha256", "link", "link_cert_sha256"}`; `POST BASE/api/handsets/{handset}/credential` gives an existing
handset a new one (the old one stops working at once).

### 3.2 Pinning the certificate

- `cert_sha256` (and the QR code's `fp`, the same 64 hex digits without colons) is the
  SHA-256 of the DER of the **leaf certificate** the Voice server presents. Check it on
  every TLS connection to that server, from the enrolment request itself on: compute
  SHA-256 over the server's first certificate and compare all 32 bytes; accept that
  certificate whatever its name or issuer (IDN Media's is self-signed, on an address);
  refuse anything else, and say "the site's certificate changed: enrol again".
- IDN Industrial's Voice port (29203) has its own self-signed certificate (made once,
  kept in the Site Manager's data, valid to 2046): pin it as above; its SHA-256 is in
  the Site Manager's log and the enrolment answer. Enrolled through the tunnel, the
  answer's `cert_sha256` is still that port's: pin it for the `urls` on the plant
  network, and check the tunnel's own (public authority) certificate the ordinary way.
- `cert_sha256: null` (a server with no certificate of its own, e.g. HTTPS turned off
  behind a tunnel): check certificates the ordinary way.
- **IDN's link** has its own certificate: pin `link_cert_sha256` (the QR code's
  `link_fp`) the same way on every QUIC connection. It is Ed25519 (Voice's link key,
  made once and carried by the site's backup): allow that signature scheme in your TLS
  stack. `hello`'s `link.quic.cert_sha256` repeats it (over the pinned connection), so
  an app enrolled before QUIC was offered learns it there.
- **When it changes**: today the certificate is made once (valid to 2046) and never
  renewed; it changes only if the Site Manager's files are lost (a site backup carries
  them). A changed certificate means enrolling again (a new code, or a new credential
  from the dispatch system), which pins the new one. There is no signed "certificate changed"
  message.

## 4. The control WebSocket

`GET BASE/ws` with the sign-in (§2); `?resume=TOKEN&have=…` for a handset coming back
(§6). JSON text messages each way, each an object with `t` (its type); sound as binary
messages (§7). A message to the server is 64 KiB at most; one from it, 4 MiB at most
(`history`, `missed`, a large site's `presence` or `hello` pass 64 KiB: allow for
them). The server never drops a JSON message; it sends waiting ones between sound
frames. A client 10,000 messages behind (not reading) is closed with `bye` "too far
behind"; one that takes nothing for `link.idle_timeout_s` (30 s) is closed with no
`bye` (it couldn't read one). A handset resumes after either (§6).

### 4.1 Keep-alive

Send `{"t": "ping"}` every `link.ping_s` (10 s) from `hello`; the server answers
`{"t": "pong"}`. A session that sends nothing (text or sound) for `link.idle_timeout_s`
(30 s) is ended (a handset's waits for its resume, §6). Signing off on purpose: send
`{"t": "bye"}`, then close.

### 4.2 Server → client

**`hello`** (first, and again when a handset's driver changes or it resumes):

```json
{"t": "hello", "sid": 17,
 "me": {"name": "Dan Smith", "user": true, "admin": false, "dispatch": false,
        "roles": ["driver"], "handset": {"id": "h4kq2x7ma", "name": "Truck 12"},
        "driver": {"id": "D1042", "name": "Dan Smith"}},
 "channels": [ {CHANNEL}, … ],
 "joined": [{"channel": 3, "may_talk": true}],
 "users": ["ann", "Dan Smith", "Handset Truck 7"],
 "settings": {"instant_from": "anyone"},
 "slots": 6,
 "link": {"ping_s": 10, "idle_timeout_s": 30, "resume_s": 120,
          "jitter_start_ms": 200, "jitter_max_ms": 2000,
          "link_connect_ms": 3000, "link_retry_s": 60, "link_keepalive_ms": 5000,
          "quic": {"port": 29203, "addrs": ["voice.example.com:29203"],
                   "cert_sha256": "9C:04:…"}},
 "media": "websocket",
 "resume": "q3…", "resumed": false, "backlog": 0}
```

`me.key`: who you are for calls and direct messages (a site user's name; a handset's
`driver:…` or `handset:…`). `media`: `websocket` for a handset (sound in the
connection's own messages, §7: on the WebSocket or IDN's link, never WebRTC), `any`
otherwise. `link.quic`: where IDN's link is (§12), `null` where it isn't offered; try
the host you reached the server by at `port` first, then `addrs`. `resume`, `resumed` (and `backlog` when resumed): handsets
only; keep the newest `resume` of every `hello`. A second `hello`
replaces what the client knew of itself (identity, rights, channel list, `joined`).
`users` is empty for a guest. A handset without a driver is `"Handset NAME"` with the
role `handset`.

**CHANNEL** (in `hello` and `channels`; only those the session may enter):

```json
{"id": 3, "name": "Dispatch", "description": "",
 "kind": "walkie", "half_duplex": true, "limit_s": 60,
 "access": "auth", "allow": [{"role": "dispatcher", "talk": true}],
 "record": true, "as_source": false,
 "jitter_start_ms": 200, "jitter_max_ms": 2000,
 "may_talk": true, "needs_password": false}
```

`kind`: `walkie` (`half_duplex`, `limit_s`: a talk's limit in half duplex, 0 none) or
`iem` (`source`; listen-only, stereo). `access`: `guest`, `open`, `password`, `auth`
(`allow`: `user` or `role`, `talk`). Optional: `talk_source`, `store_forward_s`.
`jitter_*_ms`: what to use in this channel (its own, else the site's).
`needs_password`: send `password` with `join`.

| Message | Fields | When |
|---|---|---|
| `channels` | `channels` [CHANNEL] | the list changed |
| `presence` | `users` [site users' names], `handsets` [{`id`, `name`, `key`, `driver` {`id`, `name`} or null, `state` `on`/`reconnecting`}] | someone came or went (signed-in sessions only) |
| `joined` | `channel`, `may_talk` | joined (also for a driver's assigned channels) |
| `left` | `channel`, `why`: `left`, `access changed`, `channel removed`, `your role changed`, `driver changed` | out of a channel |
| `rejoined` | `channels` [id] | a fresh handset connection took over its earlier session's channels |
| `members` | `channel`, `members` [name] | a channel's members changed (each tick, 250 ms) |
| `talkers` | `channel`, `talkers` [{`sid`, `name`}] | who talks there |
| `talking` | `on`, `why` (off): `released`, `replaced`, `time limit`, `too long`, `no sound`, `taken`, `left`, `access changed`, `your role changed`, `driver changed`, `link lost`, `a new recording` | your own talk started or ended |
| `busy` | `channel`, `by` | half duplex: someone has the floor (you weren't sent) |
| `taken` | `channel`, `by` | dispatch took your floor (your talk ended) |
| `slot` | `slot`, `talker` {`sid`, `name`, `channel`, `iem`} or null | WebRTC: what a slot carries |
| `instant` | `from` (their key: answer with `talk {user: from}`), `name`, `on` | someone talks to you directly |
| `ring` | `call`, `from` (key), `name` | a call for you |
| `call` | `call`, `state` (`ringing`, `answered`, `answered elsewhere`, `ended`), `with` (the other's key), (ended) `why` (`hung up`, `declined`, `no answer`, `link lost`, `driver changed`), `missed` | a call's state |
| `message` | `message` {MESSAGE} | a text message for you |
| `history` | `to` {`channel`, `user`}, `messages` [MESSAGE] | answer to `history` (the newest 100) |
| `missed` | `transmissions` [TRANSMISSION] | answer to `missed` (200 at most) |
| `replay_end` | `id` | a replay's last frame was sent |
| `settings` | `settings` {`instant_from`} | answer to `settings` |
| `users` | `all` [name] | the site's users (signed-in sessions) |
| `rtc_answer` | `sdp` | WebRTC answer (§8) |
| `sos` | `sos` {SOS}, `realert` (true when sent again) | dispatch: an SOS (§9) |
| `sos_sent` | `id` | the handset's SOS reached the server |
| `sos_ack` | `id`, `by`, `time_ms` | an SOS was acknowledged |
| `error` | `what` (the command), `error` | a command refused |
| `bye` | `why` (e.g. `this handset was revoked`, `this handset connected again`, `resumed on another connection`, `your account was removed`, `too far behind`, `QUIC is off on this site`, `not an enrolled handset`) | the server ends the session |
| `pong` | | answer to `ping` |

**MESSAGE**: `{"seq", "conv": "c:3" (a channel) or "d:A\nB" (two people's keys,
sorted), "from", "text", "time_ms", "handset", "from_id"}` (`handset` and `from_id` only
when a handset sent it: `from` is then its driver's name, `from_id` its key).
**TRANSMISSION**: `{"id", "channel" (or none: a call or instant talk), "to" [keys],
"from", "from_id", "start_ms", "end_ms", "frames", "handset"}`.

### 4.3 Client → server

| Message | Fields | Answer |
|---|---|---|
| `ping` | | `pong` |
| `join` | `channel`, `password` (password channels; five wrong a minute per address) | `members`, `joined`, `talkers`; or `error` (`wrong password`, `not allowed in this channel`, `the channel is full`, `no such channel`) |
| `leave` | `channel` | `left` |
| `talk` | `on`: true with one target: `channel` (id), `user` (a key: instant talk), `all`: true (dispatch: every channel you may talk in), `call` (id) | `talking`, or `busy`, or `error` |
| `talk` | `on`: false | `talking` (`released`) |
| `call` | `user` (a key) | `call` (`ringing`); the callee gets `ring` |
| `answer` | `call`, `accept` (default true) | `call` (`answered` / `ended`) |
| `hangup` | `call` | `call` (`ended`) |
| `message` | `channel` or `user` (a key), `text` (1–2,000 characters; 30 a minute) | `message` to everyone it is for; or `error` |
| `history` | `channel` or `user` (a key) | `history` |
| `missed` | `since_ms` | `missed` (transmissions you may hear, not your own) |
| `replay` | `id` | binary frames kind 2 in real time, then `replay_end`; one at a time |
| `settings` | `settings` {`instant_from`: `anyone`, `dispatch`, `never`} | `settings` |
| `users` | | `users` |
| `rtc_offer` | `sdp` | `rtc_answer` or `error` (`what`: `rtc`) |
| `rtc_off` | | sound comes over the WebSocket again |
| `sos` | `position` {`lat`, `lon`} (optional, degrees), `text` (optional, 500 characters) | `sos_sent`; handsets only |
| `sos_ack` | `id` | `sos_ack` to dispatch and the handset; dispatch only |
| `bye` | | the session ends now (a handset's doesn't wait for a resume) |

Examples:

```json
{"t": "join", "channel": 3}
{"t": "talk", "on": true, "channel": 3}
{"t": "talk", "on": false}
{"t": "message", "channel": 3, "text": "Loaded, leaving the plant"}
{"t": "missed", "since_ms": 1764140000000}
{"t": "sos", "position": {"lat": 53.3498, "lon": -6.2603}, "text": "slid off the road"}
```

## 5. Drivers (the dispatch system)

A handset's identity is its driver's, assigned by the dispatch system per handset (by its id or
name). HTTP (`X-IDN-Voice` or `Authorization`; an administrator, Industrial's API key):

```
POST BASE/api/driver
{"handset": "Truck 12",
 "driver": {"id": "D1042", "name": "Dan Smith", "roles": ["driver"],
            "channels": ["Dispatch", "Yard"]}}
→ {"handset": {…}, "channels": ["Dispatch", "Yard"], "past_gates": ["Dispatch"],
   "unknown_channels": []}

POST BASE/api/driver   {"handset": "Truck 12", "driver": null}      (off shift)
```

The same body on the site's MQTT, topic `idn/site/voice/cmd/driver`; the answer comes on
`idn/site/voice/events` as `{"event": "driver_result", "handset", "ok", "result" or
"error", "time_ms"}`. Driver: `id` 1–128 characters, `name` 1–64, at most 32 `roles` and
100 `channels` (names). A driver assigned to a second handset leaves the first.

What the handset sees, at once if connected: its talk and calls end; `left` (`driver
changed`) for every channel the previous assignment put it in and those the driver may
not be in; a new `hello` (the driver's name, key, roles, `driver`, a new `resume`);
`joined` for each assigned channel, entered past its password or Auth list (those
channels alone). Cleared: `left` for every channel the assignment put it in (Open ones
too), `hello` as "Handset NAME". A channel renamed, made or removed is looked up again
in the assignment (left or joined to match). Messages, recordings and channel events carry the driver in `from` and
the handset in `handset`.

## 6. Dropped links: resume, store-and-forward, missed

- When a handset's connection ends without `bye` (an error, the idle timeout, a close),
  its session **waits** `link.resume_s` (120 s): still a member of its channels, shown
  `reconnecting` in presence; its talk ends (its floor is freed) and calls end.
- **Resume**: connect again with the credential and
  `?resume=TOKEN&have=TALKER:SEQ,TALKER:SEQ` — `TOKEN` the newest `resume` you got in a
  `hello`, `have` the last `seq` you received from each talker (the binary frames'
  talker and seq fields, §7). The answer is a `hello` with `resumed: true`, `joined` (no
  need to join again), `backlog` (sound frames to send) and a **new** token; then
  `members` and `talkers` of each channel; then the backlog's binary frames, then live
  sound. A wrong or expired token gives a fresh session (`resumed: false`; start your
  `have` counts again). The token you resumed with keeps working until you use a newer
  one, so a lost `hello` costs nothing.
- **Why `have`**: a link that dies without a word (cellular) is noticed only at the idle
  timeout, and what was written into it meanwhile never arrived. The server keeps each
  frame it sent a handset for its window and sends again every one past your `have`.
  Without `have` it assumes you got everything it sent.
- **The backlog** is that, plus the sound that came while the link was down (each frame
  kept for the site's `store_forward_s`, 30 s, or the channel's own; older ones were
  dropped and are never sent). It is sent as fast as the link takes, with replies and
  events between frames. Play it straight away, in order, at normal
  speed — let the jitter buffer grow to hold it — then catch up to live by playing a
  little faster (up to about 1.25×) or skipping silent frames until the buffer is back
  at its target. Don't pace it by its original timing.
- **Past the window**, or for what is older than the backlog: `missed` with the time of
  the last sound heard, then `replay` each (kind 2 frames, paced in real time).
- **A fresh connection** (no token: the app restarted) takes over the handset's earlier
  session: that one gets `bye` (`this handset connected again`), and its channels are
  joined again for the new one (`rejoined`). One session per handset.
- **Reconnect** at once after a drop, then back off (1, 2, 4… up to 10 s), always with
  the latest token while within `resume_s`: QUIC first, then the WebSocket (§12). The
  token and `have` work across transports.

## 7. Sound over the WebSocket or IDN's link (handsets: always)

Handsets send and receive sound in the connection's own messages: binary messages on
the WebSocket, kind 1 frames on IDN's link (§12), the same bytes. Both are reliable:
nothing is lost; a slow link gets it late.

- **Codec**: Opus, 48 kHz, 20 ms frames (960 samples), mono, 24 kbit/s; in-band FEC on
  if the encoder has it (Opus codes it only when told to expect about 10 % loss or more;
  the client library's `fec_loss_pct`, default 10). IEM channels (kind 3) are stereo (96 kbit/s): decode with two
  channels. The server never decodes: it forwards each packet unchanged.
- **Sending** (while `talking` is on): each Opus packet as one binary message (on the
  link, one kind 1 frame): one byte
  `1`, then the packet (at most 4,000 bytes). Send packets as they are encoded, one every
  20 ms. A talk that brings no packet for `talk_silence_s` (3 s; before its first,
  `talk_first_s`, 10 s) is ended by the server (`talking` off, `no sound`).
- **Receiving**: each binary message (on the link, each kind 1 frame's payload) is one
  frame:

| Bytes | Field |
|---|---|
| 0 | kind: 1 talk, 2 replay, 3 IEM |
| 1–4 | talker: the talking session's `sid` (u32, little-endian; 0 for a replay) |
| 5–8 | channel id (u32 LE; 0 for a call or instant talk) |
| 9–12 | seq (u32 LE): the talker's packet number, one more each packet |
| 13– | the Opus packet |

  Decode one stream per (talker, channel) (and one for replay), mix them yourself, each
  channel at its own volume. A gap in `seq` is a packet the server didn't get (or
  dropped for a link too slow for `ws_queue_s`): use the decoder's loss concealment.
- **Jitter buffer**: adaptive. Start at the channel's `jitter_start_ms` (200 ms by
  default), grow after a stall (an underrun) up to `jitter_max_ms` (2000 ms), and shrink
  back slowly (a few ms a second, by dropping silence) while the link is steady.
- **A slow link**: the server keeps each frame for the site's `ws_queue_s` (10 s) and
  drops the oldest beyond; replies and events are never held up by sound.

## 8. WebRTC (optional; the LAN and the web client)

- One `RTCPeerConnection`, no ICE servers. Add one `sendrecv` audio transceiver first
  (your microphone; silent unless talking) and then 6 `recvonly` audio transceivers (the
  **talker slots**, `hello.slots`). Wait for ICE gathering to finish (no trickle), then
  send `{"t": "rtc_offer", "sdp"}`; set the `rtc_answer`'s `sdp` as the remote
  description.
- The server is ICE-lite with one host candidate: UDP 29207 (`IDN_VOICE_UDP`) on the
  address you reached it by (or `IDN_VOICE_PUBLIC`); DTLS-SRTP; Opus only. 500 WebRTC
  clients at most; one not connected in 20 s is dropped.
- When connected, sound comes on the slots; `slot` messages say which talker and
  channel each carries (null: free). More talkers than slots aren't heard until a slot
  frees. If it doesn't connect in about 5 s, send `{"t": "rtc_off"}` and use the
  WebSocket (§7). Your talk goes on the microphone track while `talking` is on.

## 9. SOS

From a handset: `{"t": "sos", "position": {"lat", "lon"}, "text"}` → `{"t": "sos_sent",
"id"}`. Dispatch gets:

```json
{"t": "sos", "sos": {"id": 4, "handset": {"id": "h4kq2x7ma", "name": "Truck 12"},
  "from": "Dan Smith", "driver": "D1042", "first_ms": 1764141000000,
  "time_ms": 1764141000000, "position": {"lat": 53.3498, "lon": -6.2603},
  "text": "slid off the road", "count": 1}}
```

Presses within `sos_merge_s` (5 s) are the same alert (`sos_sent` with the same id; not
sent to dispatch again). Later presses while unacknowledged send it again (`count` up);
unacknowledged, dispatch gets it again every `sos_realert_s` (30 s) with `realert: true`,
and every dispatcher who connects gets it. Acknowledged (`{"t": "sos_ack", "id"}` from a
dispatcher; `POST BASE/api/sos/{id}/ack`; MQTT `idn/site/voice/cmd/sos_ack` `{"id",
"by"}`): dispatch and the handset get `{"t": "sos_ack", "id", "by", "time_ms"}`. `GET
BASE/api/sos` lists open alerts.

## 10. The site's program: HTTP and MQTT

Administrators (Media) or the API key (Industrial):

| Method, path | Body | Answer |
|---|---|---|
| `GET /api/handsets` | | [{`id`, `name`, `device`, `created_ms`, `enrolled_ms`, `last_seen_ms`, `revoked`, `enrolled`, `code_expires_ms`, `driver`, `connected`}] |
| `POST /api/handsets` | `name`, `issue` (`code` default, `credential`), `url` | code: `handset`, `code`, `expires_ms`, `link`, `qr`; credential: `handset`, `credential`, `urls`, `cert_sha256` |
| `PUT /api/handsets/{handset}` | `name` | the handset |
| `DELETE /api/handsets/{handset}` | | `{}` (its sessions end) |
| `POST /api/handsets/{handset}/code` | `url` (optional) | as `issue: code` |
| `POST /api/handsets/{handset}/credential` | | as `issue: credential` |
| `POST /api/handsets/{handset}/revoke` | | the handset (its sessions end) |
| `POST /api/driver` | `handset`, `driver` or null | §5 |
| `GET /api/sos`, `POST /api/sos/{id}/ack` | | §9 |
| `GET /api/settings`, `POST /api/settings` | the fields to change | §11 |
| `GET /api/status` | | sessions, `users`, talking, calls, channels, `handsets`, `frames_dropped`, `frame_bytes`, `budget_dropped`, `sos` |
| `GET /api/presence` | | the same |
| `POST /api/messages` | `channel` (name or id), `text`, `from` | the message |
| `POST /api/links` | `user` | a one-time console sign-in link |
| `GET/POST /api/channels`, `DELETE /api/channels/{id}` | a channel | channels (with `store_forward_s`, `jitter_start_ms`, `jitter_max_ms`: a number sets, `null` unsets, left out keeps) |

`{handset}` is its id or its name (URL-encoded). Every change is audited.

MQTT on the site's broker (`idn/site/voice/…`):

| Topic | |
|---|---|
| `presence` (retained) | `{"users", "handsets", "time_ms"}` |
| `messages/CHANNEL` | each MESSAGE in a channel |
| `events` | `talk_start`/`talk_end` (`from`, `handset`, `target`, `why`, `time_ms`), `driver_result`, `sos`, `sos_realert`, `sos_ack` |
| `cmd/message` (publish) | `{"channel", "text", "from"}` |
| `cmd/driver` (publish) | §5 |
| `cmd/sos_ack` (publish) | `{"id", "by"}` |

Any login on the site's broker may publish `cmd/…`: give logins only to trusted programs.

## 11. Settings

`POST BASE/api/settings` with only the fields to change (each checked; 400 outside its
range or for a name that isn't a setting); `GET` gives `{"settings", "users", "webrtc",
"quic"}` (`quic`: where IDN's link is offered, as in `hello`, or `null`).

| Setting | Default | Range | |
|---|---|---|---|
| `keep_days` | 30 | | recordings and messages kept (0: until deleted) |
| `dispatch` | [] | | users and roles with dispatch (`[{"user"} or {"role"}]`) |
| `resume_s` | 120 | 0–3600 | a dropped handset's session waits (0: not at all) |
| `store_forward` | true | | keep sound for a handset whose link is down |
| `store_forward_s` | 30 | 0–300 | ... this long (a channel's own wins) |
| `ws_queue_s` | 10 | 1–60 | sound kept for a slow WebSocket link (a handset's: the longer of this and `store_forward_s`, and kept after sending) |
| `frame_budget_mb` | 64 | 8–4096 | sound queued for all sessions together; past it the oldest go |
| `code_hours` | 24 | 1–720 | an enrolment code's life |
| `code_length` | 10 | 10–16 | an enrolment code's characters |
| `enrol_per_minute` | 5 | 1–100 | wrong codes a minute per address (IPv6: per /64) |
| `enrol_all_per_minute` | 60 | 1–10000 | ... for the whole site |
| `plain_handsets` | false | | accept handset credentials, enrolment and provisioning over plain HTTP |
| `sos_merge_s` | 5 | 0–60 | SOS presses this close are one |
| `sos_realert_s` | 30 | 0–3600 | an open SOS sent to dispatch again (0: never) |
| `talk_silence_s` | 3 | 1–60 | a talk without sound ends |
| `talk_first_s` | 10 | 1–120 | ... before its first packet |
| `jitter_start_ms` | 200 | 20–5000 | clients' jitter buffer to start |
| `jitter_max_ms` | 2000 | start–20000 | ... and at most |
| `idle_timeout_s` | 30 | 20–600 | a silent session ends |
| `ping_s` | 10 | 2 – half the idle timeout | how often clients ping |
| `link` | true | | IDN's link (QUIC, §12) offered where the Site Manager runs its listener |
| `link_connect_ms` | 3000 | 500–30000 | give QUIC this long to connect, then use the WebSocket |
| `link_retry_s` | 60 | 5–3600 | on the WebSocket, try QUIC again this often |
| `link_keepalive_ms` | 5000 | 1000–60000, below the idle timeout | QUIC keep-alive (its idle timeout is `idle_timeout_s`) |

Clients take `ping_s`, `idle_timeout_s`, `resume_s`, the jitter values and the `link_*`
values from `hello` (`link`, and each channel's own), not from constants.

## 12. IDN's link (QUIC): the handsets' first choice

Voice over IDN's link (spec §V.14): QUIC (TLS 1.3) with **connection migration**, so a
phone moving between Wi-Fi and cellular, or a truck changing tower, keeps its
connection with nothing to resume. The same session as the WebSocket's, in the same
messages.

**Connecting.** UDP to each `HOST:PORT` in turn: the host you reached the server by at
`link.quic.port`, then `link.quic.addrs` (at enrolment: the answer's `link`). ALPN
`idn-voice/1`; any server name (the certificate is checked by its pin, not its name);
no client certificate. Check the server's leaf certificate against `link_cert_sha256`
(§3.2) and refuse anything else. Keep-alive every `link.link_keepalive_ms` (5000); idle
timeout `link.idle_timeout_s`. No 0-RTT. Don't use QUIC datagrams.

**One stream.** Open one bidirectional stream. Each direction is a sequence of frames:

| Bytes | Field |
|---|---|
| 0–3 | length (u32, little-endian) of what follows: the kind byte and the payload |
| 4 | kind: 0 JSON, 1 sound |
| 5– | payload: kind 0, one JSON message as on the WebSocket (UTF-8); kind 1, one sound message as the WebSocket's binary one (§7) |

A frame (kind and payload) to the server is 1 byte to 64 KiB; from the server, up to
4 MiB (`history`, `missed`, a large site's `presence` or `hello` pass 64 KiB). Longer
or empty ends the connection (CONNECTION_CLOSE code 2). Unknown kinds are skipped. The
server takes one stream, and only two frames' worth ahead of its reading on it: a
client that writes faster waits (QUIC flow control). A client that stops reading has
its connection closed (code 0, the reason `too far behind` or `not reading`): resume.

**Signing in.** Open the stream and send the first frame within 10 s of connecting
(one deadline for both):

```json
{"t": "auth", "credential": "idnv1.h4kq2x7ma.n3…", "resume": "q3…", "have": "17:204,23:9"}
```

`resume` and `have` as on the WebSocket (§6), only when coming back. The site's program
may give its Bearer token (Industrial's API key) as `credential`; no cookies, no guests.
The answer is `hello` (§4.2), then everything as on the WebSocket; or
`{"t": "error", "what": "auth", "error": "…"}`: close the connection then (the server
closes it with code 1 after 2 s). Errors: `not an enrolled handset (revoked, or enrolled
again)`, `not a credential of this site`, `the first frame is auth`, `QUIC is off on
this site: use the WebSocket`. A server with many connections not yet signed in (64,
or 8 from your address) refuses more for a moment: retry, or use the WebSocket.

**Fallback and retry.**

1. Try QUIC first, giving it `link.link_connect_ms` (3000 ms; before the first `hello`,
   3000) across the addresses.
2. Not connected (UDP blocked, a carrier dropping it, `quic: null`, an auth error saying
   it's off): use the WebSocket (§4) as before.
3. On the WebSocket, try QUIC again every `link.link_retry_s` (60 s) in the background.
   When it connects, sign in with the newest resume token and your `have`: the session
   moves to QUIC (`hello` `resumed: true`) and the server ends the WebSocket (`bye`
   "resumed on another connection"); then close the WebSocket.
4. QUIC that dies (the idle timeout, a phone that slept): reconnect at once with resume
   and `have`, QUIC first, then the WebSocket (§6's back-off).
5. The site turns `link` off: QUIC sessions end with `bye` "QUIC is off on this site";
   resume on the WebSocket at once (and stop retrying QUIC until `hello` offers it
   again).

On a `bye`, close the connection (the server waits up to 2 s for you to, so the `bye`
isn't lost under its close). Signing off: send `{"t": "bye"}` and close.

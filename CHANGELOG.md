# Changelog

Notable changes to the `idn-open-protocol` library and the programs built on it, for
people who integrate against the crate. Newest first.

## How to read this, and what to depend on

- **Versioning.** The crate is pre-1.0 (`0.y.z`). Under Cargo's rules a `0.y` → `0.(y+1)`
  bump may change the public API; a `0.y.z` → `0.y.(z+1)` bump does not. We follow that:
  breaking changes to the library API land on a minor bump and are listed here.
- **Pin a released tag.** Integrators should depend on a tagged release, not `main`.
  The current one is **`v0.3.6-Beta17`** (a pre-release). `main` moves between releases
  and may break at any commit; tags do not move. In `Cargo.toml`:

  ```toml
  idn-open-protocol = { git = "https://github.com/ADC-Industries/idn-open-protocol-dev", tag = "v0.3.6-Beta17" }
  ```

- **Structs you only read are `#[non_exhaustive]`.** Returned/monitoring types
  (`api::OutputStats`, `api::TappedFrame`, the `api::SendError` enum) carry
  `#[non_exhaustive]`, so new fields/variants can be added without breaking you — provided
  you match with a trailing `..` and never destructure them exhaustively.
- **Structs you build take `..Default::default()` or a constructor.** `PublisherConfig`
  implements `Default`; `OutputConfig` starts from `OutputConfig::new(name)`. Build them
  that way, not with an exhaustive struct literal, so a later added field doesn't break you:

  ```rust
  let cfg = PublisherConfig { groups: vec!["stage".into()], ..Default::default() };
  let mut out = OutputConfig::new("Program");
  out.frame_rate = (30000, 1001);
  ```

## [Unreleased]

## [v0.3.8-Beta21] — 2026-11-27 (pre-release, with installers)

Crates 0.3.8. The first public release with installers (Windows: IDN Media and IDN
Industrial; Linux: `.deb` for Debian 12, Ubuntu 24.04 and 26.04). Lab-checked on Linux;
the Windows installers are not yet checked on a Windows PC.

### Fixed
- Idle CPU (machines doing nothing should stay near 0%):
  - mDNS announcers no longer spin a core.
  - The Node's status no longer rebuilds, with device scans, four times a second; the
    drivers and virtual screens are read again every 30 s or after a change.
  - The desk's soundcheck reader sleeps until the desk takes a block instead of spinning.
  - Processes and connections that keep failing are tried again after growing waits (up
    to a minute) rather than every few seconds: display screens, the switcher's
    displays, streams and MQTT broker, the desk's plugin host, the Industrial session
    helper; the Node's IPC listener up to 2 s; IDN Data's timed-out probes up to 10
    minutes. A lost WASAPI device
    and an `idn audio` capture stay at 2–3 s, so a replugged interface is back quickly.
  - The virtual camera receives and decodes the source only while an app has it open.
  - The Industrial inbox is read again from the start only when a session subscribes, as
    a fallback every 5 minutes while a persistent session exists, else hourly (to let go
    of items past the hold), whatever the traffic.
  - A QUIC input's thread ends when its endpoint has ended instead of polling it.
  - Same-machine transports' polling wait (outside Linux) backs off from 1 ms to 4 ms
    while nothing comes.
  - The display service lists screens less often while none are found (10 s up to 30 s),
    and at once after a command reads the screens or changes a screen's settings or feed.
- The Node makes its media folder at start, so `media_send` works on a Node run by hand.
- Captions' "CPU (N threads)" label shows the threads a pass uses, not the machine's
  count.

### Changed
- The Site Manager's and the Node's pushed `state`: the live figures (uptime, CPU, clock
  offset, `last_seen`) no longer make a new version as they tick; they refresh about
  every 10 s. `GET /api/v1/state` returns the current ones.
- Switcher: `version()`/`wait_change()` unchanged (they move on timer seconds too); the
  new `state_version()`/`wait_state_change()` count only real changes, and the switcher's
  own watchers (saved mixes, pending shots, setups, MQTT) use them.

## [v0.3.8-Beta18] — 2026-11-27 (pre-release, source only)

Crates 0.3.8. The first release published in the public repository
(github.com/ADC-Industries/idn-open-protocol); development continues in a private one,
whose history is not published.

### Added
- **IDN Voice over IDN's link** (idn-voice.md §V.14; the client protocol in
  `docs/VOICE-CLIENT.md` §12): handsets connect over QUIC with connection migration, so
  a phone moving between Wi-Fi and cellular keeps its connection; the WebSocket stays as
  the fallback. ALPN `idn-voice/1`, mode open, one stream of length-prefixed frames
  carrying the WebSocket's JSON and sound messages; the first frame is `auth` (the
  handset's credential, `resume`, `have`). The same session as the WebSocket's (the
  code is now shared, `session.rs`): resume and `have` work across both. IDN Site
  Managers listen on UDP 29203 (the Voice HTTPS port's number; Industrial at
  `IDN_VOICE_HTTPS`'s address, `off` turning both off) with a link key of Voice's own
  (`voice/link.key`, carried by the site backup); the Docker images and compose files
  publish `29203/udp`; enrolment and provisioning answers and the QR link
  carry `link` (`HOST:PORT`s) and `link_cert_sha256` to pin; `hello`'s `link.quic` says
  where it is. New site settings: `link` (on), `link_connect_ms` (3000),
  `link_retry_s` (60), `link_keepalive_ms` (5000). API: `Voice::serve_link`,
  `Voice::serve_link_connection`, `Voice::offer_link`; in the library
  `link::ServiceListener` (services over the link for clients that hold no IDN key).
  Unauthenticated connections are counted until they sign in (64 at once, 8 per
  address; `ServiceListener::bind`'s `on_accept` gets a `ServicePending` to drop then)
  and their flow control is bounded (`ServiceTransport`'s `streams`, `stream_window`,
  `receive_window`; Voice: one stream, 2 × 64 KiB, 256 KiB); the stream and `auth`
  share one 10 s deadline. Frames to the server 64 KiB at most, from it 4 MiB (the
  server ends a session rather than write a longer one). Turning `link` off ends the
  sessions on it (`bye` "QUIC is off on this site"; clients resume on the WebSocket).
  The listener holds Voice weakly (Industrial: no reference cycle, Voice's store closed
  at exit); two listeners on one Voice each clear only their own offer.
- **IDN Voice's client library**, `core/idn-voice-client` (roadmap R.21): the client
  protocol (`docs/VOICE-CLIENT.md`) as a library for native apps, first an
  Android app on the Inrico B01. QUIC on IDN's link first, the WebSocket as the fallback
  and the move back (learning the link from `hello`, over a pinned connection only, when
  enrolled before it), resume
  with the newest token and `have` after a drop, Opus at 24 kbit/s with FEC in 20 ms
  frames sent only while talking, a jitter buffer per talker (from `hello`'s and each
  channel's jitter settings, growing after a stall, catching up on a backlog) with
  concealment, mixed to 48 kHz mono. A talk through a dead zone loses nothing: the
  microphone's packets are held while the link is down (`hold_mic_s`, 30 s, the oldest
  dropped past it and counted) and kept until a `pong` confirms them, then sent after the
  resume, the talk started again first, at 4× real time (listeners hear a late burst).
  In-band FEC is planned for 10 % loss (`fec_loss_pct`): at the 5 % first set, Opus coded
  none, and a dropped frame was only concealed.
  Rust API `Client` (`connect`, `join`, `talk_start`,
  `push_mic_i16`, `pull_speaker_i16`, `network_changed`, `close`, …) and a C ABI
  (`include/idn_voice_client.h`: `idn_voice_connect` with a JSON configuration and a JSON
  event callback, `idn_voice_talk`, `idn_voice_push_mic_i16`,
  `idn_voice_pull_speaker_i16`, …) for JNI. Its own two-thread runtime; nothing
  desktop-only, so it builds for `aarch64-linux-android` with the NDK (`cargo ndk … rustc
  --crate-type cdylib`; the crate itself is an `rlib`).
  Its bug run (before release): a silent link is noticed at `idle_timeout_s` (the timer
  restarted every loop); a QUIC dead zone the connection survives no longer loses the
  packets the server dropped after "no sound"; talks are numbered, so a restart starts
  the talk its packets belong to, once, and a late end of an older talk leaves a newer
  one; the app's other commands are kept until confirmed and sent again after a drop;
  stale network changes no longer end later connections; `busy` is a `talk` event; a
  connection shorter than 5 s keeps the back-off; `have` capped at 256 talkers; the
  jitter buffer jumps gaps a reliable link never fills, takes a replay counting from 0,
  and compares `seq` as serial numbers; 32 streams at most; `IDN_VOICE_CODEC`; `send_json`
  refuses `bye` and a `talk` with no target.

### Fixed
- **Captions take no CPU from frames** (an integrator's bug run): whisper.cpp starts its
  own threads each pass, and on Windows new threads don't inherit the caller's thread
  priority (ggml leaves them at NORMAL). whisper now runs in a worker process below
  normal priority by default (Windows: the below-normal priority class; Unix: nice 10);
  everything else of `idn_captions::Engine` stays in its program. New
  `EngineConfig::priority` (`Priority::Low` by default, `Lowest`: the idle class / nice
  19, `Normal`: in-process as before), `EngineConfig::worker` (the program to run; unset:
  the switch `IDN_CAPTIONS_WORKER` (`off`: none), else `idn` next to the program, then
  each `idn` on `PATH`) and `EngineConfig::worker_timeout` (120 s); `Engine::in_worker`
  and `Engine::priority` say where it runs. `idn_captions::worker::serve_if_asked()` makes
  a program a worker (`idn` calls it; an embedder can call it in its own `main` and set
  `worker` to its own program). A worker says hello first (protocol 1); a program that
  doesn't within 5 s is passed over for the next (GNU libidn's `idn` on `PATH`). whisper
  runs in-process only when no worker program starts and says hello (logged once a
  process). A slow pass is skipped and the worker kept; a worker that ends is started
  again after 1 s, 5 s, 30 s, then every 60 s (on the CPU after one that ended loading the
  model on the GPU), steps failing meanwhile, never in-process; a model the worker can't
  load is `Engine::new`'s error. Dropping the engine never waits: the worker is ended on a
  thread of its own. "auto" asks the worker for its device and measures there, under the
  machine's load at the time (delete `auto.txt` / `auto-multilingual.txt` to measure
  again). `idn captions` says a repeated step error once. Measuring a model no longer uses
  whisper-rs's `set_abort_callback_safe`, whose trampoline reads its closure as the wrong
  type (whisper-rs 0.16.0): the stop flag it saw was whatever lay there.
- **IDN Voice: a client that stops reading** held its session's writer for good, its
  JSON queue growing without end (the 10,000-behind check never ran while a write
  waited). A write that waits for the site's `idle_timeout_s`, or with 10,000 messages
  behind it, now ends the session (logged; a handset resumes). Server messages are held
  to 4 MiB on the WebSocket too.
- **IDN Voice: the site program's sessions** (Industrial's API key, user `api`) were
  signed out within 5 s, as a user missing from the site's users. A session signed in
  with the program's credential is now marked (`Who::api`) and not followed; hosts say
  which credential is the program's with `Host::is_program` (default: none; Industrial:
  its API key). Media has no program sign-in to Voice, so nothing changes there.

## [v0.3.6-Beta17] — 2026-11-26 (pre-release, source only)

The crates are version **0.3.6**. No installers: Alpha88's are the newest. Coming from
Beta15, read Beta16's section too. This release removes IDN's own legacy
(nothing is deployed): "Changed (may break)" lists every change an
integrator could see, with what to use instead. The 23 largest files were split into
modules (public paths unchanged).

### Added
- **Network audio device settings changed from the Site Manager** (roadmap R.32 stage 2,
  idn-aoip-controller.md K.14). `POST network-devices/do` takes `set_latency`, `set_rate`,
  `set_encoding`, `set_aes67`, `set_preferred_leader`, `set_word_clock`, `identify`,
  `reboot`, `factory_reset` and `clear_config` (`value`; reboot and the resets need
  `confirm`, the device's current name) and answers with what the device reads back
  (`did`). `GET network-devices/settings?device` reads what a device has and offers (its
  rates, encodings, latency choices, AES67, clock, capabilities). Operators and up,
  audited, refused on a read-only site. **Each command is offered only once checked on
  real equipment** (lab 577–586; none yet): until then it is refused with "not checked on
  real equipment yet (lab N)". The Node's lab switch `IDN_NETAUDIO_UNCHECKED=1` offers
  them for the lab runs. A value is sent only from the device's own lists. After a change
  the device list reads the device again at once.
- **`POST network-devices/do` answers `{ok, did}` for every op**, stage 1's (subscribe,
  unsubscribe, the renames) included: `did` says what was done.
- **IDN Voice handsets and drivers** (idn-voice.md §V.13; the client protocol in
  `docs/VOICE-CLIENT.md`). Handsets (an Android app on Inrico B01 radios) enrol
  once with a one-time code and its QR (`POST /voice/api/handsets`, `POST
  /voice/api/enrol`), or are provisioned with a credential at once (`issue:
  "credential"`); they sign in with `Authorization: Bearer idnv1.…` in both editions,
  never as administrators; their answers carry the Voice certificate's SHA-256 to pin
  (`VoiceConfig::cert_sha256`; IDN Media: the panel's). List, rename, revoke (live
  sessions end), remove, new code or credential. The dispatch system puts drivers on handsets and
  into their channels past those channels' gates (`POST /voice/api/driver`, MQTT
  `idn/site/voice/cmd/driver`): a live handset becomes the driver at once. SOS from a
  handset to every dispatcher until acknowledged (`sos`, `sos_ack`; MQTT `events`). A
  handset whose link drops keeps its session for `resume_s` and resumes with its token
  (`?resume=`), getting the sound that came meanwhile (store-and-forward); a fresh
  connection takes over its earlier session. Every timing and limit is a Voice setting,
  `POST /voice/api/settings` changes only the fields given, and channels may set their
  own store-and-forward and jitter values. `hello` carries `link` (ping, idle timeout,
  resume, jitter) and `media`.
- **IDN Voice over a slow link**: a WebSocket listener's sound waits in a queue of its
  own (each frame up to `ws_queue_s`, 10 s; the oldest dropped past that, counted in
  `status`' `frames_dropped`), where the newest was dropped as soon as the session's 256
  queued items were full. A talk with no sound for `talk_silence_s` (3 s) ends, so a
  talker whose link dropped no longer holds a half-duplex floor until the session times
  out.
- **IDN Voice over HTTPS in IDN Industrial**: Voice has a TLS port of its own,
  `IDN_VOICE_HTTPS` (default `0.0.0.0:29203`, `off`), with a self-signed certificate made
  once in `<IDN_DATA>/voice/` (in the backup); its SHA-256 goes to handsets to pin. Over
  plain HTTP, handset credentials, enrolment and provisioning are refused (403) unless
  they come through the site's tunnel (a trusted proxy saying `X-Forwarded-Proto:
  https`) or the new `plain_handsets` setting is on. New `Host::trusted_proxy` (default
  false). The Docker image and compose file publish 29203.
- **IDN Voice resume without loss**: a handset's sound is kept after it is sent, for its
  window, and a resume's `&have=TALKER:SEQ,…` says what the client got: the rest is sent
  again (a silent cellular drop swallowed what was written into it). New settings
  `frame_budget_mb` (all queues together), `enrol_all_per_minute`, `plain_handsets`.

### Fixed
- **IDN Voice handsets (bug run on the new code)**: a driver's name was their identity,
  so a driver named like a site user passed that user's Auth and dispatch entries and
  read their direct messages, calls, recordings and settings. Handset sessions now have
  a key of their own (`driver:ID`, `handset:ID`; `hello.me.key`, presence `handsets[].key`,
  messages' and transmissions' `from_id`); `user` list entries never match a handset;
  site users' `users` presence no longer lists handsets. Also: expired sound was sent on
  resume (now dropped when taken or moved too); a long backlog starved replies and JSON
  was dropped past 256 queued (one frame at a time now, JSON never dropped, a client
  10,000 behind is closed); two resumes with one token could both win, and a revoke
  between sign-in and the session's start was missed (both checked under the lock now);
  the token a lost `hello` replaced is still accepted; clearing a driver left Open and
  Guest channels the assignment had joined; channel renames weren't followed by drivers'
  assignments; one-driver-one-handset wasn't atomic; enrolment throttling counted IPv6 by
  whole address, had no site-wide cap and codes could be 8 characters (now 10–16);
  `POST /voice/api/settings` took unknown names silently (now 400).
- The device list's AES67 (from a device's settings read) said "on" for a device set
  off: 1 is off, 3 is on (spec idn-aoip-control.md §4.2).
- **Key-request codes were not uniform**: the six digits came from a random `u32`
  `% 1_000_000`, so low codes were a little likelier. All codes now come from
  `random::code_digits` (drawn again past the last whole round).
- **`IDN_NODE=0`, `false` or `no`** used the Node anyway: only `off` was read as off
  (`node::endpoints`, `idn` commands). It now takes the switches' words.
- **A multicast group's key, and every id, could come from the clock** when the system's
  random generator failed (`quic::server::random_bytes` fell back to a hash of the time).
  There is one source now, `random::secure_random`, and it never falls back.
- **Damaged state files were read as empty and then saved over**: the library's
  metadata, a device's site options, the site authority's CRL number, the linked
  computers' keys, the update helper's files. They are set aside as
  `NAME.bad-SECS-PID-N` and logged (`fsutil::load_json` / `load_saved`).
- The library's tests left their temporary folders behind: they use `TempDir` now
  (removed at the end).

### Changed (may break)
- **Shared helpers** (consolidation, 2026-11-26; `STYLE.md` lists them). New in
  `idn_common`: `text::parse_on_off`, `text::on_off_setting`, `hex` (`hex`, `unhex`,
  `hex_upper_colons`), `err` (`other`, `invalid_data`, `invalid_input`),
  `epoch::secs`/`millis`, `fsutil::load_json`, `net::local_addr_towards`/
  `local_v4_towards`/`outgoing_ip`, `backoff::Backoff`, `log`, `win` (Windows: `wide`,
  `wide_os`, `reg_sz`, `reg_dword`, `windows_build`), `testing::TempDir`,
  `testing::wait_for`. In `idn_open_protocol`: `random` (`secure_random`, `fill`, `below`,
  `token_hex`, `code_digits`, `code_base32`), `hash` (`sha256`, `sha256_hex`),
  `secure::crypto_provider`; in `web`: `Headers`, `bearer`, `cookie`, `set_cookie`,
  `split_host`, `is_loopback_name`, `PinLock`, `PasswordChecks`, `LoginLimits`; and
  `update::fetch_package`/`FetchOptions`, `release::Store`/`safe_version`.
- **Moved**: `idn_common::text::hex`/`unhex` → `idn_common::hex`;
  `idn_open_protocol::squic::secure_random` → `random::secure_random`;
  `library::now`/`trust::token::now` → `idn_common::epoch::secs`. The old paths are
  removed. `quic::server::random_bytes` is gone.
- **One on/off rule**: every on/off setting (`captions.on`, `data.on`, `data.library`,
  `apps.*`, `sitemanager.on`, `audio.mic`/`speaker`…, `netaudio.*`, `parallel_decode`)
  takes on/off, true/false, yes/no or 1/0 in any case; empty is off (`parallel_decode`:
  unset). Errors read `KEY "VALUE": on or off`.
- **One logger**: `idn_open_protocol::log::set_logger` is `idn_common::log::set_logger`,
  so `idn_common`'s own lines (a clock restarted, a bad `IDN_MEDIA_PORTS`, a settings copy
  that failed) reach the program's logger; with none installed they still go to stderr.
  The session agent's lines (`node::session`) go through it too.
- **Linking without a site speaks version 3 only** (§E.28): the version-2 exchange (no
  last word from the asker) and the asker's fallback to it, 16 s later, are gone.
  `trust::pair::Heard::confirms` and `Asking::version` are gone (always version 3).
- **Network-audio rings**: the device's "shares channels" flag is gone (every device
  does): `idn_common::netrings::Rings::shares_channels` is removed and the desk's claim
  moved into the flag's place in the head.
- **Network-audio rings' version is 2**: the head's layout changed (the claim at byte 56,
  the features word gone), so rings of another build are refused ("not this build's
  rings"): a desk and a device on one machine must be the same build.
- The one-time admin code uses the typeable alphabet (`random::CODE_ALPHABET`: no 0/O or
  1/I) instead of RFC 4648 base32.
- Discovery's query pacing and the audio routes' retries use `Backoff` (same waits).
- `release::Manifest::from_json` requires `product`: a manifest without one is no longer
  read as IDN Media's. `to_json` always writes `product` and `track`.
- The update helper no longer removes programs older IDN versions shipped (idn-tray,
  idn-panel, idn-remote, idn-voice) after an update.
- `update`: a damaged `staged.json`, `pending.json` or `result.json` is set aside and
  logged instead of read as none; the helper writes its job and result whole (beside,
  then renamed). A damaged `newest-installed` refuses the update ("remove it to take any
  release again") instead of allowing any release.
- `web::Logins` moved to `web::limits` (still `web::Logins`); `Logins::new(LoginLimits {
  per_address, per_name, window, known_for })`, `Default` still 5 a minute per address;
  `attempt_as`/`succeeded_as` also count per name (20 a minute by default).
- Logins (`web::Logins`): a right password takes back only its own try (not the address's
  other failures); an address that logged in as a name passes that name's limit for
  `LoginLimits::known_for` (default a day). The MQTT broker has no per-name limit.
- `web::TokenCheck::carried` and `PasswordGate::who` read the token with `web::bearer`:
  the scheme in any case, any white space; `Authorization: Bearer` with an empty token is
  no token (the session or `?token=` is used, not a 401).
- `web::loopback_host` takes every loopback name (`is_loopback_name`: 127.0.0.0/8,
  `*.localhost`, `::ffff:127.x`, `[::1]`); `web::lan_host_ok` refuses a bracketed Host
  whose port isn't a number. An in-process input (`InputConfig::local`) matches any
  loopback name.
- `web::client_ip` is new: the client's address, from the forwarding headers only behind
  a trusted proxy. `web::is_loopback_host` is new: strict, for Host-header checks (no
  `*.localhost`).
- `web::split_host` reads an empty port (`host:`, `[::1]:`) as the default port.
- The web server joins repeated `Cookie` headers, so `Request::cookie` finds a cookie in
  any of them (it read the last only).
- `PasswordGate` logins run at most 4 Argon2 checks at once in the process
  (`web::PasswordChecks::shared()`); more wait their turn.
- Simple mode's local PIN uses `web::PinLock`: a try counts before its check, checks no
  longer queue under one lock; still 5 wrong, then a minute.
- The panel certificate no longer deletes `panel.key`/`panel.crt` left by older builds.
- `web::httpc` reads `http://[v6addr]/` without a port (it kept the brackets).
- `idn_common::ports`: the notes on the old 79xx ports are gone (nothing used them).
- `idn-common` depends on `serde` and `serde_json` (for `fsutil::load_json`);
  `idn-open-protocol` no longer has the `hex` dev-dependency.
- **IDN Node, Site Manager, `idn`, tools; site trust's hello** (consolidation group A):
  - Site trust (§E.10.2): the authority serves version 2 hellos only. A hello without
    `version="2"` or later gets no answer (a device then sees no authority); every
    `idn_trust_ok` carries `join`. A join without a code in `code_and_approval` is always
    answered `code_needed` (the version-1 `closed` is gone).
  - Site trust (§E.10.4): a device no longer sends the old `psk_mac` join proof, only
    `psk_at` + `psk_mac2`.
  - `IDN_PANEL_URL` and `IDN_PANEL_APP` are removed (the panel window that read them is
    gone).
  - `idn_node::media::safe_name` is removed: use
    `idn_open_protocol::library::safe_file_name`.
  - `library::safe_file_name` is public (no folders, nothing hidden or `.part`, nothing
    Windows refuses, 200 characters); the Node's media folder uses it for files pushed to
    it, so a pushed `NAME.part` is refused. The Node no longer finds library items by their
    path (a program from before library v2): by lasting id only.
  - Node settings (`node.xml`): no more reading of the passphrase in clear (`passphrase=`),
    the old panel ports 7959/7960, the "Mixer" name for desks set up before "Network
    Mixer" (`netaudio_desk_named` no longer written), or a missing `welcomed` as done (a
    file without `welcomed="true"` shows the wizard). `hub.xml`, the "IDN Hub" state
    folders and the `idn-hub` service are no longer carried over or removed at install.
    A damaged `node.xml` is said once, through the logger (stderr when none).
  - `idn-node set` takes the documented dashed names only (`netaudio-on`, `audio-rate`,
    `update-managed-by` …): the dotted and underscored copies (`netaudio.on`, `rate`,
    `routers-list`, `update_managed_by` …) are gone.
  - `idn bridges` (the former name of `idn session`) is gone, with the cleanup of its
    autostart entry; `idn-session` no longer removes the old tray's autostart entries.
    The Node's `session_tray` module is `session_agent`.
  - Node updates fetch through `update::fetch_package` (4 MiB pieces): 4 tries a piece
    with growing pauses (2 s up to 8 s), progress every 5 %, a `.part` file until the
    package checks out.
  - Site Manager releases through `release::Store`: a release over 1 GiB, a version that
    can't be a file's name, or a file renamed by hand to another version is refused;
    deleting one that isn't there says "no such release" (NotFound).
  - Site Manager: logins (and the Node's LAN panel login) run through
    `PasswordChecks::shared()`; `/metrics` reads its token with `web::bearer` (any case
    of the scheme); Voice reads the panel cookie with `web::cookie`. A damaged
    `backup-schedule.json` or `mqtt.json` is set aside and logged, not read as off and
    saved over.
  - Site Manager: a machine's services list is its report's only (no list worked out
    for a Node without `<service>`); a clock is off only by its `enabled` setting; a
    network link is watched only when the report says `physical="true"`; an empty
    `captions.lang` is sent (English); a Node's network devices no longer need
    `changes="1"` to be pooled or changed (the Node no longer sends it); an update to a
    machine that hasn't reported `<update>` says so ("try again once it is online").
  - Node: `require`, `auto_site`, `welcomed`, `clock`, `approve_programs`,
    `link_requests`, `panel_site_only`, `site_lock` sent to it as a setting take any
    on/off word; the network-audio API's yes/no values and the Site Manager's form
    booleans too.
  - The Node's simple-mode PIN uses `web::PinLock` (same 5 tries, a minute).
  - The site MQTT's machine topic level (`mqtt_site::topic_name`) works per character as
    the switcher's: only ASCII letters are lower-cased (the Kelvin sign is no longer a
    `k`, `İ` no longer two characters), everything else `-`.
  - The Site Manager folder the Node makes is made private (0700) every time
    (`fsutil::private_dir`), not only when made.
  - `IDN_CLOCK_PORTS`, `IDN_NODE_INPUT`, `IDN_VOICE_PORT`, `IDN_VOICE_UDP`,
    `IDN_SITE_MANAGER_DIR`, `IDN_PASSPHRASE`, `IDN_BACKUP_PASSPHRASE`,
    `IDN_SITE_PASSWORD`, `IDN_TOKEN` and idn-release's PIN variables are read through
    `idn_common::switches`: set but empty is unset.
  - `idn-site-manager` and `idn-release` no longer depend on `sha2`; `idn-release`
    depends on `idn-common`.
- **Sound, ASIO, OBS, plugins, Voice app, network-audio device** (consolidation group C):
  - `idn-voice` (desktop app): a damaged `voice-app.json` is set aside
    (`voice-app.json.bad-SECS`) and logged, not reset and saved over; saved whole through
    `fsutil::write`. Server addresses on the old Voice port (`:7962/`) are no longer moved
    to 29203 (nothing deployed used it).
  - `idn-asio`: a damaged `asio.json` is set aside and logged; the driver starts on its
    defaults.
  - OBS plugin: a damaged `idn-output.json` is set aside and logged. The source's old
    `buffered` checkbox is no longer read (the `timing` list alone decides; unset is
    automatic), and SpeedHQ decoding `hybrid` is no longer read as `gpu`.
    `IDN_OBS_SHQ_GPU` takes on/off words (1/0 still work); `two_at_once` takes them too.
  - `IDN_AUDIO_HOST`, `IDN_CWASIO_DIR`, `IDN_PLUGIN_HOST`, `IDN_OBS_IDCT`,
    `IDN_SHQ_GPU_MOST_MB` are read through `idn_common::switches`: set but empty is unset
    (an empty `IDN_AUDIO_HOST` was "not available").
  - `idn-asio-abi` depends on `idn-common`; its registry reads trim the value and read an
    empty one as missing (an empty `CLSID` unregisters as "not listed").
  - `idn-aoip-device`'s network-device lines go to the logger (stderr when none is set).
  - A lost sound card's reopen waits use `Backoff` (same 3, 6 … 60 s).
- **IDN Industrial, IDN Voice, MQTT broker** (consolidation group D):
  - Industrial Site Manager: the web UI's logins go through `web::Logins` (5 wrong an
    address and 20 a name in a minute; a right password takes back its own try) and the process's `PasswordChecks`; its session cookie is made with
    `web::set_cookie` and its token is hex. The API key and the UI session are read with
    `web::bearer`/`web::cookie` (any case of `Bearer`, every `Cookie` header).
  - Industrial Site Manager: releases are kept through `release::Store`
    (`only_product("industrial")`, 1 MiB pieces). Another product's release is refused
    with "a release of media, not of industrial"; a file renamed by hand to another
    version is no longer offered.
  - Industrial Site Manager: a damaged `settings.json` is set aside and logged (the hold
    starts at 7 days). An `api.key` that is there but can't be read stops the start
    instead of a new key being made over it; a new key is 64 hex digits.
    `IDN_QUIC`/`IDN_MQTT` take any off word (and `none`); `IDN_DATA`, `IDN_API_KEY`,
    `IDN_ADMIN_PASSWORD`, `IDN_BACKUP_PASSPHRASE`, `IDN_TRUST_PROXY`, `IDN_HTTP`,
    `IDN_PUBLIC_URLS`, `IDN_VOICE_UDP` and `IDN_VOICE_HTTPS` are read through
    `idn_common::switches` (set but empty is unset). A bad `IDN_VOICE_HTTPS` is logged.
  - Industrial Node: a damaged `site.json` is set aside and logged (the machine starts
    not joined); a damaged `secrets.toml` is set aside and said (setting a secret wrote
    over all the others). An unreadable `api.token` is an error, not replaced. The local
    API's Host check takes every loopback name (`localhost` in any case, `127.x`, `::1`,
    with or without a port). Updates are fetched with `update::fetch_package` (4 tries a
    piece, 3 s apart, into `NAME.part`). Reconnect waits keep 1 s doubling to 30 s, each
    up to a quarter shorter at random. The state folder of the old name
    (`idn-node-industrial`) is no longer moved, by the Node or its helper.
  - Industrial: Voice's logins count in the web UI's login limits (the same
    `web::Logins`).
  - Industrial: rustls configs use `secure::crypto_provider()`; a dead letter kept
    without its op is no longer sent again as a message.
  - `idn-voice`: Voice's sign-in cookie is `SameSite=Strict` (was `Lax`); the console
    link's stays `SameSite=None; Secure` over HTTPS and is `Strict` over plain HTTP.
    Login and console tokens and IEM feed tokens are hex. Password checks take turns
    with the process's others (`PasswordChecks::shared()`; `http::CHECKING` is gone).
    `IDN_VOICE_PUBLIC`, `IDN_VOICE_CERT_SHA256`, `IDN_VOICE_URLS` are read through
    switches; a bad `IDN_VOICE_UDP` goes to the logger. A feed that keeps failing waits
    3 s, then 6 s … up to a minute (the first wait was 6 s). The `ring` dependency is
    gone.
  - `idn-mqtt`: logins take a `PasswordChecks` turn, then `Logins::attempt_as` (5 wrong
    an address in a minute, no per-name limit; a right one takes back its own try). An
    address that isn't one counts with every other such. Client ids made for clients that send
    none come from `random::token_hex` (a failed random fill gave all zeros). The
    `ring` and `data-encoding` dependencies are gone.
- **Switcher, desk, displays, IEM, network audio, Data, captions, FFmpeg, OMT, feeds**
  (consolidation group B):
  - Desk (`idn-mixer`): the state's `network_audio.routes_keep_others` is gone (every
    device keeps its routes on the channels the desk doesn't claim). `idn-aoip` no longer
    gives a desk that claims nothing every transmit channel: until a desk sets its claim,
    the routes play on all of them.
  - Desk: `Settings::parking_rule`, `Settings::first_route_parking`, `PARKING_RULE` and
    `Settings::renamed_snapshots` are removed. A desk saved before the first-route parking
    rule no longer has its parked channels unset at load; a `scene:NAME` user button is
    no longer read as `snapshot:NAME`; `scenes.json`/`scene.txt` are no longer carried
    over to `snapshots.json`/`snapshot.txt`; a matrix saved with the old 21 or 17 sources
    (before 2026-11-08 / 2026-10-27) loads its sources by position; the network desk
    (`--network-audio`) no longer copies the session desk's files into its own `network/`
    folder at its first start.
  - Desk: a damaged `desk-shape.json` or `snapshot-order.json` is set aside
    (`NAME.bad-SECS`) and logged (`saved_channel_count` gives `None` for it). OSC on/off
    strings take `parse_on_off`'s words in any case. IEM flow ids and group addresses come
    from the secure random source.
  - Switcher: saved data from before scenes is no longer carried over: a show cue, saved
    show, MQTT trigger or `mixes.json` entry that puts an input that isn't a scene on a
    bus fails as the API command does (a saved mix's bus stays empty); only a starter's
    own cues still get their `LABEL (scene)`. `setup_load` no longer turns a setup's
    `overlays` into global layers. A show's desk action is read with `snapshot` only (the
    `scene` key is refused). A stream destination's body takes `url` or `file` (the
    `target` alias is gone).
  - Switcher: `setup_load` refuses a damaged setup with the reason and sets it aside as
    `NAME.json.bad-SECS` (it answered "no setup"); damaged `templates.json`,
    `audio-offsets.json`, the Planning Center token and `format.json` are set aside and
    logged, not read as empty and saved over. Its warnings about saved items it couldn't
    make go through the library log (`switcher: …`), not stderr.
  - Switcher MQTT: triggers are matched with rumqttc's matcher (a topic starting with `$`
    no longer matches `+` or `#`). The machine's topic level (in the default base
    `idn/<machine>/switcher` and the client id) is made one character at a time: ASCII
    letters lower-cased, digits kept, anything else `-` (a non-ASCII letter could become
    an ASCII one, or two characters).
  - Display service: a damaged `screens.json` or `service.json` is set aside and said, not
    read as empty and saved over at the first change.
  - `idn-aoip`: the previous engine's state (`device-id` and its TOML files) is no longer
    imported at a device's first start (spec idn-aoip-device.md §10.5 marked not built); a folder
    with only an old `device-id` no longer stops a rename from being followed.
  - IDN Data: a damaged `rows.json` is set aside and logged. `IDN_DATA_PANEL_KEY` is read
    through `idn_common::switches`; the `sha2` dependency is gone (`hash::sha256_hex`).
  - Captions: the "isn't the pinned model" line goes to the library's logger (it was
    stderr).
  - FFmpeg: `IDN_DECODER`, `IDN_GPU_DOWNLOADS`, `IDN_VAAPI_DEVICE`, `IDN_GPU_ENCODER`,
    `IDN_VOICE_FEED_TOKEN`, `IDN_LISTEN_PASSWORD` and `IDN_VIEW_PASSWORD` are read through
    `idn_common::switches` (set but empty is unset). The viewer's, site link's and
    screen share's lines go to the library's logger (they were stderr). The listening
    state's `url` reads the Host with `web::split_host`/`is_loopback_name` (every loopback
    name; an IPv6 address kept in brackets) and, opened on this machine, gives
    `net::outgoing_ip()`. An ingest that keeps failing waits 2 s, then 4 s … up to a
    minute (the first wait was 4 s).
  - `idn-iem` has a dev-dependency on `idn-common` (its tests' waits and folders); the
    library itself still depends on nothing of IDN's.
- **Saved files and folders** (bug run on the consolidation, 2026-11-26):
  - `fsutil::private_dir` follows a link (a symlink to another disk, systemd's
    `StateDirectory` under `DynamicUser`, a Windows junction) and works on its target; it
    narrows to 0700 only a folder this user owns whose mode is wider, and leaves another
    account's as it is. It returns `io::Result<bool>` now (`false`: another account's).
    The Node's Site Manager folder and the Site Manager | Industrial's data folder log
    and go on instead of stopping (a link there stopped them; a folder this user may
    write but doesn't own gave EPERM at each start). On Windows a junction's target still
    isn't locked down (`lock_down_dir` refuses links): the Node now logs that.
  - `fsutil::load_saved` / `load_json` set a damaged file aside as
    `NAME.bad-SECS-PID-N` (was `NAME.bad-SECS`: two in one second, or two processes,
    replaced each other's copy), never over another copy, and only when it is still the
    file that was read (one saved meanwhile is left in place and the `Err` says so). The
    newest 5 copies per file are kept (`fsutil::KEEP_SET_ASIDE`;
    `load_saved_keeping` for another number), the older removed. New:
    `set_aside_copies`, `remove_set_asides`, `copy_aside` (the desk's snapshots use it).
  - New `fsutil::load_json_readonly` for a file only people write and the program never
    saves: never moved; the `Err` names the file, line and column. **`asio.json`** uses
    it: a damaged one is logged and left in place, the defaults used (it was set aside).
    Every other `load_json` / `load_saved` caller's file is written by its program, so
    stays as it was.
  - A removed secret takes its file's set-aside copies with it: `set_secret(None)` in
    IDN Industrial (`secrets.toml`), a Planning Center sign-out (`planning-center.json`,
    now written readable by its user only), the OBS output's passphrase cleared, simple
    mode's PIN wiped, a Site Manager user removed. Set-aside copies keep their file's mode
    (a hard link or rename of it).
- **IDN Node | Industrial with an unreadable `api.token`** starts without the local API
  and panel (the link, modules and MQTT run) and logs why; it stopped before, and under
  `Restart=always` started again and again. The token is still never replaced on its
  own: `idn-industrial token --new` makes a new one on purpose (the old one stops
  working; restart the Node and give the programs the new one).
- **Release versions that are Windows device names** before the first dot (`CON`,
  `PRN`, `AUX`, `NUL`, `COM0`–`COM9`, `LPT0`–`LPT9`, any case) are refused by
  `release::safe_version`.
- **IDN Media's Site Manager takes IDN Media's and the host products' releases only**:
  `release::Store::products(&[..])` (an allow-list; `HOST_PRODUCTS` stands for every
  product that isn't IDN's own; `only_product` is a list of one). An IDN Industrial
  release is refused at upload with the reason, and one kept there by hand is no longer
  listed or offered.
- **IDN Media's Site Manager takes releases up to 8 GiB** (was the store's 1 GiB, a
  limit it didn't have before the consolidation), `IDN_RELEASE_MAX_GB` to change it
  (whole GiB, 1–1024); `Store::max_size` sets any store's. **Integrators: say if a
  release for several systems can be larger than 8 GiB.**

## [v0.3.5-Beta16] — 2026-11-26 (pre-release, source only)

Everything after `v0.3.5-Beta15`. The crates stay 0.3.5. No installers: Alpha88's are
the newest.

### Added
- **ASIO instances on Windows** (asio-interface.md §A.6). `idn asio register NAME
  [--description TEXT]` and `idn asio unregister [NAME | --all]` now work on Windows too,
  from a terminal opened as administrator: each name is a key `HKLM\SOFTWARE\ASIO\NAME`
  with IDN's class id, an instance with its own settings and source. `--all` takes every
  name of IDN's driver off, the installer's "IDN" too. `idn asio list` shows each name.
  `idn_asio_abi::register(name, clsid, description)` and `unregister(name, clsid)`
  (Windows; the Linux ones keep their arguments). Uninstalling (`DllUnregisterServer`)
  now takes off every name with IDN's class id, not just "IDN".
- **The desk and IDN's routes share a network-audio device's transmit channels** (roadmap
  R.33). A desk in `idn-aoip`'s rings (`idn mixer --network-audio`) claims only the
  transmit channels its outputs go on, written in the rings' page before its first
  block; the device leaves it those and keeps playing its routes on the others (before,
  the desk had them all). `--network-audio-outputs N` sends only the desk's first N
  outputs to the device (`NetAudioConfig::outputs`). The desk's state `network_audio`
  has `out_first`, `out_count` and `routes_keep_others`; the device's log says which
  channels the desk has and which routes are left out for it. Compatible both ways: the
  page stays version 1 (an older desk claims nothing and has every channel; with an older
  device the desk keeps them all, as before). `idn_common::netrings::Claim`,
  `Rings::set_claim`, `claim`, `shares_channels`, `silence_channel`. Under the Node:
  `netaudio.desk_outputs` N (`idn-node set netaudio-desk-outputs=8`, the panel's Sound →
  Network audio, the Site Manager's form), reported in `<netaudio desk_outputs>`.

### Changed
- `idn_mixer::netaudio::NetAudio::new` takes the desk's output count as a third argument;
  `NetAudioConfig` has the field `outputs` (build it with `..Default::default()`).

## [v0.3.5-Beta15] — 2026-11-26 (pre-release, source only)

The crates are version 0.3.5. From Beta13: Beta14's section below too.
No installers: Alpha88's are the newest.

### Added
- **ASIO on Linux, cwASIO's way** (asio-interface.md §A.5).
  - idn-audio's ASIO host (`IDN_AUDIO_HOST=asio`, the `asio` feature, on by default) now
    builds on Linux too. It lists the drivers in `/etc/cwASIO/<name>/` (`driver` = the
    library's path, `description`), loads them with `dlopen` and `instantiateDriver`, and
    never closes them. IDN's own driver (`libidn_asio.so`) is left out, as on Windows.
  - IDN's ASIO driver builds as `libidn_asio.so` for Linux cwASIO hosts. It exports
    `instantiateDriver`, `releaseDriver`, `registerDriver` and `unregisterDriver`. Install
    it in `/usr/lib/cwASIO/` and call `registerDriver("IDN")` as root. The settings are
    `asio.json` in the IDN state folder, as on Windows.
  - `idn_asio_abi::DriverId` (a class id, or a registration and its library), `drivers_in`,
    `register` and `unregister` (Linux), and `IDN_CWASIO_DIR` for another list folder.
  - Instances (cwASIO's multi-instance drivers, §A.6): IDN's driver takes
    `future(kcwASIOsetInstanceName, name)` before `init`, with each instance's settings in
    `asio.json`'s `"instances"` and its own source name; IDN's host names the instance on
    Linux. `idn_asio_abi::kcwASIOsetInstanceName`, `idn_asio::Config::load_instance`,
    `idn_asio::DEFAULT_INSTANCE` ("IDN").
  - `idn asio list | register [NAME] | unregister [NAME | --all]`: the ASIO drivers
    (Windows and Linux), and IDN's listed for cwASIO hosts (Linux; register and
    unregister as root).
  - The `idn-media` .deb installs `/usr/lib/cwASIO/libidn_asio.so` and lists it as "IDN";
    removing the package takes its listings off.

### Fixed
- **A sound card's callback never waits for the network side** (audio mixer, every
  `Mixer` consumer: sound cards, the IDN ASIO driver's inputs, the OBS plugin). A lane's
  ring is also locked by the pushing thread through its level keeping; a callback finding
  it held now plays the block it copied the last time (and that comes off the ring's front
  afterwards, unless a cut took it meanwhile), and a lane added or let go never holds the
  callback up (the callback keeps its own copy of the lane list). Only a block larger than
  the last, a lane's first block, or the ring held through two callbacks running, still
  waits. What the callback played from its copy comes off exactly by the ring's own
  counts of frames cut or put in front meanwhile, so nothing plays twice; lanes let go
  are freed off the callback's thread, and the copy's room is made with the lane.
- **Two IDN ASIO drivers in one program** stopping each other from their callbacks no
  longer hang: on any IDN driver's clock thread, a stop only tells the clock to end, and
  start, buffers and rate are refused (asio-interface.md §A.4); a last Release there lets
  the object go on a thread of its own once its clock has ended.

### Changed (may break)
- `idn_asio_abi::DriverEntry` has `id: DriverId` in place of `clsid: Guid`, and
  `Loaded::load` takes a `&DriverId`. On Windows, use `DriverId::Clsid(guid)`.
- From another IDN ASIO driver's callback, `createBuffers`, `start` and `setSampleRate` of
  an IDN driver are refused (they were taken, and could wait on that driver for ever).

## [v0.3.4-Beta14] — 2026-11-26 (pre-release, source only)

### Added
- **Updates managed by another product** (idn-node.md §N.23). A machine whose IDN Node
  another product installs and updates itself takes no IDN Media update from the Site Manager.
  - That product's package writes `idn-updates-managed-by` (first line: its name) in IDN's
    programs folder. Or set `update.managed_by`: through Apply, or `idn-node set
    update-managed-by=NAME` as root / an administrator.
  - The report's `<update>` gains `managed_by` and `managed_by_source` (`package` or
    `setting`); `DeviceHealth` gains `update_managed_by`.
  - `POST update` leaves such machines out, each with "updates managed by NAME: update
    it there". The machine refuses the offer and a rollback too.

- **File conversion** (`idn_ffmpeg::media::convert`): a hard-to-play upload (ProRes,
  MPEG-4 Part 2, H.264 4:2:2, interlaced, variable rate, 44.1 kHz) made an MP4 of H.264 or
  HEVC 8-bit 4:2:0 at a constant rate with AAC 48 kHz stereo, faster than real time, on the
  GPU's encoder where there is one; progress, cancel, low priority.
- **More probe facts**: `VideoInfo::avg_frame_rate`, `base_frame_rate`,
  `variable_frame_rate`, `field_order`, `interlaced`, `bit_rate`; `MediaInfo::bit_rate`.
- **`media_send replace="true"`** (idn-node.md §N.25): the receiving machine replaces its
  file of the same name once the new one is whole and checked; the reply carries `offer`.
- **The sender's transfer says `sent`** once the receiver fetched the whole file
  (`Handler::on_served`, `channel::Served`; a fetch now reads the stream to its end).
- **The Site Manager's status** passes on each machine's `apps` and the services'
  `auto` and `runs`; the settings form has the display service's two switches.

### Changed (may break)
- `idn_ffmpeg::media::VideoInfo` and `MediaInfo` have new fields (above): code building
  them with a struct literal must add them (reading them is unchanged).
- A file fetch over `channel/1` now reads the stream to its end (up to 5 s) and fails
  if more bytes follow than announced. A receiver from before Beta14 lets the stream go
  early, so its sender shows `failed: … stopped the transfer` though the file arrived:
  during a mixed upgrade, trust the receiving machine's own `received`.
- `media_send replace="true"` is refused by a receiver from before Beta14.

## [v0.3.4-Beta13] — 2026-11-25 (pre-release, source only)

### Fixed
- **ASIO bug run** (the new host and driver):
  - The driver's notices (reset, rate, latencies) are now handled on the host's thread.
    Before, they raced the buffer switch.
  - Stopping waits for a switch in progress.
  - A stream dropped in a callback no longer deadlocks.
  - An open that timed out no longer leaves a stream running.
  - A panicking callback no longer aborts the program.
  - A refused open no longer interrupts the running stream.
  - A reset request sets the buffers up again.
  - Buffer sizes snap to the driver's granularity.
  - Probes are kept 60 s, failures included, and no other driver is loaded while one runs.
  - IDN's own ASIO driver is never loaded by the host (it joined the site at every listing).
  - A driver that won't start is "busy", not "missing".
  - The host's thread pumps window messages.
  - 32-bit containers with zero top bits are read with their sign, and NaN is read as
    silence.
  - IDN ASIO driver:
    - registered "Both";
    - stops its clock before the last release;
    - drops old callbacks on a failed `createBuffers`;
    - no longer echoes a host-set rate;
    - no longer claims overload reports;
    - gives no buffer for a negative channel;
    - no panic with a 0-channel routing input.
  - Second ASIO bug run:
    - Host: driver notices are atomics tagged with their run, never locks or allocation
      on the audio thread; one buffer switch at a time; latencies held outside the
      switch's state.
    - Host probes: answered and kept only on the host's thread; failures kept 5 s; the
      running driver is not asked about rates mid-stream; a driver busy because another
      runs keeps its last good formats in the pickers.
    - Host opens and resets: defaults prefer the running driver and put wrappers last;
      an open is checked for sample types and buffer size before anything stops; a reset
      takes the new buffer size; a stuck switch can't hang the host's thread.
    - An unplugged card reads as missing, not busy.
    - Capture and Playback report a stream the driver invalidated as lost.
    - IDN ASIO driver: a control lock (no two clocks from two threads), the clock thread
      told apart per driver, the latency read without locks, no allocation on the clock
      thread, a refused `createBuffers` stops the clock, null host pointers refused.
  - Third ASIO bug run:
    - Host: each run has its own gate, so a stuck switch can't hold the next runs, and its
      driver is set aside, never released; notices are one atomic with their run; a rate
      changed meanwhile is seen at set-up; the open's probe is kept for the defaults;
      the driver is probed again after a rate change; listings reuse old answers.
    - IDN ASIO driver: the last Release from inside the switch no longer touches freed
      memory; send blocks are about 100 ms; a stop from the first callback is no longer
      lost; dispose from the callback stops the clock; panics in the sender are caught;
      the seqlock is fenced.
- **Lost sound cards opened again:**
  - The desk's input card, and the switcher's card and monitor card, are opened again in
    place (`idn_audio::reopen`: after 3 s lost, 15 s to start, backing off to 60 s).
  - The desk's output card makes the desk start again (supervised; after its first
    minute; at most once in 10 minutes).
  - A lost card is an error in the desk's `cards` (`card_lost`).
  - idn-audio: a driver's "ended" counts once the sound has stopped too.
- **The IDN ASIO driver tells hosts when its latency changes** (`kAsioLatenciesChanged`),
  and its desk-clock reading is a seqlock.
- **`idn_audio::devices_known()`:** a listing made every minute (the display service's)
  doesn't load ASIO drivers already asked.
- **Windows: the Node checks that a program's log file is its own** (the file's owner
  against the program's account SIDs, on the handle it reads). Only drive paths are
  taken, an unknown account is refused, and old entries count once registered again.
  On Unix the registration check is unchanged.
- **The program's account is taken from the pipe connection** (Windows): the Node
  impersonates the client for a moment once it has written its hello, instead of looking
  up a process id that may have been reused. A log file's owner is a typed
  `diag::Owner` (a uid or SIDs).
- **ASIO host:** every channel count from 1 to the driver's full count is offered, so the
  desk taking 8 of 64 converts 8. `probe_device` on the running driver no longer opens a
  stream (two dropouts). The host is split into `asio_host/{device,probe,engine,switch}`
  with no behaviour change.

## [v0.3.4-Beta12] — 2026-11-25 (pre-release, source only)

### Added
- **The display service with no desktop (Linux, idn-display.md §D.8.9).** On a headless
  box the service holds the graphics cards (DRM master), names the screens by connector
  (`HDMI-A-1`, `DP-2`) and hands each screen's process a DRM lease; the same API, modes,
  hot-plug, identify, thumbnails, walls and sound. The IDN Node runs it from boot, with
  no one logged on, when `apps.display_boot` is on (`idn-node set
  apps-display-boot=on`, with `apps.display`); the Node's unit gains the `audio` group.
  `AppsSetting::display_boot` is a new public field (build it with `..Default::default()`).
- **`Authority::lan_client_config(fp)` / `Anchor::lan_client_config(fp)`** (an integrator's
  ask): a TLS client for a site machine's HTTPS ports (the display service's 29223, the
  Node panel) pinned to that machine's fingerprint, at any address; `Anchor::verify_lan_of`.
- **Identify on a wall tile** shows its place: `Wall R1 C2 of 2×3 · GROUP`.
- **ASIO without Steinberg's SDK**. New crate
  `idn-asio-abi`: the ASIO interface written from cwASIO's MIT headers
  (spec/extensions/asio-interface.md). idn-audio's `asio` feature is now IDN's own host (a
  cpal custom host; `IDN_AUDIO_HOST=asio` as before; f32 streams, input and output on one
  driver share its buffers); the IDN ASIO driver (idn_asio.dll) is all Rust (the C++ shim
  is gone). Builds need no `CPAL_ASIO_DIR`, bindgen or C++ for ASIO; the `asio-sys` patch
  is gone. Integrators building idn-audio or idn-display for Windows no longer compile
  Steinberg code. The cwASIO MIT notice ships in LICENSE.txt.

### Fixed
- **Pairings and joins dropped under load** (reported on Beta11): an interrupted socket read
  (EINTR: a debugger stopping every thread, a signal) was taken for a lost connection,
  ending the trust session and refusing the relayed pairings waiting on it; it is read
  again now (`transport::is_poll_timeout`, `proto::recv_into`).
- **A typed join code** is used up by the site's answer, no longer at the start of the
  exchange: a join cut short keeps it for the next try (it left the device waiting for
  approval with the code gone). Each answer has 10 s (was 10 s for the whole exchange).
- **`Enrolled{connected:true}`** is set once messages can go, and `connected:false`
  before waiting pairings are refused; `request_key` and `pairing_code` return
  `NotConnected` instead of dropping a message sent as the connection ended.

## [v0.3.4-Beta11] — 2026-11-25 (pre-release, source only)

Everything after `v0.3.4-Beta10`: captions in other languages and a cancel for their
downloads, pairing relays, `app:*` control tokens for administrators, and three bug runs
(the whole codebase by area, by kind of bug, then public APIs, Windows, recovery and the
day's own changes). The crates stay 0.3.4. No installers: Alpha88's are the newest.
**Mark each box that relays pairings once** (`SiteCommand::Relay`).

### Added

- Captions in other languages: the multilingual whisper models tiny,
  base, small and medium (pinned like the others), `idn_captions::LANGUAGES` (24 common
  languages: code and name), `model_speaks(model, lang)`; "auto" picks among the
  multilingual models when the language isn't English. The IDN Node's captions take
  `captions.lang` (panel: "Language spoken"; Site Manager settings too; typed status
  `CaptionsService::lang`).

### Changed

- **Pairing relays**: only machines the Site Manager marks as relays
  (`SiteCommand::Relay { fingerprint, on }`, `Authority::set_relay`; `DeviceHealth::relay`,
  `EnrolledDevice::relay`) get the site's pairing codes, so only a client pairing through
  one is PIN-verified (`code_ok`). Through any other machine a pairing waits for a
  person. Before, any machine of the site could ask for its own code and verify its own
  request (a compromised display box got a lasting API key). **Mark each box that relays
  pairings once after updating.**

### Added

- Captions cancel (an integrator's ask on v0.3.4-Beta10): `ensure_model_stop`,
  `auto_choose_stop`, `Engine::measure_stop` take an `Arc<AtomicBool>`; set, they end
  within about a second (a download or a measuring pass included) with the error
  `idn_captions::STOPPED`, nothing left under the model's name.
  `web::httpc::download_stoppable` likewise (`ErrorKind::Interrupted`). Two downloads of
  the same model to the same folder at once are safe (each its own part file; only a
  whole, checked file is renamed into place).

### Changed (may need a change in a host)

- New public fields since v0.3.4-Beta10 (code that builds these structs field by field,
  e.g. in tests, adds them): `EnrolledDevice::relay`, `DeviceHealth::relay`,
  `CaptionsSetting::lang`; `SiteCommand::Relay` is a new variant (an exhaustive `match`
  adds it); `idn_captions::MODEL_LIST` / `MODEL_PINS` are now 9 long.

### Fixed (bug run B, 2026-11-25: public APIs, Windows, recovery, today's changes)

- Pairing relays: the program hosting the Node is recognised by its process, not its
  executable's path (on Windows the paths never matched, and on Linux not after an
  upgrade replaced the running file: its pairings lost their PIN). Errors through the
  Node keep their kind (`PermissionDenied`, `AlreadyExists`, …). A stale unnumbered
  admin answer from an older authority isn't taken by the next call; a disk error at
  the authority isn't read as a wrong admin code.
- Stoppable downloads: a stop over HTTPS no longer loops (it was told as `Interrupted`,
  which TLS reads again), connecting is stoppable too, and the idle time counts across
  reads; checking a model already there and loading it see the stop as well. The
  captions `Engine` refuses an English-only model with another language.
- A channel dropped takes its endpoint out of the site's directory (port 0 withdraws).
- Site Manager: English is sent as `en` when chosen (empty was left out, so a machine in
  Spanish was never set back); a template saved without its Simple mode group no longer
  keeps its old PIN.
- Windows: a file offered is bound to its volume and file index (the folder-swap fix
  worked only on Unix); the Virtual Display Driver's settings folder is taken over by
  SYSTEM before it reads or writes there; the Node at log-on (not SYSTEM) can serve more
  than one program, and only its own user may use its pipe; the Industrial helper
  follows the desktop (it tells the service as it leaves) and is started again when a
  user logs on (a SYSTEM helper went on in the user's session), without a console
  window; mDNS loops treat a read timeout as one; program paths compare without letter
  case; the ASIO driver's settings button opens the panel as the IDN Node shortcut does.
- Recovery: the Industrial local MQTT endpoint doesn't half-start (it acknowledged
  messages it dropped); a silent Modbus device behind a gateway goes bad; the Node's
  update state follows the helper once it has the package (a refused one left
  "installing" for good); the mixer opens its control surface again while it is missing
  or gone; a VISCA camera that closed its connection is connected again before a
  command; IEM phones notice a silent desk on their update channel; a remote site link
  closed while connecting ends; Voice feeds say why they ended and wait longer while
  they keep failing; stream ingest backs off from a dead address; a program that said it
  can't run here (exit 5) is tried again a minute later; an HTTP poll's value not taken
  is sent again; Voice recordings that couldn't be written aren't listed; PTZ recalls
  start again after a failed thread start.

### Fixed (bug run A, 2026-11-25, by kind of bug)

- Security:
  - Pairing relays: on a relay machine only the program hosting its IDN Node
    and an administrator get the pairing code, and only their relayed requests carry a
    PIN (another user's program there could read the code and verify its own request; a
    program of the hosting user counts as an administrator of that Node, as it can read
    its keys anyway); a machine that leaves or withdraws its join is no longer a relay.
  - Audits: an address keeps only its scheme and host (`rtmp://host/…`): stream keys in
    paths and `user:pass@` were kept, readable with a read token.
  - Discovery Server: a client's monitor events reach a listener only about devices it
    advertises; a device's monitored properties are bounded (64, values of 256 bytes);
    full listings at most once a second a connection; 8192 adverts in all.
  - `idn-session --open` (and every browser opening on Linux) runs the system's
    `xdg-open` by its full path, without `BROWSER` or library preloading handed down.
  - Site Manager: control tokens for a program's engine (`app:*`, e.g. `app:myapp`) are
    for administrators only (an Operator's amounted to the host's site administrator
    under its default access, and a token can't be revoked as a key can); operators
    still get read tokens.
  - Remote-admin answers are numbered: two requests at once (or an answer after its
    caller gave up) no longer swap answers.
- Correctness:
  - Site Manager: a template read from the status (its PIN shown as `pin_set`) saves
    back, keeping its PIN; an empty `captions.lang` isn't sent (older Nodes refused the
    whole message); a backup folder inside the Site Manager's own (saved before that was
    refused) is left out of the backups.
  - Admin code: a failure at the site (not a wrong code) is answered `retry`, and the
    device tries again; a claim not saved is taken back with its code.
  - Leaving a site removes the old site's options too.
  - Tally: the sender's combined tally (NDI echo) and OMT's for a receiver joining
    during a change are no longer left stale.
  - IEM: talking reads the desk with the sessions let go; Voice writes recordings on a
    thread of their own (not under the hub's lock); the ASIO driver's clock thread no
    longer allocates every buffer.
  - Captions "auto": one choice for English and one for other languages.
  - OMT bridge: a BT.601 picture without alpha is converted (it went out as BT.709).
  - Shared-memory handouts don't wait 2 s on a silent connection; Industrial reloads
    pushed settings off the link's runtime; a PTZ camera's recalls run one at a time,
    the latest wanted; the GRUB edit is written whole and synced; bounds on held remote
    keys, a site browse's list and the Modbus simulator's header.

### Fixed (bug run 2026-11-25, the whole codebase)

- Security:
  - IDN Node: while programs need approval, the Node's own command-line tools get no
    panel session (a waiting program ran `idn-node open`, read the printed address and
    linked the machine, cleared the PIN or switched services); only the session agent's
    `--open` and an administrator. Crash reports from programs wait for approval too.
  - Site Manager: the status shows a template's PIN as `pin_set`, not its hash.
  - Trust: a device's site options follow the site's CA (a previous site's higher
    version kept its options, `secure.require` among them, in force); a refused admin
    code isn't sent again at each reconnect; a machine leaving revokes every certificate
    it holds.
  - MQTT: the failed-login limit holds with many CONNECTs at once; an MQTT 5 clean
    session can't be made lasting at DISCONNECT.
  - IDN Voice: guests no longer get the signed-in users' names.
  - OMT: the VMX preview buffer is sized for either height (a sender's interlaced flag
    that didn't match the coding had libvmx write past it).
- Correctness:
  - sync/1 measures and compares on the clock frames are stamped with (site time on a
    site): a sender's system clock off the site's had every frame dropped as late. Sync
    groups no member is in any more are dropped.
  - Trust: the authority's wait after an approval no longer spins for good on Windows.
  - Library marks: a refused request no longer leaves an empty holder behind.
  - Push channels: a token is tried before it is said to be taken.
  - Desk: the clock keeps real-time priority after a short stall (counted on its blocks'
    own work, not on starting late).
  - Supervised programs (Linux) end with their supervisor, their ports freed.
  - OMT receiver: `set_preview` / `set_tally` during a connection's start aren't lost;
    the bridge changes an output's alpha and frame rate in place (receivers kept); the
    sender's combined tally can't be stored out of order.
  - Site backups skip restores set aside (`restore-pending.*`), and a schedule's folder
    can't be inside the Site Manager's own; crash reports are pruned oldest first; the
    GRUB edit for virtual screens keeps the original, writes atomically and is undone when
    update-grub fails.

## [v0.3.4-Beta10] — 2026-11-25 (pre-release, source only)

Everything after `v0.3.3`: an integrator's ask on it (the OMT sender's colour matrix). The
crates are version 0.3.4. No installers: Alpha88's are the newest.

### Added

- OMT sender: `Sender::set_color_space(Option<u16>)` states the colour matrix its pictures
  were made with (601 or 709), marked on each picture; `None` (the default) keeps VMX's
  rule by the height (709 from 720 lines, 601 below).

## [v0.3.3] — 2026-11-25 (pre-release, source only)

Everything after `v0.3.1-Beta8`: integrators' OMT and captions asks, and the library marks'
holder-name limit made one. The crates are version 0.3.3 (there is no
0.3.2). Still a pre-release. No installers: Alpha88's are the newest.

### Added

- OMT receiver (`idn_omt::Picture`): `color_space` (601 or 709, as the sender gave it),
  `premultiplied` (the sender's alpha flag) and `preview`; `Receiver::set_preview(bool)`
  takes the sender's previews (an eighth of each side) for thumbnails; the sender marks
  premultiplied alpha with `Sender::set_premultiplied`. The `idn omt recv` bridge uses the
  picture's matrix and alpha convention.
- Captions: `auto_choose_with(dir, cfg, log, progress)`, the downloads as numbers
  (model, bytes, size) as `ensure_model` gives them.

### Fixed

- Library marks: a holder's name is at most 64 characters everywhere, as
  `LibraryServer::marks` has taken since library v2 (Beta8's index check, and the
  spec and notes with it, said 128).

## [v0.3.1-Beta8] — 2026-11-24 (pre-release, source only)

Everything after `v0.3.1-Beta7`: each machine's hardware in device health, idle Nodes
reporting every 30 s, and two bug runs (the new code since bug run 4, then the whole
codebase). No installers: Alpha88's are the newest.

### Added

- **What machine each one is**, for a site's machine list: `DeviceHealth::machine`
  (`DeviceMachine { cpu, cpus, gpus, os, os_version, kernel, boot_time, memory_total,
  memory_free }`) and `DeviceHealth::node_started`, as each machine's Node last reported
  them. Nodes report them in a new `<machine>` element (`idn_common::machine` reads them
  on Linux, Windows and macOS). `machine` is `None` for older Nodes.
- `Authority::clean_name` (a device name as the site keeps it) and
  `Authority::take_unclaimed_keys` (keys a relayed pairing's client took back as they
  were sent; the Site Manager's host revokes them).

### Changed

- An idle IDN Node no longer resends its whole report every 2 s: its uptime, CPU, GPU,
  free memory and services' running times count as a change only with the 30 s report
  (or while a matrix watches). CPU and GPU use are measured as a report is sent.
- `Enrollment::withdraw_key_request` on a request already answered: refused
  (`AlreadyExists`), the answer kept for `key_answer` (it was dropped, and a key with it).

### Fixed (bug run 2026-11-24)

- Channel, a device with more than one endpoint: a program's connection that only
  received was closed as idle after 5 minutes (only the announced endpoint's use
  counted); an announced endpoint not answering cost every call 5 s (now skipped for
  30 s while another endpoint is connected); a push refused by the Node never reached a
  program beside it; a fetch whose connection closed while awaiting the reply didn't try
  the next endpoint.
- Library cache: `want` / `pin` look at once on a library before v2 too.
- Relayed pairing: a key approved as its client took the request back is revoked (it was
  valid at the site and nobody's).
- IEM: turning the desk's `iem_control` off ends the sessions a `control` token opened.
- Site Manager: the approval name sent to the machine's Node is cleaned as the
  certificate's; `last_seen` / `node_started` no longer move by a second between looks.
- Machine info: `/usr/lib/os-release` fallback and single-quoted values; Windows lists the
  display adapters present on PCI (not removed or virtual ones).
- `LibraryServer::close` documents that background work stops at once.

### Fixed (bug run 2026-11-24, wider: the whole codebase)

- Security:
  - IDN Node: a program's name and path can't carry line breaks or tabs into the
    program rules (a program waiting for approval could add an "allow" rule for another
    program, taking effect at the next restart).
  - Channel: a file offered is served only if it is still the same file (Unix: device
    and inode), so a folder swapped for a link after `media_send`'s check can't send the
    Node's own files; a pushed file must match its offer's size and version.
  - Trust: five wrong admin codes use the code up; a device that claims admin keeps its
    first certificate's record (revoking it lists both), and applies later CRLs to its
    new credentials.
  - Library cache: files named by a hash for ids that aren't v2 ones (a v1 library's
    path ids could write outside the cache); library marks bounded (known ids, 10 000 a
    holder, 256 holders).
  - Open web pages (WHEP, listening): this machine's name is taken only in the
    system's search domains, not under any domain.
  - Compressed video (H.264/HEVC/AV1) has the input's picture-size limit, and the
    decoder refuses pictures above 8K from the stream's own description.
  - Site Manager: a user's tokens withdrawn include those issued in that same second.
  - IEM: a subscribe flood from many addresses no longer walks the rate table under the
    sessions lock at each packet.
- Correctness:
  - IDN Node: restart from the Site Manager works on a machine the site locked;
    rollback is refused while an update downloads or installs (it installed anyway).
  - Site Manager: a scheduled backup no longer writes back a schedule changed while it
    ran.
  - Display: thumbnails that answer after their asker gave up no longer stay in memory.
  - Trust: the site's transmitter list stays under half a trust message (one machine's
    long labels ended every asking machine's trust session).
  - Inputs: a control message cut short by the write timeout ends the connection (the
    next was read from its middle); a sync clock step hands held frames over first, in
    order.
  - Outputs: audio at a rate receivers don't play (outside 8–384 kHz) is refused with
    `SendError::BadAudio` instead of being dropped by every receiver.
  - Desk: an insert chain's latency is taken as at most a second (a plugin saying -1 had
    the clock thread ask for a 16 GiB delay line); the plugin host sums latencies
    saturating.
  - IDN Voice: a role change recomputes each channel's talk right (and ends a talk no
    longer allowed) instead of keeping it, and no longer drops password-channel members.

## [v0.3.1-Beta7] — 2026-11-24 (pre-release, source only)

Everything after `v0.3.1-Beta6`: an integrator's question on a taken API port.
No installers: Alpha88's are the newest.

### Added

- `idn_site_manager::host::Handle::api_error()`: why the API asked for (`Config::api_port`)
  isn't served, e.g. its port taken by another program. `start` still succeeds then (the
  site runs without its API, `api()` is `None`); no other port is tried. Documented on
  `Config::api_port`, `Handle::api` and in MEDIA-API §5.

## [v0.3.1-Beta6] — 2026-11-23 (pre-release, source only)

Everything after `v0.3.1-Beta5`: two integrator reports on Beta5, and no PTZ over MQTT.
No installers: Alpha88's are the newest.

### Fixed

- **The name given when approving a join** (`SiteCommand::Approve { id, name }`). The
  machine's certificate is now issued under that name, so the authority's
  `devices()`, `Handle::device_health()` and the site's directory show it, and the
  machine reads it with the new `Enrollment::name_in_site()`. A machine with an IDN Node
  is still named on its Node too. Library: `Authority::approve_as(id, name)`. Before, the
  name only went to a Node, so a program enrolled by itself kept the name it asked with.
  (An integrator's report on Beta5.)

### Added

- **`DeviceHealth::services`**: each machine's services as its Node last reported them,
  `DeviceService { id, name, state }`; `id` is what `SiteCommand::Restart` takes.
  (An integrator's report on Beta5.)

### Changed

- **No PTZ over MQTT** (idn-switcher, §S.33). The switcher's MQTT link no longer
  takes `ptz`, as a command or as a trigger's command: cameras move over the IDN control
  channel only, from the switcher's API, panel and cues. A trigger set with `ptz` is
  refused; one already saved isn't run (its status says why). `preview` over MQTT still
  recalls a preset input's camera preset, over the control channel as before.

## [v0.3.1-Beta5] — 2026-11-23 (pre-release, source only)

Everything after `v0.3.1-Beta4`: an integrator's report on Beta4. No installers: Alpha88's
are the newest.

### Fixed

- **`host::Handle::join_own_site` after a site restore.** A membership from another
  site is now set aside in `dir/old-site-ID/`, and the program joins this site anew with
  the same identity. "Another site" covers a site restored from another box's backup,
  or one made anew. Before, `join_own_site` returned the old membership, which stayed
  `Enrolled { connected: false }` and wasn't in the site's device list. (An integrator's
  report on Beta4.)

## [v0.3.1-Beta4] — 2026-11-22 (pre-release, source only)

Everything after `v0.3.1-Beta3`: two integrator reports on Beta3 (show boxes). No
installers: Alpha88's are the newest.

### Fixed

- **A device with two channels** (`channel`, §E.12.3). A machine's IDN Node and a
  program on it, or a program's own channel beside the one `Library` starts, are two
  endpoints with one device certificate. Before, the other device kept one connection per
  device. Each new connection replaced the last, closing it as "duplicate" and cutting any
  transfer under way, so a cache's large copy could be cut whenever the program's channel
  reconnected. A request to the device then went to whichever endpoint had connected
  last. Now:
  - a connection is kept per endpoint;
  - `Channel::request` asks the device's announced endpoint first, then its others,
    until one knows the type (`Reply::Unknown` only when none does);
  - `Channel::send` reaches each endpoint;
  - a push's file is fetched back from the endpoint that offered it.

  A library's machine can therefore send requests to a program on a show box with no
  change to the program.
- **`Cache::state` of an item just asked for** is `Waiting`, not `Missing`. `want(id)`, or
  a newly pinned id that the cache hasn't seen in the library, makes the cache look at
  once. Before, the item stayed `Missing` until the next look, up to 5 s.

### Added

- `Library::find_named_via`, `find_on_via` and `list_via` take the program's own
  `Arc<Channel>`, so the program has one endpoint toward the library's machine.
  - That channel's handler doesn't need to answer the library's requests.
  - On a machine where the IDN Node holds the machine's endpoint, start that channel
    `client_only`.

## [v0.3.1-Beta3] — 2026-11-22 (pre-release, source only)

Everything after `v0.3.1-Beta2`: integrator reports on Beta1, and keyed links removed. No installers: Alpha88's are the newest.

### Removed — action may be needed
- **Site-keyed library links**: `Library::keyed_link`, the IDN Node's
  `/lib/item/ID/NAME?key=…` and the `"keyed"` field of `idn.library.link`'s answer. A
  host program's long-lived `app:…` key crossed the network in plain HTTP, in the
  address. Use `Library::link(id)` (its own short-lived token), or serve previews from
  `path_of` on the library's own machine. `web::KeysTold` and
  `TokenCheck::from_enrollment_told`, added for them, are gone too.

### Added (integrator reports on Beta1)
- More endings taken as media: video mpg, mpeg, m2v, m2ts, vob, 3gp, 3g2, ogv, flv, f4v,
  dv, asf; pictures jfif, heif, avif, tga; sound ac3, mka, caf. `library::take_endings`
  adds a host's own.
- `LibraryServer::close()`: returns once its work has stopped and everything is saved.
  One instance on a folder at a time in a program: a second `open` waits up to 10 s for
  the first's end (one dropped while its worker held it saved its index over the next
  instance's).
- `Library::marked(holder)`, `LibraryServer::marked(holder)`, `idn.library.marks` with
  `"how": "list"`: a holder's marks read without a change.

### Fixed
- The library's `facts.json`, unreadable, is set aside and said, not saved over.

## [v0.3.1-Beta2] — 2026-11-22 (pre-release, source only)

Everything after `v0.3.1-Beta1`: integrator asks on Beta1 and an IEM change. No
installers: Alpha88's are the newest.

### Added (integrator asks on Beta1)
- `Library::list(member, timeout)` → `Vec<LibraryInfo { name, machine, fingerprint,
  items, v2 }>`, `Library::find_named(member, name, timeout)`,
  `Library::find_on(member, fingerprint, timeout)`, `Library::host_fingerprint`: a site
  with more than one library (a host program's box and an IDN Node) reaches the one it wants.
- `Enrollment::withdraw_key_request(ticket)`: a relay takes a waiting pairing back (trust
  `idn_key_withdraw`, IPC `key_withdraw`); the host gets `KeyRequestCancelled`.

### Changed — action may be needed
- **IEM:** a phone's `control` desk token is taken on the IEM flow only
  when the desk's new setting `iem_control` is on (`desk {"iem_control": true}`; off by
  default): the token crosses the network in plain UDP. `iem:<mix>` and `read` tokens are
  unchanged.

## [v0.3.1-Beta1] — 2026-11-22 (pre-release, source only)

Everything after `v0.2.0-Alpha98`, and the first Beta: the crates are version **0.3.1**
(a minor bump: the API changes below). A profile run, five bug reviews over the whole
codebase (details in `REVIEW-2026-11-22.md`), and dependency updates. No installers:
Alpha88's are the newest.

### Changed — action may be needed
- **Version:** the crates are 0.3.1; pin `tag = "v0.3.1-Beta1"`.
- **API:** `LibraryServer::open` refuses a library whose `index.json` can't be read
  (`Index::load`; `Index::open` sets it aside). `diag::tail_owned` is new. The site's
  authority refuses to start with an unreadable `options.xml`, as with `devices.tsv`.
- **Node:** while programs need approval, linking and joining from the command line
  (`idn-node link …`, `idn-node join`) need root / an elevated administrator, and `pairs`
  waits for approval. A panel a program opened can't create or join a site. A remote
  download of a loopback or link-local address is refused.
- **Push channels** (the Site Manager's, the Node panel's) close when their session or
  login ends. MQTT connections close when their login is removed or its password changes.
- **Library (§E.12.11.1):** adding or removing an indexed folder (`roots`) and changing
  the trash's days (`settings`) over the site channel need a site administrator
  (`Peer::admin`); listing stays open. `LibraryServer::add_root` (a host's own call) is
  unchanged.
- **Relayed pairing over the Node's IPC** (`key_request`, `pairing_code`, `key_answer`,
  `key_used`) waits for the program's approval like the identity's other uses, and a
  ticket is answered only to the program that asked (by its program file).
- **Node panel:** `idn-node open` now opens a *limited* session while programs need
  approval; the full one comes from `idn-session --open` (the menu entry) or an
  administrator. The site's Site Manager settings (`sitemanager.*`) over IPC need an
  administrator, and its password is 8 characters at least.
- **Switcher:** saved stream destinations get random ids (`d` + 12 hex digits; earlier ids
  keep working). `camera` is no longer an MQTT command (setup, behind the password).
- **Site Manager:** setting the site password for the first time also withdraws the
  built-in administrator's engine tokens.
- **Spec numbering:** the site's media library is §E.12.11, the site's transmitters
  §E.12.12.
- **Certificates:** with rcgen 0.14 a site's CA certificate encodes its key usage as DER
  requires; its bytes change once, its key, name and every fingerprint don't. Device
  certificates are unchanged.

### Performance (profile run; a library of 20 000 files)
- Looking at the folder 1.4 s → 70 ms, walked without the index held; the catalog
  1.43 s → 67 ms (no second walk), written straight to text (183 ms → 5.5 ms);
  `idn.library.item` 1.43 s → 40 µs.
- A show box's `Cache` follows the library's `changes` instead of reading the whole
  catalog every 30 s.
- Keyed links tell the site a key's use once a minute (it was every range request).

### Fixed (highlights; about 90 in all, see REVIEW-2026-11-22.md)
- Security: a program waiting for approval could approve itself (`idn-node open`) or
  take a relayed pairing's key; a limited panel session could reset the site
  administrator's password; the panel's PIN was written to the audit in clear; a rogue
  authority could guess a typed join code many times in one join; a revoked machine's
  trust session went on being served; an unreadable API-keys file made revoked keys valid
  again; the Node's log-file list could be steered to other users' files.
- Robustness: the MQTT broker could be made to queue without bound (retained flood,
  unacknowledged messages, a 16 MB CONNECT before login); QUIC bodies were allocated at
  the length a peer claimed; one slow receiver could stall every output's frames; idle
  trust relays held their slots; a capture card that lost its signal hung its stop.
- Saved state: many settings files were read as empty after an error and then saved over
  (desk presets, switcher shows/templates/MQTT/destinations, Industrial pins and SQL
  cursors, Node pools); they are now set aside and said, or refused.
- Library: an upload's folder can no longer leave the library on Windows (`C:/…`); the
  library's own folder out of reach no longer removes every item; two uploads of one name
  no longer overwrite each other; partial uploads are cleared.
- Concurrency: two sender locks taken in opposite orders; the Node's link listener now
  stops with the Node; site-link sessions close their connection.

### Dependencies
- tungstenite / tokio-tungstenite 0.30, argon2 0.6 (same defaults: existing password
  hashes and backups check the same), hkdf 0.13, sha2 0.11, rcgen 0.14, libloading 0.9,
  sysinfo 0.40, odbc-api 30; 64 compatible updates.

## [v0.2.0-Alpha98] — 2026-11-22 (pre-release, source only)

Everything after `v0.2.0-Alpha97`. No installers: Alpha88's are the newest.

### Added — media library v2 (a host program's library on IDN; idn-extensions.md §E.12.11.1)
- `library::index` holds lasting item ids and SHA-256, changes, marks and the trash.
  `library::server::LibraryServer` is embeddable: the IDN Node runs it, and a program
  hosting the Site Manager runs it with `serve(member)`. `library::cache::Cache` is a show
  box's cache: pins, states with progress, a size cap.
- The client `Library` gains `item`, `changes`, `upload` (deduplicated by SHA-256),
  `delete`, `delete_in_use` (the name typed back), `trash`, `restore`, `marks`, `space`,
  `set_trash_days`, `add_root`, `remove_root` and `keyed_link`.
- `idn_ffmpeg::media::library_facts`, and `MediaInfo::audio_tracks`. IDN Data's metadata
  gives `alpha` and `audio_tracks`.
- `trust::token::engine_of` (unverified; picks the check).

### Changed — action may be needed
- Library `Item::id` is now the item's lasting id, not its path; `Item::path` and
  `Item::sha256` are new, as are `alpha` and `audio_tracks`. `lib:PATH` still fetches.
- `MediaInfo` has a new field, `audio_tracks`.

### Fixed
- (Found in this round's tests, never released) the IDN Node's media module held the
  machine's site membership for checking keys on links; after the Node rebuilt its
  membership, the old one stayed connected and the site's requests to the machine went
  unanswered. It now asks the Node for the membership each time.

### Added (integrator asks on Alpha97)
- A relay is told at once when a typed pairing code was wrong:
  `Enrollment::key_status(ticket)` → `trust::KeyStatus::Waiting { code_wrong }` or
  `Answered(KeyGrant)` (`key_answer` is unchanged). Trust protocol `idn_key_status`; IPC
  `key_answer` adds `code="wrong"`.
- `Enrollment::pairing_code_left(timeout)` → `(code, until, left)`: the seconds left, for a
  relay whose clock is off. `Authority::pairing_code_left`.
- `Authority::set_pairing_code_life` (hidden, tests), as `set_key_request_life`.

## [v0.2.0-Alpha97] — 2026-11-22 (pre-release, source only)

Everything after `v0.2.0-Alpha96`. No installers: Alpha88's are the newest.

### Added
- **Pairing codes from the site**: `Enrollment::pairing_code(timeout)`
  (IPC `pairing_code`; trust protocol `idn_pairing_code`), `Authority::pairing_code_for`.
  A relayed request typed with its relay's code is marked `code_ok`
  (`KeyRequest`, `AuthorityEvent::KeyRequested`, `SiteEvent::KeyRequested`, push channel),
  checked by the site; a right code is used up, and 5 wrong tries burn it.
- **`host::Handle::join_own_site(dir, name, role)`**: a service on the Site Manager's
  machine joins its site at once.
- **Zoom transition** (`zoom`, switcher §S.4): the preview grows from the centre over the
  program, eased in and out; CPU and GPU.

### Removed
- The appliance code (`appliance/`): IDN no longer makes an appliance.

### Changed — action may be needed
- `AuthorityEvent::KeyRequested`, `SiteEvent::KeyRequested` and `KeyRequest` have a new
  field, `code_ok`.

## [v0.2.0-Alpha96] — 2026-11-20 (pre-release, source only)

Everything after `v0.2.0-Alpha95`. No installers: Alpha88's are the newest.

### Changed — building and testing this repository
- Each crate's integration tests are one test program, `tests/all/main.rs`, with a module
  per former file (76 programs became 10). nextest filters name them
  `binary(all) & test(/^module::/)`. A full test build linked and wrote so much that the
  shared host's disk stalled its other services. A full run now spends 1% of its time
  with the disk stalled (it was ~26%), and the debug build takes 278 s (it was 654 s).
- `[profile.dev.package."*"] debug = false` (our crates keep line tables). The test
  script builds without incremental caches, under a 4-core CPU ceiling, at low priority,
  holding the host's shared `/tmp/heavy-build.lock`.

### Fixed (integrator reports on Alpha95)
- **A new named key could vanish from the list** (about 1 run in 3): noting a key's use
  and making or revoking one each read the keys' file, changed it and wrote it back, so
  one write undid the other. A key so lost still verified and couldn't be revoked. Every
  change to the file now takes one lock.
- **A relay is told when its request ends unanswered**: the site refuses it ("ended
  unanswered") when its 10 minutes pass. Expiry is checked each second by the Site
  Manager's host, and on listing or answering. A relay that loses its connection marks
  its waiting tickets refused, so `key_answer` never stays `None` forever.
  `Authority::expire_key_requests` is public.
- **`Enrollment::tell_leaving()` errs when not connected** (`NotConnected`): nothing
  would have reached the site.
- **No latency step ~2 s after a lane's first start**: the start's check now leaves a
  level within a source block and a device block of its aim as it is. That difference
  is the start's timing, and `heard_at` already says the true latency. Correcting it was a
  2–7 ms cut or pad at the first quiet moment (3 real-card runs in 10). Larger bursts and
  a source coming back after a pause are set right as before.

## [v0.2.0-Alpha95] — 2026-11-20 (pre-release, source only)

Everything after `v0.2.0-Alpha94`. No installers: Alpha88's are the newest.

### Added — pairing relayed through another machine (idn-node.md §N.42.3)
- Relay side: `trust::Enrollment::request_key(client, pin)` and `key_answer(ticket)`
  (through the IDN Node too: IPC `key_request`, `key_answer`). Trust protocol:
  `idn_key_request` and `idn_key_result`.
- Site side: `Authority::key_requests`, `answer_key_request`, and the events
  `AuthorityEvent::KeyRequested` and `KeyRequestCancelled`. On the host:
  `host::Handle::key_requests`, `approve_key_request`, `deny_key_request`, and
  `SiteEvent::KeyRequested` / `KeyRequestCancelled` (push channel `key_requested`,
  `key_request_cancelled`, without the PIN).
- `host::Handle::api_keys()` → `Vec<ApiKeyInfo>`.
- Leaving tells the site: `Enrollment::tell_leaving()` (`idn_leave`), and the IDN Node's
  leave sends it first. `AuthorityEvent::Left`, `SiteEvent::DeviceLeft` (push
  `device_left`): the machine is no longer listed and may join again.
- Keys with a shorter life: `SiteCommand::ApiKey { days }` (1–3650),
  `Authority::issue_key_lasting`. Keys past their life drop out of the list and their
  revocations are forgotten.

### Changed — action may be needed
- At most 500 active named API keys (was 64; revoked ones never counted).
- `SiteCommand::ApiKey` has a new field, `days` (`None`: 10 years, as before).
- `AuthorityEvent` has new variants (`KeyRequested`, `KeyRequestCancelled`, `Left`): an
  exhaustive `match` needs them.

## [v0.2.0-Alpha94] — 2026-11-20 (pre-release, source only)

Everything after `v0.2.0-Alpha93`. No installers: Alpha88's are the newest.

### Added
- `host::Handle::verify_token` notes a named API key's use (`last_used`, `last_machine`
  in the Site Manager's list), at most once a minute a key, as the Nodes report keys used
  on other machines.

### Fixed
- Several Site Managers in one process (a host's parallel tests), stopped from tokio's
  blocking threads: IDN Voice's thread panicked ("Cannot drop a runtime in a context where
  blocking is not allowed"). Voice's tasks held the Site Manager's state, and the last of
  it, with the MQTT broker's runtime, went on Voice's threads. Voice now holds it weakly,
  and the Site Manager's runtimes (Voice's, the broker's) shut down safely wherever they
  are dropped.
- An API key revoked in the second it was made still worked: its "not before" is now a
  second on.

## [v0.2.0-Alpha93] — 2026-11-20 (pre-release, source only)

Everything after `v0.2.0-Alpha92`: the gaps an integrator found embedding the Site Manager. No
installers: Alpha88's are the newest.

### Added — the Site Manager in a host
- **Typed site events:** `idn_site_manager::host::Handle::events(since)` and
  `events_wait(since, timeout)` give `SiteEvents { events, next, missed }`, each a
  `SiteEventEntry { seq, at, level, text, event: SiteEvent }`: join requested, cancelled,
  denied, refused, withdrawn; device joined, removed, online, offline; join code used,
  expired, burned, tried wrong; and `Other` (the roll-up's and the machines' events). The
  push channel carries the new ones too (`join_requested`, `join_cancelled`,
  `device_joined`, `join_withdrawn`, `join_code_used`, `device_online`,
  `device_offline`). `web::problems::Events::since_numbered` / `wait` underneath.
- **The caller in the audit:** `Handle::run_from(user, &Caller { address, client }, cmd)`
  and `issue_token_from(user, &Caller, …)`.
- **Typed machine health:** `Handle::device_health()` → `Vec<DeviceHealth>` (online, last
  seen and build kept after a machine goes offline, services restarting, an update
  running, its problems).

- **Tokens for a host's own API:** the audience `app:<name>` (`app:myapp`) beside the
  engines (`token::APP_PREFIX`, `token::engine_ok`); the Site Manager issues them, and
  `host::Handle::verify_token(token, app)` checks one (signature, audience, life, the
  user's tokens withdrawn).
- **Voice optional:** idn-site-manager's `voice` feature (default on); off, the crate
  builds without IDN Voice's WebRTC stack (str0m, aws-lc-sys: no cmake). `host::Config::voice` turns it off at run
  time; `SiteManagerPanel::start_with(…, voice)`.

### Fixed
- The Site Manager's audit wrote no `client` for changes over HTTP (`X-IDN-Client`): it
  does now, as the engines' audits do.
- Dropping a hosted Site Manager's `Handle` without `stop()` inside async code (a task, a
  `#[tokio::test]` unwinding) panicked on IDN Voice's thread ("Cannot drop a runtime in a
  context where blocking is not allowed"): Voice's runtime is let go to end by itself
  there.
- The site password takes 8 characters or more, as users' passwords (it took 6).
- A new user's name is letters, digits and `. _ - @` (`a b/c` was taken); users made
  before keep theirs.

### Changed — action may be needed
- `idn_site_manager::host::Config` has a new field, `voice` (`true` for the Site Manager
  as before).

## [v0.2.0-Alpha92] — 2026-11-20 (pre-release, source only)

Everything after `v0.2.0-Alpha91`: the problems an integrator's port tests found. No installers:
Alpha88's are the newest.

### Fixed
- **`AudioReader` plays keep their length** (port 1): where the sound ends before the out
  point (a soundtrack shorter than the container), silence fills the play, so play `n`
  starts at `n × (out − in)` whether the caller seeks or reads straight through (it
  started where the sound ended, drifting by the gap each loop).
- **`AudioReader::read` gives every sample up to `length()`** (port 2): the resampler's
  last samples (a WAV was 17 frames short) and the silence to the end.
- **MP3 is sample-accurate** (port 3): times count from the file's first timestamp, as
  FFmpeg's tools do (an MP3's started 25 ms late: its encoder delay), a gap before the
  first sound is silence, a seek decodes from half a second before (no warm-up silence),
  and after a seek the resampler starts where the file's samples and the output's
  coincide (it was a fraction of a sample off). WAV, AAC and MP3 match FFmpeg's decode.
  `VideoReader` and `thumbnail` count from the same start.
- **`thumbnail` gives its picture's time** (port 6): `time` and `file_time` (they were 0).
- **A lane starts at its level** (audio 1): a source pushing evenly (in real time) starts
  at once, silence before its first block making up the level, so a sample's latency is
  the same from the first block on; nothing it pushed is cut (a new lane cut its first
  block, started a block over its level, and cut ~8 ms at a quiet moment ~2 s later: a
  step in the lip-sync). A route's uneven first blocks still wait 40 ms.
- **`Lane::latency()` / `heard_at()` are exact whenever asked** (audio 1): they count the
  time until the device takes its next block (they read up to a device block low, by
  when in its period they were asked), and before the lane plays they give the level it
  will start at (they rose from 0).
- **A busy sound card is `Busy`, not missing** (audio 2): `find`ing a device no longer
  opens it; `probe_device`, `Playback::start` and `Capture::start` report one another
  program holds as busy (`io::ErrorKind::ResourceBusy`; `PermissionDenied`, `NotFound`
  likewise).
- **Probes are quick and safe together** (audio 3): one at a time, the device opened in its
  default format and closed without starting (tens of ms, was ~0.55 s); a missing one's
  message is one line with a count (it listed every PCM).

### Added
- `VideoReader::decoding()` (`media::Decoding`: the GPU decoder in use, or software and
  why) (port 4).
- `VideoInfo::end` / `AudioInfo::end`: where each stream itself ends (port 5; a play's
  default end stays the file's duration, the same for picture and sound).
- `DeviceInfo::busy`: a device another program holds, still listed.

### Changed — action may be needed
- `VideoInfo`, `AudioInfo` and `DeviceInfo` have new fields: a struct literal of them
  needs them (they are returned, not usually built).
- `Lane::latency()` is up to a device block (5 ms at a 20 ms latency) more than before:
  the true value. A lip-sync offset tuned against the old one can drop that much.

## [v0.2.0-Alpha91] — 2026-11-19 (pre-release, source only)

Everything after `v0.2.0-Alpha90`. No installers: Alpha88's are the newest.

### Added
- **Decode capabilities** (integrator gap 5): `idn_ffmpeg::media::capabilities` (the GPU
  decoder and its codecs, FFmpeg's software ones, FFmpeg's and its libraries' versions).
- **Sound-card health** (gap 7): `Playback::status()` / `Capture::status()`
  (`DeviceStatus`: running, failing, lost, with the last error's words) and
  `on_status(f)` (called at each change). `Mixer::errors()`.
- **Sound-card probe** (gap 8): `idn_audio::probe_device(name, Direction)`: opens now or
  not, why not (missing, busy, no permission, other), the most channels.

### Fixed
- idn-ffmpeg's README said x264 is never used: it is H.264's last CPU fallback where the
  machine's own FFmpeg has it and there is no GPU encoder or OpenH264 (gap 9; kept). Decoding never uses an encoder.

### Changed — action may be needed: idn-ffmpeg's features (gap 11)
- New default features `capture`, `whep` and `voice`. With `default-features = false`
  the crate leaves out `capture`, `remote`, `wayland`, `whep`, `voice_feed` and
  `voice_out` and their dependencies (idn-input, wayland-*, ashpd, tokio, str0m,
  tungstenite): a decode-only build. Default users are unchanged; the `idn-codec` tool
  needs all three.

## [v0.2.0-Alpha90] — 2026-11-19 (pre-release, source only)

Everything after `v0.2.0-Alpha89`. No installers: Alpha88's are the newest.

### Added
- **Media files in-process** (`idn_ffmpeg::media`, integrator gaps 1–4): `VideoReader`
  (pictures for any time on the caller's clock, decoded ahead; in/out points, plays on one
  timeline, exact seek that can wait), 10-bit sources as 16-bit samples (`PixelFormat::P010`,
  `Yuv420p16`) with `bit_depth`, `AudioReader` (a file's sound as float at a chosen rate
  and channel count, read at a sample position), `probe` and `thumbnail`.
- **When a pushed sample is heard** (integrator gap 6): `Mixer::output_delay()` /
  `idn_audio::Playback::output_delay()` (the device's buffer and latency from its
  callback timestamps), `Lane::latency()` (that and what the lane holds) and
  `Lane::heard_at()`.

### Changed — action may be needed: FFmpeg bindings 9
- idn-ffmpeg is built on `ffmpeg-sys-next` 9 (was 8), so it builds beside `ffmpeg-next` 9
  in one program (both link `ffmpeg`). Any FFmpeg from 3.0 to 9.0 still works (the
  Windows kits' 7.1 and 8.1 checked). Cross-building for Windows with MinGW, bindings 9
  read libavutil's D3D11/D3D12 headers, which reach clang's AMX tile type: the repository's
  `.cargo/config.toml` adds `-D__X86INTRIN_H` to `BINDGEN_EXTRA_CLANG_ARGS_x86_64_pc_windows_gnu`
  (a program of your own cross-building the same way needs it too).

## [v0.2.0-Alpha89] — 2026-11-19 (pre-release, source only)

Everything after `v0.2.0-Alpha88`. No installers: Alpha88's are the newest.

### Added
- **Each desk input's source, typed** (an integrator's ask): the desk state's `input_sources`
  (`idn_mixer::engine::InputSource`): an IDN route's transmitter, channel and machine, and
  where its sound starts (`origin {kind, device, channel}`); a network-audio ring input's
  device and the device and channel it subscribes to; a sound card's input; an IEM
  talkback. The site's transmitters carry their origins (`<idn_site_transmitters>`
  `origin_*`, `SiteTx::origin`); idn-aoip's IDN source says where each channel comes
  from (`DEVICE/CHANNEL`, kept up with its subscriptions).
- **Events:** the display service's `decoder_trouble` (pictures failing to decode, or GPU
  → CPU decoding, while playing; the screen's `stats.decode_errors`); the Node's
  `join_code_wrong` (`tries_left`, at each wrong try) and `already_in_site` (a join code
  on a machine already in a site, refused).

### Changed
- `trust::SiteTx` is a struct (`name`, `channels`, `machine`, `origin`), was a
  `(String, u16, String)` tuple.

## [v0.2.0-Alpha88] — 2026-11-19 (pre-release)

Everything after `v0.2.0-Alpha87`.

### Added
- **Desk processing (2026-11-19):** gate `hold`, `mode` (gate, expander, ducker), `ratio`,
  `key` (another channel) and key filters; compressor `mix` (parallel), `detect` (peak or
  RMS), `auto_release`, `key`, `key_hpf`; high/low-pass slopes 6–48 dB/octave; send
  `tap` points (input, pre-EQ, post-EQ, pre/post fader); `dim_db`; talkback `dim_main`
  and `to_iem` (into the IEM flow while talking); DCA and mute-group colours;
  `channel_reset`; recall `parts` and `fade_s`; `snapshot_order`; each IEM mix's `who`.
  Then: noise suppression `denoise_amount`; `insert_point` (pre or post the
  processing); EQ shape `veq`; `channel_count` and `restart`; the routing view's
  network sources with their channel counts (`<idn_site_transmitters/>`, §E.12.12;
  `Enrollment::site_transmitters`; the Node's `<site_transmitters/>`).
  The round trip: the state's calculated `latency` and `latency_test {output, input}`
  (a click looped back and timed).
- **Display service (2026-11-19):** screen `mode` from its `modes`, `idle: fallback`,
  `test_pattern`, `restart`, `label` (identify shows it with the host and IPs),
  `feed_by`/`feed_at`, `lan {on}`, `sound_cards`, `dropped_last_min`, problems
  `screen_smaller` and `dropping`.
- **Node and Site Manager (2026-11-19):** leave the site (`site/leave`, `<site_leave/>`,
  `idn-node leave`, `NodeCore::leave_site`); `site_devices` in the status (the authority
  sends the count with the directory, `Enrollment::site_devices`); programs' logs
  (`GET /api/v1/logs`); typed `host::site_status` / `notices_since` / `problems_now`;
  scheduled site backups (`backup_schedule`, `backup_now`, the status's `backups`);
  daily settings copies in the desk's, switcher's and display service's folders
  (`idn_common::fsutil::daily_settings_copy`).
- **Leftovers (2026-11-19):** the switcher's `format {format}` (kept for the next start;
  under the IDN Node it starts again in it at once; the state's `format_next`;
  `service::saved_format`); scene layers' `radius` (rounded corners, both compositors)
  and `key: none` (a picture's alpha ignored); `gpu_percent` in the switcher's state
  (`idn_common::gpu_load`) and in the Node's report and the Site Manager's machines;
  named API keys' `last_used` / `last_machine` (engines report a key's use through their
  Node: `Enrollment::report_key_use`, `AuthorityEvent::KeyUsed`); checking for updates
  now (`POST /api/v1/update/check`, `<update_check/>`, `NodeCore::check_updates`,
  `AuthorityEvent::UpdateCheck`; the Node's `update.available`, the Site Manager's
  `node.update`); the switcher's state typed below its top level
  (`state_schema::SwitcherState` and its parts, `state_schema::undeclared`); the Site
  Manager on the shared API front (`GET /api/v1/state` with `?after=`, `state_schema`,
  per-control sequence numbers over HTTP; the status's `version`; `web::Who::session`);
  the profile scripts on today's switcher API (scenes on the buses, global layers).
  Then: GPU % on Windows (the `GPU Engine` performance counters, the program's own and
  the machine's); the Node panel on the shared API front (`web::engine`: sequence
  numbers over HTTP, `GET /api/v1/state` with `?after=`, `GET /api/v1/schema`).

- **Switcher (integrator requirements, 2026-11-18):** a mix that **follows the program**
  (`add_mix {format, follow: true}`): the program's scenes and transitions centre-cropped
  to its shape, for "also make a vertical version" (its own buses refused; `mixes[].follows_program`);
  **saved stream destinations** (`destination_save {name, body}` → `{id}`,
  `destination_remove`, `go_live {ids}`; keys kept by the switcher, the state's
  `destinations` without them); `streams[].live_since` and a recording's `disk_free` /
  `disk_total`; **scene thumbnails** `GET /api/v1/scene.jpg?id&w` (drawn whether on a bus or
  not; its inputs come into use for 5 s); a scene's **desk snapshot recalled as it goes
  live** (`scene_snapshot {id, desk, snapshot}`, kept by label); `scene_layer_visible`
  takes a `fade` for this once; `captions {on, layer}` also shows or hides a global
  captions layer in every scene (one CC button); layers keep a `locked` flag;
  `cpu_percent` in the state.
- **Site Manager and Node (integrator requirements, 2026-11-18):** named long-lived **API keys**
  (admins: `api_key {name, aud, scope}` → `{id, key}` once, `api_key_revoke {id}`,
  `GET /api/v1/api_keys`; a key lives up to 10 years and stops at once when revoked;
  `last_used` not filled yet); **release tracks** (`product@beta` when a release is made;
  a machine's device option `update.track`; a beta release goes to beta machines only) and
  **other products' releases** handed to the program hosting the Node
  (`idn_node::host::on_host_release`); `restart {fingerprint, service}` (admins) and
  `identify {fingerprint, seconds}` (operators) on a machine; `approve {id, name}` names it
  once in; `pending[].asked_at`; machines' `uptime_s`, `cpu_percent`, `last_seen_s`.
- **Joining: no "the code matches" on the machine any more**: the
  Site Manager's approval is enough (the code still shows on both to compare).
- **The desk's stream bus, output endpoints, 16 DCAs, a clock chosen by hand** (integrator
  requirements, 2026-11-18):
  - a dedicated stereo **Stream** bus beside main: each channel's `stream` send (on at
    0 dB after its fader on a new desk, so it starts as the main mix), its own EQ,
    dynamics, limiter, inserts and fader, its own loudness meter (`meters.stream_lufs`);
    `bus {bus: "stream"}`, `send {n, bus: "stream"}`, and the `bus:stream` token scope now
    means this bus (`bus:name:NAME` still finds a bus by name). Its outputs are
    `bus:84`/`bus:85` in the output patch;
  - named **output endpoints**: `endpoint_add {name, outputs}` → `{id}`, `endpoint`,
    `endpoint_remove`; each is its own IDN output `"<desk> <name>"` (role
    `desk-endpoint/1`), listed in the state's `endpoints` with its source name;
  - **16 DCAs** (were 8);
  - the desk's **clock** chosen by hand: `desk {clock: "auto" | "card" | "site" | "own"}`
    (one not available falls back to the automatic choice); the state's `clock.chosen`.
- **Failure states as typed events, problems and the site's log** (integrator requirements;
  spec idn-node.md §N.41, idn-switcher.md §S.37, idn-display.md
  §D.8.5; docs/MEDIA-API.md "Events and problems").
  - Every engine's push channel carries `{"type": "event", "event", "level", "text", …}`;
    states gain `problems`; the same events reach the Site Manager's log through the Node
    (alerts with `event` and `detail`), the same one about the same thing once a minute
    at most (`repeats`). Not MQTT. Library: `web::problems` (`Events`, `LogForward`,
    `loss_cause`), `Node::event`, `web::events::run_as_requests_with`.
  - Switcher: `lost` (reason, cause), `other_site`, `reconnecting` (attempt),
    `recovered`, `gone`, `renamed`, `format_mismatch`, `camera_unreachable`,
    `desk_link_lost`/`desk_link`, `overloaded` (compositor or encoder),
    `stream_auth_expired` (the stream's `reason`), `stream_failed`, `dropped_frames`;
    inputs' `lost`/`attempts`, feeds' `linked`, streams' `reason`/`overloaded`;
    `GET /api/v1/events/recent`.
  - Display: `other_site`, a loss's `cause`, `level`/`text` on every event, `problems`;
    the failures to the site's log.
  - Node: `status.problems` and events for a join denied, revoked, a code expired or
    burned, the Site Manager unreachable (15 s), two sites visible, the site clock's lock
    lost; programs' events on its push channel; `<problems>` in its report.
  - Site Manager: `status.attention` (offline, a failed service, an update failed or
    rolled back, a version or build other than its own, the clock unlocked, the Nodes'
    own), `status.versions`, `status.rollout` (sent, ok, failed, waiting); events for
    those, site monitoring's conditions, joins denied or refused, codes wrong, burned or
    expired, and the machines' events; `GET /api/v1/events/recent`.
  - `denied`: a command refused (403) on a push channel is followed by this event on
    that caller's channel.
  - Listening on phones: a listener whose connection was lost counts in its channel's
    `dropped` and is a `listener_dropped` event.
  - Desk (spec idn-mixer.md §M.27): `lost`/`recovered` for routed sources (with
    `cause`), `failover` (a backup input took over, and back), `plugin_failed`,
    `recorder_failed`, `disk_low`, `x32_unreachable`, `card_lost`, `overloaded` (the
    DSP over its time; the state's new `slow_blocks`), `iem_phone_dropped` (mix,
    peer); `problems` in its state; `denied` on its push channel (read-only, outside a
    token's bus or IEM mix, locked); `GET /api/v1/events/recent`;
    `Console::{failure_events, problems, watch_problems}`. A bus's or an IEM mix's token
    gets no typed events but its `denied`.
- **Audio: who receives what, and how** (spec idn-routing.md §A.16).
  - Each route's `via` (`machine`: the shared-memory ring; `network`), `cast` and
    `transport`, from the router's blocks (`route_state` `via`).
  - Receivers' kind: `AudioReceiver::register_as(…, ReceiverKind)` (`program`,
    `sound_card`, `asio_driver`, `network_device`, `desk`, `switcher`; `other` when not
    said); idn-audio, the ASIO driver, OBS, idn-aoip, the editors' returns and the
    switcher say theirs.
  - Transmitters' origin: `Output::set_audio_origin` (`idn-audio send` says its card and
    inputs); a pool's source from its inputs (`DEVICE/CHANNEL`). The desk's receiver is
    a `desk`; its output's origin is the desk and each output's bus or channel name
    (said again on a rename), its IEM output's the IEM channels.
  - The matrix's transmitters have `origin` and `received_by` (machine, program,
    receiver, kind, channels, via); `GET /api/v1/audio_receivers`.
- **Desk: in-ear monitoring for phones** (spec idn-mixer.md §M.26 and
  the new idn-iem.md; docs/MEDIA-API.md §4).
  - A channel's `iem: true` puts it in the IEM flow: after its processing, before its
    fader and mute; a linked pair as one. The desk mixes nothing and uses no aux bus.
  - The flow goes out as an IDN audio source `HOST (<desk> IEM)` (role `iem/1`,
    multicast for receivers that ask), and for phones as one Opus stream a channel
    (mono, 48 kHz, 5 ms, 64 kbit/s) in UDP packets, multicast (port 29253) and unicast
    (port 29224; the network desk's 29225), coded only while a phone listens.
    `GET /api/v1/iem` describes it.
  - Phones' sessions need a site token from the network (`iem:<mix>`, `read`,
    `control`); at most 32 unicast phones (`IemConfig::max_phones`); keepalive 1 s,
    timeout 5 s; requests bounded; the audit has them.
  - IEM mixes saved with the desk (`settings.iem_mixes`, at most 16): `iem_mix_add`
    (answers the id), `iem_mix` (partial), `iem_mix_delete`; the state's `iem`.
  - `iem:<mix id>` tokens change that mix alone and read the IEM channels and their mix.
    `iem_mix` works while the desk is locked.
  - Snapshots: `snapshot_save {"include_iem": true}` holds the mixes and marks; its
    recall puts them back; without it a recall leaves them.
  - Talkback: a phone's push-to-talk plays on its mix's talkback input, one of 16 desk
    inputs after the routed ones (the state's `inputs` counts them).
  - `send {n, bus, level?, on?, pan?, pre?}`: one channel's send into one aux bus (a
    bus token's into its own).
  - New crate `media/idn-iem` (the wire format and Opus coding, shared with the phone
    library); new dependency `opusic-c` 1.6.1 (libopus built from source, BSD-3-Clause).
    Ports `idn_common::ports::{IEM, NET_DESK_IEM, MCAST_IEM}`. Program: `--iem-port`,
    `--no-iem-net`. Library: `ConsoleConfig::{iem, token_check}`,
    `Console::{iem_mix_add, iem_mix, iem_mix_delete, send, iem_description,
    snapshot_save_with, command_answer_as}`, typed `Command::{IemMixAdd, IemMix,
    IemMixDelete, Send}`.
- **The phone mixing library** (spec idn-iem.md §I.8;
  `media/idn-iem/README.md`): what a musician's phone app embeds.
  - `idn_iem::receiver::Receiver::connect(Config)`: the desk's description, a UDP
    session (unicast, multicast, or auto: multicast with unicast as the fallback;
    keepalives; subscribed again on UNKNOWN or silence), the push channel; HTTPS with
    the desk's certificate or the site CA pinned (or a SHA-256 fingerprint); refusals
    as typed `Error`s (`TokenNeeded`, `TokenRefused`, `Full`, `NotAllowed`, `Tls`, …).
  - A jitter buffer by seq (starts at 15 ms, grows on late frames, shrinks when spare,
    10–100 ms), one Opus decoder per channel in the mix, lost frames concealed.
  - `render(&mut [f32])`: the mix, stereo 48 kHz, by level, pan (a pair's balance),
    mute, solo and master, gains gliding over each 5 ms frame.
  - The mix followed live (`mix()`, events `MixChanged`); the phone's changes
    (`set_level`, `set_pan`, `set_mute`, `set_solo`, `set_master`, `set_in_mix`) sent
    as partial `iem_mix` commands with per-control sequence numbers.
  - Push-to-talk: `talk_start`, `talk_feed` (any rate and channels), `talk_stop`.
  - `status()`: transport, buffer, loss, round trip, latency estimate, counters.
  - C ABI (`media/idn-iem/include/idn_iem.h`, `idn_iem_*`, JSON for structured data);
    feature `play` (cpal output and microphone); `idn iem listen` in the `idn` command.
  - New dependencies of `idn-iem` (feature `receiver`): `rustls`, `ring`, `tungstenite`,
    `socket2`, `serde_json` (all already in the workspace); `cpal` with `play`.
- **The display service** (idn-display.md §D.8, docs/MEDIA-API.md
  §9).
  - `idn display --service`, one per computer, run by the Node in the desktop session
    when `apps.display` is on. Each screen given a **feed** (a source, or a routing
    source it follows) runs an `idn-display --commands` on it; a feed change goes to
    that process, the picture held until the new feed's first one (no black, no
    restart).
  - API on 127.0.0.1:29222 (open) and on the network at 29223 (HTTPS, site tokens of
    the engine `display` only; read-only tokens refused changes), audited:
    `state` (screens, feeds, status, receiver figures, settings), commands `feed`,
    `settings`, `identify`, `forget` (typed, `GET /api/v1/schema`), `thumbnail` (JPEG),
    the push channel with typed events (`lost`, `reconnecting`, `fallback`,
    `recovered`, `playing`, `format`, `routed`, `screens`, `stopped`).
  - Per screen, kept by its name in `screens.json`: rotation, overscan, fit / fill /
    stretch, the sound (card, channels, level), the fallback (the last picture held,
    then the site's logo after 5 s by default; black, last or a slideshow; a reconnect
    cap) and a video wall's tile (rows, columns, bezels in mm) with `sync/1`
    presentation in step.
  - Library: `InputStats::retries` (failed attempts since the source was last
    connected); `idn_common::jpeg` (the switcher's JPEG encoder, for planes or RGBA).
  - Ports 29222 and 29223 join IDN's block (`idn_common::ports::DISPLAY`,
    `DISPLAY_LAN`).
- **Desk: an X32's preamps over Ethernet** (spec §M.23.1).
  - With the X32's X-USB card as the desk's card, the desk reads and sets the gain, 48 V
    and X32 polarity of the X32 input behind each channel. It follows the X32's card
    routing, and changes made on the X32 show live.
  - Setting `x32 {address}` (`""`, `auto` or an IP address; `--x32`), kept with the
    desk but not in snapshots.
  - State `preamp` (per channel, and on each channel strip) and `x32`.
  - Command `preamp {n, gain_db?, phantom?, invert?}`; OSC
    `/ch/N/preamp/gain|phantom|invert`.
  - Library `idn_mixer::x32`.
  - The protocol comes from public documentation only.
- **Switcher setups keep the program's live setup:** `setup_save` also keeps the
  program's transition (a luma wipe's pattern by label), the 8 stingers, the global layers
  (`global-layers.json`), the multiview (`multiview.json`), the inputs kept active
  (`keep-active.json`) and the inputs' sound offsets; `setup_load` sets them before it
  answers. Setups saved before this leave them as they are.
- **Switcher scene layers** (spec §S.12): every layer has a stable `id` (kept when
  `scene_items` sends the list back; a new layer gets a new one), a `name`, a `volunteer`
  flag (volunteers may toggle it) and a `fade` (seconds, default 0.5). New command
  `scene_layer_visible {scene, layer, on}` shows or hides one layer by id with its fade:
  live, not an undo step, on the MQTT allowlist (also a trigger action,
  `TriggerAction::LayerVisible`). `scene_item_sound` takes `layer` (the id) besides `item`
  (the place); the engine's `scene_item_sound` takes a `scene::LayerRef`.
- **Switcher global layers** (spec §S.5): `global_layer {layer, add_to_scenes?}` /
  `global_layer_remove {id}`, the state's `global_layers`, `Switcher::set_global_layer`,
  `remove_global_layer`, `global_layers` (see Changed).
- **Switcher `keep_active {id, on}`** (spec §S.3): an input kept receiving while it isn't
  in use; kept by label (`keep-active.json`). State `inputs[].active` (receiving now) and
  `inputs[].keep_active`; `SwitcherConfig::idle_after` (10 s).
- **Switcher multiview layout** (spec §S.7): `multiview_layout {tiles}` (tiles `{kind:
  program|preview|input|scene|mix, id, rect, label, tally}`, or `null` for the default,
  which grows with the inputs) and `multiview {output}` sending it, or not, at run time
  (`Switcher::set_multiview_layout`, `multiview_layout`, `multiview_output`;
  `idn_switcher::multiview::{TileSpec, TileKind}`); kept in `multiview.json`. State
  `multiview {tiles, output, custom}`. Scenes are drawn at their tile's size.
- **Switcher state:** `transition.soft` and `mixes[].transition.soft` (a wipe's edge),
  `mixes[].transition.luma`.
- **Join modes and join codes** (spec §E.10.10, idn-node.md §N.11.8).
  - A site has one join mode, the site option `join.mode`: `approval`,
    `code_or_approval` or `code_and_approval`. New sites start in `code_or_approval`;
    sites made before keep `approval`. Industrial sites stay in `approval`.
  - The Site Manager issues join codes with `POST /api/v1/join_code` (admins only, as a code lets a machine in;
    `SiteCommand::JoinCode`). A code is 6 digits shown `482 913`, single use, lasts 10
    minutes, burns after 5 wrong tries, and survives a restart.
  - `status.site.join_mode` and `join_codes` (expiry and tries, never the digits);
    pending joins have `by_code`. The audit records each code's use, wrong tries and
    burn.
  - A machine joins with `idn-node join CODE`, the Node panel's *Join code* field, `POST
    /api/v1/site/join` or IPC `site_join`. New Node states: `code_needed`,
    `code_accepted`, `wrong_code` (`code_left`) and `code_expired`.
  - The code is checked inside the enrolment exchange as a SPAKE2 password, bound to both
    keys. A rogue Site Manager can't learn it offline from an exchange. An address with
    10 failed codes in 10 minutes waits.
  - Trust hello version 2 carries the mode. Older devices and authorities keep the
    approval path.
  - Library: `Authority::{join_mode, set_join_mode, issue_join_code, join_codes,
    cancel_join_code}`, and `Enrollment::{join_with_code, site_join_mode}`.
  - New dependency: `spake2` (RustCrypto, MIT OR Apache-2.0).
- **Site-signed access tokens for the engines** (idn-node.md §N.31.5).
  - The Site Manager signs `idn1.…` tokens with the site's CA key: `POST /api/v1/token
    {aud: {engine, machine?}, scope, ttl_s?}` (`SiteCommand::Token`) for the logged-in
    user (viewer: `read`; operator: all but the Node's `control`; admin: all), and
    `host::Handle::issue_token` for a host program's own users. 12 hours at most.
  - Scopes `read`, `control`, `bus:<bus>` (the desk; below), `iem:<mix>` (read-only
    until the IEM mixes are built).
  - The switcher, the desk, IDN Data and the Node's LAN panel take them (`Authorization:
    Bearer`, `?token=` or a first `{"token"}` message on the push channel), checked with
    the site's certificate their Node holds. A read-only token's changes are refused
    (403 `read-only`); a forged, expired or other engine's, machine's or site's token gets
    401.
  - Removing a user or changing their role or password withdraws their tokens at once
    (the site option `tokens.not_before.<id>`, sent to every machine).
  - Library: `trust::token`, `Authority::{issue_token, site_id, ca_public}`,
    `web::{Who, TokenCheck, SiteKeys}`, `web::PasswordGate::{with_tokens, tokens_only,
    who}`, `web::lan::start_when`, `events::run_as_requests_awaiting`;
    `Service::{handle_as, command_as, run_as, events_as, token_check}`,
    `Console::{command_as, events_as, token_check}`.
- **Desk: one bus for a `bus:` token** (spec §M.25), for another program (a streaming program)
  running one mix, the stream's.
  - The bus: `bus:main`, `bus:aux:N`, `bus:group:N`, `bus:matrix:N`, or by its name,
    `bus:name:NAME` / `bus:NAME` (`bus:stream`), found each time the token is used.
  - Allowed: the channels' sends into it (an aux's `sends` element, a group's `groups`
    flag, the main bus's `main`; a matrix mix's `sources`) and the bus's own fader, mute,
    EQ, compressor, limiter and graphic EQ. Everything else: 403 `outside this token's
    bus (…)`, in the audit.
  - It sees its bus alone: `GET /api/v1/state`, `/meters` and `/controls` and the push
    channel are cut down to the bus (a new state shape for this token, `{scope, bus,
    channels, …}`); other pages and the audit topic are refused.
  - Library: `idn_mixer::engine::scoped`, `Console::{access, permits, state_for,
    meters_for, controls_for, bus_state}`; `Console::command_as` now refuses outside the
    caller's scope too.
- **Every engine keeps an audit** (idn-node.md §N.31.6): `audit.jsonl` in its state folder
  in the Site Manager's line format, with `client`: every change from HTTP, the push
  channel, MQTT (switcher), OSC, the control surface and snapshot cues (desk), refused
  ones too. A fader moved continuously is one line with `repeats`. `GET /api/v1/audit?n=`
  and the push topic `audit`. The Site Manager's lines gain `client`; its redaction is
  `web::audit::redact` (`users::redact` re-exports it).
- **IDN Data on the network:** port 29221 (HTTPS), site tokens only, once the machine is
  in a site (`--lan-port`, `--no-lan`).
- **Site-signed certificates on the network ports** (spec
  idn-extensions.md §E.10.11, idn-node.md §N.31.5; docs/MEDIA-API.md "Certificates"). An
  enrolled device asks the authority for a certificate for its HTTPS ports over the trust
  connection (`<idn_lan_cert>`: its device key; its host name, `.local`, `localhost`, LAN
  IPv4 addresses and loopback as SubjectAltNames, plus the address the authority sees;
  `urn:idn:lan:<fingerprint>`; server authentication only; 7 days, renewed at half or
  when its names or addresses change; kept as `lan.crt`). The IDN Node passes its
  machine's to its programs (`identity` `lan_cert`) and signs their handshakes with the
  device key. The Node's LAN panel, the switcher's, the desk's, IDN Data's and the display
  service's network ports present it with the site CA after it while the machine is in a
  site, so a client pins the site CA alone; looked at on every connection (joining,
  leaving, renewals without a restart). A client offering no Ed25519 (a browser), and a
  machine not in a site, get the program's own self-signed certificate. New:
  `Enrollment::lan_cert`, `trust::pki::{LanCert, LAN_CERT_LIFE, LAN_URN, pem}`,
  `Ca::issue_lan`, `Anchor::verify_lan`, `Authority::ca_pem`,
  `web::lan::{server_config, start_for, Membership, process_membership,
  use_membership}`; the Site Manager's `GET /api/v1/site_ca` (PEM, no login).

### Changed
- **One API front for the engines (refactor 3, 2026-11-19):** the desk, the switcher and
  the display service answer HTTP and the push channel through
  `idn_open_protocol::web::engine` (`Engine`, `handle_as`, `events_as`): the state's long
  poll, `POST /api/v1/<cmd>` with the change header and per-control sequence numbers,
  refusals audited, `denied` events, subscriptions (`stats`, `meters`, `audit`), and a
  state sent only when what the caller sees changed. Each engine keeps its own pages
  (`Engine::route`). Visible differences: the display service and the switcher now also
  send a state only when it changed for that caller (as the desk did), and look again
  each second.
  The Site Manager joined it later the same day (its logins, sessions and roles giving
  the caller; its push channel and file pages its own); its audit now names a change by
  its command (`route`), as the engines do, not by its path (`/api/v1/route`).
- **Typed states and one command contract (refactor 4, 2026-11-19):** the desk's state
  (`DeskState`), the display service's (`DisplayState`, `ScreenView`) and the Site
  Manager's status (`SiteStatus`; it was built from `format!` strings) are made from
  types with JSON schemas (`GET /api/v1/state_schema`, `/api/v1/status_schema`); the
  switcher's state is typed down to its inputs, layers, buses, mixes, streams and
  problems (`SwitcherState`), those parts made as the types (`Layer::from(&Item)`).
  Later the same day the Site Manager's commands moved to `commands.rs`, run by name
  without a made-up request (one role check, `State::may_run`; one audit line,
  `audit_answer`), and its status is made at most every 250 ms and after any change
  (it was made and hashed for every long-poll look). The desk checks every command
  against its typed form before it runs (`typed::parse`); a bus number as text is part of
  the typed forms (`NumText`).

### Fixed
- A desk with no state or recordings folder (embedded) recorded into the working
  directory; it now records in the desk program's folder.
- The switcher's state no longer gives every input's `source` a `"virtual": null`.
- The desk's §M.28 processing made a busy desk 24% slower with none of it used (the
  plain gate and compressor stopped being inlined); plain strips run as before, and the
  keyed variants are cheaper too (profile round 13, `notes/PROFILE-13.md`).
- Bug runs P and Q (2026-11-19): leaving the site ignored the site lock; a gate or
  compressor keyed from a channel off the desk kept hearing its last block; a send
  tapped at the input of a resting bank repeated a block; `channel_reset` dropped the
  IEM mark; a remote restart switched on a service that was off; an endpoint output of
  0 in a hand-written desk file underflowed; users named `key:…` shared an API key's
  revocation.
- The Node's status left out `code_left` and `join_mode`: `idn-node join` never said
  the tries left after a wrong code.
- **Switcher `audio_offset` is kept** by the input's label (`audio-offsets.json`) and
  comes back at start; it was lost at a restart. An input brought back by undo has its
  offset again.
- **Switcher mixes keep a wipe's soft edge and a luma wipe** (its pattern by label) across
  a restart and in setups; a luma wipe came back as a mix.
- **Desk: a push channel sends a state only when what its caller sees changed** (its
  `version` aside). An IEM mix's phone (and a bus token's panel) was sent its state at
  every change anywhere on the desk, a FOH fader move included.
- **Desk: the IEM multicast copy leaves by the LAN's interface** (IP_MULTICAST_IF): the
  Node's chosen interface, else the one towards the Site Manager, else the default
  route's, or `IemConfig::interface` / `--iem-interface IPV4`. On a machine with an
  overlay too (wt0) it could go out there. The state's `iem.multicast.interface`.

### Changed — action may be needed: the Site Manager's and the Node's push channels send `state`, and the Node's version key is `node_version`
- `GET /api/v1/events` on the Site Manager and the Node panel is the engines' channel
  (`web::engine::events_as`): `{"type": "state", "version", "state"}` at once and when the
  status changes (it was `{"type": "status", "status"}` each second), typed events,
  commands answered in order, `denied`. Read `state` where you read `status`. The Site
  Manager's topics are as before but `audit`, which now sends the lines written after you
  subscribe (`{"type": "audit", "entries"}`) instead of the newest batch every 10 s.
- The Node's status is typed (`idn_node::panel_state::NodeStatus`, `GET
  /api/v1/state_schema`); the Node's own version moved from `version` to `node_version`
  (`version` is the state's number, as everywhere).

### Changed — action may be needed: the switcher works as OBS does (scenes only on the buses, global layers, no input limit)
A clean break (spec §S.3, §S.5, §S.12): the old forms are refused;
only saved data is carried over.
- **Only scenes go on the buses.** `program`, `preview`, `mix_program` and `mix_preview`
  (and `Switcher::program`/`preview`/`mix_program`/`mix_preview`, `TriggerAction::Program`
  /`Preview`) take a scene; an input is refused ("put it in a scene"). Adding inputs no
  longer fills program and preview; the first scenes do. Saved data that put an input on
  a bus — a show's or saved show's cue, an MQTT trigger, a mix in `mixes.json`, a
  starter's cue — is carried over once (at start, `setup_load`, `starter`): the input in
  a scene of its own named `LABEL (scene)` (one full-frame layer, plus the global
  layers), the cue or trigger then naming that scene. A scene of one full-frame layer
  passes its input through (the bus takes its picture as before: no drawing, no added
  latency).
- **Overlays are global layers.** The `overlay`, `overlay_set`, `overlay_on_air` and
  `overlay_transition` commands, the state's `overlays`, `OverlayMode`,
  `engine::OVERLAYS`, `TriggerAction::Overlay` and the GPU compositor's overlay slots are
  gone. A global layer is defined once and added to every new scene as an ordinary layer
  there (moved, hidden per scene); a change of it reaches every copy. A setup saved with
  overlays has each overlay with a source carried over as a global layer in its scenes,
  shown as it was on air. Fade to black still takes everything.
- **No input limit:** `engine::MAX_INPUTS` (16) is gone. An input not in use isn't
  received (a network input disconnects 10 s after its last use, and connects when its
  scene comes to preview, program or a mix, or the multiview shows it); `keep_active`
  keeps one ready.
- **Scenes have at most 16 layers** (were 32 items; `scene::MAX_ITEMS`).
- **State:** `inputs[].program` / `preview` are now the input's tally (shown there,
  through a scene too), not "is the bus's source"; the top-level `program` / `preview`
  are the scenes. The multiview's `tiles` are the layout's (`kind`, `id`, `rect`, `label`,
  `tally`).
- **GPU compositor (`idn_switcher::gpu`):** `Scene::overlays` and `gpu::Overlay` are gone
  (program, preview and a transition's material only: a quarter of the picture memory).

### Changed — action may be needed: engines on a machine in a site open their network port for tokens
- The switcher (29211), the desk (29215) and the Node's LAN panel (29201) now listen on
  the network once the machine is in a site, even without a LAN password: only a valid
  site token (or the password, where one is set) gets in. `--no-lan` keeps the switcher's
  and the desk's closed. Clearing the Node's LAN password no longer turns its LAN panel
  off while the machine is in a site.
- `web::lan::front`'s handler takes the caller too: `|request, who| …`.
- On a machine in a site, these ports (and IDN Data's and the display service's) present
  the site's certificate for the machine to TLS clients that offer Ed25519 (rustls does):
  a client that pinned a program's own self-signed certificate or its fingerprint (an IEM
  phone pinning the desk's) stops connecting there. Pin the site CA (`GET
  /api/v1/site_ca`).

### Changed — action may be needed: join comparison codes are 6 digits; both ends need this version
- **The site join's comparison code** (§E.10.4) is six digits, made from the same hash
  as before (its first 8 bytes modulo 10⁶), in place of 4 base32 characters: one code
  format everywhere, as the linked computers' code already was. A machine on an older
  version shows a different code from a new Site Manager (and the other way round), so
  such a join is refused: update both ends. `JoinRequest::code` and
  `TrustState::Pending/ConfirmCode { code }` are six digits (`"482913"`); show them with
  the new `trust::show_code` (`"482 913"`), and compare a typed code with
  `trust::same_code` (with or without the space). The Node panel, `idn-node status` /
  `code` / `link`, the Site Manager's log and `pending` list, and the session's notices
  show `482 913`; `idn-node link accept` and the panel's `pairs/answer` / `pairs/confirm`
  take the code with or without the space.

### Changed — action may be needed: the desk's scenes are snapshots, in the API too (no aliases)
The sound desk's saved desk states are called **snapshots** (the switcher's scenes keep
their name). A clean break: the old names are refused; only saved data is carried over.
- **Commands:** `scene_save` → `snapshot_save`, `scene_recall` → `snapshot_recall`,
  `scene_delete` → `snapshot_delete` (same bodies, `scope` included).
- **Typed API (`idn_mixer`):** `Command::SceneSave` / `SceneRecall` / `SceneDelete` →
  `Command::SnapshotSave` / `SnapshotRecall` / `SnapshotDelete`; `engine::typed::SceneScope`
  → `SnapshotScope`; `engine::RecallScope` → `engine::SnapshotRecallScope`;
  `Console::scene_save` / `scene_recall` / `scene_recall_scoped` / `scene_delete` →
  `Console::snapshot_save` / `snapshot_recall` / `snapshot_recall_scoped` /
  `snapshot_delete`; `cues::scene_of` → `cues::snapshot_of`.
- **State (and the push channel):** `scenes` → `snapshots` (the names), `scene` →
  `snapshot` (`{name, changed}`).
- **User buttons:** the action `scene:NAME` → `snapshot:NAME` (the `controls` grammar
  too). Saved desks and snapshots are read with the new name.
- **OSC:** `/scene/recall` → `/snapshot/recall`, `/scene/save` → `/snapshot/save`.
- **Files:** `scenes.json` → `snapshots.json`, `scene.txt` → `snapshot.txt` in the desk's
  state directory, carried over once at the first start that finds the old ones (said in
  the log).
- **Switcher cues:** a show's desk action is `{"do": "mixer", "desk" | "url", "snapshot"}`
  (was `"scene"`; a saved show's is read once under the old name and saved under the
  new). It goes to the desk over IDN as `<idn_desk_snapshot name="…"/>` (was
  `<idn_desk_scene/>`: a switcher and a desk need the same version), to a desk's address as
  `/snapshot/recall` or `POST /api/v1/snapshot_recall`. `Switcher::desk_scene` →
  `Switcher::desk_snapshot`.

### Changed
- **The desk has 8 EQ bands and 8 mute groups** (were 6 and 6; `desk_sizes.eq_bands` and
  `desk_sizes.mute_groups` say so). A desk or snapshot saved with fewer loads with the
  extra bands flat (0 dB: not run, so they cost no DSP until used) and the extra mute
  groups off; a settings file with more than 8 bands or mute groups (a later build's)
  loads with the first 8 instead of being refused whole. New desks' default bands: low
  shelf 100 Hz, peaks at 250, 500, 1000, 2500, 4000 and 6000 Hz, high shelf 10 kHz.
- **`shutdown::install` works on Windows.** It handles the console's control events
  (Ctrl+C, Ctrl+Break, the console window closed, log-off, shutdown) and, through a hidden
  window, the session's end (log-off, shutdown) for programs without a console (such as
  `idn-session`): the stop notices go, so a stopped program's sources leave other
  machines' lists at once rather than after about 2 minutes. Log-off is ignored in
  session 0 (services). The IDN Node sends the stop notices of anything still alive when
  it stops (its Windows service's stop too).
- **Push channel replies:** a reply has no `id` when the command message had none (it
  was `"id": null`).

## [v0.2.0-Alpha87] — 2026-11-18 (pre-release)

Everything after `v0.2.0-rv6`. The pre-release tags `v0.2.0-Alpha39`, `-Alpha70`,
`-Alpha85` and `-Alpha86` are steps along the way.

### Changed — action may be needed: IDN Media as a server and libraries (R.34)
- **No UI but the IDN Node panel.** The switcher's, sound desk's, Site Manager's and IDN
  Data's own pages are gone, with IDN Remote, `idn-panel`, Media's IDN Voice app and pages,
  idn-display's studio monitor and source picker, and the listening and watching pages
  (WHEP stays as an API). The embedding application is the UI. Their HTTP/JSON APIs and MQTT stay; the
  reference is [docs/MEDIA-API.md](docs/MEDIA-API.md).
- **The tray is replaced** by the windowless per-user `idn-session`
  (`idn_open_protocol::node::session::Agent`): the session's services and the Node's
  notices. A host with its own tray item runs the `Agent` instead.
- **Embedding:** `idn_node::host::{start, Config}` → `Handle` runs the IDN Node inside a
  program (its services stay child processes; `idn_common::supervisor::set_programs_dir`
  says where IDN's programs are). `idn_site_manager::host::{start, Config}` → `Handle`
  (`run(user, &SiteCommand)`) runs the Site Manager. The switcher is a library service:
  `idn_switcher::service::{Service, ServiceConfig}` (one a process).
- **Push channel:** `GET /api/v1/events` (WebSocket) on the Node, the Site Manager, the
  switcher and the desk: state, status and meters as they change, and commands with
  replies. Other sites' pages are refused (403).
- **Per-control sequence numbers:** a change may carry `control` and `seq` (HTTP:
  `X-IDN-Client`, `X-IDN-Control`, `X-IDN-Seq`); a late, older change is skipped.
- **Typed commands with a JSON schema:** `idn_switcher::service::typed::Command`,
  `idn_mixer::Command`, `idn_site_manager::typed::SiteCommand`; `GET /api/v1/schema` on
  each.
- **What the panels worked out is in the APIs:** the Site Manager's routing matrix
  (`/api/v1/matrix`, `route_cell`), role capabilities, machine settings schema, copying
  settings and templates; the desk's strips, controls, derived values and linked pairs;
  the switcher's title templates, `overlay_set`, `slide`, cue edits and filters schema.

### Changed
- **The repository is laid out by edition** (README "Layout"): `core/`, `media/`,
  `industrial/`, `tools/`. A git dependency on `idn-open-protocol` is unchanged (Cargo
  finds the crate in its folder). The Node service moved out of the library into the
  `idn-node` crate: `node::server`, `node::supervisor` and `node::update` are now
  `idn_node::server`, `idn_node::supervisor`, `idn_node::update`; the Site Manager
  (`site_manager`) is the `idn-site-manager` crate with the `idn-discovery-server`
  program. `idn_open_protocol::node` keeps `Node`, `clock`, `routing`, `video` and
  `bridges` (the Node's settings).
- **Switcher stream inputs are site sources** (idn-switcher §S.22): each stream is taken
  in by `idn-codec ingest` (new `--status`, `--parent-pipe`), started by the switcher as
  its own program and announced like any source, so other receivers can use it;
  `idn_switcher::streams::StreamIn`. The state's stream gains `receivers`.
- **A cue recalls a mixer's scene over IDN** (idn-switcher §S.27, idn-mixer §M.21): by
  the desk's source name, on the switcher's connection to it; the desk takes it only from
  devices of its site. New: `Output::recv_messages` (`api::ReceiverMessage`: receiver,
  XML, `site`), `Switcher::desks`, `Switcher::desk_scene`, `idn_mixer::cues`,
  `Console::receiver_messages`. Cues by address (`url`) keep working.

### Added
- **IDN Data, stage 1: data feeds** (roadmap R.22, idn-data.md): the `idn-data` crate and
  program (`idn data`, panel on port 7975), run by the Node as a service (`data.on`,
  idn-node §N.24; `node::bridges::DataSetting`). Each feed (a Google Sheet, CSV, TSV,
  JSON, XML/RSS) is read every so often and sent as a text-only source. **Text-only
  sources** in the library: `text` (`text/1`, idn-extensions §E.22: `TextSource`,
  `TextReceiver`, `DataMessage`). The switcher's data titles take a feed by name
  (`"data": {"feed"}`, `Switcher::data_feeds`); its reading and formats moved to
  `idn_data::feeds` (`idn_switcher::data` re-exports them).
- **IDN Data, stage 2: video downloads** (idn-data.md §D.6): yt-dlp fetched from its
  GitHub releases, checked against `SHA2-256SUMS` and checked weekly
  (`idn_data::ytdlp`); a link's formats offered plainly with presets (1080p default, best
  up to 4K, best available, sound only) and hardware-decoding notes; download jobs with
  progress into the media folder as MP4 (`idn_data::downloads`); `web::httpc::get_up_to`.
  The Windows installer now also installs `ffmpeg.exe` (same LGPL build).
- **Files between the site's machines** (R.22 stage 3, idn-node §N.25, idn-data §D.7):
  the Node has a media folder every user reads and runs the site's file channel
  (`channel/1`) once enrolled; `<media/>` and `<media_send to path|file/>` (only files of
  the media folder); the receiving Node keeps pushed media files, checked, never over a
  finished file (`idn_node::media`). IDN Data downloads into the Node's media folder and
  sends files from its panel (`send`). A program's own channel endpoint through the Node
  is refused while the Node's runs.
- **The site's media library** (R.23, idn-extensions §E.12.11, idn-node §N.26, idn-data
  §D.8): `idn_open_protocol::library` (`Library::find/catalog/thumbnail/fetch/prefetch/
  link/download/job`, `scan`, `Catalog`, `Item`); `ChannelConfig::client_only`; the
  channel passes `idn.library.*` requests to the application. The Node serves its media
  folder as the library with `data.library` on, with play-direct links (HTTP ranges,
  port 7977). IDN Data keeps it: lengths, sizes, codecs (`ffprobe`), thumbnails,
  titles, tags, folders, uploads, deletes, and programs' download requests
  (`idn_data::library`). The Windows installer also installs `ffprobe.exe`.
- **Open Media Transport** (idn-omt.md, idn-node §N.27, vMix F9): the `idn-omt` crate
  (`Sender`, `Receiver`, OMT 1.0's wire format, the VMX codec from vendored libvmx, MIT)
  and `idn omt list|recv|send`, the bridge between OMT and IDN with tally both ways; the
  Node runs it (`omt.sends`, `omt.recvs`, `node::bridges::OmtBridges`). The library's
  `discovery::service` announces and finds any DNS-SD service type.
- **Live captions (G2, stage 1)** (idn-captions.md, idn-node §N.28): the `idn-captions`
  crate and `idn captions` (whisper.cpp on the CPU through whisper-rs; models downloaded on
  first use; drafts every second, final at a pause; a word list); caption lines on text
  sources (`text::CaptionLine`, `TextSource::send_caption`, `TextReceiver::captions`,
  `new_finals`); `web::httpc::download` (to a file, streamed); the switcher's closed
  captions from a captions source (`Switcher::captions_from`, `captions {from}`) and
  titles from one (`{caption}`, `{line}`, `{final}`); the Node service (`captions.*`,
  `node::bridges::CaptionsSetting`). `whisper-rs-sys` vendored with a MinGW link patch.
- **Updates from the Site Manager (idn-node §N.23, G10).** `idn_open_protocol::release`
  (signed release files) and the `idn-release` tool; `release-keys.txt` (the public keys
  every build trusts: ADC Industries' interim file key, 999d622f…, until the YubiKeys); `node::update` (fetch,
  install now or at the next start, check-in, rollback by itself and by hand) and
  `idn-node update-apply`; the Site Manager's Updates card and `/api/v1/release/…`,
  `/api/v1/update`, `/api/v1/rollback`; `web::serve_with` (larger bodies on one path);
  `Authority::update_node`; the installer's `/UPDATE`; `scripts/release.sh`,
  `scripts/compat.sh`. The release key is on a YubiKey (`idn-release` crate: `card-status`,
  `card-pins`, `card-keygen`, `make --card`); a release's signature covers the SHA-256
  digest of its manifest; `release::make_signed`, `release::signed_digest`. Until then, a
  key file sealed with a passphrase (`release::keygen(path, Some(passphrase))`,
  `release::key_file_info`; `release::make` takes the passphrase).
- **Starters (G6; switcher §S.32, mixer §M.20).** Built-in starting points: the switcher's
  titles, timer and cue list for a Sunday service or a simple stream; the mixer's channel
  names, filters, compressors, DCAs and mute groups for a spoken service or a worship
  band. `starter {name}` in both; `starters` in both states. `Console::touch`.
- **The first-run wizard (idn-node §N.22, G6).** A new machine's Node panel asks its name,
  its site (looking now: `POST /api/v1/sites/search`), the approval, simple mode's PIN and
  what it is for. `Settings::welcomed`, `Change::welcomed`, `NodeCore::search_sites_now`.
- **The Site Manager's forms, copies and templates (idn-node §N.21, G6).** "All settings"
  for each machine, "Copy settings to…" one or many machines, site-wide templates
  (`templates.json`); `POST /api/v1/apply`, `POST /api/v1/template`. The Node takes
  `services_off` and `approve_programs` from the Site Manager; `Change` gains
  `services_off`.
- **Simple mode (G6; switcher §S.31, mixer §M.19, idn-node §N.20).** A volunteer's panel
  in the switcher and the mixer; leaving it takes a button or the machine's PIN, kept by
  the Node and settable from the panels or the Site Manager (which can hold it).
  `idn_open_protocol::simple::SimpleMode`; `Node::panel_lock`, `panel_unlock`,
  `set_panel_pin`; `node::server::Change` gains `panel_pin`, `panel_pin_hash`,
  `panel_site_only`, `from_site` (a struct literal of it needs them: use
  `..Default::default()`); `Console::touch`. The Site Manager's status gains each
  machine's `settings`.
- **Listening on phones (idn-node §N.19, G3).** `idn listen "Name=SOURCE"…` (and the Node's
  `listen` service: `listen.channels`, `.port`, `.title`, `.kbps`; the panel's card):
  channels of IDN sources played in phones' browsers over WebRTC, sound only, with a QR
  code page. `whep::ViewerConfig` gains `listen`, `title`, `listen_kbps`;
  `node::bridges::services` takes a `ListenSetting`; Opus is now constrained VBR.
- **Streams that ride out a bad connection (switcher §S.30, G4 part 1).** In
  `idn_ffmpeg::gateway`: `Gateway::add_opts` with `AddOptions` (`adaptive_floor_mbps`: the
  stream's own encoder, its rate following the upload; `via`: the local address to send
  from), `default_settings()`; `DestinationStatus` gains `adaptive`, `target_kbps`,
  `video_kbps`, `sent_kbps`, `lag_ms`, `via` (a struct literal of it no longer compiles:
  use the gateway's). Reconnects after 1, 2, 3, then every 5 s (was up to 30 s); a stream
  over 3 s behind skips to the next keyframe. `Gateway::record(id, path, container)`
  records a stream as its encoder makes it (no second encode). The switcher: backup
  ingests, `record`, health and alerts, `GET /api/v1/addresses`; `adaptive` is off
  unless asked for.
- **Alerts to the Site Manager (idn-node §N.18).** `Node::alert(level, text)`; the Node
  passes alerts on; `Authority::alerts()`; the Site Manager's card shows them.
  `diag::free_space(path)`.
- **Hand your own wgpu device to the GPU reconstruction.** `idn_shq_gpu::Gpu::adopt(device,
  queue, env)` wraps a host's existing `Device`/`Queue` instead of opening one, so a process
  that already uses wgpu keeps a single device (some Vulkan loaders race with more than
  one). See `idn-shq-gpu/README.md`.
- **Composite the switcher on a host's wgpu device.** `idn_switcher::SwitcherConfig::gpu_device`
  (feature `gpu`) takes `Some((device, queue))`; the engine composites on it via
  `gpu::Gpu::with_device`, which checks the device meets the compositor's limits. See
  `idn-switcher/README.md`.
- **Feed the switcher live pictures in-process.** `idn_switcher::Source::Feed` adds an input
  your app pushes pictures to with `Switcher::feed(id, Arc<Frame>)` — a key or lower third
  your app renders, shown with no network hop.
- **Feed the mixer audio in-process.** `idn_mixer::Console::feed(name, first, sample_rate,
  &channels)` places audio your app holds (program sound, say) on the desk's inputs without
  the routing matrix; `feed_stop(name)` frees it.
- **Delay inputs (switcher).** `idn_switcher::Source::Delay { input, by_ms }` shows another
  input `by_ms` milliseconds late (the switcher buffers and replays its pictures).
- **Triggers (switcher).** `idn_switcher::Switcher::set_triggers` runs internal actions
  (program/preview/cut/FTB/overlay — no shell) on events (an input on air, a transition
  finishing).
- **Recording auto-split.** A file recording can be cut into parts (`name-2.mp4`, …) every
  N seconds and/or megabytes: the switcher's `stream_add` takes `split_s`/`split_mb`
  (`idn_ffmpeg::gateway::Gateway::set_split`). Each part starts at a keyframe. (Crash-safe
  recording already shipped — files are fragmented MP4.)
- **Noise suppression, built in (OBS #12 / mixer M11).** `idn_audio::denoise::Denoiser`, an
  RNNoise-class network (`nnnoiseless`, BSD-3-Clause, model built in), shared by both:
  a mixer channel's `denoise` (panel NR; adds 10 ms, the other channels delayed alike so
  everything stays in line, presets carry it) and the switcher's stream sound
  (`Switcher::audio_denoise`, or `"denoise"` on the `audio` command; its stamps move with
  it so it stays in sync). Steady room noise drops by ~38 dB in the tests.
- **Per-input sound sync offset (switcher, OBS #11).** `Switcher::input_audio_offset(id, ms)`
  (`audio_offset` command, panel Sync button) delays an input's sound by 0–500 ms on top of
  the stream's delay while its sound is used, to put a camera's sound back with its picture.
  (The mixer already has per-channel input delay and PFL monitoring; per-source tracks come
  with multi-track recording.)
- **ISO recording (switcher, vMix F1).** Record any input on its own file beside the program:
  `Switcher::tap_input(id, f)` hands its picture every program frame (full size, the
  program's stamps — the files line up in an editor) with its own sound, on its own worker;
  the program's `iso_add` command (panel: an input's ISO button) gives each its own gateway
  encoder and file (split options too), listed with the streams, stopped by
  `stream_remove`. The library gains `TappedFrame::picture` / `with_audio` / `sound` to build
  frames for a recorder.
- **Recording pause/resume (OBS #6).** `Gateway::set_paused(id, bool)` (the switcher's
  `stream_pause`, panel Pause/Resume): one file, the paused part simply not in it — on resume
  the next keyframe is asked for at once and the file's time carries on.
- **Multi-track sound (OBS #6).** `StreamSettings::audio_tracks` (1–8): channels 1–2, 3–4, …
  as separate AAC tracks in a recording (streams keep track 1). The switcher records the
  program with tracks: `stream_add {"file": …, "tracks": [input ids]}` — track 1 the stream's
  sound, then each chosen input's own sound through its own lane at the program sound's
  latency (`Switcher::tap_sound_tracks`).
- **Hotkeys (switcher panel, OBS #13).** Every action has a key, assignable in the panel's
  Keys dialog and kept by the switcher for every panel (`/api/v1/hotkeys`): cut, auto, FTB,
  inputs 1–9 to preview/program, overlays 1–4, transition kinds. Defaults keep Space/Enter.
- **Scenes (switcher, Wave 1 stage 1a — OBS #1).** `Source::Scene` + `Switcher::scene_items`:
  free-form layered scenes as inputs — items placed, sized, cropped, turned, faded,
  blended (normal/add/multiply/screen on luma) and bordered over a transparent canvas,
  nesting other scenes (groups), kept by the program across restarts; a panel scene
  editor. CPU-drawn: a camera + turned logo + blended lower third takes ~16 ms at 2160p and
  ~2–5 ms at 1080p; a whole-frame turned item is slow at 2160p (GPU drawing is stage 1e).
- **Eight overlay channels (switcher, Wave 1 stage 1b — vMix F2).** Overlays 1–8 (from 4) in
  the API, state, both compositors and the panel (hotkeys F1–F8). `OVERLAYS` is 8.
- **Luma wipes and stingers (switcher, Wave 1 stage 1c — OBS #4, vMix F2).** `Kind::Luma
  { pattern, soft, invert }` wipes by a greyscale picture input; `Kind::Stinger(1–8)` plays a
  clip input over the bus with program/preview swapping at its cut point
  (`Switcher::stinger`, `on_stinger` to start a file clip). GPU matches the CPU to the bit.
- **Scenes drawn on the GPU (switcher, Wave 1 stage 1e).** Where the program is composited
  on the GPU, scenes are drawn there too (`gpu_scene::SceneGpu`, `Gpu::scene_drawer`, one
  compute pass a scene, the CPU's arithmetic: within a step of `scene::render`). The
  whole camera turned at 2160p: 12 ms from 75 ms on the CPU; an upright scene 3.4 ms at
  1080p (from 5). Falls back to the CPU if the GPU fails; `IDN_SWITCHER_SCENES_CPU=1`
  keeps them there.
- **Virtual camera (idn-display §D.5).** `idn-display --virtual-camera`: v4l2loopback on
  Linux; on Windows 11 "IDN Camera" through Media Foundation, with new crates `idn-vcam`
  (the shared memory's layout, NV12 packing) and `idn-camera-source` (`idn_camera.dll`,
  the media source; the installer registers it). The switcher's Screens card has
  "Virtual camera" (`screen: "vcam"`).
- **Closed captions (switcher §S.29).** New module `idn_open_protocol::cea608` (`Captioner`,
  `a53`, `row_pairs`, `wrap`, `parity`); `Output::caption_next`; `TappedFrame::captions`;
  `VideoEncoder::captions_next` (a default method); the gateway puts them in the video.
  Switcher: `Switcher::caption`, `captions`, the Captions card.
- **Setups, undo and the look (switcher §S.28, Wave 3 workspace).** The switcher keeps
  every input (files, pictures, colours, network sources too); `setup_*` and
  `undo`/`redo` commands; the state's `version` counts show and undo changes. Shared
  panels: a theme and accent control (`#look`, `html[data-theme]`/`[data-accent]`).
- **Site backup and restore (idn-node §N.17, G8).** New module `idn_open_protocol::backup`
  (`make`, `open`, `restore`, `stage`, `apply_staged`); the Site Manager's *Backup* card
  and `/api/v1/backup`, `/api/v1/restore`; `idn-discovery-server --restore FILE` and the
  console's `backup FILE`; `idn-site-manager-industrial backup|restore FILE`. A stopped
  `DiscoveryServer` closes its open connections.
- **Logs and crash reports (idn-node §N.16).** New module `idn_open_protocol::diag`
  (`init`, `init_in`, `log_dir`, `crash_reports`, `mark_reported`, `tail`): a program's log
  file and crash reports; `Node::connect` hands them to the Node; the authority's
  `crash_reports`, `crash_report`, `collect_logs`, `log_snapshots`, `log_file`; the Site
  Manager's *Logs and crash reports* page.
- **The show: a cue list (switcher §S.27, G1).** New module `idn_switcher::show` (`Show`,
  `Cue`, `Action`, `Runner`, `run_external`, Planning Center `Pco`); the switcher's
  `cue_*`, `show_*` and `pco_*` commands and the panel's Show card. The OSC codec moved
  from `idn_mixer::osc` to `idn_open_protocol::osc` (the mixer re-exports it).
- **Screens: outputs full screen on displays (Wave 3, spec/extensions/idn-display.md).** New
  program `idn-display` (and `idn display`): a source full screen on a screen through wgpu,
  or straight on a DRM/KMS connector on Linux with no desktop. Switcher:
  `idn_switcher::displays::Displays`, `Switcher::outputs()`, the `screen` command and the
  panel's Screens card.
- **Each switcher output's own mix from a desk (switcher §S.26, mixer §M.18).** The
  switcher's routable receiver has 10 channels (`FEED_CHANNELS`: the program's pair, then
  four mix slots'); a mix with a routed pair sends it instead of the stream's sound. The
  program and mixes announce `switcher-feed/1`; the switcher follows desks' `<idn_feeds>`
  (`idn_switcher::engine::parse_feeds`, `Feed`). Mixer: its output announces `desk/1`;
  `Console::switcher_feed(name, bus)`.
- **Switchers' outputs as desk inputs (mixer §M.17, switcher §S.25).** `OutputConfig::roles`
  (tokens announced in discovery only; a struct literal needs it). The switcher's outputs
  with sound announce `switcher/1` (`idn_switcher::engine::ROLE`); new `NAME (Monitor)`
  output; Preview carries its input's sound. Mixer: `idn_mixer::switchers`,
  `Console::switcher_inputs(on)` / `switcher_input(name, on)`; patching a channel's input
  pins it on the desk (`shown`).
- **The switcher's monitor (§S.24).** `Switcher::monitor(preview, solo, volume_db, mute,
  card)`, `monitor_solo(id, on)`, `monitor_listen(from, wait)` (the monitor's sound for a
  browser) and `meters()`; the state's `monitor`; `idn_switcher::card::CardOut`. Hear the
  stream as streamed, preview's sound, or soloed inputs, on a sound card and/or in the
  panel (Listen here).
- **OSC control (mixer, M9).** New module `idn_mixer::osc` (`Osc::start(console, port,
  allow)`, the codec `decode` / `encode` / `packets`, and `command_for` / `query` mapping
  addresses to commands); `idn-mixer --osc PORT [--osc-allow IP,…]`. The HTTP commands
  moved into the library as `Console::command(cmd, &body)`, so an embedding program can
  use the API's commands without HTTP (it checks `Console::locked()` itself).
- **Six-band EQ, output delays, integrated loudness, recall safes (mixer, M3, M4, M7,
  M10).** `idn_mixer::desk::EQ_BANDS` is 6: the `eq` arrays of `Channel` and `Bus` are
  `[Band; 6]` (**breaking** for code that builds them as `[Band; 4]`; saved desks with four
  load with two flat bands added). `Settings::output_delay` (ms an output channel, ≤500);
  the meters' `lufs` gains the integrated reading and `Console::reset_loudness()`;
  `Channel::safe`, `Bus::safe` and `Console::scene_recall_scoped(name, RecallScope)`.
- **Built-in effects and graphic EQ (mixer, M1).** New module `idn_mixer::fx`: `Effect`
  (reverb, delay, chorus) on FX buses and `Geq` (31 bands) on main/aux/matrix/headphones,
  as `Bus::effect` and `Bus::geq` (a struct literal needs them). New desks' FX 1–3 are
  Reverb, Delay, Chorus (named so); saved desks are unchanged. Panel: effect knobs on the
  Effects page and FX bus pages, a Graphic EQ module.
- **Multitrack recording and virtual soundcheck (mixer, M5).** New module
  `idn_mixer::record` (`Recorder`, `Soundcheck`, `Session`, `WavIn`): channels' inputs as
  they come to 32-bit float WAVs (crash-safe headers every 2 s, parts at 4 GB), played
  back in place of the live inputs (session-mapped, or any folder of 16/24/32-bit PCM or
  float WAVs). `Console::record_start`, `record_stop`, `recordings`, `soundcheck`,
  `soundcheck_transport`, `soundcheck_live`; `ConsoleConfig::recordings` (a struct
  literal needs it), `--recordings`. Panel: a Record page.
- **`localtime` (library).** The machine's local time (`localtime_r` / `GetLocalTime`), moved
  from the switcher so the mixer can name recordings by it.
- **Automix, ducking, audio follows video (mixer, M8).** `Settings::automix` with channel
  `automix` / `automix_weight` (gain sharing), `Settings::duckers` (4 `Ducker`s: keys,
  targets, threshold, depth, attack, hold, release), `Settings::afv` (`Afv`) with channel
  `follows`: a metadata-only connection to the switcher reads its tally; `Desk::set_on_air`;
  the meters' `auto` per channel. The switcher's program output now sends `<idn_tally …/>`
  (§S.23) on each change and every 2 s. Panel: Setup cards and the channel page's controls.
  `Channel` and `Settings` gain fields (a struct literal needs them).
- **Stream inputs (switcher, Wave 2 stage 2f — vMix F10).** `idn_ffmpeg::ingest` now listens
  as an RTMP server (`rtmp://0.0.0.0:1935/live/KEY` or `?listen=1`: OBS or a phone pushes to
  it) and paces HLS/HTTP streams by their own times (read ahead on a thread, handed out a
  second behind) instead of a segment at a time. The switcher program's stream inputs
  (`add_input {"stream": url}`: SRT, RIST, RTSP, RTMP, HLS, UDP), their status in the
  state with passwords hidden, kept in `sources.json`; panel Add stream.
- **PTZ presets as inputs, virtual shots (switcher, Wave 2 stage 2e — vMix F8).** New
  module `ptz` (`Control::Network | Visca { Ip | Tcp | Udp }`, `Move`, `ViscaLink`):
  cameras moved over their own NDI/IDN connection (`ntk_ptz_*`) or VISCA.
  `Switcher::set_camera`, `ptz`; `Source::Ptz { camera, preset }` recalls its preset from
  preview (not while the camera is on air through another input); `Source::Virtual {
  input, view }` + `virtual_view` (smooth, eased moves). Cameras are wanted at full size
  and tallied through their preset inputs, shots and delay inputs. Program `camera`,
  `ptz`, `virtual_view`, `cameras.json`, shots kept by camera label (waiting for it);
  panel PTZ pad and presets, "+ Shot", a Shot dialog to drag. New `Source` variants.
- **Titles from data (switcher, Wave 2 stage 2d — vMix F6).** `title::Title::data`
  (`data::Source`): CSV, TSV, JSON, XML and RSS/Atom from a file or an HTTP(S) address, a
  Google Sheets link read as its CSV export; `{Column}`, `{row}`, `{rows}`; read every N
  seconds on a thread of its own; rows by hand (`Switcher::title_row`,
  `title_data_read`) or by themselves. New module `data` (`parse`, `sheets_csv`). Program
  `title_row`; panel Data section in the title editor. `Title` gains `data` (a struct
  literal needs it).
- **Title designer, fields, clocks, timers (switcher, Wave 2 stage 2d — vMix F7).** Titles
  carry fields (`{name}` in a text; `Switcher::set_title_fields`, `add_title_field`),
  `{clock}`/`{date}` in local time, `{timer:NAME}` (`Switcher::timer`, `TimerMode::Down |
  Up | Until`, `TriggerEvent::TimerEnd`), and picture layers (`LayerKind::Picture`); new
  `clock` module, `title::render_with`, `Switcher::input_jpeg`. `title::Title` gains a
  `fields` map (a struct literal needs it). Program: `title_fields`, `title_field_add`,
  `title_template`, `timer`, `GET /api/v1/input.jpg`, `timers.json`, `templates.json`;
  panel: a drag-and-size designer with live preview, fields with −/+, templates, more
  Add text styles (name and role, clock, countdown, scoreboard), a Timers card.
- **`web::httpc` (library, feature `http-client`).** The Industrial edition's small
  HTTP(S) client moved into the library (`idn_industrial::httpc` re-exports it), with
  `get(url, timeout)` following redirects.
- **Lists (switcher, Wave 2 stage 2c — vMix F4).** `idn_ffmpeg::player::ListPlayer`: videos,
  sound files and pictures (their own seconds) played one after another on one output —
  next, previous, goto, pause, restart, items replaced live, loop or stop, shuffle. The
  switcher program's list inputs (`add_input {"list"}` with items or a folder, `list`,
  `list_items`), kept in `sources.json`; panel Add list, ◀ ▶ Play Restart Items; the
  PageDown/PageUp hotkeys move a list too.
- **Video filters (switcher, Wave 2 stage 2b — OBS #3).** `Switcher::set_filters(id,
  Vec<filter::Item>)`: chroma key (with spill), colour key, luma key, a picture mask,
  colour correction, `.cube` LUTs (3D/1D), sharpen, scroll — run on each input's own thread
  as its pictures arrive, so it is filtered everywhere it shows; `filter::Chain` for your
  own frames. Program `filters` command, `filters.json` by label; panel Filters dialog;
  the multiview shows inputs with alpha over black.
- **`shq::Plane::buffer(len)` (library).** A plane-sized buffer from the memory pictures
  hand back with `Plane::recycle`, for code that makes pictures each frame. The recycle
  pool now keeps 8 spares (was 4) and, when full, a bigger buffer replaces its smallest,
  so it follows the biggest pictures in use rather than keeping the first ones it saw.
- **Titles, text and slideshows (switcher, Wave 2 stage 2a — OBS #2).** `Source::Title` +
  `Switcher::set_title(id, title::Title)`: boxes and text layers (any installed font,
  shaping and fallback through cosmic-text =0.19.0, DejaVu Sans built in; outline, soft
  shadow, alignment, wrapping) over a transparent canvas, drawn when set. `Source::Slideshow`
  + `Switcher::slideshow(id, SlideAction)` + `set_picture_loader`: pictures by hand or every
  N s, cut or cross-faded, looping or stopping (`TriggerEvent::SlideshowEnd`,
  `TriggerAction::Slide`). Program: `add_input {"title"|"slideshow"}`, `title`, `slideshow`,
  `GET /api/v1/fonts`, kept in `sources.json`; panel: Add text (four styles), a title
  editor, Add slideshow, ◀ ▶ Play, PageDown/PageUp hotkeys. New `Source` variants: a
  `match` on `Source` needs arms for them.
- **Independent mixes (switcher, Wave 1 stage 1d — vMix F3, OBS #5).** Up to four mixes
  (`MIXES`) besides the program, each its own network source with its own size (a vertical
  1080×1920 too), program/preview, transition, AUTO and CUT, sharing the inputs and scenes
  and carrying the stream's sound: `add_mix`, `remove_mix`, `mix_program`, `mix_preview`,
  `mix_cut`, `mix_auto`, `mix_transition`; `tap_mix` streams or records one
  (`stream_add {"mix": id}`). Drawn on the CPU; kept in `mixes.json`; a Mixes card.

## [0.2.0] — 2026-10-28

### Added
- **IDN Node | Industrial** edition: a new `idn-industrial` crate and its programs
  (industrial node, session helper, Site Manager). It does not link or change the
  `idn-open-protocol` library API; existing integrations are unaffected.

### Changed — action may be needed
- `api::OutputStats` and `api::TappedFrame` are now `#[non_exhaustive]`. If you
  constructed either with a struct literal, or matched one exhaustively, that no longer
  compiles from outside the crate. **Migration:** you never need to *build* these (they are
  returned to you); read the fields you use, and add `..` to any pattern. This is a
  one-time change so that later additions — see below — no longer break anyone.
- Over the `0.1.x` line, monitoring counters were added to `OutputStats` for compressed
  video (`codec/1`: `codec_encoded`, `codec_passed`) and encrypted multicast (`mcast/1`:
  `mc_key_changes`, `mc_receivers`, `mc_resent`). Code that exhaustively constructed or
  matched `OutputStats` on an earlier `0.1.x` needed those fields added; with the
  `#[non_exhaustive]` above, that class of break is now closed.

## [0.1.0] — initial clean-room release

The first public cut of the library: `Publisher`/`Output`, `send`/`send_planes`/
`send_audio`, taps, discovery (mDNS + Discovery Server), the IDN link, secure sessions,
encrypted multicast (`mcast/1`), compressed video (`codec/1`, via `idn-ffmpeg`), and the
programs (`idn-node`, `idn-recv`/`idn-send`, `idn-mixer`, `idn-switcher`, `idn-audio`,
the OBS plugin). The version stayed `0.1.0` across this work, so builds taken at different
commits can differ; `0.2.0` is the first bump, and tagged releases start the stable
pin points described above.

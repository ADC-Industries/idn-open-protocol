# Integrating IDN Open Protocol into a host application

For developers wiring `idn-open-protocol` into an application such as the presentation
server, behind its video sink/source trait. All the code below also lives in
[`examples/integration.rs`](../core/idn-open-protocol/examples/integration.rs), which is built and run, so it
matches the API. Run it with `cargo run --release -p idn-open-protocol --example integration`. Every type
and option is described in the API reference: run `scripts/build-docs.sh`, then open
`docs/api/index.html` — or, once you depend on the crate, `cargo doc -p idn-open-protocol
--open` from your own workspace builds the same reference. The IDN specification (the
documents referred to as `spec/…` and `spec/extensions/…`) is published separately and is
coming soon.

IDN Open Protocol is NDI-Compatible: its senders and receivers interoperate with NDI 6.3
software, verified against NDI 6.3 in a lab. NDI® is a registered trademark of Vizrt NDI
AB; this project is not affiliated with or endorsed by Vizrt.

**Versioning — what to depend on.** Pin a tagged release, not `main`: the current one is
**`v0.3.8-Beta18`** (a pre-release, source only: no installers are published yet, so
build the programs from source as the README describes). Tags don't move; `main` does, and may break at any commit between releases. [../CHANGELOG.md](../CHANGELOG.md) lists breaking changes and the rules the API
follows: types you only read (`OutputStats`, `TappedFrame`, the `SendError` enum) are
`#[non_exhaustive]` — match them with a trailing `..` — and config structs are built from
`Default`/`::new` (`PublisherConfig { .., ..Default::default() }`, `OutputConfig::new`), so
a later added field doesn't break your build.

**IDN Media as a server and libraries.** IDN Media has no UI but each computer's IDN
Node panel: the program that embeds it provides the UI. To host IDN Media's Node, Site Manager, switcher, desk or
session agent in your program, or drive them over HTTP, the push channel or MQTT, see
[MEDIA-API.md](MEDIA-API.md). This document is about the protocol library itself.

**In-ear mixes on phones.** The musician's phone app embeds IDN's mixing
library, crate `idn-iem` (`media/idn-iem`): it takes the desk's IEM flow (Opus, unicast or
multicast), mixes it by the musician's mix saved on the desk and followed live, and sends
push-to-talk back. Rust API `idn_iem::receiver::Receiver` (`connect`, `render`,
`set_level` …, `talk_*`, `status`, events); a C ABI for Kotlin (JNI) and Swift
(`include/idn_iem.h`, built as a `cdylib` for Android or a `staticlib` for iOS with
`cargo rustc --crate-type`). Its build targets, threads and latency budget:
[media/idn-iem/README.md](../media/idn-iem/README.md); the protocol: spec idn-iem.md.

## 1. The three pieces

| You need | Use | Threads |
|---|---|---|
| Named outputs (program, lower third, stage display…) | `api::Publisher` + one `api::Output` each | Each output has its own worker |
| Inputs (cameras, other software) | `input::Input` | Each input has its own worker |
| A source list for the UI | `discovery::Browser` | One background thread |

Nothing in the library blocks your render loop, panics on bad input, or prints. Errors
are returned as values. For diagnostics, install a logger once:

```rust
idn_open_protocol::log::set_logger(|m| tracing::debug!("{m}"));
```

## 2. Outputs

```rust
let publisher = Publisher::new(PublisherConfig::default())?;   // one per process
let mut cfg = OutputConfig::new("Lower third");                 // full name: "HOST (Lower third)"
cfg.alpha = true;                                               // SHQ7: key + fill
cfg.frame_rate = (30000, 1001);                                 // any N/D, per output
let out = Output::new(&publisher, cfg)?;

let mut f = VideoFrame::rgba(1920, 1080, rgba, AlphaMode::Premultiplied);
f.timestamp = Some(t_100ns);   // optional: 100 ns since the Unix epoch; None = now
out.send(f)?;                  // returns at once
```

- **`send` never blocks.** Frames go into a one-slot mailbox and the newest wins. If you
  submit faster than the output processes, older frames are replaced and counted in
  `out.stats().replaced`. Your one-slot channel in front of the sink can therefore go:
  calling `send` directly from the render loop is fine.
- **Static frames are almost free.** If a frame is byte-identical to the previous one,
  its encoding is reused, which is ideal for lyrics re-sent at the output rate. A static
  1080p30 output costs about 15× less CPU than a changing one. Passing the same
  `Arc<[u8]>` again is the cheapest; equal contents in a new buffer are detected too
  (a memcmp, about 1 ms at 1080p).
- **Alpha.** The wire carries *straight* alpha with full range 0–255, verified against NDI
  6.3 (spec §3.10.2). Pass your premultiplied RGBA with `AlphaMode::Premultiplied`; the
  library un-premultiplies. Opaque outputs (`alpha: false`) send SHQ2 and ignore alpha.
- **Colour.** Limited-range BT.709, the matrix NDI 6.3 uses at every resolution (§3.10.3).
  Input is treated as sRGB-encoded R′G′B′.
- **Audio.** Attach planar float samples for that frame's duration with
  `f.audio = Some(AudioBlock { sample_rate, channels })`. Audio and video of one `send`
  stay together, and audio of a frame replaced before it was sent goes out with the next
  one. Or send audio on its own from your audio callback, in blocks of any length:
  `out.send_audio(block, None)`. With `None` the timecode continues sample-accurately
  from the previous block, and you can pass your own instead.
- **Lip sync.** Attached audio's first sample gets its frame's timecode, so attach the
  samples that start with that picture. Audio queued for one frame but attached to the
  next comes out about a frame late (33 ms at 30 fps). By default, video and
  `send_audio` both use the output's clock (`out.timecode_now()`). To use your own media
  timeline, set `frame.timecode = Some(t)` on video and pass `Some(t)` to `send_audio`,
  both on the same timeline.
- **Format changes.** The frame size can change from one frame to the next.
  `out.set_alpha(true)` and `out.set_frame_rate((60, 1))` take effect from the next
  frame. Connected receivers stay connected and just get the new format.
- **Metadata.** `out.send_metadata(xml)` sends one XML element to the connected
  receivers. `out.recv_metadata()` returns what receivers sent you (tally is handled for
  you).
- **Bandwidth.** `cfg.quality` (default 92) sets picture quality. `cfg.target_mbps =
  Some(x)` caps each receiver's stream by lowering quality (never below 50) while over
  budget.
- **Adaptive quality (`adapt/1`, off by default).** With `cfg.adapt = true`, quality
  follows each IDN receiver: down 4 steps when it has trouble (skipped, lost or late
  frames), up 1 step after 3 clean seconds, never below `cfg.quality_floor` (default 50).
  Below the floor frames are skipped instead. NDI receivers keep the fixed quality.
  `idn-send --adapt --floor 60`.
- **Changed regions (`regions/1`, off by default).** With `cfg.regions = true`, IDN
  receivers that also have it on get only the parts of each picture that changed: a
  lyrics slide or lower third costs about 1–2 KB per frame instead of tens, with a
  whole picture every 2 s. Receivers still get every picture whole; NDI receivers are
  unaffected. `idn-send --regions`.
- **Names.** The full name `HOST (name)` must fit in 63 bytes (one DNS label;
  `api::MAX_FULL_NAME`). `Output::new` refuses a longer one, an empty one, or one with
  control characters, with an error saying how long the name may be on this host.
- **Discovery.** Outputs are announced on mDNS, on every network the machine is on, each
  with its own address (a server on the church LAN and a video network is found on
  both). With `PublisherConfig { discovery_server:
  Some("host:5959".into()), .. }` they register with a Discovery Server instead, and mDNS
  is then off by default: finders configured with a server also browse mDNS and would
  list everything twice (§8.3.7).
- **Multicast (off by default).** `cfg.multicast = Some(McConfig::default())` sends one
  multicast stream for all receivers that ask (needs IGMP snooping on the switches; poor
  on Wi-Fi). `McConfig.mode`: `Plain` (default, as NDI), `Encrypted` (IDN displays on a
  link get the stream sealed and signed, `mcast/1`; every other receiver gets unicast,
  so nothing is clear on the network) or `PerReceiver` (IDN displays keep their own
  encrypted connections; NDI receivers get plain multicast). `signatures` (default on)
  stops one display from forging frames for the others; `key_interval` (default 30 min)
  sets how often the key changes, besides whenever a display leaves.
  `out.stats().mc_receivers` and `mc_key_changes` show it working. On the input side,
  `Transport::Multicast` with the link takes it; `stats().mc_rejected` above zero means
  someone is injecting packets. `idn-send --multicast --mc-encrypted`.
- **Receivers.** `out.receivers()` tells you how many are connected (useful for a "live"
  indicator).

## 3. Inputs

```rust
let mut cfg = InputConfig::new("CAMHOST (PTZ 1)");   // or "10.0.0.20:5961"
cfg.receiver_name = "Presentation server".into();
cfg.rgba = Some(AlphaMode::Premultiplied);            // optional RGBA for the renderer
let input = Input::open(cfg)?;
loop {
    match input.recv(Duration::from_millis(100)) {
        Some(InputEvent::Video(v)) => upload(&v),          // decoded SpeedHQ
        Some(InputEvent::Compressed(c)) => decode(&c),     // H.264 / HEVC for your decoder
        Some(InputEvent::State(s)) => ui_state(s),         // Connecting / Connected / Lost / Routed
        Some(InputEvent::Audio(a)) => audio(&a),
        _ => {}
    }
}
```

- **Two kinds of video.**
  - `Video`: SpeedHQ, decoded by the library. `v.picture.planes` holds Y, Cb, Cr (and A
    for alpha sources) in the source's sampling: 4:2:2 for SHQ2/SHQ7, 4:2:0 for SHQ0.
    `v.rgba` holds RGBA8 if you set `cfg.rgba`; hand the buffer back with
    `input.recycle_rgba(buf)` when you are done with it (a new 4K buffer costs about
    4 ms of page faults). Using the planes and converting on the GPU is cheaper.
  - `Compressed`: NDI|HX cameras (H.264 or HEVC). The library contains **no H.264/HEVC
    codec**. Feed `c.extra` (parameter sets, present on keyframes) and then `c.data` (one
    access unit, Annex B) to your decoder (FFmpeg with VA-API). Delivery starts at a
    keyframe. After a loss, non-keyframes are dropped until the next keyframe, and the
    camera is asked for one (§7.1.4).
- **Timing.** Frames are delivered on arrival, as NDI 6.3 receivers do. Each carries
  `header.timestamp` (the sender's wall clock; it can jump, so don't schedule by it) and
  `received` (a local `Instant`). A renderer that composites the *latest* frame of each
  input at its own output rate needs no further synchronisation: camera clock drift
  becomes an occasional repeated or skipped camera frame.
- **Reconnects** are automatic. Every 2 s the library resolves the name again and
  reconnects, and `State` events let the UI show "reconnecting". This covers camera
  reboots, which were verified against real senders.
- **Back-pressure.** If you stop reading, media events beyond `cfg.queue` (default 8) are
  dropped (oldest first) and counted in `stats().dropped`. State events are never
  dropped. If the machine is too busy to decode every picture, a picture is skipped
  whenever a newer one has already arrived (`stats().late`), so inputs stay current
  instead of falling behind. Set `skip_late: false` to decode every frame, for example
  when recording. Unread metadata, error and log events are kept up to 256 (4 MB),
  counted in `stats().events_dropped` beyond that.
- **4K.** Inputs decode two SpeedHQ pictures at once by default (`parallel_decode`:
  `None` follows the machine's setting, else on; also `input.set_parallel_decode`),
  handed over in order, so 4K at 50/60 fps keeps up.
  If the picture is shown smaller, say so with `cfg.max_size` / `input.set_max_size`:
  IDN sources then send it smaller, and sources that can't scale (NDI cameras) are
  decoded at half size when that still fills it (a 4K camera on a 1080p screen: about
  two thirds of the decoding time, the RGBA conversion a quarter). With a decoder (`cfg.codec`, `CodecUse::Allow`), 4K from an
  IDN output with a GPU encoder arrives as H.264/HEVC instead (section 2).
- **Picture size limit.** Pictures larger than `cfg.max_picture` (default 4096 × 4096,
  at most DCI 4K in pixels) are rejected unread and counted in `stats().video_errors`:
  the size is the sender's claim, and a few bytes can claim a huge one.
- **Interlaced** sources deliver fields (`header.field_id` 1 and 2). Use
  `input::weave_fields` to combine them (it returns `None` for two fields that don't
  belong together; skip that frame), or show each field.
- **Control.** `input.set_tally(program, preview)`, `input.set_low_bandwidth(true)` (the
  640-wide proxy stream), `input.send_metadata(xml)`.
- **Smaller pictures (`adapt/1`).** `InputConfig { max_size: Some((960, 540)), .. }`
  asks an IDN sender for a picture fitting inside that size (aspect kept, never scaled
  up); `input.set_max_size(Some((w, h)))` changes it live, `None` returns to full size.
  A confidence monitor or a phone saves the bandwidth and the decode. Receivers asking
  for the same size share one encode. `idn-recv --max-size 960x540`.
- **Changed regions (`regions/1`).** `InputConfig { regions: true, .. }` takes only the
  changed parts from an IDN output that has the option on; the input pastes them into the
  previous picture and hands over whole pictures as usual. `stats().region_refreshes`
  counts the times it had to ask for a whole picture (after a lost one).
  `idn-recv --regions`.

## 3a. Compressed video (`codec/1`, H.264/HEVC/AV1)

For remote rooms and Wi-Fi: an output encodes with the GPU and sends a few Mbit/s
instead of SpeedHQ's tens, to IDN inputs that want it, and by default 4K pictures to
IDN inputs that can decode them (`CodecPolicy::Auto`); SpeedHQ on the CPU is the
fallback, and NDI receivers always get it. The codecs come from the separate
`idn-ffmpeg` crate (the system's FFmpeg; VA-API on AMD/Intel under Linux, NVENC on
NVIDIA, AMF and Quick Sync under Windows; hardware encoding only):

```rust
let ff = Arc::new(idn_ffmpeg::Ffmpeg::probe());          // once per process
let mut out = OutputConfig::new("Remote room");
out.codec = Some(CodecOutput::new(ff.clone()));          // offers what the GPU encodes
let mut input = InputConfig::new("HOST (Remote room)");
input.codec = Some(CodecInput::new(ff, CodecUse::Prefer));
```

- **Who gets it.** An input with `CodecUse::Prefer` gets compressed video. One with
  `CodecUse::Allow` gets it for pictures larger than 1080p (the default
  `CodecPolicy::Auto`: 4K goes through the GPU, SpeedHQ on the CPU is the fallback), or
  always when the output sets `CodecPolicy::Force`, or never with `CodecPolicy::Offer`.
  The choice follows the size each input gets: one that asks for 1080p of a 4K output
  gets SpeedHQ. Inputs without a `codec` (or `CodecUse::Never`) always get SpeedHQ.
  `CodecInput::prefer` orders the codecs (default H.264, HEVC, AV1). For a "CPU only"
  setting, leave `codec` at `None`. The site option `codec.policy` takes `offer`,
  `auto` or `force`.
- **Bit rate.** `CodecOutput::mbps_1080p` (default 8), scaled by picture area.
- **Pictures** arrive as usual (`InputEvent::Video`), as 4:2:0 planes; `header.fourcc`
  says `H264`, `HEVC` or `AV01`. With a decoder, NDI|HX sources are decoded too.
- **Costs.** About 7–8 ms of GPU time per 1080p frame per distinct size (receivers at
  the same size share one encoder); about 5 ms more latency than SpeedHQ.
- **Decoding** is in software on every core by default, moving to the GPU at the next
  keyframe if software falls behind: on an AMD APU, reading pictures back from the GPU
  cost more than decoding them on the CPU (4K H.264: 184 pictures/s and 5 ms in
  software, 90 and ~40 ms through the GPU). Outputs above 1080p encode four slices so
  that software decoders split each picture. `IDN_DECODER=gpu|software` overrides. The
  GPU decoder is VA-API (AMD, Intel on Linux), NVDEC (NVIDIA, Linux and Windows) or
  D3D11VA (any GPU on Windows), whichever the machine has.
- **Machine setup.** The service user must be in the `render` group to reach the GPU.
  `idn-codec probe` lists what the machine encodes and decodes; `idn-codec send` and
  `idn-codec recv ADDR` test a link end to end. For distribution, use an FFmpeg built
  without GPL parts (Ubuntu's includes x264/x265; the adapter never uses them).
- Outputs with alpha send SpeedHQ to everyone (compressed video has no key channel).

## 3b. Streaming, recording and files (gateways)

Also in `idn-ffmpeg` (the GPU encodes; hardware only):

```rust
use idn_ffmpeg::gateway::{Container, Destination, Gateway, StreamSettings};
let gw = Arc::new(Gateway::new(ff.clone()));
gw.add(Destination::Rtmp("rtmp://a.rtmp.youtube.com/live2/KEY".into()))?;
gw.add(Destination::Rtmp("rtmps://live-api-s.facebook.com:443/rtmp/KEY".into()))?; // same encode
gw.add(Destination::File("/srv/recordings/2026-10-25.mp4".into(), Container::Mp4))?;
let tap = gw.attach(&program_output);    // everything the output sends; drop to stop
for s in gw.status() { /* target (key hidden), Live / Retrying(why) / …, bytes */ }
```

- **Sharing.** Destinations with the same `StreamSettings` share one encode (one GPU
  cost however many platforms); `add_with(dest, settings, true)` gives one its own.
  Defaults: H.264 at 6 Mbit/s, the output's size, keyframes every 2 s, AAC 160 kbit/s.
- **A stream that follows its upload:** `add_opts(dest, settings, AddOptions {
  adaptive_floor_mbps: Some(1.5), via: Some(local_ip), .. })` gives it its own encoder
  whose rate goes down while the connection can't keep up and back up after a good
  minute, sent from `local_ip` (RTMP). `status()` has `video_kbps`, `sent_kbps`,
  `lag_ms`. A stream more than 3 s behind skips ahead to the next keyframe.
  `gw.record(id, path, Container::Mp4)` records a stream exactly as its encoder makes it
  (every packet, also what the stream skipped; no second encode).
- **Reconnects** are automatic per destination (after 1, 2, 3, then every 5 s);
  `gw.remove(id)` finishes a recording
  properly (complete file). MP4 recordings are fragmented: a crash loses only the last
  couple of seconds.
- **Record an input:** call `gw.send(&frame)` with its pictures (as `TappedFrame`).

Files as sources:

```rust
use idn_ffmpeg::player::{FilePlayer, PlayerConfig};
let player = FilePlayer::open(&publisher, OutputConfig::new("Welcome video"),
                              "/srv/media/welcome.mp4", ff.clone(), PlayerConfig::default())?;
player.pause(); player.seek(Duration::from_secs(30)); player.resume();
```

- Any file FFmpeg reads plays to every receiver (decoded, SpeedHQ). H.264/HEVC files
  without B-frames and with keyframes ≤ 2.5 s apart also go straight to `codec/1` inputs
  without re-encoding (`player.status().pass_through` says why not otherwise). To make
  files that pass through: H.264, no B-frames, 2 s keyframes (e.g. what the gateway
  records).
- `output().stats().codec_passed` / `codec_encoded` show what the GPU did.
- Sound: AAC files (MP4, MKV) pass their AAC through too, to the inputs that get
  compressed video and have the `idn-ffmpeg` decoder; they still hand your app float
  audio. Everyone else gets the file's sound decoded. A player of your own can do the same
  with `Output::send_audio_coded`.

Lab tool: `idn-codec send --stream URL --record FILE.mp4`, `idn-codec play FILE [--loop]`.

## 4. Source list

```rust
let browser = Browser::start(BrowserConfig::default())?;  // or { discovery_server: Some(..) }
for s in browser.sources() { /* initial list */ }
while let Some(ev) = browser.recv(timeout) {
    match ev { SourceEvent::Added(s) | SourceEvent::Changed(s) => …, SourceEvent::Removed(s) => … }
}
```

Sources that vanish without a goodbye (power loss) drop off after about 20 s. Networks
that come up after the browser started (DHCP at boot, a cable plugged in) are searched
within 5 s. Unread changes to the same source are merged into one event. For manual
entry, pass `ip:port` to `InputConfig::new` directly; no discovery is needed.

## 5. Encryption, 10-bit and HDR (IDN extensions)

These work between IDN devices. NDI devices are not affected: they keep working as
before and simply don't get the extra features.

**The IDN link (on by default).** Between two IDN devices, everything (control, video,
audio, tally, metadata) goes over one QUIC connection, **always encrypted**, on UDP at the
output's port number. Nothing needs configuring:

- Without a passphrase, the link is encrypted but not authenticated: it stops anyone on
  the network from watching, not from impersonating. With a passphrase or site trust (below)
  the same link is also authenticated. `input.stats().link_mode` is `open`, `s1` or `s2`.
- Each device keeps its own key in `LinkConfig::default_dir()` (`$IDN_STATE_DIR`, or
  `~/.local/state/idn-open-protocol`). If a source shows up later with another key (a
  reinstalled machine, or another device using its name), the input reports it
  (`stats().key_changed`, an `InputEvent::Error`) and keeps receiving.
- If UDP is blocked, the devices fall back to the normal NDI way of connecting within
  about a quarter of a second, and remember it for that source for 10 minutes.
- Switch it off with `PublisherConfig { link: None, .. }` on outputs, or by leaving
  `Transport::Link` out of `InputConfig::transports` on inputs. A receiver that wants
  multicast adds `Transport::Multicast`, as before.

**Encryption (optional).** Give every IDN device on the site the same passphrase:

```rust
let sc = idn_open_protocol::secure::SecureConfig::new("St Mark's AV team")?;
let publisher = Publisher::new(PublisherConfig { secure: Some(sc.clone()), ..Default::default() })?;
let input = Input::open(InputConfig { secure: Some(sc), ..InputConfig::new(name) })?;
```

- IDN devices that share the passphrase encrypt and authenticate everything: control,
  video, audio and tally. `input.stats().encrypted` tells the UI.
- Devices without the passphrase, and all NDI devices, still connect unencrypted. This
  is the default ("encrypt when possible").
- `sc.require = true` makes it "require encryption": on a publisher it applies to every
  output and refuses NDI receivers too; on an input it refuses sources that can't
  encrypt.
- Once a pair of devices has encrypted, they refuse an unencrypted connection to each
  other later, because an attacker could strip the encryption offer. Keep that memory
  across restarts with `sc.sticky = StickyStore::with_file(path)`. Offer a "reset trusted
  devices" button that calls `sc.sticky.forget_all()` for when a device is replaced.
- Encrypted media travels over QUIC with standard TLS 1.3, like unencrypted media but
  with real keys. If UDP is blocked, it falls back after 2 s to the encrypted control
  connection (TCP). `input.stats().secure_fallback` tells the UI, and `idn-doctor
  --passphrase …` explains it. Media is never sent unencrypted once both sides share
  the passphrase.

**Site trust: approve, deny, revoke (recommended for your own software).** Instead of
(or as well as) a shared passphrase, each device gets its own certificate from a site
authority that runs inside an IDN Discovery Server. The app that hosts the server shows
the approve/deny/revoke UI:

```rust
// On the machine that runs the Discovery Server (e.g. the presentation server):
let authority = Authority::open(AuthorityConfig { state_dir: dir.into(), passphrase: Some(p.into()) })?;
let _ds = DiscoveryServer::start(5959, Some(authority.clone()))?;
authority.open_pairing(Duration::from_secs(120));      // "Add device" button
// authority.recv() → JoinRequested(r): show r.name and r.code, then approve(r.id) or deny(r.id)
// authority.devices() / revoke(fingerprint): the device list with a Revoke button

// On every device (including that same app, for its own outputs and inputs):
let me = Enrollment::start(EnrollmentConfig { discovery_server: "192.168.1.10".into(),
    state_dir: dir.into(), name: "BOOTH-PC (Presentation)".into(), role: DeviceRole::App,
    passphrase: Some(p.into()), admin_code: None })?;
let sc = SecureConfig::with_site(me.clone());          // use it like any SecureConfig
// me.recv() → TrustState::Pending { code }: show the code so the operator can compare it
```

- With the site passphrase set on both sides, devices join with zero clicks (the bonded
  link between your own machines). Without it, the operator opens the pairing window and
  approves after checking that both screens show the same six-digit code (`482 913`:
  show it with `trust::show_code`).
- Revoke cuts a device off at once, everywhere, and it can't join again (even with the
  passphrase) until cleared. Enrolled devices talk to each other only with their
  certificates; they don't fall back to the passphrase with each other, so revocation is
  real. A device that is enrolled after a session started switches to site trust by
  itself within about a second.
- The authority's state directory holds the site CA key: back it up
  (`authority.export_ca_key()`). Certificates never expire. If the server is off,
  enrolled devices keep working; only joins and revocations wait.
- NDI devices can't take part; NDI's own Discovery Server can't host the authority (an
  IDN device pointed at one notices within 2 s and uses the passphrase instead).

**10-bit and HDR sources.** Send `VideoFrame::p216(w, h, data)` (16-bit 4:2:2 Y'CbCr,
NDI's P216 layout) instead of RGBA. Then describe the source:

```rust
cfg.color_info = Some(ColorInfo { bits: 10, primaries: "bt_2020".into(),
    transfer: "bt_2100_pq".into(), matrix: "bt_2020".into(), ..Default::default() });
```

The colour space is sent the way NDI 6 does it (`ndi_color_info`), so NDI HDR receivers
understand it. On the receiving side, set `InputConfig.precise = true` to get
`picture.precise`, the planes with 3 extra fraction bits (P216 convention). Then use
`v.color` (primaries, transfer, matrix, range, bits) for the display conversion.
Smooth areas keep about 11 bits, enough to avoid banding in gradients and lighting
washes. Fine detail is still limited by the quality setting.

**Site monitoring.** With a Discovery Server configured, every output and input
reports its live status there automatically: receivers, frames per second, drops,
round-trip time, encryption, what each input is watching. Turn it off with
`PublisherConfig.monitoring` / `InputConfig.monitoring`. A status panel reads it all with
`discovery::Monitor`:

```rust
let monitor = Monitor::start("192.168.1.10")?;          // the Discovery Server
for d in monitor.devices() {                            // or monitor.recv() for changes
    // d.kind, d.name, d.connected(), d.receiving_from(), d.fps(), d.frames_dropped(),
    // d.encrypted(), d.is_idn(), d.get("video-resolution")…
}
```

NDI devices appear too if their application advertises them, with NDI's standard values
(no frame counters for NDI receivers). While the server is unreachable the last known
list stays and `monitor.server_reachable()` is false. On the command line:
`idn-dsctl --server HOST status`. Outputs and inputs re-register by themselves when the
Discovery Server restarts, and an output starts fine even if the server is down.

**Source list hints.** `Source::is_idn()` and `Source::offers("secure")` come from the
announcement, before connecting, so the picker can show which sources are IDN and which
can encrypt. They are hints; what a session actually uses is decided on connect.

**Clean shutdown.** Call `idn_open_protocol::shutdown::install()` once at start-up, or
drop your outputs before exiting. Otherwise a Ctrl+C or a service stop leaves the
sources listed in every NDI app for up to two minutes. `install` leaves alone any
signal your app already handles (Windows: a console handler your app sets after it comes
first); in that case call `shutdown::run_hooks()` from your own handler.

**Control and file channel (site trust only).** Enrolled devices can send each other
commands, requests and files, with no video connection between them:

```rust
use idn_open_protocol::channel::{Channel, ChannelConfig, Handler, Peer, PathSink, Reply};
let channel = Channel::start(ChannelConfig::new(enrollment.clone()), Arc::new(MyApp))?;
for d in channel.devices() { /* name, kind, fingerprint: every enrolled device online */ }
channel.send(&d.fingerprint, "presenter.next-slide", b"")?;                  // one way
let r = channel.request(&d.fingerprint, "stage.clock", b"", Duration::from_secs(2))?;
channel.fetch(&d.fingerprint, "welcome-video", &mut PathSink::new(path), &mut |done, total| {})?;
channel.push(&d.fingerprint, "slides-1021", &file, Duration::from_secs(5))?; // they fetch it
channel.set_status("Presenter", "Playing (Welcome10-21-26.mp4)", &[("slide", "3")]);
```

- Message types are your own names (`presenter.next-slide`); payloads are yours too, up
  to 16 MiB. New features need no library change.
- Your `Handler` sees who sent each message (`Peer`: name, kind, admin role) and decides
  what to accept. Any enrolled device may send to any other.
- Files resume where they stopped and are checked with SHA-256. You decide where they
  go (`FileSink`; `PathSink` writes a plain file). Transfers back off as soon as the
  network gets busy.
- **User settings, changeable while running:** `channel.set_max_rate(Some(50_000_000))`
  caps what the device sends at 50 MB/s (`None`: no cap), and
  `channel.set_trouble_response(..)` chooses what happens when live video reports
  trouble: `PerDevice` (default: slow only transfers with that device), `AllTransfers`,
  or `Off`. A slowed transfer runs at half its speed until 5 s after the trouble.
- Without site trust there is no channel (`Channel::start` fails with
  `PermissionDenied`). A revoked device is cut off within about a second.
- The status appears in the Discovery Server monitoring panel as
  `Presenter->Playing (Welcome10-21-26.mp4)`.

**Stage displays in step (sync).** Displays in the same sync group show each frame at the
same moment, within a frame:

```rust
use idn_open_protocol::sync::{SyncConfig, SyncDelay};
let left = Input::open(InputConfig {
    sync: Some(SyncConfig::new("stage", SyncDelay::Auto)),       // or Frames(3), Millis(100)
    ..InputConfig::new("SERVER (Stage left)")
})?;
```

- Turn it on only for inputs that need lockstep: it adds the group's delay. Other inputs
  are unaffected.
- Groups are independent: `stage` (the stage screens) and `screens` (the presentation
  screens) each have their own delay. Give every display of a group the same setting.
- `Auto` finds the smallest delay all displays of the group need, through their sender;
  `Frames(n)` and `Millis(ms)` fix it.
- The input hands each frame over at its presentation time: show what you get at your
  next refresh. Frames that are late anyway are dropped (`stats().sync.late`) so the
  display stays in step.
- If the site's clocks are synchronised (PTP), set `clock: SyncClock::System`.
- **One stream per frame:** `OutputConfig { frame_streams: true, .. }` and
  `InputConfig { frame_streams: true, .. }` (off by default) let a link abandon frames
  that can't arrive in time instead of delaying the next ones; worth trying on Wi-Fi.

## 5a. Site-wide options (`options/1`)

With site trust set up (§5), the operator can set the IDN options once at the Discovery
Server, for the whole site, for one device, or for one output or input, instead of on
each machine: the link on or off, encryption required, the multicast mode, per-frame
streams, changed regions, compressed video policy, sync groups and delays, adaptation
and its floor, transfer caps, gateway defaults, the listing port. The keys are listed in
the spec (§E.20.3).

**Local wins.** The library never applies site options by itself. Your enrolled device
receives the site's document; you ask for the view that applies to an output or input
and let it fill your configuration, *then* set whatever your own user chose, so that a
local setting always stands:

```rust
use idn_open_protocol::api::{Output, OutputConfig, Publisher, PublisherConfig};

let site = enrollment.options();                     // the latest document (version 0 = none)
let fp = enrollment.fingerprint();

let mut pcfg = PublisherConfig::default();
pcfg.apply_site(&site.for_device(&fp));              // link.enabled, secure.require, listing.enabled
// ... then your user's own publisher settings

let mut ocfg = OutputConfig::new("Program");
ocfg.apply_site(&site.for_output(&fp, "Program"));   // quality, frames, regions, adapt, mcast, codec policy
if let Some(q) = settings.quality_chosen_by_user { ocfg.quality = q; }   // local wins
let program = Output::new(&publisher, ocfg)?;
```

Inputs (`InputConfig::apply_site` with `site.for_input(&fp, receiver_name)`) and the
control channel (`ChannelConfig::apply_site`, and `Channel::apply_site` for changes while
running) work the same way; `StreamSettings::apply_site` in `idn-ffmpeg` covers the
gateway defaults. To follow changes, wait on `enrollment.wait_options(version, timeout)`
and apply the new view where your user has no local setting; most options take effect
at the next session, the channel caps at once.

**Setting options.** At the server's console: `options`, `set adapt.floor 60`,
`set channel.max_rate_mbps 50 device <fingerprint>`, `set mcast.mode encrypted output
Program`, `unset …`. From an application embedding the server: `Authority::set_option`.
From a device with the admin role: `idn-dsctl --server … --trust-dir … set …`.

**Access tokens.** The same site key signs access tokens for the media engines
(idn-node.md §N.31.5): `Authority::issue_token(user, name, engine, machine, scope, ttl)`
gives `idn1.…` (12 hours at most), and `idn_open_protocol::web::TokenCheck::from_enrollment`
checks one with the enrolled device's CA certificate and the site options (the users'
`tokens.not_before.<id>`, which `Authority::set_option` sets). A host serving its own API
on the network can put `web::PasswordGate::tokens_only(check, …)` and `web::lan::front` in
front of it, and keep an audit with `web::audit::Audit`. The engines' HTTP side is in
[MEDIA-API.md](MEDIA-API.md) ("Who may do what").

**Certificates for the network ports.** An enrolled device also asks the authority for a
certificate for its HTTPS ports (extensions §E.10.11: its device key, its host name, LAN
addresses and `localhost` as SubjectAltNames, 7 days, renewed at half):
`Enrollment::lan_cert()`. `web::lan::server_config(dir, name, membership)` (and
`web::lan::start`, which takes the membership of the program's IDN Node connection)
presents it with the site CA after it while the machine is in a site, else the program's
self-signed certificate; it looks again on every connection. A client pins the site CA
(`Authority::ca_der()` / `ca_pem()`, the Site Manager's `GET /api/v1/site_ca`) with an
ordinary verifier (rustls `RootCertStore` + `WebPkiServerVerifier`).

## 6. Performance

Measured on one core of an AMD Ryzen 7 6800H (8 cores / 16 threads, Linux, release
build), with the SSE2 encoder and converter (every x86-64 CPU has SSE2). The **minimum recommended machine is an AMD Ryzen 7 H 255 class mini PC** (8 Zen 4
cores / 16 threads, Radeon 780M), the same class or slightly faster, so these figures are
representative or a little pessimistic. Multi-core figures scale roughly with cores.

| 1080p frame | Cost |
|---|---|
| RGBA → 4:2:2 conversion (opaque) | 3–4 ms |
| RGBA → 4:2:2 + alpha (premultiplied input) | 6 ms |
| Encode, graphics | 4–5 ms |
| Encode, camera-like content, Q92 | 9 ms |
| Re-send of an identical frame | ≈ 0 (encoding reused) |
| Decode, camera-like content, Q92 | 12 ms |

On the minimum machine (8 cores):

| Load | CPU |
|---|---|
| Typical: program changes every frame, 3 mostly static outputs, 4 camera inputs | well under 1 of 8 cores |
| Worst case: 4 outputs at 1080p30 all changing (camera-like), plus 4 SpeedHQ inputs | about 3 of 8 cores |

Plan for about half a core per 1080p30 output whose content changes every frame,
transport included. Measured end to end with one receiver: camera-like content took 39–49%
of a core over TCP, UDP or QUIC, and dense synthetic detail at about 240 Mbit/s (a worst
case) took 52–54% over TCP and about 75% over QUIC. Allow about 0.1 cores per SpeedHQ
input.

Per-frame work runs on one shared pool of threads, by default one per physical core and
at most six. Change it at any time with `idn_open_protocol::set_worker_threads(n)`
(`worker_threads()` and `default_worker_threads()` report it), for example from a
settings page; `IDN_WORKER_THREADS` sets it at start. 0 hands the choice back: the
environment variable, else the machine's setting in the IDN Node, else automatic. More
threads make each large picture faster and cost a little more CPU time in total.

**Audio routing.** To let the IDN Site Manager's routing matrix send sound to your
program, offer a receiver through the IDN Node,
`node::routing::AudioReceiver::register(&node, "Stage monitors", 8, &labels)`, and start
a `node::routing::Router` on it: it opens one input per transmitter in the routes (sound
only, only the channels needed) and hands you each block with a map of block channel →
your channel; play each transmitter through its own buffer and drift correction
(`idn_audio::Playback::lane()` does that for a sound card). Routes are kept by the Node.
Name your outputs' channels with `OutputConfig::audio_labels` for the matrix.

**The machine's video settings.** A program connected to the IDN Node also gets the
machine's video settings (`idn_open_protocol::node::video::machine()`; set in the Node's
panel or from the Site Manager): threads, two pictures at once, encoding and decoding. Leave your own setting unset to follow them: don't call
`set_worker_threads` (or call it with 0), leave `InputConfig::parallel_decode` at `None`,
don't call `Ffmpeg::set_decoder`, and build an output's encoder from
`machine().encoding` (`Encoding::policy()`: `None` means SpeedHQ only, no encoder).
Offer "the machine's setting" as the default choice in your settings page, with your own
values as overrides.

**4K on the CPU (SpeedHQ).** Dense 4K content at Q92 (about 3.9 MB a picture, a worst
case), measured with `cargo run --release --example bench_4k`; the inverse DCT and the
colour conversions use AVX2 where the CPU has it, with results identical to the scalar
code:

| 3840 × 2160 picture | 1 thread | 6 threads (default) |
|---|---|---|
| Send: RGBA → planes + encode | 47 ms | 11.7 ms |
| Send with alpha | 71 ms | 15.2 ms |
| Receive: decode + RGBA | 55 ms | 18.5 ms (54 fps; 65 fps two at once) |
| Receive with alpha | 68 ms | 19.0 ms (53 fps; 61 fps two at once) |
| Receive at half size (shown at 1080p) | 34 ms | 7.8 ms (lab, NDI camera-like 4K: 65–70% of full) |

So the default pool sends 4K60 and, decoding two pictures at once, receives 4K60 on this
class of machine. Between IDN devices, a GPU encoder takes 4K off the CPU altogether:
section 2, compressed video.

Under overload (every CPU busy), outputs drop frames rather than queue them. In a test
with three 1080p30 outputs and all 16 CPUs saturated, receivers got 13–21 fps, with a
longest gap of 0.2 s and latency under 0.6 s. Before this, the same test stalled for
seconds, with latency up to 17 s. HX (H.264/HEVC) inputs are decoded by the host; the 780M
decodes both in hardware through VA-API.

Typical bandwidth at Q92, 1080p29.97: a mostly flat slide or graphics output needs about
9 Mbit/s, and a busy camera picture about 45 Mbit/s. Lower quality saves a lot on busy
pictures: the same camera picture needs about 19 Mbit/s at Q70. Flat content saves less:
graphics at Q80 needed about two-thirds of their Q92 bit rate.

## 7. Network checklist (for whoever installs it)

- **Wired gigabit** for cameras and the presentation server. Avoid Wi-Fi for video.
- **A separate AV network or VLAN**, not shared with guest or office Wi-Fi: anyone on the
  same network can see and watch the feeds, and phones add load.
- **Firewall:** allow inbound TCP for the control ports and inbound UDP for QUIC media on
  the receiving machines, and inbound UDP on the same port numbers as the control ports
  on sending machines (the IDN link; without it IDN devices fall back to TCP).
- **HX PTZ cameras:** set the keyframe interval to about 1 second. A receiver that joins
  can only start at a keyframe, and these cameras don't produce one on request.
- **Run `idn-doctor`** on the presentation server. It checks interfaces, a shared network,
  sources and reachability, and with `--test "HOST (name)"` it measures a stream's frame
  rate, bitrate and errors. It explains each problem in plain language.

## 8. Platform and dependencies

- Linux x86-64 (Debian 12, Ubuntu 24.04 and 26.04) and Windows x64 are built and tested
  in the lab; macOS is untested. `shutdown::install` handles SIGINT/SIGTERM on Unix and,
  on Windows, the console's control events (Ctrl+C, Ctrl+Break, the window closed,
  log-off, shutdown) and the session's end through a hidden window (programs without a
  console too). A Windows service's stop is the service's to handle: call
  `shutdown::run_hooks()` on it (the IDN Node service does).
- The library's dependencies (`core/idn-open-protocol/Cargo.toml`), all pure Rust or
  bundled, MIT/Apache-compatible:
  - networking: `socket2`, `libc`, `bytes`;
  - QUIC (NDI-Compatible and encrypted media): `quinn`, `quinn-proto`, `tokio`, with
    `rustls` and `ring` for TLS 1.3 and `rcgen`, `x509-parser`, `time` for certificates;
  - the passphrase-derived site key: `argon2`, `hkdf`, `sha2`, `rand`;
  - joining a site with a typed code (§E.10.10): `spake2` (RustCrypto, MIT OR Apache-2.0);
  - the site clock (PTP): `statime`, IDN's own fork in `third-party/statime`;
  - sample-rate conversion: `rubato`;
  - the APIs and their push channel: `serde_json`, `tungstenite` (WebSocket);
  - on Windows: `windows-sys`;
  - optional: `wgpu`, `ash`, `bytemuck`, `windows` (feature `gpu`: GPU pictures between
    programs on one machine) and `webpki-roots` (feature `http-client`).

  The library has no FFmpeg and no codec library. FFmpeg is used only by the `idn-ffmpeg`
  crate and what is built on it: the switcher (recording, streaming, media inputs), the
  OBS plugin, Industrial, and the `idn-codec` program.
- `quinn-proto` is vendored in `vendor/quinn-proto` with one small patch: NDI-Compatible
  QUIC never changes keys, so quinn's routine first key update is skipped for it (see
  `vendor/README.md`). Re-apply the patch when updating quinn.
- Licence: MIT OR Apache-2.0, at your option (the same as the host), copyright ADC Industries.

## 9. A box nobody sits at: packages, Node services, and mixing live on a Dante-compatible network

For a host that ships a machine of its own (a rack box, an image): IDN is installed as
packages and run by the IDN Node, a system service. IDN makes no image or appliance of its
own; the box around the packages is the host's.

**Packages** (amd64, for Debian 12, Ubuntu 24.04 and 26.04; made by `scripts/package.sh
--media TAG` into `Installers/Media`):

| Package | What it is | Licence |
|---|---|---|
| `idn-media` | The Node (the service `idn-node`), the `idn` command and its tools, the switcher, the mixing desk, the panels, the network-audio device `idn-aoip` (below); FFmpeg bundled | MIT OR Apache-2.0 (FFmpeg: LGPL) |
| `idn-site-manager` | A site's authority and web panel, on its own (a server) | MIT OR Apache-2.0 |

The network-audio device is IDN's own (MIT OR Apache-2.0): it is in `idn-media` (which
replaces the separate `idn-aoip` package of earlier builds) and in IDN Media's Windows
installer.

**Settings without a screen.** Everything the Node's panel sets, `idn-node set KEY=VALUE`
sets too (as root, or a member of the group `idn`), and `idn-node status` reads back; a
Site Manager can set the same on every machine of a site. A bad value is refused with its
reason and nothing is changed.

**The network-audio device.** `idn-aoip` makes the machine one more device on a network of
Dante-compatible equipment: that network's channels come into IDN as a source, IDN's sound
goes out to it, and the building sets its subscriptions in the controller it already uses.
It is unofficial: not made, authorised or supported by Audinate; Dante is a trademark of
Audinate. Say so wherever you name it, and don't put "Dante" in a product or program name.

```sh
# The Node's site clock on that network's time (equipment without AES67 speaks PTP v1):
idn-node set clock-protocol=v1          # or: both, to carry it to IDN machines on v2
# The device, from boot:
idn-node set netaudio-on=true netaudio-channels=16
# With more than one network card, the audio network's by its address:
idn-node set netaudio-interface=10.0.0.7
```

| `netaudio.` | Values | Default |
|---|---|---|
| `on` | true, false | false |
| `channels` | 1–128 each way | 64 |
| `device` | its name on that network: 1–31 letters, digits and `-` | `HOST-IDN` |
| `interface` | the network card's IPv4 address | the system's choice |
| `latency` | its receive latency, 1–40 ms | 2 |
| `receivers_latency` | what the devices receiving from it are set to, 1–40 ms | as `latency` |
| `desk`, `desk_name`, `desk_block`, `desk_outputs` | the desk in the device's channels (below); `desk_outputs` N: its outputs on the device's first N channels, IDN's routes on the rest | false, `Network Mixer`, 64, all |
| `read_only` | the Site Manager only reads the network's devices (no subscriptions or renames from it) | false |

Its sample rate is the machine's (44.1, 48 or 96 kHz: `idn-node set audio-rate=…`, or the
site option `audio.rate`; idn-node.md §N.40), set from IDN only: Dante Controller shows it,
fixed. The device follows the network's PTP leader; it leads only where it is told to.

**Mixing live.** Routed through IDN like any source, sound from that network through the
desk and back takes about 20 ms on one machine: each program re-times what it gets in a
buffer of its own. For monitors and front of house, run the desk in the device's channels
directly:

```sh
idn-node set netaudio-desk=true         # needs netaudio-on
```

The Node then starts `idn mixer --network-audio DEVICE` after the device. The network's
channels arrive on the desk's channels 1, 2, … (named "Net 1", …), and the desk's outputs
go out on the device's channels in order (output 1 and 2, the main mix, on its channels 1
and 2), a block of the desk and a few milliseconds in all: about 9 ms from the network in
to the network out with both devices at 2 ms and 64-sample blocks (measured on one
machine; a real network adds its own). While the desk runs, it has the device's transmit
channels its outputs go on (all of them as the Node runs it; `idn mixer
--network-audio-outputs N` takes the first N only). IDN's routes to the device keep
playing on the others; a route to one of the desk's channels waits (the device's log
names it) and plays again when the desk stops.

What the box needs for this:

- **Linux or Windows.** (On Windows not beside Dante Virtual Soundcard or Dante Via: they
  are devices too and hold the same ports.)
- **One machine.** The desk finds the device through the state folder they share (the
  Node gives both the same one).
- **Real-time scheduling for the service** (the Node's unit sets `LimitRTPRIO=60`; a log-on
  gets the same from the package's `idn` group), and nothing heavy beside it: the path keeps about 2 ms of
  room. A block that lands late is counted and the desk then writes a millisecond further
  ahead (to 8 ms more), so it settles on what the machine can keep.
- **Not with a sound card on the same desk**: the desk follows one clock.

**Reading its state.** The desk's panel API (`GET /api/v1/state` on its port) has
`network_audio`: `state` ("running", "waiting for the device", "no site clock"), `in` and
`out` (channels), `out_first` and `out_count` (the transmit channels the desk has; the
device's routes keep the others), `write_ahead_ms`, and the counters `resyncs`,
`late_ticks` and `late_writes` (all three should stand still on a healthy box). `idn-node status` lists the
two services `netaudio` and `netaudio-desk` with how long each has run.

**Embedding instead.** A host that embeds the desk (`idn_mixer::Console`) rather than
running `idn mixer` sets the same thing in its config: `ConsoleConfig::network_audio =
Some(idn_mixer::netaudio::NetAudioConfig { device, first, outputs: None })` (`outputs`:
how many of its outputs go to the device; `None`, all it has room for). The device runs as its own
program (`idn-aoip`); the desk only maps the memory it shares and reads the Node's clock.
Its engine (`core/idn-aoip-device`) is MIT OR Apache-2.0 like the rest, so a host may also
run it in its own process.

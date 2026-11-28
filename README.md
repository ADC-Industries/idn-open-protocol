# IDN Open Protocol

**IDN** (Intelligent Device Network) Open Protocol is an open, NDI-Compatible network
video protocol implemented in Rust. It sends and receives live video, audio and metadata
on a local network, and interoperates with NDI 6.3 senders and receivers. It was written
clean-room, from public documentation and black-box testing, without vendor code.

## What's in it

- **User guide** for churches' volunteers and technical directors:
  [docs/guide](docs/guide/README.md) (setting up, quick starts, the switcher, the mixer,
  the site, simple mode, troubleshooting). The installer puts it beside the programs as
  pages ("IDN Guide" in the Start menu); `scripts/build-docs.sh` makes them here.
- **Specification**: the IDN specification (the documents referred to in the code as
  `spec/…` and `spec/extensions/…`) is published separately and is coming soon.
- **Library** (`idn_open_protocol`):
  - many named outputs per process, fed RGBA frames without blocking;
  - inputs that deliver decoded SpeedHQ pictures, or H.264/HEVC access units for your own
    decoder;
  - a live source browser;
  - mDNS and Discovery Server support;
  - QUIC, UDP, multi-TCP and multicast transports.

  See [docs/INTEGRATION.md](docs/INTEGRATION.md) for the guide, and
  [CHANGELOG.md](CHANGELOG.md) for versioning and what to depend on — pin the tagged
  release **`v0.3.8-Beta22`** (a pre-release), not `main`. For the API reference, run
  `scripts/build-docs.sh` and open `docs/api/index.html` (static HTML, works offline);
  if you depend on the crate, `cargo doc -p idn-open-protocol` from your own workspace
  builds the same reference, so you don't need this repo's script.
- **Tools**, as one command, `idn` (`media/idn-cli`; `idn help` lists them, `idn COMMAND --help`
  each one's options). The old names (`idn-send`, …) are the same commands: run under such
  a name (a link or a copy), `idn` runs that command.

  | Command | Purpose |
  |---|---|
  | `idn send` | Test source |
  | `idn recv` | Receiver and recorder |
  | `idn find` | List sources |
  | `idn publish` | Announce sources |
  | `idn channel` | The control and file channel between devices (`channel/1`; a lab tool) |
  | `idn router` | Routers: one name whose receivers (IDN and NDI) follow the source it is switched to (the Node's Routers card) |
  | `idn doctor` | Network setup checker |
  | `idn discovery-server` | Discovery Server; with `--trust-dir` the site authority and the IDN Site Manager panel |
  | `idn dsctl` | Discovery Server client |
  | `idn ptz` | Camera control |
  | `idn audio` | Sound cards (`media/idn-audio/README.md`) |
  | `idn mixer` | The mixing desk (`media/idn-mixer/README.md`) |
  | `idn data` | IDN Data: data feeds as text-only sources (`media/idn-data/README.md`) |
  | `idn omt` | Open Media Transport: OMT sources as IDN sources and back (the Node's OMT card) |
  | `idn captions` | Live captions on this machine from a sound source (whisper.cpp; the Node's Live captions card) |
  | `idn node`, `idn route`, `idn routes` | IDN Node: the machine's site identity, panel and audio routes (`media/idn-node/README.md`; the program `idn-node`) |
  | `idn switcher` | The video switcher (`media/idn-switcher/README.md`; the program `idn-switcher`, which needs FFmpeg) |
  | `idn display` | A source full screen on a display: projectors, confidence monitors (`media/idn-display/README.md`; the program `idn-display`) |
  | `idn codec` | H.264/HEVC through FFmpeg (the program `idn-codec`) |

## Layout

One Cargo workspace, by edition:

| Folder | What |
|---|---|
| `core/` | What both editions share: the protocol library `idn-open-protocol` (with the basic tools, `idn-send`, `idn-recv`, …), `idn-ffmpeg` (hardware codecs, stream gateways), `idn-shq-gpu`, `idn-omt` (Open Media Transport), and three small shared crates by what they need: `idn-common` (below the library, the standard library only: saving files safely, the state folder, real-time threads, HTML escaping, the helper-program supervisor), `idn-dsp` (sound processing: noise suppression) and `idn-feeds` (data feeds read as rows, for IDN Data and the switcher's titles) |
| `media/` | The media edition, a server and libraries with no UI but the Node's control panel (the UI is the embedding application's; [docs/MEDIA-API.md](docs/MEDIA-API.md)): `idn-node` (the Node service, embeddable, and `idn-session`), `idn-site-manager` (the Site Manager and `idn-discovery-server`, embeddable), `idn-data` (IDN Data), `idn-captions` (live captions), `idn-switcher` (engine and service), `idn-mixer`, `idn-plugin-host`, `idn-audio`, `idn-asio`, `idn-display`, `idn-vcam`, `idn-camera-source`, `idn-obs` and `idn-obs-loader` (the OBS plugin), `idn-audio-plugins` (IDN Send / Return, CLAP and VST3), `idn-cli` (the `idn` command), `idn-aoip` (an endpoint on networks of Dante-compatible equipment, on IDN's own engine `core/idn-aoip-device`) |
| `third-party/` | Others' libraries IDN keeps its own copies of, each under its own licence with a README-IDN.md of what IDN changed: `statime` (PTP, with v1; Apache-2.0 OR MIT) |
| `industrial/` | IDN Industrial: `idn-industrial` (Node, Site Manager and session helper \| Industrial) |
| `tools/` | ADC Industries' own: `idn-release` (signs releases), `idn-guide` (the user guide as pages) |

The library keeps what every program needs, including how a program talks to its Node
(`idn_open_protocol::node`) and the Node's settings; the Node service itself is in
`idn-node`, the Site Manager in `idn-site-manager`. `release-keys.txt` (the keys every
build trusts) stays at the top.

## Building from source

Linux: Ubuntu 24.04 or newer, or Debian 13. (Debian 12 can't build it: its FFmpeg is 5.1
and IDN needs 6.1 or newer, and its Vulkan headers lack `VK_EXT_layer_settings`. The
packages built here still install on Debian 12; they bring their own FFmpeg.)

1. Rust from [rustup](https://rustup.rs) (stable), and git.
2. The system packages (Ubuntu/Debian names):

   ```sh
   sudo apt install build-essential pkg-config cmake clang libclang-dev \
     libglib2.0-dev libgtk-3-dev libwebkit2gtk-4.1-dev libasound2-dev libpcsclite-dev \
     libobs-dev libavutil-dev libavcodec-dev libavformat-dev libswscale-dev \
     libswresample-dev libavfilter-dev libavdevice-dev libvulkan-dev glslc \
     libxkbcommon-dev libwayland-dev libudev-dev
   ```

3. Every program and library: `cargo build --release --workspace` (a plain
   `cargo build --release` builds only the `idn-open-protocol` library, the workspace's
   default member). On a 4-core machine this takes about 10 minutes and about 3 GB of
   `target/`. The programs are in `target/release/` (`idn`, `idn-node`,
   `idn-site-manager`, `idn-switcher`, …); `target/release/idn doctor` checks the machine.
4. One crate: `cargo build --release -p idn-open-protocol` (the library alone needs only
   Rust and a C compiler).

Windows builds are cross-compiled from Linux (`scripts/setup-windows-kit.sh`, then
`scripts/package.sh`); building natively on Windows isn't covered yet.

Test with `cargo test --release --workspace`. The NDI|HX compatibility tests need ffmpeg
with libx264 and libx265 and are skipped without them; set `IDN_REQUIRE_FFMPEG=1` to make
a missing ffmpeg a test failure instead.

## Trademarks

NDI® is a registered trademark of Vizrt NDI AB. Dante® is a registered trademark of
Audinate Pty Ltd. Other names are trademarks of their respective owners. IDN is not
affiliated with or endorsed by them. "NDI-Compatible" means that IDN interoperates with
NDI devices and software.

## License

Copyright (c) 2026 ADC Industries.

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option. Except: the OBS plugin (`media/idn-obs`, `media/idn-obs-loader`) is
GPL-2.0-or-later (it links libobs); and `third-party/` and `vendor/` keep each library's
own licence (see [NOTICE](NOTICE)). No MIT/Apache crate links the GPL one.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in this project by you, as defined in the Apache-2.0 license, shall be dual
licensed as above, without any additional terms or conditions.

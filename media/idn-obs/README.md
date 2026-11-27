# idn-obs: OBS Studio plugin on the IDN Open Protocol

Status: **first version, 2026-10-23**, built against OBS Studio 32.1 (Ubuntu's
`libobs-dev`) and checked in a headless OBS on the build machine: the IDN Source
receives from `idn-send` (listed as the sender's receiver, no skips), the IDN Output is
found by `idn-find` and received by `idn-recv` at 30 fps with sound, 0 errors. Linux
first. Awaits the lab (items 66–68).

## What it does

- **IDN Source** (OBS source type `idn_source`): a source picker filled from the
  finder (mDNS and the Discovery Server), video and audio from an IDN `Input` into
  OBS, and OBS's program/preview state sent back as tally (`Input::set_tally`).
- **IDN Output** (OBS output type `idn_output`): OBS's program (optionally preview as
  a second output) as an IDN `Output`, converting what OBS renders (NV12, I420 or
  BGRA) to what the sender takes (`VideoFrame::rgba`, or planar 4:2:2 through
  `send_planes`), audio through `send_audio`.
- **Settings** (OBS properties): source name / output name, passphrase or site trust
  (Discovery Server + trust directory), transport choices (link, QUIC, TCP,
  multicast), compressed video (`codec/1`) use and preference, low bandwidth.

## Rules

- **Clean room.** Built from OBS Studio's public plugin API only: the installed
  `libobs` headers and docs.obsproject.com. Never from DistroAV (the OBS–NDI plugin)
  or any NDI SDK or NDI plugin source.
- **Licence.** libobs is GPL-2.0-or-later, so this plugin as distributed is GPL; the
  IDN Open Protocol library it links is MIT OR Apache-2.0 (compatible) and unchanged.
- **Timestamps.** OBS timestamps (nanoseconds, `os_gettime_ns` clock) are mapped to
  IDN timecodes and timestamps so that sound and picture leave together and arrive
  together at NDI receivers (lab items 49/60: a file's audio ran 200 ms ahead when a
  decoder's latency went unaccounted). The plugin hands each frame to the library with
  the timecode derived from OBS's timestamp, not from arrival time.
- **No library changes for OBS's sake** beyond what any host application needs; a C
  ABI for other hosts is a separate, later item.

## Install (Linux)

```
cargo build --release -p idn-obs
sudo cp target/release/libidn_obs.so /usr/lib/x86_64-linux-gnu/obs-plugins/
sudo mkdir -p /usr/share/obs/obs-plugins/libidn_obs
```

(Ubuntu's OBS 32 package loads plugins from the system directory; it did not pick the
plugin up from `~/.config/obs-studio/plugins/…` or `~/.local/share/obs-studio/plugins/…`
in testing.) OBS's log shows `[idn-obs] idn-obs 0.1.0 loaded (libobs API 32.1.0)`.

Then in OBS: *Sources → + → IDN Source* (pick a source found on the network, or type an
address). For the output: *Tools → IDN Output…* opens its settings page in the browser:
turn sending OBS's program on and off, set its name, quality, encoding, video threads
and network options, and see its receivers and frames. It can also send the **preview**
as a second source (default name "OBS Preview"): studio mode's preview scene, or the
current scene outside studio mode, rendered by an OBS view at the output's size and
rate, following each preview change, with OBS's sound; it uses the program's quality,
encoding and network settings. The page is served by the plugin
on this computer only (`http://127.0.0.1:29217/`, another free port if a second OBS has
that one); to keep it inside OBS, add that address under *Docks → Custom Browser Docks*.
The settings are kept in OBS's plugin configuration (`idn-output.json`), and an output
that was on starts with OBS. (OBS gives plugins no dialog for outputs, and a Qt one can't
be cross-built for Windows with this plugin's toolchain, hence a page like the IDN
Node's.) The output can still be made from a script
(`obs.obs_output_create("idn_output", "IDN", settings, nil)` then `obs_output_start`).
OBS's log (Help → Log Files) shows `[idn-obs]` lines and the library's own.

The output's settings (in the `settings` object): `name`, `quality` (50–100),
`encoding` and `video_threads`, plus the network ones (`use_node`, `passphrase`,
`server`, `trust_dir`):

| `encoding` | What IDN receivers get (NDI receivers always get SpeedHQ) |
|---|---|
| `machine` (default) | The machine's setting in the IDN Node (its panel's "Video" section); `auto` without one |
| `auto` | H.264/HEVC from the GPU for pictures larger than 1080p (4K), SpeedHQ otherwise |
| `offer` | SpeedHQ, unless a receiver asks for compressed video |
| `cpu` | SpeedHQ only |
| `force` | H.264/HEVC from the GPU for every receiver that can decode it |

The GPU encoder is NVENC, AMF or Quick Sync on Windows (through OBS's FFmpeg), VA-API on
Linux; without one, everything is SpeedHQ, and the log line at start says which.
`video_threads` (0 = the machine's setting in the IDN Node, else automatic: the physical
cores, at most 6) sets the threads that encode, convert and decode pictures, shared by
every IDN source and output in OBS; the IDN Source has the same setting and "Decode two
pictures at once" (`two_at_once`: `machine`, the default, then on without a machine
setting; `on`; `off`) for 4K at 50/60 fps. Builds before setup2 kept that as a checkbox
(`parallel_decode`), which is no longer read.

## Behaviour notes (from the lab, 2026-10-23)

- The IDN Source's **Timing** (default *Automatic*, which is buffered for every
  sender since 2026-10-28): OBS's buffered timing puts IDN senders' sound within about
  −12 to +5 ms of the picture, against +14 to +45 ms unbuffered. (Automatic used to be
  unbuffered for NDI senders, after an NDI 6.3 sender seemed to lose a third of its
  frames buffered; the lab couldn't reproduce that.) *Unbuffered* shows each picture as
  it arrives, for the lowest delay. The old "Buffer frames for smooth timing" checkbox,
  where it was set, still counts.
- Tally: program = the source is active in the program output; preview = studio mode
  is on and the preview scene holds it. Both come from the frontend's scene events,
  not from "shown" (a source shown in program is also "shown").
- Timestamps: OBS's monotonic clock is translated to wall-clock IDN timestamps by the
  two clocks' offset, read side by side, so a frame carries its render time (pinning
  the first arrival instead put every timestamp ~80 ms ahead and sound 60 ms early).
- Each IDN Source is a receiver of its own, "HOST (OBS <source name>)" by default:
  senders keep records (encryption used before, tally) by receiver name, so sources
  sharing one name would be taken for one receiver — and a source that encrypted with a
  sender once blocks a plain-text session from that name afterwards (the sticky rule,
  spec §E.8.5), which looks like "connected but no picture" at a plain receiver.

## Compressed sources (NDI|HX, codec/1)

The plugin links `idn-ffmpeg`, so an IDN Source decodes H.264/HEVC pictures (NDI|HX
cameras and converters, and IDN senders that offer `codec/1`) through FFmpeg. On Linux
that is the system's FFmpeg; on Windows the FFmpeg that OBS itself ships (in OBS's
`bin/64bit`: FFmpeg 7.1, `avcodec-61.dll` and friends, for OBS 32; FFmpeg 8.1,
`avcodec-62.dll`, for OBS 33), so nothing extra is installed and the plugin's imports
resolve from OBS's own directory. As those names change with OBS's FFmpeg, the plugin is
built once for each, and what OBS loads as `idn_obs.dll` is a small loader
(`media/idn-obs-loader`, no FFmpeg of its own) that loads the build for the FFmpeg it
finds in OBS (see Windows below). An OBS update from 32 to 33 keeps the plugin working
(2026-11-12: the FFmpeg 7 build alone didn't load in OBS 33.0.1, Windows error 126).

An IDN Source takes compressed video from IDN senders when the sender chooses it (by
default for 4K pictures; SpeedHQ is best on a wired LAN otherwise); the "Compressed
video from IDN senders" property can set it to Never or to Prefer (Wi-Fi, remote
rooms). "Decoding of H.264/HEVC" chooses where: the machine's setting in the IDN Node (the
default; automatic without one), automatic (the CPU, moving to the GPU if the CPU falls
behind), the GPU (VA-API on Linux, NVDEC on NVIDIA, D3D11VA on Windows) or the CPU. NDI|HX sources are
compressed regardless. The source logs what it receives: `video H264 1280x720 30/1
via link`.

## SpeedHQ on the GPU

SpeedHQ pictures (IDN senders and NDI full-bandwidth sources, progressive 4:2:2 or 4:2:0,
with or without alpha) can be entropy-decoded on the CPU and reconstructed (inverse DCT)
on the GPU (`idn-shq-gpu`, the switcher's shader), then read back for OBS: a hybrid, since
no SpeedHQ decoder runs wholly on a GPU. In f64 where the GPU has f64 shaders, the same
samples as the CPU decoder; else f32 (at most one step off). The source's setting
"Decoding of SpeedHQ" chooses:

- **Automatic** (default): when it starts, the source tries the GPU (f64, then f32 if
  f64 doesn't keep up) and then the CPU decoder for a moment each, measuring the CPU a
  picture costs (every thread's) and the hold-back, and keeps the GPU if it keeps up and
  saves a tenth of the CPU or more (CPU left for the rest of the machine, a mixing desk
  for one, is worth the GPU's 10–15 ms more hold-back). A trial that falls clearly
  behind ends at once. It chooses again when the picture format changes, when the GPU
  falls behind (30 pictures in a row over 85% of the frame period) and every 10 minutes.
  Not when "Decoding of H.264/HEVC" is CPU.
- **GPU**: the GPU always (f64 or f32 by measurement; f32 if neither keeps up).
- **CPU**: the CPU decoder always.

The log says what it chose and why, for example `SpeedHQ decoded by the GPU (f64) (less
CPU; the GPU (f64): 12.8 ms of CPU a picture, hold-back 15.0 ms; the CPU decoder: 15.1 ms
…)`. Measured on a Radeon 680M at 2160p30 (lab 198): a grainy camera 15.1 → 12.8 ms of CPU
a picture on the GPU; a test card 6.8 on the CPU against 7.9 on the GPU.
`IDN_OBS_SHQ_GPU=0` or `1` overrides the setting (CPU, GPU); `IDN_OBS_IDCT=f32` or `f64`
fixes the GPU's arithmetic. A GPU that fails is left for the CPU decoder.

## IDN Routed Audio

A sound-only source that is a routable receiver on the IDN Node (spec/extensions/
idn-routing.md): it appears under its name in the IDN Site Manager's routing matrix (and
the Node's own panel), and plays whatever is routed to it, from any number of IDN or NDI-Compatible
sources, each with its own buffer and drift correction. Settings: the receiver name, 1–8
channels (OBS's speaker layouts), low latency (a 30 ms buffer instead of 60, on a wired
network; OBS's own audio ticks of about 21 ms make anything lower underrun, and changing
it doesn't restart the source) and lossless float sound. It needs the IDN Node on the machine.

## IDN Node

When the machine runs the IDN Node (spec/extensions/idn-node.md), each IDN Source and the
IDN Output use it by default ("Use the IDN Node"): OBS then encrypts with the machine's
site identity and passphrase, enrolled once for the whole machine, and appears in the
Node's panel. The passphrase, Discovery Server and trust-directory fields apply when the
option is off or no Node runs. The plugin (GPL) and the Node talk only over the Node's
local IPC.

## Windows

Cross-built from Linux with MinGW-w64 (`gcc-mingw-w64-x86-64 mingw-w64-tools`,
`rustup target add x86_64-pc-windows-gnu`): the OBS headers come from OBS Studio's
source at the installed version's tag (`libobs/`, `frontend/api/obs-frontend-api.h`,
plus a small `obsconfig.h`), the OBS DLLs from the official Windows release zip; the
FFmpeg headers from FFmpeg's source at the version OBS bundles (7.1.1 for OBS 32.1,
plus minimal `avconfig.h`/`ffversion.h`), with import libraries made from OBS's FFmpeg
DLLs (`gendef avcodec-61.dll; dlltool -d avcodec-61.def -D avcodec-61.dll -l
libavcodec.dll.a`, and the same for avformat, avutil, swscale, swresample):

```
FFMPEG_DIR=/path/with/{include,lib} IDN_OBS_INCLUDE=/path/with/obs-headers \
  IDN_OBS_LIB=/path/with/obs.dll cargo build --release --target x86_64-pc-windows-gnu -p idn-obs
```

The second build, for OBS 33, is the same with FFmpeg 8.1.2's headers and import
libraries made from OBS 33.0.1's DLLs (`avcodec-62`, `avformat-62`, `avutil-60`,
`swscale-9`, `swresample-6`; the kit's `ffmpeg-obs33`, made by
`scripts/setup-windows-kit.sh`), in its own target folder. `scripts/package.sh` builds
both and the loader, and checks each build's imports against the OBS it is for
(`scripts/dev/obs-imports-check.py`: every libobs and FFmpeg function there).

Installed as (IDN Media's installer does it):

```
obs-studio\obs-plugins\64bit\idn_obs.dll                   the loader (idn_obs_loader.dll)
obs-studio\data\obs-plugins\idn_obs\bin\idn_obs_ffmpeg7.dll  the plugin for OBS 32
obs-studio\data\obs-plugins\idn_obs\bin\idn_obs_ffmpeg8.dll  the plugin for OBS 33
```

(The builds are in the data folder because OBS loads every DLL in `obs-plugins\64bit`
as a module of its own.) The loader looks for `avcodec-62.dll`, then `avcodec-61.dll`,
beside `obs64.exe` (what OBS itself ships decides), then loaded from elsewhere (another
plugin's own FFmpeg), loads that build from its own folder and OBS's (never the working
folder or PATH) and passes OBS's module calls on; a build that doesn't load is logged
and the next tried. When OBS has neither, or no build loads, OBS's log says so and the
plugin stays off. Tried under Wine with OBS 33.0.1's and 32.1.0's own `bin\64bit`: the FFmpeg 8 and
the FFmpeg 7 build respectively, each with that OBS's FFmpeg; with the builds missing,
the log line and `obs_module_load` false. The Windows build of the library takes the default
route's network interface only and has no Ctrl+C goodbye handling yet.

## Shape (as built; checked against the headers)

- `obs_module_load` registers the two types with `obs_register_source` /
  `obs_register_output`; `obs_module_unload` stops everything.
- IDN Source: `create` opens an `Input` on a thread; `OBS_SOURCE_ASYNC_VIDEO |
  OBS_SOURCE_AUDIO`; video via `obs_source_output_video` as I420/I422/I444 (+alpha)
  planes with the announced colour matrix, audio via `obs_source_output_audio` (planar
  float). HDR: a source whose frame metadata says PQ or HLG (`ndi_color_info`, also on
  NDI|HX frames, where it overrides the H.264/HEVC VUI) is by default converted to SDR
  in the plugin with the library's tone map and handed over as sRGB RGBA, white at the
  source's "SDR white" setting (default 80 nits: what a Windows laptop in HDR mode shows
  its own desktop at, its "SDR content brightness"; raise it if an HDR source looks too
  bright). The alternative setting hands OBS the HDR frame (`trc` PQ/HLG, BT.2100), which
  OBS tone-maps with Settings → Advanced → Video → *SDR White Level*; in the lab that left
  an 80 nit white at Y 177 even with the level at 80. The OBS log says what each source
  was handed as. Properties: a list of sources from `browse`, a typed-in address, receiver
  name, buffering, the trust/passphrase settings. Tally from the frontend's events
  (`obs_source_active`, the preview scene's items).
- IDN Output: `obs_output_info` with `raw_video` / `raw_audio` callbacks, asking OBS
  for a fixed format (`obs_output_set_video_conversion`), `start`/`stop` create and
  drop the `Output`.
- Build: `cargo build --release -p idn-obs`; install the `.so` into OBS's plugin
  directory (`~/.config/obs-studio/plugins/idn-obs/bin/64bit/`) with a `data/` folder
  for locale strings.

## Lab

Tested in the lab against the idn tools and NDI 6.3 peers: an NDI 6.3 receiver takes
the IDN Output; the IDN Source shows an NDI 6.3 sender; tally round trips; timecodes
of picture and sound agree at the receiver.

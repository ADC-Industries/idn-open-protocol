# idn-ffmpeg

Hardware video encoders and decoders for the IDN Open Protocol's compressed video
(`codec/1`), through the system's FFmpeg libraries:

- **Encoding (hardware only):** VA-API on AMD and Intel GPUs (Linux), NVENC on
  NVIDIA, AMF on AMD and Quick Sync on Intel (Windows): H.264, HEVC and AV1 as the GPU
  allows. Without one, outputs stay on SpeedHQ.
- **Decoding:** software on every core by default, moving to the GPU (VA-API on AMD
  and Intel under Linux, NVDEC on NVIDIA, D3D11VA on any GPU under Windows) at the
  next keyframe if software falls behind the stream. On an AMD APU software decoding was
  faster (4K H.264 in 4 slices: 184 pictures/s and 5 ms, against 90 and ~40 ms through
  the GPU, whose memory is slow to read back). `Ffmpeg::set_decoder` or
  `IDN_DECODER=auto|gpu|software` chooses, else the machine's setting in the IDN Node
  when each decoder opens; `idn-codec recv --decoder …` likewise. `idn-codec send` without
  `--policy` takes the machine's encoding.

It also has the gateways (R.12): `gateway` streams an output to RTMP(S)
and SRT and records it to MP4 or MKV (one shared encode, reconnects per destination),
and `player` plays a video file as an output (decoded for everyone, H.264/HEVC passed
through to `codec/1` inputs when the file allows). The player decodes automatically
(the GPU first, software if the GPU's download proves slow, remembered per machine),
or as chosen (`PlayerConfig::decoder`, `idn-codec play --decoder gpu|software`): with
the GPU chosen, pictures are downloaded and converted on helper threads, a few at once,
while the next ones decode. `player::ListPlayer` plays a list of videos, sound files and
pictures one after another on one output (a switcher's list input), by hand or in turn,
looping or not.

`media` decodes files in-process for a host's own timeline (a presentation program's
backgrounds and media cues, drawn in its own compositor and mixed in its own mixer), with
no output and no pace of its own:

- `media::VideoReader::open(path, &ffmpeg, VideoReaderConfig { in_point, out_point,
  loops, format, size, ahead, decoder })`: pictures decoded ahead on a thread of its own
  (the GPU where it decodes the codec, software if the GPU refuses the file).
  `frame_at(t)` gives the picture showing at `t` on the caller's clock, never waiting;
  `frame_at_wait(t, timeout)` waits for it (an exact seek's first picture). The timeline
  starts at the in point and runs on across `loops` plays (`None`: forever); past its end,
  the last picture (`ended()`). Each `VideoFrame` has its `time` on that timeline, its
  `file_time` and `play`, its colour (matrix, transfer, primaries, range) and `rotation`.
  `decoding()` says whether it decodes on the GPU (which) or in software, and why (chosen,
  no GPU decoder, a stream the GPU doesn't take such as 4:2:2 H.264 on VA-API, or the GPU's
  decoding failed).
- **Times** are the file's as FFmpeg's own tools count them: from its first timestamp (an
  MP3's encoder delay skipped). A play's default end is the file's duration, the same for
  its picture and its sound, so the two loop together (a 3.0 s video with a 3.5 s
  soundtrack holds its last picture 0.5 s); `probe`'s `VideoInfo::end` and
  `AudioInfo::end` give where each stream itself ends, for an out point of its own.
- **Formats** (`PixelFormat`): `Nv12` or `I420` (8-bit), `P010` (16-bit samples, the
  value in the top bits: DXGI/Vulkan's P010) or `Yuv420p16` (planar 16-bit), `Rgba`. The
  default is NV12 for 8-bit sources and P010 for deeper ones (10-bit HEVC, AV1);
  `bit_depth` says the source's significant bits. Planes are packed rows,
  little-endian for 16-bit samples.
- `media::AudioReader::open(path, AudioReaderConfig { sample_rate, channels, in_point,
  out_point, loops })`: a file's sound (a video's soundtrack, an audio file) as
  interleaved float at that rate and channel count (a mono file copied into every
  channel). `read(pos, &mut buf)` reads from timeline sample `pos` (a seek when it isn't
  where the last read ended), sample-accurate for every codec (WAV, AAC, MP3, …): the
  samples FFmpeg's own decode has there, read straight through or after a seek. Each play
  is exactly `out − in` long (silence where the sound ends before the out point), so play
  `n` starts at `n × (out − in)`; `read` gives every sample up to `length()`, then
  silence.
- `media::probe(path, &ffmpeg)`: the container, duration, the video's codec, size, frame
  rate, pixel format, bit depth, alpha, rotation, transfer and whether this machine's
  GPU decoder takes the codec; the sound's codec, rate and channels.
  `media::thumbnail(path, at, max_width)`: the picture showing at `at` as RGBA, with its
  own time in the file (`time`, `file_time`).
- `media::capabilities(&ffmpeg)`: the GPU decoder this program uses and the codecs
  FFmpeg has a configuration for there (the GPU may still refuse a profile), the codecs
  FFmpeg decodes in software, FFmpeg's version and its libraries'.
- `media::convert(input, output, &ConvertConfig { codec, frame_rate, deinterlace,
  max_size, video_kbps, audio_rate, audio_kbps, gpu, low_priority }, &ffmpeg, progress,
  &cancel)`: a file made into an MP4 every player takes (H.264 or HEVC 8-bit 4:2:0 at a
  constant frame rate, AAC stereo at 48 kHz), deinterlaced and turned upright, encoded on
  the GPU where it can, at a low priority, faster than real time. Progress about four
  times a second; the cancel flag stops it. It writes `OUTPUT.part` and renames it once
  complete, so a cancelled or failed conversion leaves nothing. `ConvertResult` says the
  encoder, size, rate, length, frames repeated, dropped and deinterlaced, and how long it
  took. From the command line: `idn codec convert IN OUT.mp4 [--fps N] [--max WxH]
  [--deinterlace auto|on|off] [--cpu] [--codec h264|hevc] [--kbps K]` (progress on the
  error output each second, the result on standard output; Ctrl+C cancels).

**Features.** `default` is everything. A program that only decodes (the codecs, the
player, `media`, the gateways) builds with `default-features = false`, without these:
`capture` (screens and webcams as sources, remote sessions' keyboard and mouse,
Wayland's screens: idn-input, wayland-client/-protocols, ashpd, tokio), `whep` (browser
viewing and phone listening over WebRTC: str0m), `voice` (IDN Voice feeds: tungstenite).
(`idn-open-protocol` itself still brings tokio and tungstenite: its QUIC transport and
push channel.) `gpu` keeps decoded pictures on an application's GPU device; `sound` adds
a screen's own sound (idn-audio). The `idn-codec` tool needs `capture`, `whep` and
`voice`.

`whep` is browser viewing over WebRTC (`idn view`) and, with `ViewerConfig::listen`,
listening on phones (`idn listen`): named channels, sound only. It serves WHEP and a
state, no page: the watching and listening pages (and the QR code) are the host program's.

The library (`idn-open-protocol`) has no codecs of its own; this crate is the adapter
behind its `codec::EncoderFactory` and `codec::DecoderFactory` traits.

## Linking

FFmpeg is **always the system's, linked dynamically** — this crate never bundles or
statically links it, and there is **no feature flag** that changes that (the policy is
system FFmpeg, dynamic only). The link is through `ffmpeg-sys-next` (avcodec, avformat,
swscale, swresample):

- By default it finds FFmpeg with **pkg-config** — the dev packages below are enough.
- To point at a specific build, set `FFMPEG_DIR` to its prefix (the directory with
  `include/` and `lib/`); the crate then links that one. This is how the Windows build is
  pointed at a pinned LGPL FFmpeg 8, for instance.
- `libavdevice` (screen and webcam capture, R.19) is **not** linked: only three of its
  functions are used, and it isn't present everywhere FFmpeg is, so it is loaded at run
  time with `libloading` when that capture is used.

Build the FFmpeg you distribute **without GPL components**. Decoding (the codecs, the
player, `media`) never uses an encoder, GPL or not. Encoding uses the GPU's encoders; the
one CPU encoder that can be GPL is H.264's last fallback: OpenH264 (BSD) first, and
libx264 only where the machine's own FFmpeg has it and there is neither a GPU H.264
encoder nor OpenH264 (a Linux machine on its system FFmpeg, for site links and browser
viewing: `Ffmpeg::cpu_h264`). libx264 is never shipped with IDN: the FFmpeg the Windows
installer fetches is the LGPL build with OpenH264, and on Linux it is the user's own
library. x265 is never used (HEVC on the CPU is Kvazaar, BSD). Ubuntu's own FFmpeg is
fine to build against and ship as the LGPL libraries.

## Build

```
cargo build --release -p idn-ffmpeg
```

needs FFmpeg's development files and clang for the bindings. On Debian/Ubuntu:

```
sudo apt install libavcodec-dev libavformat-dev libavutil-dev libswscale-dev \
                 libswresample-dev libavdevice-dev clang
```

(`libavdevice-dev` only so the runtime `libavdevice` is present for screen/webcam capture;
it is loaded dynamically, not linked.) The user running it must be in the `render` group
for GPU encode/decode. `idn-codec probe` shows what the machine can do.

## Licensing

This crate is MIT OR Apache-2.0. FFmpeg is LGPL and is linked dynamically, not included
(see **Linking** above). The browser-viewing path (WHEP) uses `str0m` with pure-Rust
crypto — no OpenSSL.

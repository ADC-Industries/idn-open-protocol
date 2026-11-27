# idn-audio

Sound cards for the IDN Open Protocol, as a separate optional crate (like `idn-ffmpeg`):

- **Capture:** a sound card's input becomes a source (an IDN output carrying audio), for
  a mixer feed, a microphone, or Dante through Dante Virtual Soundcard.
- **Playback:** a source's sound plays on a sound card's output, for monitoring, a
  foyer feed, or back into Dante.

WASAPI on Windows, CoreAudio on macOS, ALSA on Linux (`cpal`); JACK with the `jack`
feature; ASIO sound cards on Windows with the `asio` feature, **on by default**
(`IDN_AUDIO_HOST=asio`).

ASIO is IDN's own host (`src/asio_host.rs`, a cpal custom host) on IDN's interface
definitions (`core/idn-asio-abi`), written from the headers of cwASIO (MIT, © 2024 Stefan
Heinzmann; spec/extensions/asio-interface.md): no Steinberg SDK at build or run time, no
`CPAL_ASIO_DIR`, no bindgen. One ASIO driver runs at a time in a program (ASIO's own
rule); an input and an output stream on it share its buffers. Streams are f32; the
driver's 16/24/32-bit, float and 32-bit-container types are converted. On Linux and macOS
the feature compiles nothing. ASIO is a trademark of Steinberg Media Technologies GmbH.
The GPL OBS plugin never links this crate.

## Clock drift

A sound card's clock runs tens of parts per million off the machine's and the
network's. Both directions correct for it with a slow servo on a windowed-sinc
resampler (`rubato`): captured sound keeps its sample count true to the clock, so its
timecodes never creep against the video's; played sound holds its buffer at the chosen
latency instead of starving or overrunning. The correction is a few ppm at a time,
never more than 1000 ppm (under two cents of pitch): no clicks, no audible change. The
stats report the correction in use and, for playback, the buffer fill, underruns and
dropped sound.

## Tool

```
idn-audio devices
idn-audio send --name Mixer [--device "Dante Virtual Soundcard"] [--channels 2] [--rate 48000]
               [--labels "Kick,Snare,Vox"]
idn-audio recv "HOST (Mixer)" [--device "Speakers"] [--latency 60]
idn-audio recv --routable "Stage monitors" [--device "Speakers"] [--channels 8]
```

Audio routing (spec/extensions/idn-routing.md): `recv --routable NAME` offers the
device's outputs (or the first `--channels`) as receiver NAME on the machine's IDN Node and
plays whatever is routed to it. Routes are made in the IDN Site Manager's routing matrix
(or `idn-node route` on the machine) and kept by the Node, so they come back after a
restart. Each source plays through its own buffer and drift correction, summed on the
device; only the channels routed travel on the network (`channels/1`). `send --labels`
names a source's channels in the matrix.

Low latency: `send --low-latency` sends 2 ms blocks (default 20) and `recv
--low-latency` holds a 10 ms buffer (default 60), for monitoring and live sound on a
wired network; the network itself adds well under a millisecond (`cargo run --release
--example audio_latency` in the main crate measures it). A path with more jitter than 10
ms covers settles where it needs to: each underrun while playing steadily adds 2 ms to
the buffer (up to 20 ms more), and ten clean minutes give 1 ms back (never down to where
an underrun happened); the recv line shows what was added ("N after underruns"). On the
capture side, a jump of over 30 ms in the sender's timing (lost or held-back input, not
clock drift) restarts its drift correction from there rather than steering at its limit;
the send line counts these as `resyncs`. The buffering, drift correction and mixing are
the library's (`idn_open_protocol::audio`); this crate adds the sound cards.

`--device` takes a device's name or the audio system's identifier shown in brackets
(ALSA lists many entries under one name); an exact match wins, else the first entry
containing the text. The playback device is asked for a buffer of about a quarter of
`--latency`; where it insists on a larger one (some ALSA devices open with hundreds of
milliseconds), the buffer target grows by that block so nothing is lost.

Encryption and site trust flags are the same as the other tools (`--passphrase`,
`--trust-dir`, `--server`).

## Library

`Capture::start(&CaptureConfig, sink)` calls `sink(AudioBlock)` for each block (on the
audio thread; `Output::send_audio` is safe there). `Playback::start(&PlaybackConfig)`
then `push(sample_rate, &channels)` for each `AudioIn`; `Playback::lane()` (or
`Playback::lanes()` from another thread) adds a source with its own buffer and servo,
pushed with a channel map. `devices()` lists the sound cards. See the crate docs.

When a pushed sample is heard (lip-sync): `Playback::output_delay()` is the device's
buffer and output latency, from its callback timestamps (zero until the first callback);
a lane's `latency()` is that, the time until the device takes its next block, and what the
lane still holds (`queued()`): a sample pushed now is heard after it, at `heard_at()`. A
host showing a picture with its sound shows it then. A source pushing in real time starts
at once at the level its lane holds (silence put before its first block makes up what it
lacks), so `heard_at()` is the same from its first block on, and nothing it pushes is cut;
before it plays, `latency()` is already the level it will start at.

A device's health: `Playback::status()` / `Capture::status()` give a `DeviceStatus`
(`Running`; `Failing`: errors in the last 2 s; `Lost`: no sound taken or given for a
second, unplugged or taken by another program, or a capture ended), with the last
error's words; `on_status(f)` calls `f` at once and at each change of state until the
stream is dropped (reopen a lost one, show why). `probe_device(name, Direction::Output |
Input)` opens the device in its default format as playback or capture would and closes it
again without starting it (tens of milliseconds; probes run one at a time, safe to call
from several threads): whether it opens now, why not (`DeviceProblem::Missing`, `Busy`,
`NoPermission`, `Other`, with the audio system's words), and the most channels it
offers. A device another program holds is found and reported `Busy`, by the probe and by
`Playback::start` / `Capture::start` (an `io::Error` of kind `ResourceBusy`;
`PermissionDenied` and `NotFound` likewise); `devices()` still lists it, with `busy` set
and its formats unknown.

Features: `asio` (Windows, on by default) adds ASIO sound cards (IDN's own host, no
SDK); `default-features = false` leaves it out and keeps
everything else (WASAPI, ALSA, CoreAudio, PipeWire through ALSA); `jack` adds JACK on
Linux.

Build: `cargo build --release -p idn-audio` (Linux needs ALSA's development files,
`libasound2-dev`). Cross-building for Windows: `cargo build --release --target
x86_64-pc-windows-gnu -p idn-audio` with `gcc-mingw-w64-x86-64`, `g++-mingw-w64-x86-64`
installed (the compilers are set in `.cargo/config.toml`); ASIO needs nothing more. On Windows
the default host is WASAPI (every device, Dante Virtual Soundcard included);
`IDN_AUDIO_HOST=asio` opens the interface's ASIO driver instead, for its lower latency.

Licensing: MIT OR Apache-2.0; `cpal` is Apache-2.0, `rubato` MIT.

## This computer's sound, and IDN as a microphone (Windows)

- `idn audio send --name Desktop --loopback`: what a playback device plays (default the
  system's), as an IDN source.
- `idn audio send --app spotify`: one program's sound only, with its child processes
  (Windows 10 2004 and later; a browser plays from a child). Named after the program
  unless `--name`. When the program exits the command ends with status 3, so a supervisor
  (the session agent, `idn-session`) starts it again for the program's next run.
- `idn audio mic`: the routable receiver "IDN Microphone" (`--name`), whose sound plays
  into a virtual cable (IDN's virtual sound card on Windows 11, the bundled Virtual Audio
  Driver; VB-Audio's free VB-Cable, VoiceMeeter, Virtual Audio Cable; the first found, or
  `--device`); programs record it from the cable's other side ("Virtual Mic Driver",
  "CABLE Output").
- In the IDN Node's panel, "Sound on this computer" sets these to run whenever you are
  logged in: the session agent (`idn-session`, started at every log-in) keeps them
  running.


## AES67

AES67 equipment (stage boxes, consoles, Dante devices in their AES67 mode) directly on the
network, without a sound card (spec/extensions/idn-aes67.md):

```
idn audio aes67 list                                  # streams announced on the network (SAP)
idn audio aes67 recv "Console Out 1-2" [--name FOH]   # a stream (by name, or an SDP file) as an IDN source
idn audio aes67 send "HOST (Mixer)" --name "IDN Mix" [--channels 8] [--address 239.69.1.10]
```

Streams sent are L24 at the machine's sample rate (48 kHz unless the site or `--rate` says 44.1 or 96; §N.40) in 48-frame packets (1 ms at 48 kHz, 1.088 ms at 44.1, 0.5 ms at 96), announced with SAP every 30 s and
withdrawn when they stop; `--sdp-file` writes the SDP for receivers that don't read SAP.
With the IDN Node's site clock (PTP, the service on the standard ports) the packets are
timed by it and the SDP names its grandmaster, and a received stream on the same
grandmaster gets timestamps from its media clock, so it lines up with every other IDN
source. Received streams may be L24 or L16, any packet time; `--jitter-ms` (4) is how
long a missing packet is waited for. `--ttl 0` keeps a stream on this machine (tests).

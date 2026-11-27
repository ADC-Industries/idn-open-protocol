# idn-mixer

A mixing desk on the IDN routing matrix (roadmap R.16):
input channels with trim, polarity, input delay, high- and low-pass, gate, eight-band parametric EQ,
compressor, de-esser, limiter, noise suppression, mute, fader, pan and sends; a main bus, 16 aux buses
(mono or stereo, pre- or post-fader), eight group buses, eight matrix mixes (mixes of
buses, for lobby/overflow/stream feeds), eight FX buses and a monitor pair with solo (PFL);
eight DCA groups, eight mute groups and talkback; meters with a spectrum analyser (RTA) and
EBU-R128 loudness (LUFS); channel copy/paste and presets; and snapshots (saved desk states).

It is an engine first: the `idn_mixer` library (`Console::start`, merge-patch commands,
a state with change events, meters; `Desk` for the processing alone) is what other
programs embed. The `idn-mixer` program runs it as a service with an HTTP/JSON API and a
push channel (a WebSocket); it has no UI of its own: the client draws the desk from the
state (`strips`, `desk_sizes`, `GET /api/v1/controls`).

## Embedding (the library)

`Console::start(ConsoleConfig)` returns a running desk. It owns its clock thread (and the
plugin-host and card threads when used) and stops when the `Console` is dropped,
withdrawing the output. A runnable example is
[`examples/desk_integration.rs`](examples/desk_integration.rs) (`cargo run -p idn-mixer --example
desk_integration`).

**Commands** are JSON merge patches — the same shapes as the program's API — as methods:
`channel(n, &patch)`, `bus(which, &patch)`, `bus_strip`, `dca`, `mute_group`, `talkback`,
`insert`, `preset_save` / `preset_load`, `snapshot_save` / `snapshot_recall` / `snapshot_delete`,
`patch_output`, and more. Each patch merges into the addressed desk type (`desk::Channel`,
`desk::Bus`, …) and bumps the version; channels and buses are one-based. All return
`Result<(), String>`.

**State, meters and change events.** `version()` + `wait_change(after, timeout)` as the
switcher's; `state()` and `meters()` are `serde_json::Value` snapshots (the shapes
`GET /api/v1/state` and `/meters` return). `spectrum(n)` is a channel's analyser.

**Persistence.** `ConsoleConfig::state_dir` is where snapshots are kept
(`snapshots.json`); `None` keeps them in memory for the life of the desk. Snapshots and channel
presets are saved and recalled by name.

**In and out.** Inputs come from the IDN routing matrix through a `Node`
(`ConsoleConfig::node`) — route transmitters to the desk's receiver — or straight from a
sound card (`card_in`). The buses go back into the matrix and out as the IDN source, and
`output()` is the main mix as an `idn_open_protocol::api::Output`: tap it
(`output().tap(|f| …)`) for the mixed audio in your process, no network hop. To push audio
your app holds *into* the desk in-process (program sound as a channel, say), call
`feed(name, first, sample_rate, &channels)`: the samples land on desk inputs from `first`
(1-based), and a channel whose `source` points there plays them. `feed_stop(name)` frees
it. (You can still bring audio in by publishing an IDN output and routing it, or a sound
card, instead.)

**Plugins** run in the `idn-plugin-host` helper (`ConsoleConfig::plugin_host`, else the one
beside your program, or `IDN_PLUGIN_HOST`); `plugin_list`, `insert`, `plugin_params`,
`plugin_set` and `plugin_editor` drive them.

## In and out

The desk offers itself on the machine's IDN Node as a routable receiver (default
"Mixer", 64 inputs): route the stage box's, the presentation server's and the
microphones' channels to it in the IDN Site Manager's routing matrix. Its buses go out as
the IDN source "Mixer" with 64 channels (`--outputs`), filled by the output patch; a new
desk's carries Main L/R, Aux 1–8 L/R (a mono aux on both of its channels), Monitor L/R,
Groups 1–8, Matrix 1–4 and FX 1–4 returns (52 channels; aux 9–16, matrix 5–8 and FX 5–8
are patched by choice); route those onward the same way (stage monitors, the livestream
switcher's "routed" sound, OBS). Inputs play by site time where the site clock runs, so
channels from different machines line up to the sample.

## Program

```
idn-mixer [--name NAME] [--channels N] [--inputs N] [--outputs N] [--port N] [--latency MS]
          [--block 32|64|128|256] [--rate 44100|48000|96000] [--no-node]
          [--lan-password - | --lan-password-env VAR] [--lan-port N] [--no-lan]
          [--card-in DEVICE [--card-in-channels N] [--card-in-first CH]]
          [--card-out DEVICE] [--list-cards] [--surface MIDI-PORT] [--list-midi]
          [--x32 ADDRESS|auto]
          [--network-audio DEVICE [--network-audio-first CH] [--network-audio-outputs N]]
          [--osc PORT] [--osc-allow IP,IP…] [--recordings DIR]
```

The API is on `http://127.0.0.1:29214/api/v1/` (the push channel `GET /api/v1/events`);
with a password (`echo … | idn-mixer --lan-password -`) also at
`https://THIS-MACHINE:29215/` behind a login; on a machine in a site that port also takes
the Site Manager's tokens without a password (spec idn-node.md §N.31.5; `--no-lan` keeps
it closed). A `read` token reads; a `control` token does everything; a `bus:` token
(`bus:aux:5`, `bus:matrix:2`, `bus:main`, or by name, `bus:name:stream`) changes only the
channels' sends into that bus and the bus's own fader, mute, EQ and dynamics, and sees
that bus alone in the state, meters, controls and push channel (spec §M.25; another
program, such as a video switcher's UI, running the stream's mix). OSC and the Mackie surface stay
full control on the LAN. Every change (API, OSC, surface, cues) is written in
`audit.jsonl` in the state directory (`GET /api/v1/audit`). Snapshots are kept in the state directory
(`snapshots.json`). Each input's buffer is 10 ms by default (the low-latency mode); the desk
works in blocks of 128 samples by default (2.67 ms at 48 kHz; `--block`).

Each channel has input and output meters, gain reduction, fader, pan, mute and solo;
trim, input delay, high- and low-pass, noise suppression (NR), gate, EQ, compressor,
de-esser and limiter; its settings can be copied to another channel or saved as a
preset. NR (an RNNoise-class network, built in — no plugin) takes steady noise such as
fans, hum and air conditioning out of speech; it adds 20 ms, and while any channel has it
on the others are delayed alike, so everything stays in line (the state's `latency_ms`
counts it). A spectrum analyser (RTA) is available on a channel, and the main mix has its
loudness (LUFS). Two channels (an odd-numbered one and the next) link into a stereo pair
(`"link": true` on the first): the pair plays with the first's settings, the second
keeping its own name and inputs and given the rest.

## A Behringer X32's preamps

With an X32's X-USB card as the desk's card (`--card-in x32 --card-out x32`), the desk
also controls the X32's preamps over Ethernet (spec §M.23.1). It uses the X32's OSC remote
protocol on UDP 10023, from public documentation only.

- **Setting**: `--x32 ADDRESS|auto`, or `desk {"x32": {"address": …}}`. It is kept with
  the desk, not in snapshots.
- **State**: each channel's `preamp` is `{usb, input, headamp, gain_db, phantom, invert,
  x32_channel}`, or null where its input carries none. The X32 itself is `x32`
  (connected, model, firmware, the 32 USB channels' routing).
- **Command**: `preamp {n, gain_db?, phantom?, invert?}`. It is not a channel setting, so
  it is never saved or snapshotted. A locked desk refuses it.
- **OSC**: `/ch/N/preamp/gain|phantom|invert`.
- **Live**: changes made on the X32 show live.
- **Polarity**: `invert` is the **X32 polarity**. It flips the X32's own channel strip,
  not what the desk receives over USB; the channel's own `polarity` changes the desk's
  sound.
- **Library**: `idn_mixer::x32` is the client on its own: finding the X32, the routing,
  and preamp reads and sets.

## In-ear monitors for phones (§M.26)

- A channel's `iem: true` puts it in the IEM flow, after its processing and before its
  fader and mute (a linked pair as one). The desk mixes nothing for the musicians.
- The flow: an IDN audio output `<name> IEM` for wired listeners, and an Opus copy for
  phones (crate `idn-iem`: the wire format and coding, spec idn-iem.md) as UDP multicast
  (29253) and unicast (29224; `ConsoleConfig::iem`, `--iem-port`, `--no-iem-net`), coded
  only while a phone listens. `GET /api/v1/iem` / `Console::iem_description()` describes
  it.
- Mixes kept with the desk (`settings.iem_mixes`): `iem_mix_add` (answers the id),
  `iem_mix` (partial), `iem_mix_delete`; an `iem:<mix>` token changes that mix alone and
  sees the IEM channels and its mix. Snapshots hold them with `include_iem`.
- Talkback: 16 desk inputs after the routed ones, one per mix; a phone's push-to-talk
  plays there.
- `send {n, bus, level?, on?, pan?, pre?}`: one channel's send into one aux bus.
- Code: `iem.rs` (mixes, the channel list), `engine/iem_link.rs` (the flow, sessions,
  talkback), `engine/iem_api.rs` (commands, description, state).

## Plugins

CLAP and VST3 plugins go on channels and buses as inserts, after the compressor and
before the fader (`insert`): add from the list of plugins on
this machine (each format's standard folders, and `CLAP_PATH` / `VST3_PATH`), bypass,
reorder, remove, and set their parameters as plain controls. There is no limit to how many:
what limits a chain is time, all plugins finishing within one block (128 samples by default, 2.67 ms at 48 kHz). A block that
doesn't finish is passed on dry and counted on the strip (the meters' `late`).

Plugins run in `idn-plugin-host`, a helper program next to `idn-mixer`, so one that
crashes takes only itself down: the strips pass on dry, and the host starts again with
the plugins as they were (states are kept about once a second, and in snapshots). Plugins
add one block (2.67 ms by default) on channels and another on buses, only while there are any, plus the
longest chain's own latency; every strip is delayed alike, so everything stays in line.

## Groups, matrix mixes, talkback, sound cards, control surfaces

- **DCA groups** (8): a fader and mute over their member channels (a channel's `dca`),
  carrying no sound of their own.
- **Mute groups** (8): one mute over their member channels (e.g. "all band", "all
  vocals"), the membership a channel's `mute_groups`.
- **Matrix mixes** (8): mixes of the other buses (main, auxes, groups, FX) for extra
  outputs — a lobby or cry-room feed, the stream mix — each with its own processing.
- **Talkback**: an engineer's mic (a chosen channel) to the main and chosen aux buses,
  push-to-talk or latched.
- **A sound card directly**, without the routing matrix: `--card-in DEVICE` puts its
  inputs on the desk from channel `--card-in-first` (default 1); `--card-out DEVICE`
  plays the desk's outputs on a card in order (a stereo card: the main mix).
  `--list-cards` names the devices; `default` is the system's.
- **Mackie Control surfaces** (`--surface NAME`, `--list-midi` for the ports): eight
  channel faders, the master fader, mute, solo, select, V-pots for pan, bank and channel
  buttons, the scribble strips (names and levels) and meters. Motor faders follow the
  desk, except one being touched.
- **OSC** (`--osc PORT`, UDP; `--osc-allow IP,IP…` to listen to those senders only —
  OSC has no password): tablet apps such as TouchOSC or Open Stage Control, show
  controllers and scripts. The addresses follow the desk's settings, numbered from 1:
  `/ch/3/fader -6.0` (dB), `/ch/3/fader/travel 0.75` (0…1, the desk's fader law),
  `/ch/3/mute 1`, `/ch/5/eq/2/gain 3.0`, `/ch/5/sends/2/level -10`, `/aux/1/mute`,
  `/main/fader`, `/group/N`, `/matrix/N`, `/fx/N`, `/dca/N`, `/mutegroup/N/on`,
  `/headphones`, `/talkback`, `/desk/automix`; `/snapshot/recall "Sunday"`, `/snapshot/save "Sunday"`,
  `/button/N/press`, `/clear_solo`, `/record/start`, `/record/stop`, and
  `/cmd "command" "{json}"` for any API command. An address with no arguments is a
  query (the reply holds every value under it); `/subscribe` (renewed within 10 s)
  sends back every change made anywhere, `/subscribe "meters"` the meters too. A locked
  desk refuses changes and sends the control back where the desk is.

## Automix, ducking, audio follows video

- **Automix** (gain sharing): the channels set to it share their level by how loud each
  is — one talking at full level, quiet ones down, two alike −3 dB each, a weight per
  channel for who wins. On with `desk {"automix": true}`; a channel joins with its
  `automix`.
- **Ducking** (4 duckers): while speech on the key channels is over a threshold, the
  target channels (music, a video's sound) go down by a depth, with attack, hold and
  release.
- **Audio follows video**: a channel follows a switcher input and comes up while it is on
  air (down or off when not, with a fade). The switcher is the desk's `afv.switcher`;
  the desk reads its tally messages over a metadata-only connection.

All three act after the fader, so monitor (pre-fader) sends are untouched.

## Built-in effects and graphic EQ

The FX buses have built-in effects — a reverb, a stereo delay (ping-pong too) and a chorus —
so a desk needs no plugins for them; a new desk's FX 1–3 come set to them. The main, aux,
matrix and headphone buses have a 31-band graphic EQ (third-octave, ±15 dB) for tuning a
room or the stage monitors.

## Switchers' outputs as inputs

With the machine's IDN Node, every switcher output with sound on the network (the
program, its mixes, Preview, and the switcher's monitor) comes onto two free desk inputs
by itself, named after it (`Switcher (Monitor) L`/`R`), on no channel: patch it where you
want it (the monitor to the headphones, say). The state's `switcher_inputs` lists them;
`switcher_input` brings one in or takes it off, and one taken off stays off. A channel you patch an input to stays
on the desk until you park it, even while that input is silent or unrouted.

Each switcher output that takes sound (the program, each mix) can take its sound from
the desk (`switcher_feed`): choose a desk bus (Main, a matrix, a group, an aux) and the switcher takes that
mix for it — the stream's mix for the program, another for the lobby.

## Output delays, loudness, recall safes

Each output channel can be delayed up to 500 ms (delay speakers, sound for a late
picture). The main mix's loudness is momentary, short-term and integrated LUFS
(`loudness_reset` starts the integrated reading again). A channel or bus marked `safe`
keeps its settings through a snapshot recall (a stereo pair as one), and a recall can be
limited to the channels, the buses or the output patch (its `scope`).

## Multitrack recording and virtual soundcheck

`record_start` / `record_stop` record each chosen channel's input as
it comes — before any processing — to its own 32-bit float WAV, one folder a recording
(`--recordings DIR`, default `recordings` in the state directory; embedded without one, in the desk program's folder), crash-safe to the
last two seconds. Soundcheck plays a recording (or a console's own folder of WAVs) in
place of the live inputs, with play, pause, locate and loop, so a band's mix can be made
from Sunday's recording on Tuesday; any channel can stay live.

## Starters

`starter` sets the desk up for a Spoken service or a Worship band: channel names and
colours, stereo pairs, high-pass filters, compressors, DCAs and mute groups (your patch
and levels stay). Save it as a snapshot once it sounds right.

## API

`GET /api/v1/state[?after=VERSION]` (long poll), `GET /api/v1/meters` (per channel
`[in peak, in RMS, out peak, out RMS, gate GR, comp GR]`, per output `[peak, RMS]`, dB, and
the main mix's loudness in LUFS, EBU R128), and `GET /api/v1/spectrum?channel=N` (a
channel's spectrum analyser, RTA). Commands are `POST /api/v1/<command>` with
`X-IDN-Mixer: 1`; settings change by merge patch (arrays element by element, `null` keeps
one):

| Command | Body |
| --- | --- |
| `channel` | `{"n": 3, "fader": -4.5, "pan": -0.3, "delay": 8, "denoise": true, "mute": false, "eq": [null, {"gain": 3}], "comp": {"on": true}, "sends": [{"on": true, "level": -6}], "mute_groups": [true, null, …]}` |
| `bus` | `{"bus": "main" \| 1–16, "name": "Drummer", "fader": 0, "pre": true, "stereo": false, "mute": false}` |
| `group`, `matrix`, `fx` | the bus fields, addressed by family: `{"group" \| "matrix" \| "fx": N, "name": …, "fader": 0, "mute": false}` (groups 1–8, matrix 1–8, FX 1–8) |
| `mute_group` | `{"group": 1–8, "name": "All band", "on": true}`; membership: `channel` with `"mute_groups": [true, null, …]` |
| `talkback` | `{"channel": 2, "on": true, "level": -10, "main": true, "aux": [true, null, …]}` (push-to-talk via repeated `on`) |
| `dca` | `{"dca": 1–8, "name": "Band", "fader": -6, "mute": false}`; membership: `channel` with `"dca": [true, null, …]` |
| `desk` | `{"automix": true, "duckers": [{"on": true, "keys": [true, null, …], "targets": [null, null, true], "threshold": -40, "depth": -15, "attack": 20, "hold": 300, "release": 800}], "afv": {"on": true, "switcher": "HOST (Switcher)", "off": -144, "fade": 500}}` (§M.12); a channel joins with `channel {"automix": true, "automix_weight": 3, "follows": "Cam 1"}` |
| `fx`, `bus`, `matrix` (effects, GEQ) | `{"fx": 1, "effect": {"kind": "reverb", "size": 0.8, "predelay": 30}}`; `{"bus": "main", "geq": {"on": true, "gains": [0, …, 3, …]}}` (§M.14) |
| `record_start`, `record_stop` | `{"channels": [1, 2, 5] \| null, "name": "Sunday"}` (null: the channels on the desk); `GET /api/v1/recordings` lists them (§M.13) |
| `soundcheck` | `{"name": "Sunday" \| null}`; `soundcheck_transport {"action": "play" \| "pause" \| "locate" \| "loop", "seconds": 90, "on": true}`; `soundcheck_live {"n": 3, "live": true}` |
| `copy_channel` | `{"from": 3, "n": 5}` (copies channel 3's processing to 5) |
| `preset_save`, `preset_load` | `{"name": "Lead vocal", "n": 3}`; `preset_delete`: `{"name": …}` |
| `insert` | `{"channel": 3 \| "bus": "main" \| 1–16, "op": "add", "key": "…"}`; `"op": "remove" \| "move" \| "bypass"` with `"index"` (and `"to"`, `"on"`) |
| `plugin_set` | `{"channel": 3, "index": 0, "param": 7, "value": 0.5}` |
| `snapshot_save`, `snapshot_recall`, `snapshot_delete` | `{"name": "Sunday"}`; a recall can take `"scope": {"channels": [1, 2], "buses": false, "outputs": false}` (§M.15) |
| `loudness_reset` | `{}` (the integrated loudness starts again) |
| `switcher_feed` | `{"name": "HOST (Switcher)", "bus": "matrix:1"}` (`main`, `matrix:N`, `group:N`, `aux:N`; `null`: the switcher's own sound) (§M.18) |
| `switcher_inputs`, `switcher_input` | `{"on": true}`; `{"name": "HOST (Switcher (Monitor))", "on": false}` (§M.17) |

`GET /api/v1/plugins[?rescan=1]` lists the plugins on this machine (`key`, `name`,
`vendor`, `format`); `GET /api/v1/plugin_params?channel=N&index=I` (or `bus=B`) a
plugin's parameters (`id`, `name`, `min`, `max`, `default`, `value`, `text`).

## Performance

32 channels with everything on (high-pass, gate, EQ, compressor, four sends, bus
compressors): 0.3–0.4 ms per 2 ms block in a release build on the lab host, about 17% of
one core.

## Next

A plugin's own window where the mixer runs in a desktop session (inside the embedding application).
Up to 64 input channels already (`--channels 64`).

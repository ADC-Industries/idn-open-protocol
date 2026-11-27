# The mixer

The IDN Mixer is a digital sound desk's engine. Microphones, the band and the
presentation's sound come in over the network (or a sound card). The mixes go out to the
speakers, the stage monitors, the livestream and phones in the room. **Its screen is
the control application's**: IDN Media's desk has no window or page of its own. Tick **Mixer** in the
computer's IDN Node panel (*Services*) and the Node runs it.

This chapter covers getting sound in and out of it, which is set up on the computers and
the network; the desk's own controls are in the control application.

## How sound gets in and out

The desk is one **receiver** on the network called `HOST (Mixer)`, with 64 inputs. On
the site's audio routing (the control application) you route channels to it: the stage box's,
microphones', the presentation computer's.

Its mixes go out as one **source**, also `HOST (Mixer)`, whose channels are:

| Channels | Carry |
| --- | --- |
| 1–2 | Main mix (left, right) |
| 3–18 | Aux 1 to 8, two channels each (aux 1 is 3–4, aux 2 is 5–6, …) |
| 19–20 | Headphones (the monitor pair) |
| 21–36 | Groups 1 to 8 |
| 37–44 | Matrix 1 to 4 (matrix 1 is 37–38, matrix 2 is 39–40, …) |
| 45–52 | FX 1 to 4 returns |

The desk has 16 aux buses, 8 groups, 8 matrix mixes and 8 FX buses. Aux 9 to 16, matrix
5 to 8 and FX 5 to 8 are on the source only once the output patch puts them there.

Route those onward the same way: to the amplifiers' receiver, the stage monitors, the
overflow room. Phones and captions take a pair by its first channel ([Listening on
phones](02-quick-starts.md#listening-on-phones)). *Routing* → *Output patch* changes what
each output carries.

The switcher takes its sound from a desk bus without any routing (the desk's
*switchers' outputs*, set in the control application).

## A Behringer X32 over USB

An X32 with its stock **X-USB** card brings all 32 of its channels onto IDN, and takes 32
back, over one USB cable. No Dante card is needed.

1. **On the X32**: set its **sample rate to 48 kHz** (in its setup). In its routing, send
   the 32 channels you want to the card's outputs (usually the local inputs 1–32), and
   choose the card as the source of the channels that should come back from IDN. The
   X32's own manual shows the screens.
2. **The cable**: a USB 2.0 cable of up to 5 m from the X32's card to the computer. Use
   an active extension for longer runs.
3. **The computer**: on Windows, install Behringer's X-USB driver first. On Linux and
   macOS nothing is needed.
4. **Without the desk**, as 32 channels on the routing matrix: in the computer's IDN
   Node panel, *Sound on this computer* → *Change*, tick **X32 / X-USB (32 in, 32 out)**.
   - The X32's 32 channels appear as the source `HOST (X32)`, named *Ch 1* to *Ch 32*.
   - The receiver `HOST (X32 return)` plays 32 channels back into it.
   - Route both on the site's audio routing (the control application).
5. **With the desk on that computer**, the X32 is the desk's sound card. Start the mixer
   with `--card-in x32 --card-out x32`: the X32's 32 channels arrive on desk channels
   1–32, and the desk's outputs 1–32 go back to it, in order (*Routing* → *Output
   patch* chooses what each carries). `idn-mixer --list-cards` marks the X32 with these
   flags.

The X32's clock then runs everything on that computer: the desk follows it, and so does
the IDN ASIO driver for a DAW on the same computer. Other machines on the site play by
the site clock as usual. Expect about 2–4 ms each way at small buffers; an X32 → desk →
X32 round trip of about 4–10 ms.

### The X32's preamps from the desk

The desk can also set the X32's preamps over the network, so gain and 48 V are at hand
from the control application. The audio stays on the USB cable.

1. **Connect** the X32's network port to the same network as the desk's computer. Note
   the X32's IP address from its setup screen.
2. **Point the desk at it**: set the desk's X32 address (in the control application's desk setup).
   Through the API it is `desk {"x32": {"address": "192.168.1.62"}}`, or
   `--x32 192.168.1.62` at the start. Use `auto` to take the first X32 found on the network. The setting stays with
   the desk; snapshots don't change it.
3. **Use it**: every desk channel whose input is one of the X32's USB channels shows the
   preamp behind it:
   - the X32 input, read from the X32's card routing;
   - gain, −12 to +60 dB;
   - 48 V;
   - the X32 polarity.

   A gain turned on the X32 itself shows on the desk within a second. A channel fed by a
   bus or by the card's loop back has no preamp.
4. **Mind the two polarities.** The X32 polarity flips the X32's own channel strip,
   which is not what reaches the desk over USB. To flip what the desk hears, use the
   desk channel's own polarity.

48 V only changes when someone turns it on or off. A snapshot recall never touches the
X32. The X32 serves at most four remote programs at a time (X32-Edit and phone apps
count), so close one if the desk doesn't connect.

## On a network of Dante-compatible equipment

Many churches already have stage boxes, a desk card or powered speakers on a network of
Dante-compatible equipment. One computer (Linux or Windows) on that network becomes one
more device on it: the network's channels come into IDN, and IDN's sound goes out to it. You keep setting
the subscriptions in the controller the building already uses (Dante Controller, for
instance), and IDN's devices show there like any other.

This is unofficial: it is not made, authorised or supported by Audinate. Dante is
Audinate's trademark.

**What you need**

- Nothing more to install: IDN Media includes it (Linux and Windows). The computer
  doesn't need a screen or anyone logged on.
- Not a computer that runs **Dante Virtual Soundcard** or **Dante Via**: they are devices
  too and use the same ports. One device program per computer.
- A network card on the audio network. If the computer has more than one card, note the
  address of the one on the audio network.
- The computer's **site clock on that network's time**. Most equipment without AES67 turned
  on uses PTP version 1: in the computer's IDN Node panel, *Site* → *Site clock* →
  *Settings* (switch on *Advanced* to see it) → **Protocol**: *v1* (or *both*, so the
  site's other IDN machines follow that time too).

**Turning it on**

1. In that computer's IDN Node panel, open *Sound* → **Network audio**.
2. Switch on **Be a device on that network**. Set **Channels each way** (up to 128; 64 if
   left empty). Under *Advanced*, set **Network card** to the audio network's card
   address if there is more than one.
3. After a few seconds the device appears in your controller as `COMPUTER-IDN`. Subscribe
   its receive channels to what IDN should hear, and other devices to its transmit
   channels.
4. In IDN, the network's channels are the source `COMPUTER (AoIP)`, and the receiver
   `AoIP` plays into the device's transmit channels. Route both in the site's audio
   routing (the control application) like any other.

The same settings are in the Site Manager's machine settings (the control application), and on a
computer without a screen: `idn-node set netaudio-on=true netaudio-channels=16` (see
[Reference](08-reference.md)).

**Mixing live on the same computer**

Routed through IDN, sound from the network through the desk and back takes about 20 ms,
because each program waits a little for its sound to arrive. For front of house and stage
monitors, let the desk work on the device's channels directly:

1. On the same page, switch on **Run the sound desk on the device** (under *Mixing live
   on this computer*). The computer starts the desk itself; nobody needs to log on.
2. The network's channels arrive on the desk's channels 1, 2, 3 … (named *Net 1*,
   *Net 2*, …). The desk's outputs go out on the device's transmit channels in order:
   outputs 1 and 2 (the main mix) on channels 1 and 2, and so on (*Routing* → *Output
   patch* chooses what each output carries).
3. The control application works with this desk as with any other.

From the network, through the desk and back to the network takes about **9 ms** this way
(64-sample blocks, both devices at 2 ms latency; your network adds a little). While this
desk runs, IDN's routes into the `AoIP` receiver wait, and play again when the desk stops.

A few things to know:

- **Not with a sound card on the same desk**: the desk follows one clock, the network's.
- **Keep that computer for sound.** If it does a lot else, a block can arrive late. The
  desk counts it and then works a millisecond further ahead (up to 8 ms more), so it
  settles on what the computer can keep up with. *Troubleshooting* says how to see this.
- **The desk's block** (under *Advanced*): 64 samples is the default. 32 is quicker and
  harder on the computer; 128 or 256 is gentler.

**Upgrading from a release with the older network audio device**

The first time the new network audio device starts, it takes over what the old one kept:
its device ID (so Dante Controller and other controllers see the same device, not a new
one), its channel names and its inputs' subscriptions. The old files are left in place.

**Seeing the network's devices**

The Site Manager (in the control application: its network devices) lists the devices on that network as
the computer sees them: each one's name, maker, channels, and what each of its inputs is
subscribed to, and whether that subscription works (a red dot with *sample rates don't
match* is equipment at another rate), read again every 30 seconds (and at once when a
device says it changed). It is empty until a machine has the network audio device on.

Operators can change the devices from there, as their own controller would: rename a
device or one of its channels, and clear an input's subscription. In the audio routing
the devices are rows and columns of the matrix too: a click between one device's output
and another's input subscribes that input (a click on a subscribed one clears it). The
device keeps the change and tells the network, so Dante Controller shows it too. IDN's
own network audio device is set in its computer's Node settings instead. A site that
wants the control application to show the devices and not touch them sets **Read only** in the network audio settings of
the computer running the device (Node *Sound* page, or `idn-node set netaudio-read-only=true`): read only on any computer is read only for the whole site.

**Between the network and IDN.** A click in the matrix between a network device and an
IDN transmitter or receiver routes through a computer's own network audio device: a
device's output to an IDN receiver takes one of that device's inputs (subscribed to the
output) and routes it on in IDN; an IDN transmitter to a device's input takes one of its
outputs. The channels a route used are let go a few minutes after nothing uses them any
more; while a computer is off its routes keep their channels. A computer that runs the
sound desk in its network audio device isn't used for this (its channels are the desk's).

## In-ear monitors on the musicians' phones

The desk sends the channels the musicians want in their ears to their phones; each phone
mixes them itself, so every musician has their own mix without using up the desk's aux
buses.

- **Choosing the channels.** Mark a channel **IEM** (`iem: true`). A stereo pair is marked
  as one. The phones get it after its trim, filters, EQ and dynamics, before its fader:
  the front-of-house fader and mute don't change what the musicians hear.
- **The mixes.** Add a mix per musician (`iem_mix_add {"name": "Drums"}`; its id, here
  `drums`, never changes). Each mix has a master level and, for each IEM channel it uses,
  a level, a pan, mute and solo. The musician changes their own on the phone and the
  sound person can change it from the desk; both see the other's changes at once. The
  mixes are kept with the desk; a snapshot keeps them too only when saved with
  **include IEM mixes** (`snapshot_save {"name", "include_iem": true}`).
- **The phones** sign in with a token the Site Manager gives for one mix (`iem:drums`):
  it lets the phone change that mix and nothing else. A phone takes the sound as
  multicast on Wi-Fi that carries it well, else on its own stream (up to 32 phones). The
  phones add about 20–40 ms to what the desk hears.
- **Talkback.** Each mix has a talkback input (named `Talkback Drums`, the inputs after
  the routed ones). Patch it to a channel; while the musician holds the talk button on
  the phone, their voice is on that channel.
- **Wired listeners** (another computer feeding in-ear transmitters) take the same
  channels as an IDN source, `HOST (Mixer IEM)`, in the routing matrix.
- The network needs UDP 29224 (the phones' streams and talkback) and 29253 (multicast)
  open on the desk's computer.

## Its API

For the control application and other programs (a developer's reference: `docs/api` in IDN's source):
`http://127.0.0.1:29214/api/v1/` on the computer itself (`GET state`, `GET meters`,
`POST` a command: `channel`, `bus`, `snapshot_recall`, `record_start`, …; `GET iem`
describes the IEM flow for the phones), and
`GET /api/v1/events`, a WebSocket with the state as it changes and the meters when asked
for. Started with a network password (`idn mixer --lan-password -`), also on HTTPS port
29215. In a program of its own: the `idn-mixer` library's `Console`.

## Control surfaces and tablets

- **A Mackie Control surface** (a MIDI fader box): start the mixer with
  `--surface "NAME"` (`idn mixer --list-midi` lists them). Faders, mutes, solos, pan,
  bank, scribble strips and meters follow the desk.
- **OSC** (TouchOSC, Open Stage Control, show controllers): start it with `--osc 9000`
  and `--osc-allow` the tablet's address. OSC has no password; allow only your own
  devices.
- **A tablet:** the control application's.

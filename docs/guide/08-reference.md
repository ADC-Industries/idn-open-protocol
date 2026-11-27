# Reference

## Network ports

For whoever looks after the church's network and firewalls. The installer opens these on
Windows for private networks (for the IDN programs themselves). Everything stays on the
local network except the stream, downloads, updates you fetch, and a link to another
location.

**One rule covers IDN: TCP and UDP 29200–29499.** IDN keeps to that block so it never
takes a port NDI or Dante uses: IDN, NDI Tools and Dante Virtual Soundcard can all run on
one computer. The only ports outside it are NDI's own discovery ports (IDN uses them to
be found by NDI programs, and leaves them to NDI software already using them), AES67's,
the clock's (PTP, fixed by its standard) and MQTT's.

| Port | What | Needed on |
| --- | --- | --- |
| TCP 29200 | A computer's IDN Node panel (this computer only) | none from the network |
| TCP 29201 | A computer's IDN Node panel from the network (with a LAN password) | if used |
| TCP 29202 | The Site Manager's API (the control application) | the Site Manager |
| TCP 29203 | Voice's API (the Site Manager's port + 1) | the Site Manager |
| UDP 29203 | Voice for handsets over IDN's link (QUIC; they keep talking when they change networks) | the Site Manager |
| TCP 29204 | Linking two computers without a site | if used |
| UDP 29205 | Link to another location (forward it on the router) | the main site's gateway |
| UDP 29207 | Voice calls | the Site Manager |
| TCP 29210, 29211 | The switcher's API (this computer; from the network) | the switcher |
| TCP 29212 | IDN Data's API | the IDN Data computer |
| TCP 29213 | The media library's files, played from another computer | the library computer |
| TCP 29214, 29215 | The mixer's API (this computer; from the network) | the mixer |
| TCP 29216 | Listening on phones (WHEP for the control application's page; the sound then comes on 29300–29499) | the listening computer |
| TCP 29217 | The OBS plugin's panel (this computer only) | none from the network |
| TCP 29218 | Watching a source in a browser (WHEP, for the control application's page or a WHEP player) | if used |
| UDP 29224 | The mixer's in-ear monitor streams and talkback for phones (29225 for the desk on a network audio device) | the mixer, if phones are used |
| UDP 29250–29253 | Multicast video and sound, and the mixer's in-ear monitors for phones (each on its own group address) | receivers of multicast |
| TCP/UDP 29300–29499 | Video and sound between computers, one port per connection | every computer |
| UDP 5353 | mDNS: finding sources and the Site Manager (shared with NDI and Dante) | every computer |
| TCP 5959 | The Site Manager's Discovery Server (NDI's port) | the Site Manager |
| TCP 5960 | Source lists (NDI's port; left to NDI software already using it) | every computer |
| UDP 319, 320 | The site clock (PTP; shared with Dante) | every computer on a site |
| UDP 5004, 9875 | AES67 sound and announcements | AES67 computers |
| TCP 6400–6600 | Open Media Transport (vMix) | OMT computers |
| TCP 1883 | MQTT, when the site's broker is on | the Site Manager |
| TCP 1935 | Streams pushed to the switcher (OBS, phones) | if used |
| TCP 29219, 29220 | The sound desk on a network audio device's channels (its API here; on the network) | that computer |
| UDP 4440, 4455, 8700, 8800; mDNS 5353; change notices 8702 (multicast 224.0.0.231); multicast audio 4321 (and it sends to 4321, 8708) | The network audio device (idn-aoip) on a network of Dante-compatible equipment: the ports every device there uses; its unicast audio on 14336–14591 (a port the system picks when those are taken). Without 8702 the device list catches changes only every 30 seconds; without 4321 no multicast flow is heard. Not on a computer running Dante Virtual Soundcard (the same ports) | the network audio computer |

IDN Industrial: the Industrial Site Manager's page (TCP 8080 by default), its QUIC link
(UDP 29206), MQTT (TCP 1883), Voice's calls (UDP 29207) and, on each machine, the Industrial Node's local page
(TCP 29208, this computer only).

A program that uses IDN as a library can choose other media ports
(`idn_common::ports::set_media_range`, `set_multicast_ports`); for the IDN programs,
`IDN_MEDIA_PORTS=FIRST-LAST` (or `off`: the system picks).

## Sample rate

IDN runs at **44.1, 48 or 96 kHz**; 48 kHz unless something says otherwise.

- **The whole site:** the Site Manager's site option **audio.rate** (from the control application). Every computer's sound desk, switcher sound and
  network audio device run at it. **AES67 streams** sent stay at 48 kHz, AES67's rate.
- **One computer at another rate:** that computer's IDN Node panel, *Audio routing* →
  **Sample rate** (or the Site Manager's machine settings; on a computer without a
  screen, `idn-node set audio-rate=96000`, and `audio-rate=site` for the site's again).
- **One program:** a sound desk can have its own (its setting, from its next start);
  `--rate` on a desk's or switcher's command line.
- **Changing it** starts the sound desks and the network audio device again: about a
  second of silence. Not during a service. The switcher takes it at its next start.
- Sound from a computer at another rate still plays: it is converted on the way in.
- **Streams** (YouTube, Facebook, any RTMP or SRT server) always go out at **48 kHz**;
  **recordings** keep the rate the sound was made at.
- **Network audio** (Dante-compatible equipment): the device runs at the computer's rate,
  set from IDN; Dante Controller shows it but can't change it. Equipment at another rate
  can't exchange sound with it: Dante Controller then says the formats don't match.
  Dante Via runs at 48 kHz only.
- **Noise suppression** works at every rate (at 44.1 and 96 kHz it adds a few
  milliseconds; the desk lines its other channels up by the same amount).
- 96 kHz is about twice the work for the computer of 48 kHz.

## Where things are kept

| What | Windows | Linux |
| --- | --- | --- |
| Programs | `C:\Program Files\IDN Media` (`C:\Program Files\IDN` if installed before the rename) | `/usr/lib/idn-media` (the package; commands in `/usr/bin`) |
| The computer's site identity (keys, settings) | `C:\ProgramData\IDN Node` | `/var/lib/idn-node` |
| Switcher's and mixer's settings, scenes and snapshots | `%LOCALAPPDATA%\idn-open-protocol` | `~/.local/state/idn-open-protocol` |
| Logs and crash reports | `logs` in the two folders above | the same |
| Switcher recordings (a name alone) | `Videos\IDN Recordings` | `~/Videos/IDN Recordings` |
| Mixer's multitrack recordings | `recordings` in the mixer's settings folder | the same |
| Media folder (downloads, files sent) | `C:\Users\Public\Videos\IDN Media` | `/var/lib/idn-media` |
| The Site Manager's keys | the folder given to it (`C:\IDN Site`) | the same |

Back up the Site Manager from the control application (or `backup FILE` on its console), not by copying the folder.

## Glossary

**AES67.** A standard for sound over a network. Dante equipment can use it ("AES67
mode"). IDN sends and receives it.

**Aux.** A mix sent somewhere other than the main speakers, such as a stage monitor or
in-ear mix.

**DCA.** A fader that turns a set of channels up and down together, with no sound of
its own.

**Discovery Server.** The part of the Site Manager that tells computers where the
sources are, across networks where automatic finding doesn't reach.

**FTB.** Fade to black.

**Grandmaster.** The one clock the site's computers (and AES67 equipment) follow.

**ISO recording.** A recording of one input on its own, beside the program.

**LUFS.** A measure of loudness. Streams are usually around −14 to −16 LUFS.

**Matrix.** A mix of the mixer's buses, for an extra feed (lobby, overflow, stream).

**mDNS.** How computers on one network find each other with no server.

**Global layer.** A switcher layer set up once and added to every new scene: a logo,
captions. Each scene can move or hide its copy.

**Layer.** One thing in a scene: an input, another scene or a colour, placed, sized and
shown or hidden.

**Multiview.** The switcher's screen of program, preview and every input and scene in
small tiles; its layout can be changed.

**NDI.** A common way of sending video over a network. IDN is NDI-Compatible: it
receives NDI sources, and NDI programs receive IDN's. NDI® is a registered trademark of
Vizrt NDI AB; IDN is not affiliated with or endorsed by Vizrt.

**Preview / Program.** Program is on air; preview is what comes next.

**PTP.** The protocol that keeps the site's clocks together.

**PTZ.** A camera that pans, tilts and zooms by remote control.

**Receiver.** Something that takes sound or video: the mixer, a speaker computer, OBS.

**Router.** A source with no picture of its own that sends its viewers to the source it
is switched to, and moves them all when it is switched.

**Routing matrix.** The grid that says which receiver channel plays which transmitter
channel.

**Scene.** In the switcher, a stack of layers (two cameras side by side, a logo): what
goes to preview and program. (The mixer's were called scenes too until 2026-11-18: see
Snapshot.)

**Site.** All the IDN computers of a church or building, with one Site Manager.

**Site passphrase.** A shared secret: a computer that has it joins and encrypts
without approval.

**Snapshot.** In the mixer, the whole desk saved by name, recalled later.

**Source.** Something sending video or sound on the network, named `COMPUTER (name)`.

**SpeedHQ.** The light video compression NDI uses; easy on the network's computers,
heavy on the network.

**Stinger.** A short animated clip that covers a transition.

**Transmitter.** A source, seen from the routing matrix.

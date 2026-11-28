# Troubleshooting

Start with the computer's own view: **IDN Node** in the Start menu opens its panel in the
browser. The top of the panel says what needs attention, and the *Log* card at the bottom
has the latest messages.

For the network, **IDN command prompt** → `idn doctor` checks this computer and the
network and says what's wrong, in plain words. It ends with "All good." or a count of
problems and warnings. Each ✖ is a problem, each ⚠ a warning.

## The IDN Node panel doesn't open

| What you see | What to do |
| --- | --- |
| The panel says the computer waits | Approve it on the Site Manager (in the control application), or, if several sites answered, choose one in *Sites found on the network*. |
| "The IDN Node doesn't answer" | The IDN Node isn't running. Restart the computer. If it still doesn't, run the installer again: it repairs the installation and keeps the computer in the site. |
| Sound from programs, screens or webcams don't come | The session agent (idn-session) runs them while someone is logged on: it starts at every log-on; log off and on again, or start `idn-session` (Linux: `idn session` in a terminal says what it does). |

## Sources don't appear

A camera or computer isn't in the switcher's list, the Site Manager or `idn find`.

1. **Is the network Private?** On Windows: Settings → Network → the connection → set it to
   **Private**. IDN programs are only allowed through the firewall on private networks.
2. **Is it on the same network?** Cameras on a different network (another subnet or
   VLAN) aren't found automatically. A site with a Site Manager finds them through it.
   Without one, ask whoever looks after the network.
3. **Wi-Fi guest networks** keep devices from seeing each other. Use the church's own
   network.
4. **Is it in a different group?** Sources are in the group `public` unless someone
   changed it.
5. Run `idn doctor`. "No mDNS answers at all" means a firewall on this computer blocks
   UDP port 5353.

You can always add a source to the switcher by its address (in the control application), such as
`192.168.1.50`.

## The picture stutters or freezes

- **Wi-Fi:** video over Wi-Fi drops pictures. Use a cable for cameras and the switcher.
  `idn doctor` warns about it.
- **A slow link:** a 100 Mbit/s cable or switch port is too slow for several HD sources.
  `idn doctor` says what speed each connection runs at. Use gigabit.
- **4K at 60 without a graphics card:** the switcher needs a GPU for 4K at 60 pictures a
  second. Run at 4K30 or 1080p, or use a computer with a graphics card.
- **The computer is busy:** close what else runs on the switcher's computer (browsers
  with video, games, updates).

## The livestream

**The stream shows a warning or an error** (in the control application; the switcher's state gives each
stream its health and why). The usual causes:

- **The connection dropped:** it reconnects by itself within seconds. If it doesn't,
  check the internet connection.
- **The upload can't keep up:** the internet upload is too slow for the bit rate. Let the
  stream lower its bit rate when the upload can't keep up (the stream's `adaptive`), stream a
  smaller picture, or use a faster connection.
- **The platform refuses it:** check the stream key. YouTube can give a scheduled stream
  a new key.

**A backup:** add the platform's backup address to the stream (`backup`), on a second
internet connection if you have one (`backup_via`). The platform switches to it if the
main one drops.

**Keep a copy:** recording what is sent (the stream's `record`) keeps the stream in a
file, so you can upload it afterwards if the stream fails.

## Sound

**No sound on the stream.**

1. The switcher's sound (in the control application; `audio` in its state): is it *routed* (from the
   mixer), *program* (follows the program) or something else? Listen to the switcher's
   monitor in the control application (the switcher's API, `monitor/listen`): it plays what the stream
   carries.
2. The desk's feed for the switcher (in the control application; the mixer's `switcher_feed`): is a bus set
   for the switcher's program?
3. The site's audio routing (in the control application; the Site Manager's `matrix`, each route's
   `state`): look at the routes to the switcher.

**A route's state says:**

| State | Means | Fix |
| --- | --- | --- |
| connecting | Starting | Wait a few seconds. |
| source not found | The sending computer or program isn't on the network | Start the program; check that computer. |
| no such channel | The source has fewer channels than the route asks for | Route a channel that exists. |
| receiver's program not running | The receiving program (the mixer, OBS) isn't started | Start it. |

**Sound and picture out of step.** A camera's picture is often later than its sound.
In the switcher (from the control application), the input's sound sync offset (`audio_offset`) delays its
sound to meet the picture. For the mixer's sound, the stream sound's delay (`audio`'s
`delay_ms`, up to 500 ms) does the same for the stream.

**A howl or hum on one channel.** In the mixer, the channel's high-pass filter (100 Hz
for voices) takes away rumble; **NR** takes away steady hum and fan noise.

**The network audio device doesn't show in the controller** (Dante Controller or
similar).

1. That computer's IDN Node panel, *Sound* → *Network audio*: is the device's pill
   *running*? If it says *Can't run here*, *Apps and services* → *Services* gives the
   reason: if idn-aoip isn't installed, install IDN Media again. If it says a port is
   **in use**, another
   device program runs on that computer (Dante Virtual Soundcard, Dante Via): close it,
   or use another computer.
2. With more than one network card, set **Network card** (under *Advanced*) to the audio
   network's address.
3. *Site* → *Site clock*: it should be *locked*. Equipment without AES67 needs
   **Protocol** *v1* (or *both*), under *Settings* with *Advanced* on.
4. **A Windows PC running Dante Via or Dante Virtual Soundcard on the same network:**
   its firewall can block requests from the IDN computer even with those programs
   allowed as apps (seen with Dante Via on a private network). Add an inbound rule on
   that PC allowing everything from the IDN computer's address, then try again.

**Clicks or short gaps while mixing live on the network.** Open the desk's state (its
API, `/api/v1/state`, `network_audio`; the control application shows it): if `late_writes` rises, the computer is
doing too much beside the desk. The desk moves its work a millisecond further ahead each
time (up to 8 ms) and settles. If it keeps rising, close other programs on that computer,
or choose a larger **desk block** (128). If `resyncs` rises, the site clock is unsteady:
check *Site clock*.

**The control application's network audio list is empty** (the Site Manager's `network-devices`, or the
devices of its `matrix`). It fills once a machine with the
network audio device on has read the network (within about a minute of starting). A
device listed with an error (such as "no answer") is one that stopped answering; it goes
from the list a minute after it leaves the network.

## Phones can't listen

- The phone must be on the **same network** as the computer running *Listening on
  phones*. A guest Wi-Fi often keeps phones away from the church's computers; ask
  whoever looks after the network to allow TCP 29216 and UDP 29300–29499 (the
  connection, then the sound itself).
- "No sound from this channel right now": the source is silent or not running (the
  mixer isn't started).

## Captions

- **Captions are slow or fall behind:** the computer is too slow for the model. Set the
  model to *Automatic*, which measures the computer, or choose a smaller one (base.en).
  A computer with a graphics card does much better.
- **Captions run on the CPU though the computer has a GPU:** by default captions use a
  graphics card of its own, never a GPU built into the processor, because that one also
  draws the screens and captions on it make frames late. To use the built-in GPU anyway,
  set *Runs on* (advanced, on *Live captions*) to *Any GPU*, and check that frames stay
  smooth. *Any GPU* still takes a graphics card first when there is one. `idn captions
  --list-gpus` in a terminal lists the GPUs captions can use, each with its number and
  name (the setting `captions.gpu` takes either, for one GPU of that computer).
- **Words are wrong:** add names and unusual words to *Names and words to spell right*.
- **Captions during songs:** captions leave lines out while music plays, because the
  speech model makes words up over music. If sung lines are wrongly captioned, or a
  sermon over a music bed is left out, use a mix of the speaking microphones (a matrix,
  `|ch=37`) rather than the main mix with the band.
- **A line said twice is shown once:** the same line again within 8 seconds is left out
  (the speech model repeats itself over music). A repeat after that shows.

## The Site Manager

- **The control application can't reach it:** the Site Manager isn't running. On its computer, the IDN Node
  panel's *Services* card shows it (start it there); run by hand, check its command
  prompt window is open. The computers keep working meanwhile.
- **A certificate warning:** its certificate is its own; compare the fingerprint with
  the one in the Site Manager's log.
- **A new computer doesn't show up to approve:** open the site for new computers first
  (the control application, or `pairing` on its console). Check the computer's panel: if it says several
  sites were found, choose yours there. If *Find the site on the network by itself* is
  off, type the Site Manager's address in its *Discovery Server*.

## Switcher or mixer won't start

- **"port 29210: Address already in use"** (29214 for the mixer): it is already running
  (the IDN Node panel's *Services* card shows which runs it).
- **"no IDN Node":** the computer's IDN Node isn't running (its panel doesn't open). The
  program then works, but without the site's encryption or the desk's sound.
- **The switcher needs FFmpeg:** run the installer again with *FFmpeg* ticked.

## Collecting logs for support

- **From the Site Manager:** ask it for that computer's logs (in the control application), then download.
- **On the computer itself:** the logs are in
  - Windows, the IDN Node: `C:\ProgramData\IDN Node\logs`;
  - Windows, the switcher, the mixer and the tools: `%LOCALAPPDATA%\idn-open-protocol\logs`;
  - Linux: `/var/lib/idn-node/logs` and `~/.local/state/idn-open-protocol/logs`.
- Crash reports are in the same folders and reach the Site Manager by themselves.

When asking for help, say what you did, what you expected, what happened, and the time
it happened. Attach the logs.

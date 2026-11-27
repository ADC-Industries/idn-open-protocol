# Machines and the site

## A computer's IDN Node panel

**IDN Node** in the Start menu (or the applications menu), or `idn-node open`: the panel
opens in the browser. It only opens on the computer itself, through a one-time link, so
web pages and other programs can't change it.

### This machine

The name, the site, whether it is approved (*Enrolment*), the site passphrase, the
network interface, and the build. **Change settings**:

| Setting | What it does |
| --- | --- |
| Name | What everyone sees in source names. |
| Discovery Server | The Site Manager's address; empty finds it by itself (with *Find the site on the network by itself*). |
| Network interface | The network to use, by this computer's address on it; empty chooses by itself. Set it on a computer with two networks (a video network and the office network). |
| Site passphrase | Joins the site without approval, and encrypts with the site's key. |
| Require encryption | Refuse unencrypted connections. Cameras and NDI devices without IDN can't connect then. |
| LAN panel password | Opens this panel to other computers (below). |
| Administrator code | Only to make this computer a site administrator. |

**Join code** (on the *Membership* page): the six digits the Site Manager gave for this
computer (`482 913`), typed with or without the space. The computer then joins at once, or
waits for approval if the site wants both. What it says:
- *Code accepted*;
- *Wrong code (n tries left)*;
- *The join code expired or was used*: ask for a new one;
- *The site needs a join code*.

The same from a terminal: `idn-node join 482913`.

**Try again now** reconnects to the site at once. After a wrong or expired code it asks
for approval instead, where the site allows that. **Set up again** runs the first-run
setup.

### Services

What this computer runs for the site, each with a switch. Switching one off keeps its
settings. Some run all the time (from start-up); some run only while someone is logged
in (sound, screens and webcams, which need the desktop).

### Video

For every IDN program on this computer: threads per picture, two pictures decoding at
once, how outputs send (automatic, the graphics card only when asked, the processor
only, the graphics card for all), and how H.264/HEVC is decoded. Leave them on automatic
unless the troubleshooting chapter says otherwise. A setting made in a program itself
wins.

### Sound on this computer

- **IDN Microphone:** what the routing matrix sends it, Teams, Zoom or a browser can use
  as a microphone. On Windows this needs a virtual sound card: on Windows 11 IDN's own
  (the installer offers it; the programs then choose "Virtual Mic Driver"), on Windows 10
  VB-Audio's free VB-Cable (the programs choose "CABLE Output").
- **Programs' sound:** a program's sound alone, as a source, one per line:
  `spotify=Spotify`, `chrome=Browser sound`.
- **IDN Speaker** (Linux): a speaker programs play into, sent as a source.

### AES67

Sound to and from AES67 equipment: stage boxes, consoles, Dante devices in AES67 mode.
**Look for streams (30 s)** lists what the equipment announces. Add the streams to
receive, and the IDN sources to send (1 to 64 channels each). **Low latency** for a
wired network. Use the site clock (below) and the same PTP grandmaster as the equipment.

### IDN Data, Media folder

**Run IDN Data on this machine** turns on the data feeds and the media library
(IDN Data's API, on port 29212; it has no page of its own). *This machine's media folder
is the site's media library* shares it with the site's other computers (one machine a
site: a copy of settings or a template never carries it). The *Media folder* card shows
where files land, and transfers in progress.

The data feeds (a Google Sheet, CSV or JSON, an RSS feed, read every so often, for the
switcher's titles), the library (folders, upload, tags) and downloading a video from
YouTube, Vimeo and most sites are set in the control application, over IDN Data's API (see
`docs/MEDIA-API.md` §7). Only download what the church has the rights to play.

### Live captions, Listening on phones

See [Live captions](02-quick-starts.md#live-captions) and [Listening on
phones](02-quick-starts.md#listening-on-phones).

### Open Media Transport (OMT)

Video to and from vMix, OBS with the OMT plugin and OMT cameras. One per line:
`BOOTH (Program)=Program` offers an IDN source as OMT; `VMIX (Output 1)=vMix out` takes
an OMT source in.

### Routers

A router is a source with no picture of its own, such as `BOOTH (Lobby)`. Screens, OBS,
vMix or any receiver showing it show the source it is switched to. When you switch the
router, they all follow at once, without anyone touching them. Use one for the lobby
screens, the overflow room or a confidence monitor.

- Type the routers, one a line: `Lobby=BOOTH (Program)`. The source may be left empty
  for now. **Save**.
- To switch one, pick its new source beside its name and click **Switch**.

The routers run from start-up. The Site Manager can set them in a computer's *All
settings*. A receiver that connected while the router was away stays on the last
source until it reconnects.

### Screens and cameras

This computer's screens (1 is the main one) and webcams as sources, while someone is
logged in. **Encoding** decides how IDN receivers get them. SpeedHQ keeps the graphics
card out of it. NDI receivers always get SpeedHQ.

**Virtual screens** are screens with nothing plugged in: the computer draws them like a
real monitor, and you send them like any other screen (a stage display made on this
computer, a second desktop for a remote session). Choose a size and **Add a virtual
screen**. On Windows it appears in Display settings at once (IDN installs the Virtual
Display Driver with the first one and removes it with the last). On Linux it appears
after the computer restarts, on a free connector of the graphics card; **Remove** and
restart takes it away again.

### The screens of this computer (display service)

A computer behind a TV or projector (a lobby screen, a confidence monitor, a video wall)
shows what the control application tells it: switch on **This computer's screens (the display
service)** on the Services page (or `idn-node set apps-display=on`). Then each screen
can be given a **feed**, a source or a router (it follows wherever the router is
switched), and changing it shows no black between. When the feed goes, the screen holds
its last picture for 5 seconds, then shows the site's logo (`logo.png` or `logo.jpg` at
the top of the media folder) until the feed is back; the control application can choose black, the last
picture or a slideshow instead, and how often to try again. It also sets each screen's
rotation, overscan, fit, sound and video-wall tile (with the bezels' width, so lines run
straight across), and can show each screen's number to find it. A screen with no feed
keeps the desktop. Don't give the same screen to the switcher's Screens card as well.

### Linked computers

Two computers with IDN Media and no site can be linked: they then encrypt between them
and may use each other's screens with Remote control (from the control application). **Link to another
computer** lists those on the network; **Ask to link** shows a six-digit code (such as `482 913`) here and on
the other computer, which says who asks (a notification, and its panel). There, check the codes match, then
**Accept** (tick **Remember this computer** to keep the link; otherwise it lasts until
the other computer disconnects, e.g. a visiting preacher) or **Reject**. **Unlink** on
both computers to undo a link. Untick **Other computers may ask to link to this one** to
refuse requests. A computer that joins a site turns its links off: in a site, only the
site's computers are trusted.

### Link to another location

See [A second campus](02-quick-starts.md#a-second-campus).

### Site clock

PTP, shared by every computer of the site, so sound from several computers plays in
step. On by default. *State* says whether it is locked, and to which grandmaster. If the
site has AES67 or Dante equipment with its own grandmaster, keep the same **PTP domain**
(0 unless your equipment says otherwise).

**Protocol:** leave it on *PTP v2 (AES67)* unless the site has Dante-compatible equipment
that doesn't have AES67 switched on; that equipment keeps time with the older PTP v1,
which v2 can't hear. Then choose *PTP v1* on the computers that should follow it, or
*v1 and v2, as a bridge* on the one computer that joins that equipment's network to the
rest of the site (the others stay on v2). If the Node's status warns of *two time
domains*, a v1 leader on the network won't follow the site's grandmaster: make one of
them the leader for both.

### Audio routing, Programs, Network, Site options, Log

- **Audio routing:** this computer's part of the routing matrix. A site with a Site
  Manager routes every computer from there.
- **Programs:** what is using the IDN Node right now. **Ask before a new program uses
  this machine's identity** makes a new program wait for your approval here.
- **Network:** the sources found.
- **Site options:** what the site has set for this computer.
- **Log:** the last messages.

## The Site Manager

The Site Manager keeps the site together. It has no page of its own: **the control application** is where
you work with it (or any program using its API, `https://SITE-MANAGER:29202/api/v1/`,
logged in with the site password; `GET /api/v1/events` pushes changes). What it does:

- **Adding machines:** opens the site for new computers; each asks with its name,
  address and a six-digit code (`482 913`) to compare with its own IDN Node panel;
  approve, deny, revoke. Both ends need IDN from 2026-11-18 on (older versions showed 4
  characters): a mismatch shows two different codes, so don't approve it.
- **Join codes and the join mode:** a join code is six digits, issued for one computer by
  an operator or an administrator. It works once, for 10 minutes, and is used up after
  5 wrong tries. The computer types it and joins without anyone comparing codes. The
  site's join mode decides what a computer needs:
  - *code or approval* (new sites);
  - *code and approval*;
  - *approval* (sites made before 2026-11-18, until changed).

  The Site Manager lists the codes waiting by their expiry and tries left, never the
  digits again. Each code's use and burn is in the audit.
- **Machines:** every computer with its programs, state and settings, changed from one
  place; templates of settings for several computers; locking a computer's own panel.
- **Updates:** a release given to the site once, installed on the computers now or at
  their next start, with a rollback. A computer whose IDN is installed and updated by
  another program (such as the control application) shows "Updates managed by" that program and is updated there, not
  from the Site Manager.
- **Audio routing:** every sound source and receiver of the site, and the network audio
  devices (Dante-compatible equipment), routed from one matrix; presets of routes.
- **Streams, site options, users and roles, the audit, backups, alerts, logs and crash
  reports.**

On the Site Manager's own computer its console (`idn discovery-server --trust-dir …`, run
by hand) takes commands too: `pairing`, `pending`, `approve ID`, `deny ID`, `revoke
FINGERPRINT`, `devices`, `set KEY VALUE`, `backup FILE`, `panel-password PASSWORD` (the
site password), `help`.

## The site's MQTT broker

The Site Manager can run an MQTT broker for the site: a meeting point where the switcher,
Companion, Home Assistant, Node-RED or a lighting controller talk to each other. On the
Site Manager (from the control application): MQTT **on** (the address is fine as it is), then a login for
each system on another computer (a name and a password of 8 characters or more).
Programs on the Site Manager's own computer need none.

The broker also tells others about the site: `idn/site/machines` lists the computers and
whether they're online, and `idn/site/events` says when one comes, goes, joins or leaves.

After restoring a site backup, add the MQTT logins again (the broker's own data isn't in
the backup).

## Using the panel from another computer

**A computer's IDN Node panel:** set a **LAN panel password** in its *Change settings*.
The panel is then also at `https://THAT-COMPUTER:29201/` for anyone with the password. It
uses its own certificate, so the browser warns once: compare the fingerprint with the one
in the Node's log.

**The switcher, the desk and the Site Manager** have no pages: the control application works with them,
on their computer or from another one (see [The switcher](03-switcher.md#its-api)).

# IDN Node

One service per machine that holds the machine's IDN site identity, with a control panel
on `localhost` (design: spec/extensions/idn-node.md, roadmap R.13).

Without it, every IDN program on a machine (the presentation server, the OBS plugin,
the tools) keeps its own key, its own enrolment with the site authority and its own
passphrase. With it, the machine is enrolled once, the passphrase is set once, and the
programs connect through the Node:

- **Keys stay in the Node.** Programs get certificates and ask the Node to sign their TLS
  handshakes (it signs nothing else). The device key, the passphrase and the `link/1` key
  never leave the service's state directory.
- **Media stays in the programs.** They send and receive video and audio themselves, as
  without the Node.
- **The library still works alone** for machines without the Node, embedded receivers
  and custom software.

## Install

Linux (systemd), as root:

```
sudo idn-node install
```

creates group `idn` and account `idn-node`, writes `/etc/systemd/system/idn-node.service`,
starts it, and adds the user who ran `sudo` to group `idn` (log out and in again). Add
other accounts that run IDN programs (a presentation server's service user) with
`usermod -aG idn ACCOUNT`. State: `/var/lib/idn-node`; socket: `/run/idn-node/node.sock`.

The session agent on a Linux desktop: `idn-session`, no window and no icon (IDN Media has
no UI but this control panel; the UI is the embedding application's, whose tray item can
embed the same agent).
The package starts it at every log-in (`/etc/xdg/autostart/idn-session.desktop`); a copy
installed by hand adds it once per user (not as root):

```
idn session --autostart
```

(`--no-autostart` hides both entries again). It runs this session's services (sound,
screens, webcams, the switcher and desk when set to run here), tells the Node the
programs with sound, the displays and the encoders here, and shows notices: another
computer asking to link, the site to confirm, waiting for approval. One runs a session; a
second waits. `idn-session --open` opens the control panel in the browser ("IDN Node" in
the applications menu).

Windows: run `idn-media-setup-x64.exe` (IDN Media's installer) as administrator. It installs the
service, the session agent `idn-session` (started at every log-on; notices as message
boxes), "IDN Node" in the Start menu (the control panel in the browser), the tools,
firewall rules and the OBS plugin, and asks for the site passphrase on a fresh machine.
Running it again upgrades; uninstalling asks whether to remove the machine's site identity
too (default: keep it).

By hand, as administrator:

```
idn-node.exe install
```

registers and starts the service "IDN Node" (or, when it exists, points it at this copy and
restarts it). State: `%ProgramData%\IDN Node` (SYSTEM and
administrators only); pipe `\\.\pipe\idn-node` (SYSTEM, administrators and interactive
users; network logons refused).

Without installing (a lab machine, or no administrator rights): `idn-node run --user`
keeps everything in your own directories and listens on your own endpoint.

`idn-node uninstall` removes the service; the state directory (the machine's key and site
membership) stays.

## Use

A new machine's panel starts with a short setup: its name, the site (found on the
network), the approval code, simple mode's PIN, and what the machine is for. "Set up
again" in the panel goes through it any time.

```
idn-node open                    # the control panel in the browser (one-time link)
idn-node status                  # name, site, enrolment, connected programs
idn-node set server=192.168.1.10 # join the site on that Discovery Server
idn-node set passphrase=-        # the site passphrase, read from standard input
idn-node code                    # the pairing code while the server's operator approves
idn-node set name=BOOTH-PC require=true interface=192.168.1.20
idn-node set lan-password=-      # turn on the panel on the network (https, port 29201)
```

The panel (`http://127.0.0.1:29200/`) shows the machine, the programs connected to the
Node with their outputs and inputs, the sources on the network, the site options in
effect and a log, and changes the settings. It only opens through `idn-node open` (or the
menu entry the installer adds): each opening uses a one-time link, so other programs and
web pages on the machine can't reach it.

Programs find the Node by themselves: the tools use it when neither `--passphrase` nor
`--trust-dir` is given (`--no-node` to opt out); the OBS plugin has "Use the IDN Node", on
by default. In your own program:

```rust
let node = idn_open_protocol::node::Node::connect("My app")?;
let publisher = Publisher::new(PublisherConfig {
    secure: Some(node.secure_config()),
    link: Some(node.link_config()),
    ..Default::default()
})?;
```

**Listening on phones**: the panel's *Listening on phones* card (or `idn-node set
"listen-channels=English=BOOTH (Mixer); Español=BOOTH (Mixer)|ch=39"`) offers those sources'
sound to phones on this network over WHEP on `http://THIS-MACHINE:29216/` (an API, no
page: the page phones open and its QR code come from the embedding application). The Node
runs it from start-up.

**Simple mode's PIN**: the PIN an embedding application's simple mode asks for (to leave it at the
switcher and the mixer), one for the machine, kept here (`idn-node set panel-pin=2468`;
empty: just the button). The Site Manager can set it too, and hold it (then it is refused
here).

`IDN_NODE=/path/to/socket` (or a pipe name) points programs at a particular Node;
`IDN_NODE=off` keeps them standalone.

## Video settings for the machine

The panel's "Video" section sets, for every IDN program on the machine: threads per
picture, two pictures decoding at once, how outputs send to IDN receivers (automatic,
the GPU only on request, CPU only, the GPU for all) and how H.264/HEVC is decoded
(automatic, GPU, CPU). Unset, each has its usual default. A program's own setting wins
(the OBS plugin's settings default to "the machine's setting"). From a terminal:

```
idn-node set video-threads=6 video-encoding=cpu    # an empty value: back to the default
```

The IDN Site Manager changes them too ("Change settings" on a machine). They apply at
once to the threads, and to inputs, decoders and outputs started afterwards.

## Audio routing

Programs offer routable sound receivers through the Node (`idn-audio recv --routable
"Stage monitors"`), and outputs with sound are listed as transmitters. Routes (receiver
channel ← transmitter channel) are kept by the Node in `routes.xml`, so they survive
restarts, and are made from the IDN Site Manager's routing matrix (anyone with the site
password) or on the machine:

```
idn-node routes                                   # this machine's transmitters and receivers
idn-node route "Stage monitors" 1 "FOH (Desk)" 13  # channel 1 <- Desk channel 13
idn-node route "Stage monitors" 1 -               # clear it
```

Channels count from 1. See spec/extensions/idn-routing.md.

## Managing machines over the network

Headless receivers are managed from another computer, without logging in to them:

- **At a site with an IDN Site Manager** (the Discovery Server started with
  `--trust-dir`, see below): a new machine with the Node installed finds the site on the
  network by itself and asks to join. In the embedding application (the Site Manager has
  no panel of its own: the application uses its API, `https://<server>:29202/api/v1/`,
  logged in with the site password), a volunteer checks the new machine's name, address
  and code and approves it. From then on the application shows every machine with its
  programs, and changes its name,
  network interface, "require encryption" and its own panel's password. For receivers that can't show their
  pairing code, give them the site passphrase (`idn-node set passphrase=-`, or the
  installer): they then check the Site Manager's proof of it and join without approval.
- **Without a Site Manager**: set a LAN password on the machine (`idn-node set
  lan-password=-`, or its panel) and its panel is also served at
  `https://<machine>:29201/` for anyone with that password.

The LAN panel and the Site Manager's API use self-signed certificates: the browser (or
the embedding application) warns once; compare the fingerprint it shows with the one in the server's console
or the Node's log.

`idn-node set auto-site=false` stops a machine from looking for a site by itself.

### The IDN Site Manager

```
idn-discovery-server --trust-dir /var/lib/idn-site --panel-password-file /root/site-password
```

runs the Discovery Server, the site authority and its API (`--panel-port`, default
29202; administered from the embedding application), and announces the site on the network
(`--no-announce` to stop that). The site password can also be set or changed on its
console (`panel-password TEXT`) or through the API. Anyone with it can approve, deny and
revoke machines and change their settings and the site's options.

The Site Manager's functions below are the embedding application's pages (through the
Site Manager's API).

**Every setting, copies, templates**: each machine's *All settings* is one form of
everything it runs (sound bridges, screens, the site link, AES67, listening on phones,
services, video, clock, simple mode); *Copy settings to…* sends the groups you choose to
one or many machines; *Templates* keeps named sets of settings for the site, to apply to
machines whenever you like. The site options stay as they are.

**Updates**: upload a release from ADC Industries (`.idnrelease`) to the Site Manager
(from the embedding application) and update machines now or at their next restart — one machine first, the
rest once it checks in. Each machine checks the release's signature itself; one that
doesn't come back within ten minutes goes back to what it had, and *Roll back* does that
by hand. (Machines whose Node runs at log-on on Windows, or whose IDN is older than this:
update them by hand.)

**Logs and crash reports**: every IDN program keeps a log (`logs/` in its state
directory) and writes a crash report if it crashes. The Node collects them and, in a
site, sends crash reports to the Site Manager by itself; the Site Manager lists them
(in the embedding application, through its API) and asks any machine for its logs, to read or download.
They stay on the site's own server.

**Backup**: the Site Manager's backup (from the embedding application, or its API's
`backup`) downloads the
site (its keys, machines and options) in one file, locked with a passphrase you choose; keep both safe. A new Site
Manager restored from it (from the embedding application, before any machine joins; or `idn-discovery-server
--trust-dir DIR --restore FILE`) takes the old one's place, and every machine reconnects
by itself. On the console: `backup FILE`.

## Who can use it

Any program running as a user the operating system lets in (Linux and macOS: group
`idn`; Windows: interactive users) acts with the machine's identity, including its
administrator role if the machine has one. With **Ask before a new
program uses this machine's identity** (the Node panel's *Programs*), a new program waits
until it is approved there.

## Build

`cargo build --release -p idn-node`; for Windows `cargo build --release --target
x86_64-pc-windows-gnu -p idn-node` (MinGW-w64, see the workspace's `.cargo/config.toml`).

Licensing: MIT OR Apache-2.0.

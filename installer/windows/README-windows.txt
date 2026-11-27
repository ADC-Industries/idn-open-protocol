IDN Media (IDN Open Protocol) — Windows x64 build (cross-built from Linux with MinGW-w64, OBS 32.1)

The downloads (one build, the same programs in each):
  - idn-media-setup-x64.exe        IDN Media's installer (recommended); programs without debug symbols
  - idn-node-windows-x64.zip       the same programs with debug symbols, to run without
                                   installing or to read a crash report
  - idn-lab-tools-windows-x64.zip  the separate lab tools (idn-send.exe, idn-recv.exe, ...,
                                   idn-audio.exe, idn-mixer.exe, capture-check.exe), with
                                   debug symbols; unzip next to the programs above

Installing (recommended): run idn-media-setup-x64.exe as administrator. It asks how the IDN Node
runs: as a service (recommended: always on, one site identity for the computer) or when
you log on (no service: the session agent starts it for each user, with a site identity
of their own; silent installs: /S /LOGON). IDN Media has no UI but the IDN Node's control
panel: the switcher, the mixing desk and the Site Manager are servers with APIs, and
their UI is the application that uses them. It installs
  - the IDN Node, its session agent idn-session.exe (started at every log-on, no window:
    this session's sound, screens and webcams as the Node's settings ask; notices as
    message boxes when another computer asks to link, the site needs confirming or this
    computer waits for approval) and "IDN Node" in the Start menu (the control panel in
    the browser);
  - the idn command (the tools, sound cards and the mixing desk) and the video switcher
    into C:\Program Files\IDN Media (an earlier install keeps its folder), with firewall
    rules for private networks, and "IDN command prompt" in the Start menu;
  - the OBS plugin, when OBS Studio is installed (close OBS first);
  - FFmpeg 8 for idn-codec and the switcher, downloaded during installation (optional,
    80 MB).
  It asks for the site passphrase (optional) on a fresh machine. Running it again upgrades
  in place; the machine keeps its site identity. Uninstall from Settings -> Apps (or "Uninstall
  IDN Media" in the Start menu); it asks whether to remove the machine's site identity too
  (default: keep it, so a reinstall needs no new approval).

Without installing, the programs in the zips work as before (below).

OBS plugin (by hand)
  1. Close OBS.
  2. Copy the three files (the installer does this):
       idn_obs.dll          to  C:\Program Files\obs-studio\obs-plugins\64bit\
       idn_obs_ffmpeg7.dll  to  C:\Program Files\obs-studio\data\obs-plugins\idn_obs\bin\
       idn_obs_ffmpeg8.dll  to  the same bin folder
     idn_obs.dll is a small loader: it loads the build for this OBS's FFmpeg (ffmpeg8 for
     OBS 33, ffmpeg7 for OBS 32).
  3. Start OBS. Help -> Log Files -> View Current Log should show
     "[idn-obs] idn-obs 0.1.0 (Build …) loaded (libobs API 32.1.0)", with this zip's build.
  Sources -> + -> IDN Source: pick a source found on the network, or type host:port.
  NDI|HX (H.264/HEVC) sources are decoded through the FFmpeg that OBS ships (avcodec-62.dll
  in OBS 33, avcodec-61.dll in OBS 32), so the plugin needs no FFmpeg of its own; it must
  sit in OBS's own folders to find it.
  IDN Output: Tools -> IDN Output... opens its settings page in the browser
  (http://127.0.0.1:29217/, this computer only): on/off, name, quality, encoding, video
  threads, the preview as a second source (see idn-obs-README.md). Its "encoding" setting: auto (default:
  4K to IDN receivers through the GPU - NVENC, AMF or Quick Sync), offer, cpu or force.
  IDN Source settings: "Video threads" and "Decode two pictures at once" (4K at 50/60 fps).
  IDN Routed Audio (a source): OBS as a receiver in the site's routing matrix (a host application,
  or idn route); name it, pick the channels, and route sound to it there (needs the IDN Node).

Tools: one command, idn (Windows Defender Firewall will ask on first use: allow private
networks). "idn help" lists the commands; "idn COMMAND --help" each one's options.
  idn send --name Test --size 1280x720            send a test picture with a tone
  idn recv "HOST (name)" --seconds 10             receive and count
  idn find                                        list sources (mDNS)
  idn doctor                                      check the network set-up
  idn codec probe                                 GPU encoders/decoders (needs FFmpeg 8's DLLs:
                                                  the installer downloads them)
  The old names (idn-send.exe, idn-recv.exe, ...) still work: installed, they are the idn
  program under another name; the lab tools zip has them as programs of their own.
  Notes: the Windows build takes the default route's network interface only (one
  interface); Ctrl+C handling (clean goodbyes) is not wired on Windows yet.

IDN Node (the machine's IDN site identity, one approval for every IDN program here)
  (The installer does all of this; by hand:)
  As administrator:  idn-node.exe install      (installs and starts the "IDN Node" service)
  Control panel:     idn-node.exe open         (opens http://127.0.0.1:29200/ with a one-time link)
  Join a site:       found by itself on the network, or idn-node.exe set server=192.168.1.10;
                     then approve it on the IDN Site Manager
  Run the site here: Setup's "IDN Site Manager" component (the Node service runs it from
                     start-up; its keys in C:\ProgramData\IDN Site Manager), or by hand:
                     idn node set sitemanager-on=true  then  idn node set sitemanager-password=-
                     It has no panel of its own: an application administers it through its API
                     (https://<this machine>:29202/api/v1/), logged in with the site password
  Panel from the LAN: idn-node.exe set lan-password=-   then https://<this machine>:29201/
  Video settings:    the panel's "Video" section (threads, two pictures at once, encoding,
                     decoding) for every IDN program here; a program's own setting wins
  Status:            idn-node.exe status
  Session agent:     idn-session.exe           (no window; started at every log-on; one per
                                                  session) - idn-session.exe --open opens the
                                                  control panel; "idn session" is the same in a
                                                  console
  Remove:            idn-node.exe uninstall    (the machine's key stays in %ProgramData%\IDN Node)
  The OBS plugin and the tools use the Node when it runs (OBS: "Use the IDN Node", on by
  default; tools: unless --passphrase, --trust-dir or --no-node is given). See idn-node-README.md.

Mixing desk: idn mixer   (run by the IDN Node when set to; its API on http://127.0.0.1:29214/;
  no UI of its own: a host application provides one)
  CLAP and VST3 plugins (from their standard folders) run in idn-plugin-host.exe, which
  must sit next to idn.exe / idn-mixer.exe. See idn-mixer-README.md.

Video switcher: idn-switcher.exe   (run by the IDN Node when set to; its API on
  http://127.0.0.1:29210/; no UI of its own: a host application provides one)
  Needs FFmpeg 8's DLLs (the installer downloads them). Composites on the GPU (DX12 or
  Vulkan) where there is one, else on the CPU (--cpu forces it). See idn-switcher-README.md.

ASIO driver: "IDN" in any ASIO program's driver list (Reaper, Cubase, vMix, ...; the
  installer registers idn_asio.dll; by hand: regsvr32 idn_asio.dll as administrator).
  The program's outputs are an IDN source named after it ("THIS-PC (Reaper)"); its inputs
  are a routable receiver of the same name: route channels to it in the site's routing
  matrix (a host application, or idn route). 16 in and 16 out, 44.1-96 kHz, buffers of
  32-2048; %LOCALAPPDATA%\idn-open-protocol\asio.json can set "inputs", "outputs" (up
  to 64), "name" and "latency_ms" (default 10). The ASIO settings button opens the IDN
  Node panel.

Sound cards: idn-audio.exe (WASAPI, and Steinberg ASIO with IDN_AUDIO_HOST=asio)
  idn-audio.exe devices                                          list sound cards
  idn-audio.exe send --name NAME --device "Dante Virtual Soundcard"  a sound card as a source
  idn-audio.exe recv "HOST (name)" --device "Speakers"              a source on a sound card
  --low-latency   on send (2 ms blocks) and recv (10 ms buffer), for live sound on a wired network
  idn-audio.exe recv --routable "Booth" --device "Speakers"          a routable receiver: what
                     the site's routing matrix (a host application, or idn route) sends it plays
  set IDN_AUDIO_HOST=asio   (before a command: use the ASIO driver instead of WASAPI)
  capture-check.exe DEVICE SECONDS   a diagnostic: records a device straight to a file
  See idn-audio-README.md.

Separate programs: idn_obs.dll (GPL, part of OBS) never contains idn-audio or ASIO; OBS
gets IDN sound through the IDN Node. ASIO is a trademark and software of Steinberg Media
Technologies GmbH.

IDN's virtual sound card (for the IDN Microphone, Windows 11): the installer offers the
Virtual Audio Driver by VirtualDrivers (github.com/VirtualDrivers, MIT licence, signed by
the SignPath Foundation), a virtual speaker and microphone: Teams, Zoom and browsers pick
"Virtual Mic Driver" as their microphone. Declined at install, the IDN Node's control
panel offers it; `idn-node driver install audio` (as administrator) installs it, `idn-node
driver remove audio` removes it; uninstalling IDN Media removes it too. Its package and
licence are in drivers\audio.

Virtual screens: the Node's panel (Screens and cameras) and `idn-node virtual-screen add
1920x1080@60` (as administrator) add screens with nothing plugged in, to send like any
other, through the Virtual Display Driver by VirtualDrivers (MIT, signed), installed with
the first virtual screen and removed with the last; its package and licence are in
drivers\display, its settings in C:\VirtualDisplayDriver.

VB-CABLE (for the IDN Microphone, Windows 10): the installer offers VB-Audio's VB-CABLE, a virtual
audio cable that lets Teams, Zoom and browsers use the IDN Microphone ("CABLE Output" as
their microphone). Its package is in the vbcable folder, as VB-Audio ships it; declined at
install, the IDN Node's control panel offers it when the IDN Microphone is on. Restart the
computer after installing it. VB-CABLE is from VB-Audio (www.vb-cable.com) and is
donationware: all participations are welcome. VB-Audio's setup removes it (vbcable folder,
VBCABLE_Setup_x64.exe, Remove Driver).

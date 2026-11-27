# idn-display

Shows an IDN or NDI-Compatible source full screen on a display: a projector, a confidence monitor,
a lobby screen (spec/extensions/idn-display.md). Usually started by the switcher (its
`screen` command); on its own:

```
idn-display SOURCE [--screen N|NAME] [--window] [--audio DEVICE] [--name NAME]
idn-display --list-screens
idn-display SOURCE --kms CONNECTOR          (Linux, no desktop)
idn-display SOURCE --virtual-camera [--camera-size WxH] [--camera-fps N] [--camera-device /dev/videoN]
```

- `SOURCE`: `HOST (name)` as the sources are listed, or `address:port`.
- `--screen`: a screen by number from 1 or by name (`--list-screens`); default the first.
  The picture fills it with its aspect kept, black around; the cursor is hidden.
- `--window`: in a window instead (Esc closes it).
- `--audio`: the source's sound on that sound card (`default`: the system's), e.g. an
  HDMI projector's speakers.
- `--kms`: on Linux with no desktop, straight on a display connector (`HDMI-A-1`, …; the
  user needs the `video` group).

**Virtual camera** (`--virtual-camera`): the source as a webcam for Zoom, Teams and
browsers, at one size (default 1920×1080 at 30), fitted, black while nothing arrives.
Linux: v4l2loopback (`sudo apt install v4l2loopback-dkms`, `sudo modprobe v4l2loopback
exclusive_caps=1 card_label="IDN Camera"`). Windows 11: "IDN Camera" appears while it
runs (the installer registers its part, `idn_camera.dll`).

With the machine's IDN Node running it uses the site's encryption. It draws on the GPU
(Vulkan, DX12 or Metal), or a software renderer where there is none.

## The display service

`idn-display --service` (`idn display --service`; the IDN Node runs it in the desktop
session when `apps.display` is on) looks after this computer's screens for an embedding
application's screen-wall UI (spec §D.8, docs/MEDIA-API.md §9):

```
idn-display --service [--port N] [--lan-port N] [--no-lan] [--state-dir DIR] [--no-node]
```

- Its API is on `http://127.0.0.1:29222/api/v1/` (open on this computer) and, once the
  computer is in a site, on the network at `https://HOST:29223/` for site tokens of the
  engine `display` only. Changes carry `X-IDN-Display: 1`; every change goes into
  `audit.jsonl`.
- `POST /api/v1/feed {"screen": 1, "source": "BOOTH (Program)"}` shows a feed (a
  source, or a routing source it follows); `""` gives the screen back to the desktop.
  The picture stays until the new feed's first one, so changing it shows no black.
- `settings` sets rotation, overscan, fit / fill / stretch, the sound (card, channels,
  level), what to show when the feed goes (the last picture, then the site's logo after
  5 s by default) and a video wall's tile with its bezels; `identify` shows a screen's
  number and name; `GET /api/v1/thumbnail?screen=1` is a JPEG of what it shows.
- Each screen given a feed (or identify) runs its own `idn-display --commands --screen
  NAME`, told what to show as JSON lines on its standard input; it starts again within
  3 s if it stops. The feeds and settings are kept by screen name in `screens.json` in
  the state folder (`display` in IDN's state folder).
- Not on a screen the switcher already drives (its Screens card): two windows would
  take the same screen. A computer with no desktop (`--kms`) isn't served yet.

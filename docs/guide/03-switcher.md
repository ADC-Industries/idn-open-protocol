# The switcher

The IDN Switcher makes the program: what the livestream, the recordings and the screens
show. It takes cameras and other IDN or NDI sources, files and playlists, colours,
titles, slideshows, PTZ shots and stream inputs, as many as you need; you put them into
scenes, and scenes go to preview and program, with cut, mix, wipe and fade, stingers,
fade to black, independent mixes, a cue list (Planning Center plans too), setups with
undo, streams to YouTube and other services, recordings (one file a camera too), a
multiview you can lay out, screens on this computer's displays, and the stream's sound
with its monitor.

**Its screen is the control application's.** IDN Media's switcher has no window or page of its own: the control application
drives it, on the same computer or from another one.

## Running it

Tick **Switcher** in the computer's IDN Node panel (*Services*). The Node starts it with
the computer (or when someone logs on, when it should show screens there) and again if
it stops. It keeps everything you add (inputs, scenes, titles, the show, setups) in its
folder, and finds it again when it starts.

A computer with a graphics card (GPU) composes 4K at 60 pictures a second; without one,
1080p60 or 4K30 on the processor.

## Scenes, layers and global layers

It works as OBS does:

- **Inputs** are a list: cameras, files, titles. An input is never put on air by itself.
- **Scenes** are what you show. A scene is a stack of up to 16 layers, each an input (or
  another scene, or a colour) placed where you want it. Only scenes go to preview and
  program. A camera on its own is a scene with one layer.
- **A layer can be shown or hidden** while the scene is on air, fading in and out (half a
  second unless you set another time). A layer marked for volunteers can be switched by
  them in the control application.
- **Global layers** are for what belongs on everything: the church's logo, captions. You
  set one up once; every new scene gets it as a layer of its own, which you can move up or
  down or hide in that scene. Change the global layer (another picture, another place) and
  every scene's copy follows.
- **Only what is used is received.** A camera that isn't in a scene on preview, program or
  a mix (or on the multiview) isn't received, which saves the network; it connects as soon
  as its scene comes to preview. Mark an input **keep active** if it should always be
  ready (a camera you cut to straight from program).

Shows and triggers saved before 2026-11-18 that put a camera straight on program aren't
moved over: put the camera in a scene and save them again.

## The multiview

The multiview shows program, preview and every input and scene in tiles, with their
names and red and green borders for program and preview. The control application can lay it out: which
tiles, where, how big, with or without names and borders. It can be sent to a screen or
another computer as "*switcher name* (Multiview)", switched on and off at any time.

## Its API

For the control application and other programs (a developer's reference: `docs/api` in IDN's source, and
`docs/INTEGRATION.md`):

- `http://127.0.0.1:29210/api/v1/` on the computer itself: `GET state`, `POST` a command
  (`cut`, `auto`, `program`, `add_input`, `cue_go`, …).
- `GET /api/v1/events`, a WebSocket: the state as it changes, the meters when asked for,
  commands and their replies in order.
- From another computer: started with a network password (`idn switcher --lan-password -`,
  the password typed after it), the API is also on HTTPS port 29211, logged in with it.
  (The control application's own server, on the switcher's computer, usually talks to it there.)
- In a program of its own: the `idn-switcher` library's `service::Service`.

## MQTT: Companion, Home Assistant, lights

The switcher can join an MQTT broker, so other systems can follow it and drive it: a
Stream Deck through Companion, a Home Assistant button, Node-RED, a lighting controller.
Use your site's broker (see [The site's MQTT broker](05-machines-and-site.md#the-sites-mqtt-broker))
or one you have; the control application sets it.

- **What others see:** what's on program and preview, the tally of every input, the cue
  standing by, fade to black and the streams, under `idn/THIS-COMPUTER/switcher/…`.
- **Controlling it:** a message on `…/switcher/cmd/cut`, `…/cmd/cue_go`, or
  `…/cmd/program` with `{"input": "Pulpit"}` (a scene) does what the button does;
  `…/cmd/scene_layer_visible` shows or hides a scene's layer.
- **Triggers:** "when this topic gets a message (containing this text), do this". For
  example, the topic of a wireless button and the command `cue_go`: pressing it fires
  the next cue.
- **In a cue:** the action **MQTT message** sends a message when the cue fires: a lighting
  scene, a screen going down, a sign lighting up.

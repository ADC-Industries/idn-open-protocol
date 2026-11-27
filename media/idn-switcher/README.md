# idn-switcher

A video switcher for livestreaming (roadmap R.15), as OBS works: IDN and NDI-Compatible
inputs, files, colours and titles without a fixed limit (each received only while in use),
put into scenes — stacks of up to 16 layers, each shown or hidden with a fade; global
layers (a logo, captions) in every scene — and only scenes on program and preview, with
cut, mix, wipe (left, right, up, down, circle), fade through black, stingers, luma wipes,
a T-bar, fade to black, the stream's sound, an adjustable multiview, and the program out
as an IDN or NDI-Compatible source, to YouTube and other RTMP/SRT services, and to
recordings.

It is an engine first: the `idn_switcher` library (`Switcher::start`, commands, a state
with change events) is what other programs embed. The `idn-switcher` program runs it as
a service with an HTTP/JSON API, a push channel and MQTT; it has no UI of its own (the
embedding application is the UI).

## Embedding (the library)

`Switcher::start(SwitcherConfig)` returns a running engine. It owns its worker threads (one
per input, plus the compositor) and stops when the `Switcher` is dropped, withdrawing the
source. A full, runnable example is [`examples/switcher_integration.rs`](examples/switcher_integration.rs)
(`cargo run -p idn-switcher --example switcher_integration`).

**Inputs.** `add_input(label, Source) -> io::Result<id>` adds one (no fixed limit);
`remove_input` / `rename_input` take the id. An input is received only while it is in
use (§S.3): in a scene on program, preview or a mix (hidden layers too), on the multiview
while it is drawn (at its tile's size), a stinger or luma pattern, recorded, heard,
soloed, a camera moved over its own connection, or kept active (`keep_active(id, true)`);
a network input disconnects `SwitcherConfig::idle_after` (10 s) after its last use and
connects again when it is used. The state's `inputs[].active` says which receive.
`Source` is:

- `Source::Network(name)` — an IDN or NDI-Compatible source, `"HOST (name)"` or `"host:port"`.
- `Source::Colour(Ycc)` — a flat colour.
- `Source::Still { path, frame }` — a picture you already hold, as an `Arc<Frame>`, shown
  as-is with no encode/decode: a **static** key (a lower third, a lyric slide), no network.
- `Source::Feed` — a **live** picture your app renders. Add the input, then push each new
  picture with `feed(id, Arc<Frame>)`; the switcher shows the newest, fitted to the program,
  with no network hop. (Publishing an IDN output and adding it as `Source::Network` on
  loopback still works if you'd rather go through the network path.)
- `Source::Delay { input, by_ms }` — another input shown `by_ms` milliseconds late; the
  switcher buffers that input's pictures and replays them delayed, to line a camera up with
  a delayed feed or sound. Video only; the buffer costs the delay-in-frames × a picture of
  RAM, so a long delay on a large picture uses real memory.
- `Source::Title` — text and boxes (a name, a lower third, a caption), set with
  `set_title(id, title::Title)`; see **Titles** below.
- `Source::Slideshow(Slideshow { pictures, every, fade, looped, shuffle })` — pictures one
  after another, read with the loader you give `set_picture_loader`; moved on with
  `slideshow(id, SlideAction::Next | Previous | Goto(n) | Play | Pause)`.

**Control** (each returns `Result<(), String>`): `preview(scene)`, `program(scene)`,
`cut()`, `auto()`, `tbar(0.0..=1.0)`, `transition(Kind, secs)`, `ftb()`,
`scene_layer_visible(scene, layer, on)`, `audio(AudioSource, gain_db, delay_ms)`. Only
scenes go on the buses (an input is refused: put it in a scene). Input ids are one-based.

**State and change events.** The engine keeps a monotonic `version()`. `wait_change(after,
timeout)` blocks until the version passes `after` (or the timeout), so a UI thread
waits-reads-redraws without polling. `state()` is a `serde_json::Value` snapshot (the shape
the program's `GET /api/v1/state` returns); `multiview_jpeg()` is the multiview.

**Scenes** (§S.12). `add_input(label, Source::Scene)` makes a scene — what goes on the
buses (and on the multiview, ISO, triggers), whose picture is its **layers** drawn bottom
to top over a transparent canvas: `scene_items(id, Vec<scene::Item>)` sets them as a
whole, at most 16 — each an input (another scene too: groups and nesting, at most 4 deep,
no loops) or a colour, placed and sized in fractions of the frame, cropped, turned, faded,
blended (normal, add, multiply, screen) and bordered. Each layer has a stable `id` (kept
when the list is sent back with it; a new layer gets a new one), a `name`, a `volunteer`
flag (volunteers may toggle it) and a `fade`: `scene_layer_visible(scene, layer, on)`
shows or hides it, fading. A scene of one full-frame layer passes its input through (the
bus takes the input's own picture, fitted; nothing drawn). A scene keeps an alpha plane,
so a scene of graphics keys cleanly as a layer of another. Scenes are drawn on the GPU when the program is
composited there (one compute pass a scene, `gpu_scene::SceneGpu`; within a step of the
CPU's drawing), else on the CPU (upright items, any blend, quickly; turned items sample by
sample, slow at 2160p); the program keeps them in its state directory.

**Global layers** (§S.5). `set_global_layer(scene::Item, to_scenes)` defines a layer once —
a logo bug, captions — and adds it to every scene made from then on (with `to_scenes`,
to every scene there is too, on top): in each scene it is an ordinary layer (`global:
Some(id)`), moved up or down and shown or hidden there. Set again with its `id`, every
copy takes the change but keeps its place and visibility; `remove_global_layer(id)` takes
it out of every scene. At most 16.

**Multiview** (§S.7). `set_multiview_layout(Some(tiles))` lays it out: tiles
(`multiview::TileSpec`) of program, preview, an input, a scene or a mix, placed in
fractions, with or without a label strip and a tally border; `None`: the default
(preview and program across the top, every input and scene below, growing as there are
more). `multiview_output(on)` sends it as `NAME (Multiview)` or stops it at run time.
Scenes are drawn there at their tile's size, on the CPU.

**Titles** (§S.16). A title's layers — boxes (colour with alpha, rounded corners) and text
(any installed font, size as a fraction of the frame height, bold/italic, colour, alignment,
wrapping, outline, soft shadow) — placed in fractions of the frame and drawn over a
transparent canvas with cosmic-text (shaping, right-to-left, font fallback; DejaVu Sans
built in for machines with no fonts). Drawn when set (tens of ms), never per frame; keys
as a layer of a scene (a global layer too). `title::render` draws one on its own. `font_families()`
lists the fonts.

**Filters** (§S.17). `set_filters(id, Vec<filter::Item>)` puts a list of filters on an
input — chroma, colour and luma keys, a picture as a mask, colour correction, a `.cube`
LUT, sharpening, scrolling — run in order on each new picture on the input's own thread,
so it is filtered wherever it shows (program, scenes, mixes, ISO). Keys and
masks give it alpha. About 1 ms a filter at 1080p and 3–6 ms at 2160p on the CPU.
`filter::Chain` runs a list on a `Frame` of your own.

**Title fields, clocks and timers** (§S.19). A text's `{name}` shows the title's field
`name` (`set_title_fields`, `add_title_field` for a score), `{clock}`, `{clock:12}`, `{date}`,
`{date:long}` the local time and date, `{timer:NAME}` a timer (`timer(name,
TimerAction::…)`: down from a length, up, or down to a time of day; its end fires
`TriggerEvent::TimerEnd`). Picture layers show a logo. Titles with a clock or timer are
drawn again as their text changes. `input_jpeg` previews an input for a designer.

**Titles from data** (§S.20). A title's `data` (a CSV/TSV/JSON/XML file or address, an RSS
feed, a Google Sheets link) fills its `{Column}` placeholders a row at a time, read every
N seconds; `title_row(id, SlideAction)` moves the row (or `advance` moves it by itself).

**PTZ and virtual shots** (§S.21). `set_camera(id, Some(ptz::Control::Network | Visca { … }))`
gives a camera input its control (its own NDI-Compatible or IDN connection, or VISCA over IP/TCP/UDP);
`ptz(id, ptz::Move::…)` moves it. `Source::Ptz { camera, preset }` is a preset as an input:
it shows the camera and moves it to the preset from preview (not while the camera is on
air through another input). `Source::Virtual { input, view }` is a shot cut from another
input, moved and zoomed smoothly with `virtual_view(id, View, secs)`.

**Mixes** (§S.15). `add_mix(name, width, height)` adds an independent mix (up to `MIXES`,
four): its own source on the network, of its own size (a vertical 1080×1920 for Shorts),
with its own buses (`mix_program`, `mix_preview`), transition (`mix_transition`, not a
stinger) and `mix_cut` / `mix_auto`, sharing the inputs and scenes and carrying the
stream's sound. `tap_mix(id, f)` hands `f` its frames, to record or stream it. Mixes are
put together on the CPU at their own size (their scenes drawn on the GPU when the program
is), after the program; `remove_mix` ends one.

**Triggers.** `set_triggers(Vec<Trigger>)` runs actions automatically on events — an input
going on air (`TriggerEvent::OnAir(id)`), a transition finishing (`TransitionEnd`) or a
slideshow that doesn't loop reaching its end (`SlideshowEnd(id)`). Actions are internal
only, no shell: `Program`/`Preview` (a scene)/`Cut`/`Ftb { on }`/`LayerVisible { scene,
layer, on }`/`Slide { input, action }`. A
small watcher thread fires them; avoid cycles (an action that re-creates its own event).

**ISO recording.** `tap_input(id, f)` hands `f` that input's picture every program frame
(full size; kept full while tapped), stamped as the program frame, with its own sound,
on a worker thread of its own; feed the frames to `idn-ffmpeg`'s `Gateway::send` to record
it. Drop the returned `InputTap` to stop. (`TappedFrame::picture` builds such frames.)

**Multi-track sound.** `tap_sound_tracks(&[ids], f)` gives `f` the stream's sound with a
track per chosen input (its own sound, at the latency it would have as the stream's) —
feed it with the program's pictures to a gateway set for `1 + n` tracks
(`StreamSettings::audio_tracks`).

**Program out, in-process.** `program_output()` is the composited program as an
`idn_open_protocol::api::Output`. Tap it (`program_output().tap(|f| …)`) to receive every
composed frame in your process with no network hop; it is also offered on the network.

**GPU.** `SwitcherConfig::gpu` (default on) composites on the GPU when the machine has one,
else on the CPU; `state()["compositor"]` says which, and the program's `--cpu` flag is just
`gpu: false`. By default the compositor opens its own wgpu device (Vulkan/DX12/Metal). If
your app already uses wgpu, keep a single device in the process by handing yours in:
`SwitcherConfig::gpu_device = Some((device, queue))` (clones; the engine composites on it
via `gpu::Gpu::with_device`). That device must be created with storage buffers large enough
for six 2160p pictures (`max_storage_buffer_binding_size` and `max_buffer_size`), plus
`MAPPABLE_PRIMARY_BUFFERS` on an integrated GPU for the no-copy path and `SHADER_F64` for
CPU-exact reconstruction; an unsuitable device falls back to the CPU, with the reason in
`state()["compositor"]`. See [`core/idn-shq-gpu/README.md`](../../core/idn-shq-gpu/README.md) for the
one-device-per-process story and the readback API.

## Program

```
idn-switcher [--name NAME] [--format 1080p30|1080p60|2160p30|2160p60|WxH@FPS] [--port N]
             [--preview] [--multiview] [--input SOURCE]... [--no-node] [--cpu]
             [--lan-password - | --lan-password-env VAR] [--lan-port N] [--no-lan]
```

The API is on `http://127.0.0.1:29210/api/v1/`. With a password (`echo … |
idn-switcher --lan-password -`, or an environment variable), it is also on the network
at `https://THIS-MACHINE:29211/` behind a login (a self-signed certificate; its
fingerprint is printed at start). On a machine in a site that port also opens without a
password and takes the Site Manager's tokens (`Authorization: Bearer idn1…`, read-only
or full control; spec idn-node.md §N.31.5); `--no-lan` keeps it closed. Every change is
written in `audit.jsonl` in its folder (`GET /api/v1/audit`). The program appears on the network as
`HOST (Switcher)`, the multiview with `--multiview` (or the `multiview` command, at any
time) as `HOST (Switcher (Multiview))` (`GET /api/v1/multiview.jpg` either way). With the machine's IDN Node running, inputs and
the program use the site's encryption, and the switcher is a routable receiver named like
the program: route the desk's mix to it in the routing matrix and choose "routed" as its
sound.

**Monitor** (what you hear, as streamed — the mixer's headphones are live, a different
mix): the stream's sound, or preview's input's sound to check the next source, or any
inputs soloed; a volume and mute of its own; played on a sound card of the switcher's
machine and/or fetched with `GET /api/v1/monitor/listen` (`monitor`, `monitor_solo`).
Meters for each input, the stream and the monitor (`GET /api/v1/meters`).

**On a mixing desk**: the program, each mix, Preview (with its input's sound) and the
monitor (`NAME (Monitor)`, sound only) announce themselves as a switcher's outputs, so an
`idn-mixer` on the site puts each on two desk inputs by itself. The other way, the desk
can feed the program and each mix a mix of its own (the desk's `switcher_feed`); the
switcher routes it in by itself, and its state shows what it takes. A mix takes its feed
by itself; the program takes its feed when its sound is "routed", or, with `take_feed`
on (`audio`), whenever a desk feeds it (its own source again when the desk stops). A mix
without a feed carries the stream's sound.

**Captions**: send a line (`caption`) and it appears as closed captions in the stream's
and recordings' video (CEA-608 roll-up, what YouTube and players show), clearing after a
few quiet seconds.

**Setups**: save the whole setup by name (inputs, scenes, titles, filters, cameras'
control, mixes, timers, the show) and load another — Sunday, Wednesday, a wedding;
cameras both have stay connected. Every input added is kept across restarts. `undo` /
`redo` step back through edits of the setup — inputs added, removed or renamed, titles,
scenes, filters, the show — never the live cuts.

**Show** (a cue list, run of show): each cue fires its actions together on one GO —
the switcher's own (a scene to program, a transition, fade to black, a layer shown,
slideshows, timers, any command), a mixer's scene, a web request (a control app, Companion), an
OSC message — and can follow on by itself after a time. Back, stand by, fire, save and
load shows by name; a Planning Center service plan becomes the show with your own
personal access token (kept on this machine).

**Streams that ride out a bad connection** (§S.30): a stream reconnects within seconds;
its bit rate can go down while the upload can't keep up and back up after a good minute
(`"adaptive": true`, never under `floor_mbps`, 1.5 Mbit/s unless set); `"record"` keeps
the stream's own pictures and sound in a file, no second encode, including what a slow
upload had to skip; a `"backup"` ingest (YouTube's or Facebook's backup URL) can go
beside it, each out its own internet connection (`"via"` / `"backup_via"`: the address
this machine has on it). The state gives each stream and recording a health (`ok`,
`warning`, `error`), what it sent over the last two minutes, and why; problems are alerts, in
the state (`stream_alerts`) and sent on to the Site Manager — and so is a recording's
disk filling up.

**Starters**: `starter` `{"name": "Sunday service"}` adds a service's titles (a lower
third, a Starting soon countdown, Be right back), its countdown timer and a cue list —
Sunday service or Simple stream — to what you have (the state's `starters` lists them);
add your cameras and slides.

**Screens**: `screen` puts the program, preview, the multiview or a mix full screen on any
display of the switcher's machine — or on the **virtual camera**, a webcam for Zoom, Teams
and browsers — (a projector, a confidence monitor; with no desktop on Linux, a display
connector), each through its own `idn-display`, started again if it stops
(`GET /api/v1/screens` lists them; `screens.json`).

## API

`GET /api/v1/state` returns the state (with `frame_ms`, and the worst frame time and
the worst late start of a frame over the last 10 s, for finding stalls); `?after=VERSION`
waits (up to 20 s) until it changes. `GET /api/v1/multiview.jpg` is the multiview. Over the network, log in first:
`POST /api/v1/login {"password": …}` and keep the cookie. Commands are `POST /api/v1/<command>` with a JSON body and the header
`X-IDN-Switcher: 1`:

| Command | Body |
| --- | --- |
| `add_input` | `{"network": "HOST (name)" or "host:port", "label": …}`, `{"file": "/path/clip.mp4", "loop": true}` (video, sound or a picture) or `{"colour": "#rrggbb"}` or `{"scene": true, "label": …}` (an empty scene) → `{"id": n}` |
| `add_input` (made here) | `{"title": {"layers": [layer, …]}, "label": …}` (a title, §S.16) or `{"slideshow": {"pictures": [files] \| "folder": dir, "every": s, "fade": s, "loop": true, "shuffle": false}, "label": …}` → `{"id": n}` |
| `title` | `{"id": n, "layers": [{"type": "text", "rect": [x, y, w, h], "text": …, "font": "" (default) \| family, "size": 0.05 (of the frame height), "bold", "italic", "colour": "#rrggbb[aa]", "align": "left" \| "centre" \| "right", "valign": "top" \| "middle" \| "bottom", "wrap": true, "line": 1.2, "outline": 0–0.3 (of the size), "outline_colour", "shadow": {"offset": [x, y], "blur", "colour"} \| null, "visible"} \| {"type": "box", "rect", "colour", "radius": 0–0.5}, …]}`: a title's layers, bottom to top, as a whole; `GET /api/v1/fonts` lists the font families |
| `add_input` (a list) | `{"list": {"items": [path \| {"path", "seconds"}, …] \| "folder": dir, "seconds": 5, "loop": true, "shuffle": false}, "label": …}` → `{"id": n}`: videos, sound files and pictures one after another as one input (§S.18) |
| `list` | `{"id": n, "action": "next" \| "previous" \| "play" \| "pause" \| "restart" \| "goto", "n": place from 0}` |
| `list_items` | `{"id": n, "items": [path \| {"path", "seconds"}, …]}`: replaces a list's files (the one playing goes on) |
| `filters` | `{"id": n, "filters": [{"type": "chroma_key", "colour": "#00ff00", "similarity": 0.4, "smoothness": 0.08, "spill": 0.1} \| {"type": "colour_key", "colour", "similarity", "smoothness"} \| {"type": "luma_key", "min", "min_smooth", "max", "max_smooth"} \| {"type": "correct", "brightness", "contrast", "gamma", "saturation", "hue", "opacity"} \| {"type": "lut", "file": "….cube", "amount"} \| {"type": "sharpen", "amount"} \| {"type": "mask", "file": picture, "invert"} \| {"type": "scroll", "x", "y"}, …]}` (each with `"on"`): an input's filters, in order, as a whole (§S.17); kept by label |
| `title_fields` | `{"id": n, "fields": {"name": "value", …}}`: some of a title's fields, live (others kept) |
| `title_field_add` | `{"id": n, "name": "home", "by": 1}` → `{"value": "3"}`: a number added to a field (a score) |
| `add_input` (a stream) | `{"stream": "srt://host:9000" \| "srt://0.0.0.0:9000?mode=listener" \| "rtmp://0.0.0.0:1935/live/KEY" (a server OBS pushes to) \| "rtsp://user:pass@camera/stream" \| "https://…/index.m3u8", "label": …}` → `{"id": n}`: a live stream as an input (§S.22); reconnects by itself |
| `add_input` (cameras) | `{"ptz": {"camera": id, "preset": 1–16}}` (a preset as an input) or `{"virtual": {"input": id, "view": {"x": 0.5, "y": 0.5, "zoom": 2}}}` (a shot of an input) → `{"id": n}` (§S.21) |
| `camera` | `{"id": n, "control": "network" \| {"visca": "ip" \| "tcp" \| "udp", "address": "10.0.0.20"} \| null}`: how a camera input is moved; kept by label |
| `ptz` | `{"id": camera or preset input, "move": "drive" \| "zoom" \| "stop" \| "home" \| "autofocus" \| "recall" \| "store", "pan": −1–1, "tilt": −1–1, "speed": …, "preset": 1–16}` |
| `virtual_view` | `{"id": n, "view": {"x", "y", "zoom"}, "seconds": 1.5}`: a shot moved, eased |
| `title_row` | `{"id": n, "action": "next" \| "previous" \| "goto" \| "play" \| "pause" \| "read", "n": row}`: a title's data row (§S.20); a title's data is set with `title` as `"data": {"from": file \| address \| Google Sheets link, "format": "auto", "every": 30, "advance": 0}` |
| `title_template` | `{"name": …, "title": {"layers": […], "fields": {…}}}` keeps a template (`"title": null` removes it); `GET /api/v1/title_templates` lists them |
| `timer` | `{"name": …, "action": "set" \| "start" \| "stop" \| "reset" \| "remove", "mode": "down" \| "up" \| "until", "seconds": 300, "until": "10:30"}`: a timer shown in titles as `{timer:NAME}` (§S.19); `GET /api/v1/input.jpg?id=N&w=960` is an input's picture (a designer's preview) |
| `slideshow` | `{"id": n, "action": "next" \| "previous" \| "play" \| "pause" \| "goto", "n": place from 0}` |
| `scene_items` | `{"id": scene, "items": [{"id": layer id (kept; none: new), "name", "volunteer": false, "fade": 0.5, "global": global layer id \| null, "source": input id \| {"colour": "#rrggbb"}, "rect": [x, y, w, h], "crop": [x0, y0, x1, y1], "rotation": deg, "opacity": 0–1, "blend": "normal" \| "add" \| "multiply" \| "screen", "border": px, "border_colour": "#rrggbb", "radius": 0–0.5 (of the shorter side), "key": "alpha" \| "none", "visible": true, "volume_db", "muted"}, …]}`: a scene's layers, bottom to top, as a whole, at most 16 (fractions of the frame); `scene_item_sound` `{"id": scene, "layer": id \| "item": place, "volume_db", "muted"}` |
| `media` | `{"id": n, "action": "play" \| "pause" \| "seek" \| "restart", "position": s}` (file inputs; restart: from the start, playing) |
| `remove_input`, `rename_input` | `{"id": n}`, `{"id": n, "label": …}` |
| `audio_offset` | `{"id": n, "ms": 0–500}`: that input's sound sync offset — while its sound is the stream's it is delayed this much on top of the stream's delay, to meet its picture; kept by label (`audio-offsets.json`) and in setups |
| `preview`, `program` | `{"id": scene}` (only scenes; an input is refused) |
| `scene_layer_visible` | `{"scene": id or label, "layer": layer id, "on": true}`: shows or hides one layer, fading over its `fade`; live, not undone |
| `global_layer` | `{"layer": {layer object; "id" to change one}, "add_to_scenes": false}` → `{"id": n}`; `global_layer_remove` `{"id": n}` (§S.5) |
| `keep_active` | `{"id": n, "on": true}`: received while not in use (kept by label) |
| `multiview_layout` | `{"tiles": [{"kind": "program" \| "preview" \| "input" \| "scene" \| "mix", "id": n, "rect": [x, y, w, h], "label": true, "tally": true}, …] \| null}` (null: the default); `multiview` `{"output": true}` sends it or not |
| `cut`, `auto`, `ftb` | `{}` |
| `tbar` | `{"position": 0.0–1.0}` (1.0 completes) |
| `transition` | `{"kind": "mix" \| "fade" \| "cut" \| "wipe-left\|right\|up\|down\|circle", "duration": s, "soft": 0–0.2}`; `{"kind": "luma", "pattern": id, "soft": 0–1, "invert": bool}` wipes by a greyscale picture input; `"stinger-1"`…`"stinger-8"` runs that stinger |
| `stinger` | `{"n": 1–8, "input": id, "duration": s, "cut": s}` (`"input": null` empties it): a clip (a file with alpha) keyed over the bus for `duration`, program and preview swapping at `cut`; a file clip plays from its start each time |
| `format` | `{"format": "1080p60"}` (`1080p30`, `1080p60`, `2160p30`, `2160p60`, `WxH@FPS`): the program format, refused while anything streams or records; kept (`format.json`) for the next start without `--format`; under the IDN Node the switcher starts again in it at once (`{"restart": true}`), run by hand `{"restart_needed": true}` and the state's `format_next` |
| `add_mix` | `{"name": …, "format": "1080x1920"}` (or `"width"`, `"height"`) → `{"id": n}`: an independent mix, its own source on the network (at most 4) |
| `remove_mix` | `{"mix": id}` |
| `mix_program`, `mix_preview` | `{"mix": id, "id": scene}` |
| `mix_cut`, `mix_auto` | `{"mix": id}` |
| `mix_transition` | `{"mix": id, "kind": …, "duration": s, …}` as `transition` (no stingers) |
| `audio` | `{"source": "program" \| "routed" \| "none" \| {"input": id} \| {"card": DEVICE or null, "first": 1…}, "gain_db": dB, "delay_ms": ms, "denoise": false}` (a card: its inputs `first` and the next as left and right, a mono card's one on both; `GET /api/v1/cards` lists them; `denoise`: noise suppression on the stream's sound, built in — adds 10 ms, kept in sync) |
| `caption`, `captions` | `{"text": "Please stand"}`; `{"on": true, "clear_after": 6}` (§S.29) |
| `setup_save`, `setup_load`, `setup_delete` | `{"name": "Sunday"}` (§S.28): a setup keeps the inputs, scenes, titles, slideshows, filters, cameras' control, the inputs' sound offsets, mixes (with their transitions), timers and the show, the global layers, the multiview, the inputs kept active, and the program's transition and the 8 stingers, inputs by label; a setup saved before these had them leaves them as they are |
| `undo`, `redo` | `{}` → `{"done": "renaming Cam 2"}` |
| `cue_go`, `cue_back`, `cue_standby`, `cue_fire` | `{}`; `{"n": 3}` (§S.27) |
| `show_set`, `show_new`, `show_save`, `show_load`, `show_delete` | `{"show": {"name", "cues": [{"name", "notes", "kind", "length", "follow", "actions": [{"do": "switcher", "command": "program", "body": {"input": "Pulpit"}}, {"do": "mixer", "url": "http://127.0.0.1:29214", "snapshot": "Speech"}, {"do": "http", "method": "POST", "url": "…", "body": "…"}, {"do": "osc", "to": "host:port", "path": "/go", "args": [1]}]}]}}`; `{"name": "Sunday"}` |
| `pco_login`, `pco_logout`, `pco_plans`, `pco_import` | `{"app_id", "secret"}`; `{}`; `{}` → `{"plans": […]}`; `{"service_type", "plan"}` (Planning Center) |
| `monitor` | `{"preview": false, "solo": [ids], "volume_db": 0, "mute": false, "card": DEVICE ("" the default) or null}`, each optional (§S.24); `monitor_solo` `{"input": id, "on": true}`; `GET /api/v1/meters` the levels; `GET /api/v1/monitor/listen?from=N` the monitor's sound (16-bit stereo at the switcher's rate, its `X-IDN-Rate` header, after an 8-byte frame number) |
| `stream_add` | `{"url": "rtmp://…" \| "rtmps://…" \| "srt://…"}` or `{"file": "….mp4" \| "….mkv", "split_s": 0, "split_mb": 0}` → `{"id": n}` (a file recording with `split_s`/`split_mb` set is cut into parts `name-2.mp4`, `name-3.mp4`, … every N seconds and/or MB, each starting at a keyframe) `"tracks": [input ids]` on a file records the program with extra sound tracks — track 1 the stream's sound, then each input's own sound, lined up (its own encoder; up to 7); `"mix": id` streams or records that mix instead of the program |
| `stream_pause` | `{"id": n, "paused": true \| false}`: pauses or resumes a recording (the program's, an ISO or a tracks one) — one file, the paused part simply not in it |
| `stream_remove` | `{"id": n}` |
| `iso_add` | `{"input": id, "file": "cam1.mp4" \| "….mkv", "split_s": 0, "split_mb": 0}` → `{"id": n}`: records one input on its own (ISO) beside the program — its own encoder and file, a picture every program frame with the program's stamps so it lines up in an editor, with its own sound; listed with the streams (`"iso": input`), stopped with `stream_remove` |

Companion (Stream Deck) can send these with its generic HTTP module.

## Performance

The program is composited on the GPU where there is one (Vulkan, DX12 or Metal through
wgpu; `compositor` in the state says which), else, or with `--cpu`, on the CPU. They make
the same picture (to the bit, but for scaled pictures and a circle wipe's soft edge,
within a step). On the GPU each frame comes out a frame (one period) later, its sound
with it: the GPU makes one frame while the next one's pictures go up and the last one
comes down. A mix plus two full-frame keys and two pictures in picture, every picture
new each frame, on the lab host (Ryzen 7 6800H with its Radeon 680M):

| | 1080p | 2160p |
| --- | --- | --- |
| CPU (8 threads) | 8.7 ms | 32 ms |
| GPU | 4.0 ms | 11.7 ms (worst 15) |

so 2160p60 (16.7 ms a frame) needs the GPU, and 1080p60 and 2160p30 run on either:
`cargo run --release -p idn-switcher --example compose_bench`.

## Next

Hardware encoding of the streams from the GPU's picture, and the program sent to IDN
inputs without bringing it back to the CPU.

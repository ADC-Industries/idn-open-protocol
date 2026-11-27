# IDN Media as a server and libraries: the API reference

For developers of the program that is IDN Media's UI and anyone driving IDN
Media from code. IDN Media has no user interface of its own but each computer's IDN Node
control panel: its switcher, sound desk, Site Manager, IDN Data, listening and viewing
are engines with APIs. A host either **embeds** them (Rust libraries, in its own process)
or **talks to** them (HTTP/JSON, a push channel, MQTT), on the same computer or across
the network.

**API version: v1** (`/api/v1/…`). Additions keep v1; a change that breaks a client gets
v2 beside it. Every surface answers `GET /api/v1/schema` with the JSON schema of its
commands (generated from the Rust types, so it always matches the build).

## 1. The surfaces

| Surface | Library (embed) | HTTP (this computer / network) | Push |
|---|---|---|---|
| IDN Node (each computer) | `idn_node::host::{start, Config}` → `Handle`; clients: `idn_open_protocol::node::Node::connect_to` | panel API 127.0.0.1:29200 / 29201 (LAN password, or site tokens in a site) | `GET /api/v1/events` |
| Session agent (each desktop session) | `idn_open_protocol::node::session::{Agent, Config}` | — (`idn-session`, `idn session`) | callbacks |
| Site Manager (one a site) | `idn_site_manager::host::{start, Config}` → `Handle` (`run(user, SiteCommand)`) | 29202 (HTTPS, site password / users) | `GET /api/v1/events` |
| Switcher | `idn_switcher::service::{Service, ServiceConfig}` (`command`, `run(Command)`, `state`, `handle`, `handle_as`) | 127.0.0.1:29210 / 29211 (`--lan-password`, or site tokens in a site) | `GET /api/v1/events` |
| Sound desk | `idn_mixer::{Console, ConsoleConfig}` (`command`, `command_as`, `run(Command)`, `state`, `meters`) | 127.0.0.1:29214 / 29215 (as the switcher); the network desk 29219 / 29220 | `GET /api/v1/events` |
| IDN Data | — (program `idn data`) | 127.0.0.1:29212 (the Node's one-time token, or a site token) / 29221 (site tokens) | long poll |
| Display service | — (program `idn display --service`) | 127.0.0.1:29222 / 29223 (site tokens only) | `GET /api/v1/events` |
| Listening on phones | `idn_ffmpeg::whep::Viewer` (`ViewerConfig::listen`) | 29216 (WHEP) | — |
| In-ear mixes on phones (the musician's app) | `idn_iem::receiver::Receiver` (C ABI `include/idn_iem.h`; `media/idn-iem/README.md`) | the desk's 29214 / 29215 (`GET /api/v1/iem`, the push channel) + UDP 29224, multicast 29253 | the desk's `GET /api/v1/events` |
| Watching in a browser | `idn_ffmpeg::whep::Viewer` | 29218 (WHEP) | — |
| IDN Voice | `idn_voice::Voice` (the client protocol: [VOICE-CLIENT.md](VOICE-CLIENT.md)) | the Site Manager's port + 1 (`/voice/api/…`, `/voice/ws`) | its WebSocket |

The Node runs the others as its **services** (child processes it supervises): tick them in
its settings (`apps.switcher`, `apps.mixer`, `apps.display`, `sitemanager.on`, `listen.*`, `data.on`, …;
`idn-node set …`). A host that embeds the Node gets the same: `host::start(Config {
programs_dir, place, … })`, then the services it names run from IDN's programs folder.

- Node `Config::service`: true only when the host runs as the machine's `idn-node` service (normally `place.system`); a hosted Node installs updates with the programs folder's `idn-node`, and refuses them when there is none.
- Node `Config::diagnostics`: false keeps the host's own stderr and panic hook (true: copied into the Node's log, crash reports written).
- Site Manager `Config::host`: `Some((spki_hex, name))`, this machine for its certificate after a restore; `None`: `IDN_SITE_HOST_SPKI` / `IDN_SITE_HOST_NAME`.

## 2. Conventions

- **JSON** both ways. A refusal or failure: a 4xx/5xx status and `{"ok": false, "error":
  "…"}` (the error is a sentence for a person).
- **Commands:** `POST /api/v1/<command>` with a JSON body; answered `{"ok": true, …}` (what
  the command answers beside `ok`). The same commands over the push channel and, in a
  library, as `run(Command::…)`.
- **State:** `GET /api/v1/state` (the switcher, the desk, IDN Data, the display service,
  the Site Manager, the Node panel: `?after=VERSION` waits up to 20 s for a newer one; the
  Site Manager's and the Node's is their `GET /api/v1/status` with a `version`). Every
  state carries a `version` that grows with every change.
- **State schemas (2026-11-19):** `GET /api/v1/state_schema` (the desk, the switcher,
  the display service, the Site Manager (also its `status_schema`), the Node
  (`idn_node::panel_state::NodeStatus`)): the state's JSON
  schema. The desk's, the display service's and the Site Manager's states are made from
  these types (each key typed, deeper parts JSON where they are the engine's own); the
  switcher's is typed down to its inputs and their sources, scenes' layers, buses,
  transitions, mixes, streams and problems, and those parts are made as the types
  (`idn_switcher::service::state_schema::SwitcherState`, which reads a state with
  `serde_json::from_value` and skips keys added later). Tests check every key given is
  declared.
- **One command contract:** the desk checks every command against its typed form
  (`GET /api/v1/schema`) before it runs, from any way in (HTTP, the push channel, OSC, a
  host): an unknown name is `no such command "x"`, a wrong field `cmd: …` (400). An aux
  bus's number may be text (`"bus": "5"`), as a form sends it.
- **Changes need a header** a cross-site page can't send: `X-IDN-Switcher: 1`,
  `X-IDN-Mixer: 1`, `X-IDN-Display: 1`, `X-IDN-Node: 1` (Node panel and Site Manager). A browser that sends
  `Origin` must be on the API's own origin.
- **Logins:** on this computer the switcher and the desk need none; the Node panel takes a
  session from its one-time link (`X-IDN-Session`, or `?session=` for a WebSocket); on the
  network a caller shows a **site token** (below), or, on a machine not in a site, logs in
  with `POST /api/v1/login {"password"}` (the Site Manager also `{"user", "password"}`)
  and keeps its cookie (12 hours).
- **Certificates (TLS on the network ports):** on a machine in a site, the network ports
  (Node LAN panel 29201, switcher 29211, desk 29215 / 29220, IDN Data 29221, display
  service 29223) present the machine's certificate signed by the **site CA**, with the CA
  after it (spec idn-extensions.md §E.10.11). A client pins the site CA alone: get it
  from the Site Manager, `GET /api/v1/site_ca` (PEM, no login; its key's SHA-256 is the
  status's `site.ca`), or `host::Handle::authority().ca_pem()` in-process. Verify the
  chain to it as usual; the certificate names the machine's host name (and `.local`),
  `localhost`, its LAN IPv4 addresses, 127.0.0.1 and ::1, so a client reaching it by
  address checks the address (an IP SAN) and by name the name. It also carries
  `urn:idn:lan:<machine fingerprint>` for a client that wants to know which machine.
  The key is Ed25519 (TLS 1.3): rustls and other TLS libraries take it, browsers mostly
  don't, so a client offering no Ed25519 (a browser) is given the program's own
  self-signed certificate instead. Renewed by itself (7 days' life, renewed at half);
  joining, leaving and renewals apply without a restart; a revoked machine's lapses within
  7 days. A machine **not in a site** presents the program's self-signed certificate: pin
  the fingerprint the program prints. (A phone pinning a desk's own certificate stops
  connecting once that machine joins a site: pin the site CA instead.) The Site
  Manager's own port (29202) stays self-signed.
- **Comparison codes** (a machine joining the site, two computers linking): six digits,
  sent and kept as `"482913"`; show them as `482 913` and never call them a PIN (they are
  compared by two people, not typed in elsewhere). Where one is sent back (the Node's
  `pairs/answer` and `pairs/confirm` `code`), it is taken with or without the space. The
  Site Manager's pending joins (`status`) and the Node's `status` (`code` while pending or
  to confirm) carry it; the library: `trust::show_code`, `trust::same_code`.

### The push channel

`GET /api/v1/events`, a WebSocket (same login as the API). One JSON message per frame.

- From the server: `{"type": "state", "version", "state"}` at once and on every change
  (every server: the switcher, the desk, the display service, and since 2026-11-19 the
  Site Manager and the Node, whose state is their status; they sent `{"type": "status",
  "status"}` each second before); after `{"subscribe": [names]}`: `{"type": "meters", …}`
  (switcher every 100 ms, desk every 50 ms), the Site Manager's `network_devices`,
  `levels`, `matrix`, `reports` (`{"type": name, name: …}`), and everywhere `audit`
  (`{"type": "audit", "entries"}`: the lines written from then on); `{"unsubscribe":
  [names]}` stops them. `{"type": "error", "error"}` when a session ended. Typed failure
  events as they happen, unasked (`{"type": "event", "event", "level", "text", …}`; see
  "Events and problems" below).
- From the client: `{"id": 1, "cmd": "cut", "body": {…}}` → `{"type": "reply", "id": 1,
  "ok": true, "result": {…}}` or `{…, "ok": false, "error"}`, in the order sent (the
  reply has no `id` when the message had none). A command refused (403) is also followed
  by a `denied` event on that channel only.

### Events and problems

Every engine says what goes wrong three ways (idn-node.md §N.41): typed events on its push
channel, `problems` in its state (what is wrong now), and lines in the Site Manager's
log (through the Node; the same event about the same thing once a minute at most, with
`repeats`). Not on MQTT.

- An event: `{"type": "event", "event", "level": "error"|"warning"|"info"|"ok", "text",
  "at", …fields}`. A problem ending: the same event with `level` `ok` and `over: true`
  (or `recovered`, `desk_link`).
- A problem: `{"problem", "level", "text", …fields}` in `problems`.
- `GET /api/v1/events/recent?n=` (switcher, desk, Site Manager): the newest events first
  (the desk's: not for a bus's or an IEM mix's token).
- The Site Manager's `GET reports` `alerts[]` have `event` and `detail` for typed ones.
- Library: `idn_open_protocol::web::problems` (`Events`, `LogForward`, `event`,
  `problem`, `denied`, `loss_cause`), `Node::event`.

| Engine | Event | Fields |
|---|---|---|
| all | `denied` | cmd, error (to the refused caller's channel only) |
| switcher | `lost` | input, label, reason, cause (`other_site`, `refused`, `not_found`, `unreachable`, `ended`, `other`) |
| switcher | `other_site` | input, label, reason |
| switcher | `reconnecting` | input, label, attempt, reason |
| switcher | `recovered` | input, label |
| switcher | `gone` | input, source (`over` when listed again) |
| switcher | `renamed` | input, source, to |
| switcher | `format_mismatch` | input, label, size, fps, program {size, fps}, rate |
| switcher | `camera_unreachable` | input, label, error (`over`) |
| switcher | `desk_link_lost`, `desk_link` | output, desk |
| switcher | `overloaded` | part (`compositor`, `encoder`), frame_ms, late_frames (`over`) |
| switcher | `stream_auth_expired` | destination, name, error |
| switcher | `stream_failed` | destination, name, error |
| switcher | `dropped_frames` | destination, name, dropped, skipped_pictures, dropped_total, skipped_total |
| display | `lost` | screen, feed, reason, cause |
| display | `other_site` | screen, feed, reason |
| display | `reconnecting` | screen, feed, attempt, reason |
| display | `fallback` | screen, feed, show, reason, gave_up |
| display | `recovered` | screen, feed, after_s |
| display | `decoder_trouble` | screen, feed, failed, failed_total, decode, reason (pictures failed to decode, or GPU → CPU decoding, while playing) |
| display | `playing`, `format`, `routed`, `screens`, `stopped` | as §9 |
| desk | `lost` | source, label, inputs, reason, cause (`not_found`, `unreachable`) |
| desk | `recovered` | source, label, inputs |
| desk | `failover` | channel, name, input, backup (`over`: back on its first input) |
| desk | `plugin_failed` | strip (`host`: the plugin host restarted, with restarts), error (`over`) |
| desk | `recorder_failed` | folder, path, error |
| desk | `disk_low` | folder, path, free_gb, free_bytes (`over`) |
| desk | `x32_unreachable` | address, error (`over`) |
| desk | `card_lost` | part (`input`, `output`, `network`), device, error, errors (`over`) |
| desk | `overloaded` | part `dsp`, block_ms, period_ms, dsp_load_pct, late_blocks, slow_blocks (`over`) |
| desk | `iem_phone_dropped` | mix, peer, client, user, reason (`timeout`, `token`) |
| listening | `listener_dropped` | channel, peer, reason (on the Node's channel and log) |
| Node | `join_denied`, `revoked`, `join_code_expired`, `join_code_burned`, `site_error` | error |
| Node | `join_code_wrong` | tries_left (again at each wrong try) |
| Node | `already_in_site` | server (a join code typed on a machine already in a site: refused) |
| Node | `site_manager_unreachable` | for_s |
| Node | `two_sites` | sites |
| Node | `clock_unlocked` | state (a lock lost; `over` when locked again) |
| Node | any program's event | its fields, `app` |
| Site Manager | `offline`, `service_failed`, `update_failed`, `update_rolled_back`, `version_mismatch` | fingerprint, machine, service/line, version/error, version/build/site_manager (`over`) |
| Site Manager | `clock_unlocked`, `clock_offset`, `link_down`, `link_slow`, `stream_loss` | fingerprint, link / stream (`over`) |
| Site Manager | `join_denied`, `join_refused`, `device_revoked` | machine, fingerprint |
| Site Manager | `join_code_wrong` | machine, address, tries_left |
| Site Manager | `join_code_burned` | id, why |
| Site Manager | `join_code_expired` | id |
| Site Manager | any machine's event | its fields, fingerprint, app |

`problems` per engine: switcher `lost`, `format_mismatch`, `camera_unreachable`,
`desk_link_lost`, `gone`, `renamed`, `overloaded`, `stream_auth_expired`,
`stream_failed`; display `lost`/`gave_up`, `screen_failed`; desk `lost`, `failover`,
`plugin_failed`, `recorder_failed`, `disk_low`, `x32_unreachable`, `card_lost`,
`overloaded` (with `for_s`); Node (`status.problems`) as its events above; Site Manager
`status.attention` (below).

The desk sends its typed events only to callers that see the whole desk (not a bus's or
an IEM mix's token); `denied` goes to any caller refused (read-only, outside its scope,
the desk locked). A desk's push channel sends a state only when what that caller sees
changed, its `version` aside: an IEM mix's phone isn't sent anything for a FOH fader
(spec idn-mixer.md §M.27).

### Per-control sequence numbers

A change may name its control and a number that only grows for it: over the push channel
`"control": "ch1.fader", "seq": 57` beside `cmd`; over HTTP the headers `X-IDN-Client`
(the client's own id), `X-IDN-Control`, `X-IDN-Seq`. A change older than the last applied
for that control (a late arrival) is answered `"skipped": true` and not applied. Without
them, changes apply in arrival order.

- Over the push channel every server honours them, counted per connection. The HTTP
  headers are honoured by every server: the switcher, the desk, the display service, the
  Site Manager and the Node panel.
- A refused change (a locked desk, an error) does not use up its number: the same number
  may be sent again.
- A control not changed for 30 seconds is forgotten, so a client that starts again may
  count from 1.

### Who may do what: tokens, scopes, the audit

(idn-node.md §N.31.5, §N.31.6.) The **same computer** (127.0.0.1) stays open: everything,
written in the audit as `local`. **Callers on the network** show a token the Site
Manager signed with the site's own key; the engines check it with the site's
certificate their IDN Node holds. A machine not in a site keeps its LAN password.

- **Getting one:** `POST /api/v1/token {"aud": {"engine", "machine"?}, "scope", "ttl_s"?}`
  on the Site Manager, logged in (`SiteCommand::Token`), answers `{ok, token, scope, aud,
  iat, exp, jti}`. `engine`: `desk`, `switcher`, `display`, `data`, `node`, or
  `app:<name>` for a hosting program's own API (`app:myapp`; it checks them with
  `host::Handle::verify_token(token, "myapp")`: the site's signature, the audience, the
  life, the user's tokens not withdrawn; issue them for any machine. A named API key it
  checks has its `last_used` noted, at most once a minute, as the Nodes report keys
  used elsewhere. A host makes and revokes keys through its handle too:
  `run_from("admin", &caller, &SiteCommand::ApiKey { name, aud, scope })`, answered once
  with `{id, key, …}`, and `SiteCommand::ApiKeyRevoke { id }`);
  `machine`: a machine's fingerprint, none or `*` for any of the site's; `ttl_s` up to
  43200 (12 hours, the default).
  - By role: a viewer gets `read`; an operator anything but `control` of the Node or of
    a program's engine (`app:*`, e.g. `app:myapp`; from v0.3.4-Beta11: it
    amounted to the host's site administrator); an administrator anything.
  - A host in-process: `host::Handle::issue_token(user, &TokenAud, scope, ttl)` (or
    `issue_token_from(user, &Caller, …)`, audited with the caller's address and client)
    for its own users (no role checked: the host asks for the scope its user may have), and
    `Handle::withdraw_tokens(user)`. Issue them under a name per user or client, not
    `admin`: one person's tokens can then be withdrawn alone, and setting the site
    password withdraws every `admin` token.
- **Scopes:**
  - `read`: GETs, the state, meters, the push channel's topics, the audit; every change
    refused, 403 `read-only`.
  - `control`: everything.
  - `bus:<bus>` (the desk; a streaming program's bus): one bus, `bus:main`,
    `bus:aux:N`, `bus:group:N`, `bus:matrix:N`, or by name `bus:name:NAME` / `bus:NAME`
    (`bus:stream`: the bus named "Stream", found each time the token is used). It
    changes the channels' sends into that bus and the bus's master, nothing else (403
    `outside this token's bus (…)`), and sees that bus alone. Details in §4 below.
    Another engine treats it as `read` (changes: 403 "…not supported yet").
  - `iem:<mix id>` (the desk; a musician's phone): `iem_mix` on that mix alone (also
    while the desk is locked); every other change 403 `outside this token's IEM mix
    (…)`. It reads the IEM channels and its mix (`state`, the push channel), the IEM
    channels' meters, `controls`, `schema` and `GET /api/v1/iem`; other pages 403; no
    audit. It may take the IEM flow and talk into its mix (idn-iem.md). Details in §4.
    Another engine treats it as `read`.
- **Showing one:** `Authorization: Bearer idn1.…` on any request. On the push channel the
  same header, or `?token=` on the upgrade, or a first message `{"token": "idn1.…"}`
  (answered `{"type": "token", "ok", "user", "scope"}`; nothing is sent before it, and
  the channel ends after 10 s without one). The channel ends when its token expires or is
  withdrawn.
- **Refused (401):** a forged token, another site's, another engine's or machine's, one
  expired (60 s of clock difference allowed), one issued before its user's "not before".
  No credentials at all: 403 `not logged in`, as before.
- **Withdrawn:** removing a user, or changing their role or password (the site password:
  `admin`), sets their "not before" now; every machine gets it as the site option
  `tokens.not_before.<id>` and refuses their earlier tokens at once.
- **Format:** `idn1.<base64url(claims)>.<base64url(Ed25519 signature)>`; library:
  `idn_open_protocol::trust::token` (`Claims`, `Scope`, `sign`, `verify`, `peek`),
  `web::TokenCheck`, `web::Who`, `web::PasswordGate::with_tokens`/`tokens_only`.
- **The audit (every engine):** `audit.jsonl` in its state folder, one line per change in
  the Site Manager's format: `{at, user, peer, client, action, ok, error, detail}`
  (`detail` the body with secrets cut, `client` the `X-IDN-Client` header). `user` is
  `local`, `lan-password`, the token's user, `osc ADDR`, `surface`, `mqtt` or `cue`
  (the Site Manager: the user logged in). `action` is the command's name (`cut`,
  `route`; the Site Manager's were its paths, `/api/v1/route`, until 2026-11-19).
  Refused changes are written too. A control moved continuously by one caller (a fader)
  is one line with its last value and `repeats`. Kept two years or 50,000 lines.
  `GET /api/v1/audit?n=` (newest first, default 200, up to 5000); on the push channel
  `{"subscribe": ["audit"]}` → `{"type": "audit", "entries": […]}`. Library:
  `Service::audit()`, `Console::audit()`, `web::audit::Audit`.
- **Embedding:** the switcher's `Service::handle_as(request, who)`, `command_as(who, …)`,
  `run_as(who, Command)`, `events_as(ws, who)`; the desk's `Console::command_as`,
  `events_as`, `command_answer_as`, and for a bus or IEM token `permits`, `state_for`,
  `meters_for`, `controls_for`; `Service::token_check()` / `Console::token_check()` for a network
  front of the host's own (`web::lan::front`).
- **OSC and the desk's Mackie surface** stay full control on the LAN (OSC limited by
  `--osc-allow`, no token), written in the audit as `osc ADDR` and `surface`.

## 3. The switcher

Library: `idn_switcher::service::Service::start(ServiceConfig { switcher, dir, inputs,
node, rate, machine_rate })` (one a process), then `command(name, &json)`,
`run(Command::…)` and its typed helpers, `state()`, `handle(request)` (the whole HTTP API
in-process), `events(websocket)`, `close(within)`. The engine alone:
`idn_switcher::Switcher` (no saving, shows, players or streams).

HTTP `GET`: `state`, `schema`, `meters`, `multiview.jpg`, `input.jpg?id&w`,
`monitor/listen?from=N` (8-byte first frame number, then 16-bit stereo PCM at
`X-IDN-Rate`), `cards`, `addresses`, `screens`, `fonts`, `title_templates`,
`filters_schema`, `events`.
Commands: see `GET /api/v1/schema` (`Command`): the buses and transitions, inputs and
their editors (titles, scenes and their layers, global layers, slideshows, lists, PTZ,
virtual shots), sound and monitor, the multiview, screens, captions, the show and
Planning Center, setups and undo, mixes, streams and recordings, MQTT, starters.

The OBS model (spec §S.3, §S.5, §S.7, §S.12):
- **Only scenes go on the buses.** `program`, `preview`, `cut`/`auto`/`tbar` (on them),
  `mix_program` and `mix_preview` take a scene; an input is refused ("put it in a
  scene"). The first scenes added fill program and preview; inputs fill nothing. Saved
  data (a cue, a saved show, an MQTT trigger, a mix in `mixes.json`) that puts an input
  on a bus fails as the command does (a saved mix's bus stays empty).
- **Layers.** A scene's `items` are its layers, bottom to top, at most 16, each with a
  stable `id`, `name`, `volunteer` (volunteers may toggle it; a flag for the UI), `fade`
  (seconds, default 0.5) and `global` (a global layer's copy, or null). `scene_items {id,
  items}` still replaces the list; a layer sent with an `id` the scene has keeps it, one
  without gets a new id. `scene_layer_visible {scene, layer, on}` shows or hides one layer
  by id, fading over its `fade`; live (not an undo step; on MQTT). `scene_item_sound`
  takes `layer` (the id) or `item` (the place).
- A scene of one full-frame layer passes its input through: the bus shows the input's
  own picture, fitted as before (no drawing, no added latency).
- **Global layers** (they replace the 8 overlays: `overlay*` commands and the state's
  `overlays` are gone). `global_layer {layer, add_to_scenes?}` defines one (a layer object;
  with an existing `id`, changes it) and answers its `id`; it is added to every scene made
  from then on (as `layer.visible` says), with `add_to_scenes: true` also on top of every
  scene there is. A change reaches every scene's copy, which keeps its place and
  visibility. `global_layer_remove {id}` removes it and its copies. State
  `global_layers`. At most 16. Undoable.
- **No input limit.** An input not in use doesn't receive: in use is in a scene (shown or
  hidden layers) on program, preview or a mix, on the multiview while it is drawn (at its
  tile's size), a stinger or luma pattern, recorded, heard, soloed, a network-controlled
  camera, or `keep_active {id, on}` (kept by label). Unused for 10 s, it disconnects.
  State `inputs[].active` (receiving now) and `inputs[].keep_active`; `inputs[].program`
  and `preview` are its tally (shown there, through a scene too).
- **Multiview.** `multiview_layout {tiles}` sets a layout: tiles `{kind: program|preview|
  input|scene|mix, id, rect: [x, y, w, h] (fractions), label, tally}` in drawing order, at
  most 128; `tiles: null` the default (preview and program on top, every input and scene
  below, growing). `multiview {output}` sends it as `NAME (Multiview)` or stops, at run
  time. Scenes are drawn at their tile's size. State `multiview {tiles, output, custom}`.
  Kept (`multiview.json`, by label).

What a UI no longer works out for itself:
- `title_templates`: the built-in styles (`lower`, `two`, `caption`, `centre`, `corner`,
  `clock`, `count`, `score`; `"built_in": true`, `label`, `about`) with the saved ones (a
  saved one of the same name wins). `add_input {template, text?, label?}` makes a title
  from one (label from `text`, cut at 24 characters; text needed but for clock/count/score).
- A title referring to `{timer:countdown}` makes a 300 s "countdown" timer if none.
- `filters_schema`: `{filters: [{type, name, settings: [{key, kind: number|colour|file|
  check, default, min, max, step, about}]}], max}`, the parser's own ranges and defaults.
- `slide {action, n?}`: the slideshow or list on program (in its scene), else preview,
  else the first.
- `audio_offset {id, ms}` is kept by the input's label and comes back at start and when an
  input of that label is added (or brought back by undo).
- Setups (`setup_save`/`setup_load {name}`) also keep the inputs' sound offsets, the
  program's transition (a luma wipe's pattern by label), the 8 stingers, the global
  layers, the multiview and the inputs kept active, inputs by label; `setup_load` applies
  them before it answers (the state after it shows them). A setup saved before these
  leaves them as they are. Mixes keep their transition's wipe edge and luma pattern too.
- State: `transition.soft` and `mixes[].transition.soft` (a wipe's edge),
  `mixes[].transition.luma` (as the program's `transition.luma`).
- `iso_add` without `file`: the input's label (`_` for other characters) + `.mp4`.
- `timer` `seconds`: a number or `"m:ss"`/`"h:mm:ss"`; `until`: `"HH:MM"`.
- Slideshows' and lists' `from`: files split at `|`, or one folder (no file extension).
- Title field names with `{` or `}` are refused.
- Cues: `cue_add {cue?, at?}` (answers `n`), `cue_update {n, cue}` (the keys given),
  `cue_delete {n}`, `cue_move {n, to}`; these and `show_set` take `if_version` (refused if
  the show changed) and answer the new `version`; all undoable. Headers can't be fired or
  stood by.
- State: `show.version`; `inputs[].summary` (a title's text, fields filled, 40
  characters); `inputs[].source.virtual.crop`/`to_crop` (`[x, y, w, h]`, the engine's);
  `streams[].kind` (`stream`|`recording`) and `streams[].sent_history` (`[{at, kbps}]`,
  every 2 s, 2 minutes; not on MQTT); `network_sources` without this switcher's outputs.

**Added 2026-11-18.** `add_mix {format, name, follow: true}`: a version of
the program in another shape (its scenes and transitions, centre-cropped; its own bus
commands refused). `destination_save {id?, name, body}` → `{id}` (body: as `stream_add`),
`destination_remove {id}`, `go_live {ids}` → `{started, errors}`; the state's
`destinations: [{id, name, kind, target (no key), codec}]`. `streams[].live_since` (Unix
s) and, for recordings, `disk_free`/`disk_total`. `GET /api/v1/scene.jpg?id=N&w=320`.
`scene_snapshot {id, desk, snapshot}` (inputs' `on_air_snapshot`). `scene_layer_visible
{…, fade}`. `captions {on, layer}`. Layers' `locked`. `cpu_percent`.

## 4. The sound desk

Library: `idn_mixer::Console::start(ConsoleConfig { … })`, then `command(name, &json)`,
`run(Command::…)` and its typed helpers, `state()`, `meters()`, `events(websocket)`; the
program around it (`idn mixer`) adds HTTP. HTTP `GET`: `state`, `schema`, `meters`
(`ch[i]` = [in peak, in RMS, out peak, out RMS, gate, comp, limiter, de-esser gain
reductions], `out[j]` = [peak, RMS], `bus_gr`, `lufs` [M, S, I]), `spectrum?channel`,
`plugins`, `plugin_levels`, `plugin_params`, `routing`, `routing/levels`, `recordings`,
`controls`, `events`. Commands: `GET /api/v1/schema`. Locked (`lock`), only `lock` is taken.

State, for drawing the desk without knowing its layout:
- `strips`: every strip in order (channels, main, aux 1–16, groups, matrices, FX,
  headphones, DCAs), each `{kind, index, label, number, short, command, plugins, meter,
  bus_ext, outputs}`; `command` is the address to change it (`{"cmd": "group", "group": 2}`),
  `meter` where its levels are in `meters` (`{"ch": i}` or `{"out": [a, b], "bus_gr": k}`);
  channels add `lead`, `linked`, `input`, `on_b`; DCAs `members`.
- `desk_sizes` (aux, groups, matrices, fx, dca, mute_groups 8, buttons, eq_bands 8,
  geq_bands 31, …), `matrix_sources` (a matrix's `sources` in order: name, label, bus_strip).
- Snapshots (saved desk states; "scenes" before 2026-11-18): `snapshots` (their names),
  `snapshot` (`{name, changed}`: the one last saved or recalled, and whether the desk has
  changed since); commands `snapshot_save`, `snapshot_recall` (with an optional `scope`),
  `snapshot_delete`; a user button's action `snapshot:NAME`.
- Derived: `buttons_lit`, `any_solo`, `dsp_load_pct`, `latency_ms`, `pending_restart`
  with `restart_for` (`rate`, `block`).
- `GET /api/v1/controls` (`idn_mixer::engine::controls()`): each control's min, max, step,
  default, unit, scale (`lin`, `log`, `db`), choices and grammars (EQ shapes with
  `has_gain`, effects, GEQ bands, output taps and sources, button actions, rates, blocks).
  −144 dB is off (−∞).

A Behringer X32's preamps over Ethernet (spec §M.23.1), with the X32's X-USB card as the
desk's card (`--card-in x32`):
- **Setting** `desk {"x32": {"address": ""|"auto"|"IP[:port]"}}` (or `--x32`). It is kept
  with the desk and is not in snapshots.
- **State**:
  - `preamp[i]`: channel i+1's X32 preamp `{usb, input, headamp, gain_db, phantom,
    invert, x32_channel}`, or null where the hardware has none. Its strip carries it too.
  - `x32`: `{address, endpoint, connected, error, info {model, firmware, name,
    local_preamps, aes50}, usb [{usb, source, label, via, headamp, x32_channel,
    preamp}]}`, or null.
  - A change made on the X32 is a new state version.
- **Command** `preamp {"n", "gain_db"?, "phantom"?, "invert"?}`: gain −12…+60 dB in 0.5 dB
  steps.
  - It is not a channel setting: never saved, snapshotted or copied.
  - Refused while the desk is locked, or when the channel's input carries no X32 preamp.
  - `invert` is the **X32 polarity**: it acts on the X32's own channel strip, not on what
    the desk receives over USB. The channel's own `polarity` changes the desk's sound.
- **OSC**: `/ch/N/preamp/gain|phantom|invert`.

Channel patches: `hpf_on` / `lpf_on` switch a filter at its last frequency (`hpf_last`,
`lpf_last`; first 80 Hz / 12 kHz). A linked pair's second channel keeps its name, inputs
and `safe`; the rest of its patch goes to the pair's first (the DSP plays the pair with
it). `channels` (`{"n": [1, 2], …patch}`) patches several at once, all or none.

One bus for another program (a `bus:` token, spec §M.25; a streaming program's bus):
- **Naming the bus:** `bus:main`, `bus:aux:N` (1–16), `bus:group:N` (1–8),
  `bus:matrix:N` (1–8), `bus:name:NAME` or `bus:NAME`. A name is matched on letters and
  digits, case ignored (`bus:name:live-stream` = "Live Stream"), each time the token is
  used; no bus or two buses by that name: nothing allowed (`error` in its state).
- **It may change:**
  - an aux bus: each channel's send into it, `channel {"n": 3, "sends": [null, null, null,
    null, {"level": -6, "on": true}]}` (only that aux's element; `on`, `level`, `pre`,
    and `pan` into a stereo aux); `channels {"n": [1, 2], …}` alike;
  - a group: `channel {"n", "groups": [null, true]}`; the main bus: a channel's `main`;
    a matrix mix: its own `sources`;
  - the bus itself (`bus {"bus": 5}`, `group`, `matrix`): `fader`, `mute`, `eq_on`, `eq`,
    `comp`, `limiter`, `geq` (not groups).
- **Refused** (403 `outside this token's bus (aux 5): …`, in the audit): everything else
  (channel faders, other buses, the bus's name, plugins, snapshots, the lock, settings,
  the output patch, recording, routing, …).
- **What it reads:** `GET state` → `{version, name, locked, scope, bus {kind, index,
  number, label, color, stereo, pre, command, settings}, channels [{n, name, color, lead,
  linked, active, send}], error}`; `GET meters` → `{ch, bus: [[peak, RMS], [peak, RMS]],
  bus_gr, lufs (main only)}`; `GET controls` → the bus's and a send's limits; `schema`.
  The push channel sends the same state and meters and takes the same commands; no
  `audit` topic. Every other page: 403.
- **`send {"n", "bus", "level"?, "on"?, "pan"?, "pre"?}`**: one channel's send into one
  aux bus without the `sends` list's nulls (a bus token's into its own aux only).

In-ear monitoring (spec §M.26; the wire format for phones: idn-iem.md):
- **Marking:** a channel's `iem: true` (a linked pair as one). The flow carries every
  marked channel after its processing, before its fader and mute (the FOH fader never
  touches it). The desk mixes nothing: each phone mixes the flow itself.
- **The flow:** an IDN audio source `HOST (<desk> IEM)` for wired listeners (labels `Name
  (ch N)`), and an Opus copy for phones, multicast (UDP 29253) and unicast (UDP 29224).
  `GET /api/v1/iem` → `{flow, layout, channels [{id, name, color, pair, side}],
  idn_source, multicast {group, port}, unicast {port, max_phones}, session, codec,
  talkback, latency}`; channel `id` = the desk channel's number.
- **The multicast copy's interface:** `IemConfig::interface` / `--iem-interface IPV4`;
  else the Node's chosen interface, else the one towards the Site Manager, else the
  default route's (a machine on a LAN and an overlay such as wt0 sends on the LAN). The
  state's `iem.multicast.interface` says which.
- **Mixes** (`settings.iem_mixes`, at most 16): `{id, name, master_db, channels {"<id>":
  {level_db, pan, mute, solo}}, talkback}`.
  - `iem_mix_add {"name", "id"?}` → `{"ok": true, "id"}` (id from the name:
    `lead-vocal`);
  - `iem_mix {"id", "name"?, "master_db"?, "channels"?: {"3": {"level_db": -6}, "4":
    null}}`: partial; null takes a channel out; works while locked;
  - `iem_mix_delete {"id"}`.
  The push channel carries every change to the sound person and the musician alike.
- **State** `iem {channels, layout, mixes (with talkback_input, talking),
  talkback_inputs, phones, unicast, max_phones, port, multicast, source, frames,
  packets, …}`; `snapshots_iem`.
- **An `iem:<mix>` token's state:** `{version, name, locked, scope, channels, layout, mix,
  error}`; its meters `{ch}` (the IEM channels' rows in flow order). Its push channel
  gets a state only when this changes (not for every desk change).
- **A phone dropped** (no keepalive within the timeout, or its token withdrawn): an
  `iem_phone_dropped` event `{mix, peer, client, user, reason}`.
- **Snapshots:** `snapshot_save {"name", "include_iem": true}` holds the mixes and marks;
  its recall restores them; otherwise a recall leaves the desk's.
- **Talkback:** each mix owns one of 16 desk inputs after the routed ones (its
  `talkback_input`, named `Talkback <mix>` in `input_names`); patch a channel to it
  (`channel {"n", "source": 65}`). A phone's push-to-talk plays there; silent otherwise.
- **Where each input's sound comes from** (idn-mixer.md §M.28.3): the state's
  `input_sources`, one per desk input, `{input, kind (idn|net|card|talkback|null), label,
  via, via_channel, machine, origin: {kind, device, channel} | null}`: e.g. an IDN route
  from `FOH (Stage box)` channel 5 whose origin is `{network_device, "Stage Box Left",
  "Mic 5"}`, or a network-audio ring input `via` the local device with the device and
  channel its receive channel subscribes to. Typed: `idn_mixer::engine::InputSource`.
- **The phone's side** is IDN's mixing library, `idn_iem::receiver` (Rust) with a C ABI
  for Kotlin and Swift (`media/idn-iem/include/idn_iem.h`): connect with the desk's
  address, a token and the site CA (`Trust::Certificate`, from `GET /api/v1/site_ca`;
  the desk presents the site's certificate for its machine), `render` stereo 48 kHz from
  the app's audio callback,
  `set_level`/`set_pan`/`set_mute`/`set_solo`/`set_master`, push-to-talk, status and
  events. Spec idn-iem.md §I.8; embedding guide `media/idn-iem/README.md`; a test
  listener `idn iem listen`.

**Stream bus, endpoints, clock (2026-11-18).** `bus {bus: "stream", …}` and each channel's
`stream` send (`send {n, bus: "stream"}`); `meters.stream_lufs`; the `bus:stream` token.
`endpoint_add {name, outputs: [desk outputs from 1]}` → `{id}`, `endpoint {id, name?,
outputs?}`, `endpoint_remove {id}`; the state's `endpoints: [{id, name, source,
channels}]` (`source`: the IDN output to receive). `desk {clock: "auto"|"card"|"site"|"own"}`;
the state's `clock.chosen`. 16 DCAs.

**Processing additions (2026-11-19).** In a channel patch:
- `gate {hold (ms), mode: "gate"|"expander"|"ducker", ratio (expander, 1–10), key
  (channel from 1; 0 its own), key_hpf, key_lpf (Hz; 0 off)}`;
- `comp {mix (%, 0–100: parallel), detect: "peak"|"rms", auto_release, key, key_hpf}`;
- `hpf_slope`, `lpf_slope`: 6, 12 (default), 18, 24, 36 or 48 dB/octave;
- each send's `tap`: `input` (before the trim), `pre_eq`, `post_eq`, `pre_fader`,
  `post_fader` (null: as `pre` says).
Buses: `dim_db` (−60–0, default −20). Talkback: `dim_main` (the main down this much while
talking, −40–0) and `to_iem` (its microphone a lane of the IEM flow, heard there while
talkback is on). DCAs and mute groups: `color` (`#rrggbb`). Commands: `channel_reset {n}`
(fresh processing, levels and sends; name, colour, inputs, failover, place and link
kept); `snapshot_recall {name, scope: {…, parts: ["levels"|"sends"|"processing"|
"inputs"|"names"]}, fade_s (0–10)}`; `snapshot_order {names}` (the state's `snapshots`
in that order, the rest after by name). Each IEM mix in the state has `who`: the phones
on it now (`{name, user, talking}`).
Also: `denoise_amount` (%), `insert_point` (`post` | `pre`), EQ band shape `veq` (a
bell whose width follows its gain); `channel_count {n}` (from the next start; the
state's `channels_next`) and `restart {}` (under the IDN Node: saved and started again;
the state's `restartable`); the routing view's `network: [{name, channels}]`.
The round trip: the state's `latency {input_ms, block_ms, plugins_ms, denoise_ms,
output_delay_ms, total_ms, measured}`; `latency_test {output, input}` sends a click out
of a desk output (silent otherwise while it runs) and listens for it on a desk input
looped back to it (cable, card loopback or a network route), answering `{ms, samples}`
within about 2 s.

## 5. The Site Manager

Library: `idn_site_manager::host::start(Config { dir, passphrase, discovery_port,
api_port, announce, open_new_site, password, restored, host, voice }, |line| …)` →
`Handle`. `voice`: IDN Voice beside the API (on its port + 1); the crate's `voice`
feature (on by default) builds it in, and `default-features = false` leaves it out with
its WebRTC stack (str0m, aws-lc-sys and the cmake it needs). The site password
(`Config::password`, `Handle::set_password`) needs 8 characters, as users' passwords do;
a new user's name is letters, digits and `. _ - @`. Dropping a `Handle` without `stop()`
(in async code too) stops it all. The `Handle`:
`authority()` (machines, approvals, options, keys), `run(user, &SiteCommand::…)` (that
user's role checked, audited as theirs; needs the API), `set_password`, `api()`,
`voice_port()`, `stop()`, and:
- `api_port`: `Some(0)` takes a free port the system picks; no other port is ever tried.
  When the port asked for can't be bound (another program has it), `start` still returns
  `Ok` and the site runs without its API (the discovery server's port taken is an `Err`):
  `api()` is `None` and `api_error()` (from v0.3.1-Beta7) says why.
  `api()` gives the port actually bound and the certificate's SHA-256.
- `run_from(user, &Caller { address, client }, &SiteCommand::…)` and
  `issue_token_from(user, &Caller, …)`: the audit entry has the caller's address and client
  (`peer`, `client`), as a change over HTTP has. (`run` and `issue_token` leave them empty.)
- `events(since)` / `events_wait(since, timeout)`: the site's typed events after place
  `since` (0: all still kept) as `SiteEvents { events: [SiteEventEntry { seq, at, level,
  text, event }], next, missed }`. `SiteEvent`: `JoinRequested { id, name, kind,
  fingerprint, code, address, by_code }`, `JoinCancelled { id }`, `DeviceJoined`,
  `JoinDenied`, `JoinRefused`, `JoinWithdrawn`, `DeviceRemoved`, `DeviceOnline`,
  `DeviceOffline` (`fingerprint`, `name`), `CodeUsed { id, fingerprint, name }`,
  `CodeExpired { id }`, `CodeBurned { id, replaced }`, `CodeWrong { name, address,
  tries_left }`, and `Other { name, fields }` (a machine needing attention and its end,
  a version mismatch, the machines' own events: idn-node.md §N.41). Ask again from
  `next`. Only the newest 500 are kept: `missed` says some after `since` were dropped
  (read `authority().pending()`, `devices()`, `join_codes()` once, then go on from
  `next`). The same list as the push channel's events; it works without the API too
  (then without `CodeExpired` and the roll-up's events, which the API's monitoring makes).
- **Pairing relayed through another machine** (a client app pairing at a box without the Site
  Manager; idn-node.md §N.42.3):
  - The relay box: `Enrollment::request_key(client, pin)` returns a ticket, and
    `key_answer(ticket)` returns `None` while waiting, then a `KeyGrant { approved, id,
    key, scope, engine, error }`. This works through the box's IDN Node as well.
    `withdraw_key_request(ticket)` takes a waiting request back (a client that typed a
    wrong code and asks again): it leaves the host's list (`KeyRequestCancelled`). Added in
    v0.3.1-Beta2.
  - The host: `SiteEvent::KeyRequested { id, client, via, via_fingerprint, pin }` tells
    it a request is waiting. `key_requests()` lists them with the PIN typed. It answers
    with `approve_key_request(id, user, &caller, &TokenAud, scope, days)`, which makes the
    named key here and sends it back through the relay, or `deny_key_request(id, user,
    &caller, why)`. `KeyRequestCancelled { id }` means the relay went away, or 10 minutes
    passed.
  - The relay keeps nothing, and its keys outlive it. Approval levels are the host's to
    apply.
  - The pairing code a relay shows comes from the site: `Enrollment::pairing_code(timeout)`
    returns `(code, until)`, six digits valid for 5 minutes (until used, or burnt by 5
    wrong tries). The client types it on the relay, which passes it as `request_key`'s
    PIN. The site checks it, and the host sees `code_ok` (`SiteEvent::KeyRequested {
    code_ok }`, `KeyRequest::code_ok`), so a host's PIN-only level can pair it with no click.
    A wrong code is told to the relay at once: `Enrollment::key_status(ticket)` returns
    `KeyStatus::Waiting { code_wrong: true }` (the request still waits for a person), and
    `pairing_code_left(timeout)` also returns the seconds a code has left.
  - Only a machine the site marked as a pairing relay gets codes (from
    v0.3.4-Beta11): `SiteCommand::Relay { fingerprint, on }` (admins;
    `Authority::set_relay`), shown as `DeviceHealth::relay` and `EnrolledDevice::relay`.
    On any other machine `pairing_code` fails (`PermissionDenied`, "isn't a pairing
    relay"), and a request through it is never `code_ok`: it waits for a person. Mark
    each box that relays pairings once (before, any machine of the site
    could ask for its own code and verify its own request). On a relay machine its IDN
    Node gives the code, and passes a request's PIN, only to the program hosting the Node
    (the same process: the host with the Node embedded) or an administrator; another
    program's request goes without its PIN (it waits for a person). Through the Node the
    errors keep their kind (`PermissionDenied`, `AlreadyExists`, …).
- **A host's media library** (v2, idn-extensions.md §E.12.11.1). On this box:
  `let lib = LibraryServer::open(dir, "MyLibrary")?; let _ch =
  lib.serve(handle.join_own_site(member_dir, "MyLibrary", DeviceRole::App)?)?;`, optionally
  `lib.set_prober(Arc::new(move |p| idn_ffmpeg::media::library_facts(p, &ffmpeg)))`.
  (Since v0.3.1-Beta3: `LibraryServer::close()` stops it and returns once saved, and a
  second `open` on a folder waits for the first's end; `Library::marked(holder)` and
  `LibraryServer::marked` read a holder's marks without changing them; more endings are
  taken (mpg, m2ts, 3gp, ogv, flv, tga, heif, jfif and others), and
  `library::take_endings(&["r3d"], "video")` adds a host's own.)
  The host itself can also call `lib.import`, `lib.catalog`, `lib.delete`, `lib.marks`,
  `lib.changes` and the rest directly.
  - Show boxes use `Library::find(member)` and `library::cache::Cache` (pin what the box
    must keep; item states; a size cap). In a site with more than one library (the host's
    box and an IDN Node's media folder), `Library::find_on(member, fingerprint, timeout)`
    reaches a given machine's, `find_named(member, "MyLibrary", timeout)` one by its own
    name, and `Library::list(member, timeout)` lists them (`LibraryInfo { name, machine,
    fingerprint, items, v2 }`). Added in v0.3.1-Beta2.
  - A show box with its own channel (`Channel::start` with its own handler) can use that
    channel for the library: `Library::find_named_via(channel, "MyLibrary", timeout)`,
    `find_on_via` and `list_via`. The box then has one endpoint toward the library's
    machine. Where the box's IDN Node holds the machine's endpoint, start the channel with
    `client_only: true`. The library's box can then send it requests with
    `channel.request(box_fingerprint, …)`: a request tries the box's endpoints in turn
    until one knows the type. Added in v0.3.1-Beta4; before that, two channels of one
    box replaced each other's connection.
  - Previews: on the library's box, the host serves them itself from `lib.path_of(id)`; from an
    IDN Node's library, a site member asks `lib.link(id)` (a link with its own token, for
    a while). The links opened with a host program's long-lived key (`keyed_link`) are
    gone since v0.3.1-Beta3: the key crossed the network in plain HTTP.
- `join_own_site(dir, name, role)`: a service on the Site Manager's own machine (such as a host's library)
  joins the site as a member, approved at once, and gets its `Enrollment` (for
  `channel/1`). Its identity and certificate live in `dir`. After a site restore (or a
  new site) its old membership is set aside in `dir/old-site-ID/` and it joins the site
  it is now, with the same identity (from v0.3.1-Beta5): call it at every start.
- `api_keys()`: the named keys as `ApiKeyInfo { id, name, engine, machine, scope,
  created, by, expires, revoked, revoked_at, last_used, last_machine }`.
  `SiteCommand::ApiKey { name, aud, scope, days }`: `days` 1–3650, for a shorter life
  (none: 10 years). At most 500 keys are active.
- `SiteCommand::Approve { id, name }`: `name` (from v0.3.1-Beta6) is the machine's name
  in the site instead of the one it asked with. Its certificate is issued under it, so
  `authority().devices()`, `device_health()` (until its Node reports another) and the
  site's directory show it, and the machine reads it with `Enrollment::name_in_site()`.
  A machine with an IDN Node gets it as its Node's name too. (Library: `Authority::
  approve_as(id, name)`.) Before, the name only went to a Node, so a program enrolled by
  itself kept the name it asked with.
- Leaving: a machine's IDN Node leaving the site tells the site first. The machine is no
  longer listed (`SiteEvent::DeviceLeft`) and may join again. A program with its own
  enrolment calls `Enrollment::tell_leaving()` before forgetting its membership.
- `device_health()`: each machine of the site (not removed) as `DeviceHealth {
  fingerprint, name, kind, online, last_seen, version, node_started, machine, restarting,
  services, updating, update_managed_by, attention, relay }`. `update_managed_by`: the
  product that installs and updates its IDN Node itself (below, "Updates managed by
  another product"). `relay`: a pairing relay (from
  v0.3.4-Beta11). `machine` (from v0.3.1-Beta8):
  `DeviceMachine { cpu, cpus, gpus, os, os_version, kernel, boot_time, memory_total,
  memory_free }` as its Node last reported (CPU model and logical CPUs, GPU adapter
  names, system name and version, kernel release or Windows build, the machine's start
  in Unix seconds, memory in bytes; what its system doesn't say is empty or `None`;
  `memory_free` isn't given on macOS). `None` when no Node has reported or the Node is
  older. `node_started`: when its Node started (Unix seconds); with `boot_time` it says
  whether the machine or only the Node restarted. `services` (from v0.3.1-Beta6): `DeviceService { id, name, state }`, as
  its Node last reported: `id` is what `SiteCommand::Restart { fingerprint, service }`
  takes, `name` its name for people, `state` `running`, `starting`, `restarting`,
  `failed`, `off`, `unset`, `unsupported` or `no_session` (empty: no Node has reported).
  `last_seen` (Unix seconds) and `version` (its Node's build) are as its Node last
  reported, kept after it goes offline (none for an app enrolled by itself, or a machine
  not heard from since this start); `restarting`: its services stopped and starting
  again; `updating`: an update's state while one runs; `attention`: its problems
  (`DeviceProblem { problem, level, text, fields }`: `offline`, `service_failed`,
  `version_mismatch`, `clock_unlocked`, … as the state's `attention`).

HTTP (HTTPS 29202): `GET site_ca` (the site CA certificate, PEM, no login: what clients
pin for the machines' network ports); `POST login` / `logout`; `GET status` (machines, pending joins, audio
routes, options, templates, presets, streams, updates, MQTT, the log), `levels`,
`network-devices`, `network-devices/settings?device`, `users`, `reports`, `report?fingerprint&name`, `logfile?…`, `audit?n`,
`schema`, `events`; changes: `GET /api/v1/schema` (`SiteCommand`), plus `backup`
(answers the file), `restore` (`{passphrase, data}` base64), `release/piece?offset` (raw)
and `release/done`. Roles: viewer (reads), operator (routes, latency, network devices,
presets, logs, join codes), admin (everything else).

Joining (extensions §E.10.10, idn-node.md §N.11.8):
- **Join mode:** the site option `join.mode`, one of `approval`, `code_or_approval` (new
  sites) or `code_and_approval`. Set it with `POST option {"scope": "site", "key":
  "join.mode", "value": …}` (admin). Read it as `status.site.join_mode`; an unset option
  reads `approval`.
- **Join code:** `POST join_code {}` (admin, as approving is: in `code_or_approval` a code lets a machine in; `SiteCommand::JoinCode {cancel: None}`)
  answers `{ok, id, code: "482913", shown: "482 913", expires}`, with `expires` in Unix
  seconds.
  - Show `shown` once. The code is single use, lasts 10 minutes and burns after 5 wrong
    tries. At most 4 wait; a fifth burns the oldest.
  - `POST join_code {"cancel": id}` takes one back.
  - `status.site.join_codes` lists `[{id, expires, tries_left, issuer}]`, never the
    digits.
  - A pending join from a machine that typed a code (`code_and_approval`) has `by_code:
    true` and an empty `code`.
  - The audit has the issue (the request), and the site's own `join_code/use`,
    `join_code/wrong` and `join_code/burn`.
- **Library:** `Authority::join_mode`, `set_join_mode`, `issue_join_code`, `join_codes`,
  `cancel_join_code`, and the events `CodeUsed`, `CodeWrong` and `CodeBurned`.

What a UI needs worked out, served (the page's logic once):

- `status.me.capabilities`: the change paths (under `/api/v1/`) this user's role may use;
  `me.capabilities_limited`: those it may use in part (an operator: `routing_preset` save
  and recall, `apply` with `routers.route` only). `reports`: each crash has `app`.
- `GET matrix` (also a push-channel topic, `matrix`): `{read_only, read_only_on,
  transmitters, receivers, devices}`, IDN's and the network's devices in one table.
  Transmitters: `id` (= `name`), `kind` `idn` (`known` false: a guess or a route's unlisted
  one) | `network` (`DEVICE (network audio)`, `device`, `devname`, `own`) | `unmatched`
  (read only, `unread`: subscriptions to channels no list has). Receivers: `id`
  `idn/FINGERPRINT/NAME` or `net/DEVICE` (`net`, `readonly` for plain routes); routes
  `{ch, tx, tx_ch, state, words}` (`through`: a pool's source), pool routes drawn in the
  device's / IDN transmitter's column. `devices`: `{name, instance, own, read_only,
  renamable, outputs_renamable, read_by, …}`.
  - How each route's sound arrives (idn-routing.md §A.16): `via` (`machine`: this
    machine's shared-memory ring; `network`), `cast` (`multicast`/`unicast`),
    `transport`; null before sound comes.
  - Receivers: `rx_kind` (`program`, `sound_card`, `asio_driver`, `network_device`,
    `desk`, `switcher`, `other` when the program didn't say).
  - Transmitters: `origin` `{kind, device, channels}` (the card and its inputs behind
    `idn-audio send`; a pool's source: `network_device`, `DEVICE/CHANNEL` per channel;
    null when not said), and `received_by`: `[{machine, fingerprint, program, receiver,
    receiver_id, kind, via, channels: [{ch, tx_ch, state, via, through}]}]` — who
    receives it.
- `GET audio_receivers`: `{transmitters: [{id, name, machine, kind, channels, labels,
  origin, received_by}]}`, the matrix's "who receives me" by itself.
- `status.attention`: machines needing attention, `[{fingerprint, machine, problems}]`
  (`offline`, `service_failed`, `update_failed`, `update_rolled_back`,
  `version_mismatch`, `clock_unlocked`, and the Node's own problems); `status.versions`
  `{site_manager, build, mismatched: [{fingerprint, machine, version, build}]}`;
  `status.rollout` `{version, state, sent, ok, failed, waiting}` (null with no rollout).
- `POST route_cell {receiver_id, ch, tx_id, tx_ch, on?}` (operator; `on`: wanted state,
  none toggles): device↔device a subscription (`network-devices/do`), device output →
  IDN receiver or IDN transmitter → device input through the pool
  (`network-devices/pool`; cleared: the IDN route / the subscription), IDN↔IDN a `route`.
  Answer `{did: path, body}` or `{did: "nothing", why}`. Not clickable: unmatched columns,
  a machine's own device's outputs to IDN, network cells on a read-only site.
- `GET settings_schema`: `{groups: [{id, title, hint, fields: [{key, type
  (check|text|lines|select|number|password), label, placeholder, options, min, max,
  machine_own}]}], machine_own, copy_default}`; `GET machine_settings?fingerprint` → the
  machine's values, `{key: text}` (never the PIN).
- `POST copy_settings {from, to, groups?, pin?}` and `apply_template {name, to}` (admin):
  a machine's own keys (`clock_protocol`, `netaudio.device`, `netaudio.interface`,
  `audio_rate`, `sitelink.mode`, `data.library`, `capture.screens`, `update.managed_by`) never copied nor kept
  in a template. A copy never carries the source machine's PIN (`pin`, with the panel
  group, sets theirs); `apply_template` sends the template's saved PIN (`panel_pin`, a
  hash), if it has one. `template` keeps the saved PIN when a save of the Simple mode
  group gives none. In `apply`, `template` and `node`, a PIN of `none` (any case) is no
  PIN and an empty one is unchanged (left out).
- `POST node {fingerprint, …}`: its yes/no fields (`require`, `site_lock`, `clock`,
  `pin_off`, `pin_site_only`, `lan_off`) take a boolean or `"true"`/`"on"`/`"false"`/
  `"off"`. `user {action: "set", name, role?, password?}`: no `role` keeps the user's
  (a new user: viewer). `mqtt {on?, listen?}`: either left out stays as it is.

**Added 2026-11-18.** `POST api_key {name, aud: {engine, machine?}, scope}` (admin) →
`{id, key}` (the key shown once; lives up to 10 years), `POST api_key_revoke {id}`, `GET
api_keys`. `POST restart {fingerprint, service}` (admin), `POST identify {fingerprint,
seconds}` (operator), `approve {id, name}` (`name`: the machine's name in the site, its
certificate's: see `SiteCommand::Approve` below). Releases list `product` and `track`; a machine's
track is the device option `update.track` (`stable`/`beta`). Machines: `node.uptime_s`,
`node.cpu_percent`, `node.last_seen_s`; `pending[].asked_at`.

**Added 2026-11-26: updates managed by another product** (idn-node.md §N.23). A machine
whose IDN Node another product installs and updates itself says so, and the Site Manager's IDN Media
updates leave it alone:
- That product's package writes `idn-updates-managed-by` in IDN's programs folder (beside
  `idn-node`; `/usr/lib/idn-media/` from the Linux package): plain text, its first line the
  product's name (e.g. `MyApp`; trimmed, at most 64 characters, printable). The Node only reads
  it; it wins over the setting. Remove it when the package is removed.
- Or the setting `update.managed_by` (text; empty: not managed): `POST apply {to,
  settings: {"update.managed_by": "MyApp"}}`, or on the machine (root / an elevated
  administrator) `idn-node set update-managed-by=MyApp`. A machine's own: never copied
  nor kept in a template. The settings schema's group `update`.
- The machine reports `<update managed_by="MyApp" managed_by_source="package|setting"/>`:
  `status.machines[].node.update.managed_by` (and `managed_by_source`),
  `DeviceHealth::update_managed_by`.
- `POST update` leaves such machines out (an IDN Media release): each in `job.sent` with
  `ok: false`, `error: "updates managed by MyApp: update it there"` (every machine chosen
  managed: a 400 with that reason). The machine refuses it too, before any download.
  `POST rollback` is refused by the machine (`updates managed by MyApp: roll back
  there`); an update staged for its next start is dropped. That product's own releases
  still go to it on that machine.

**Added 2026-11-26: a machine's apps and services in the status** (idn-node.md §N.11.3).
`status.machines[].node.settings.apps` (`switcher`, `mixer`, `display`, `display_boot`:
"true"/"false"), and `services_list[]` has `auto` (starts by itself) and `runs`
(`session`, or `node`: from boot by the Node) beside `id`, `kind`, `title`, `on`. A
display box's mode: the `display` service's `runs` and `on`. The settings form's
"Switcher, mixer and screens" has `apps.display` and `apps.display_boot`; a headless box
needs both on (`display_boot` only says where it runs, Linux only). Values through Apply:
"on"/"off", "true"/"false" or JSON booleans.

**Added 2026-11-26: network audio device settings** (roadmap R.32 stage 2,
idn-aoip-controller.md K.14). Another maker's device's settings, changed through the Node
of a machine that reads it. **Each command is offered only once it has been checked on
real equipment in the lab**; until then the Node answers "WHAT: not checked on real
equipment yet (lab N)". A Node started with the lab switch `IDN_NETAUDIO_UNCHECKED=1`
offers them all (lab runs only).
- **What is offered:** `GET network-devices` has per machine `commands` (the ops its Node
  offers now; none on a read-only site) and `unchecked`, and per device `commands` (none
  for a machine's own IDN device).
- **Read first:** `GET network-devices/settings?device=NAME&machine=FINGERPRINT` (any
  logged-in user; `machine` optional) → `{name, machine, commands, unchecked, read_only,
  rate, rates, rate_fixed, encoding, encodings, encoding_fixed, aes67, aes67_now,
  aes67_reboot_needed, clock_source (internal|word_clock|aes|…), preferred_leader,
  leader_fixed, clock_fixed, word_clock_state (unknown|none|invalid|valid|missing),
  latency_ms, latency_choices_ms, can_identify, can_rate, can_encoding, can_aes67,
  word_clock_read_only, heard}`. A key the device didn't say is left out; offer a rate,
  encoding or latency only from its lists. About 3–5 s; the same device's read again
  within 5 s is the one kept (a change to it drops it).
- **Change:** `POST network-devices/do {op, device, value?, confirm?, machine?}`
  (operator; audited) → `{ok: true, did}`, `did` saying what the device reads back
  ("Stage-Box sample rate 96000 Hz (it says so)"), or, when it doesn't confirm it in time,
  "Stage-Box sample rate 96000 Hz: sent; the device doesn't confirm it yet (…)"; a 400
  with the reason when refused or contradicted. Stage 1's ops (subscribe, unsubscribe,
  renames) answer `{ok, did}` too.
  `op` and `value`: `set_latency` (ms, one of `latency_choices_ms`), `set_rate` (Hz, one
  of `rates`), `set_encoding` (one of `encodings`), `set_aes67`, `set_preferred_leader`,
  `set_word_clock` (`true`/`false`), `identify`, `reboot`, `factory_reset`,
  `clear_config` (`all` or `keep_ip`). A rate or encoding change can take up to 20 s.
  `set_word_clock true` is refused when the device says its word clock input is "none"
  or "missing"; with any other state but "valid" it needs `confirm` (the device's name),
  and the refusal says the state. A machine makes two settings changes or reads at once;
  a third is answered busy (stage 1's changes have their own four).
- **Reboot, factory reset and clear configuration** need `confirm`: the device's current
  name, exactly as the list has it (have the person type it). One device a request.
- Read only (`netaudio.read_only` on any machine) refuses every change; reading still
  works. The device list shows the new value within a few seconds (the Node has it read
  again).

## 6. The Node

Every IDN program is a Node client: `Node::connect(app)` (the machine's) or
`connect_to(endpoint, app)` (a hosted one), then `status()`, `request(Element)`
(settings, routes, pairing, `panel_token`, `data_panel`, …). The panel API
(`http://127.0.0.1:29200/api/v1/`) is the control panel's and answers `status`,
settings changes and `events`; `idn-node open` / `idn-session --open` give its one-time
link. Its `status.problems` and the push channel's typed events: "Events and problems"
(§2). A routable receiver says what it is with `AudioReceiver::register_as(…, kind)`; an
output says where its sound comes from with `Output::set_audio_origin`.

Joining with a code from the Site Manager (idn-node.md §N.11.8):
- The three ways in: `POST /api/v1/site/join {"code": "482 913"}` (panel API),
  `<site_join code/>` (IPC), or `idn-node join CODE`.
- The status's `state` then says the answer:
  - `code_accepted`: waiting for approval;
  - `wrong_code`, with `code_left`;
  - `code_expired`;
  - `code_needed`: the site wants a code;
  - `enrolled`: joined.
- `join_mode` is the site's mode (empty when not known).
- In the library: `Enrollment::join_with_code` and `site_join_mode`, and `TrustState`'s
  `CodeNeeded`, `CodeAccepted`, `WrongCode { left }` and `CodeExpired`.

Leaving the site, or cancelling a join still waiting (idn-node.md §N.42):
- `POST /api/v1/site/leave` (panel API; not from a panel a program opened),
  `<site_leave/>` (IPC; while programs need approval, root or an elevated administrator
  only), `idn-node leave`, or `NodeCore::leave_site` (a host).
- The site's certificates on the machine are forgotten, its address cleared and site
  search turned off; the status's `state` is `none`. Refused on the machine running the
  site's Site Manager. The site lists the machine until its admin removes it.
- The status's `site_devices`: the site's device count from its directory (in a site;
  null until known).

Scheduled backups (§N.42): the Site Manager's `backup_schedule {on, every_hours, keep,
folder, passphrase}` and `backup_now` (admins); the status's `backups` (never the
passphrase). The desk, the switcher and the display service also copy their settings
once a day into `settings-backups/YYYY-MM-DD/` in their folders (14 days kept).

Sending a media file to another machine (§N.25): `<media_send to="FP|NAME" path="…"
| file="NAME" replace="true"/>` answers `<ok to offer/>` (`offer`: the transfer's id).
*Added 2026-11-26:* `replace="true"` replaces a finished file of the same name on the
receiving machine once the new one is whole and its SHA-256 checked (without it: kept
beside it as "NAME (2).EXT"); a receiver from before Beta14 refuses such an offer. The
sender's `<media>` transfer (`way="out"`, by `offer`) goes `offered` → `sent` (the
receiver fetched it all, `done` = `size`) or `failed: …`; the receiving machine's own
(`way="in"`): `receiving` → `received` or `failed: …`.

Media files in-process (`idn_ffmpeg::media`, rustdoc): `probe` (*added 2026-11-26:*
`VideoInfo::avg_frame_rate`, `base_frame_rate`, `variable_frame_rate`, `field_order`,
`interlaced`, `bit_rate`; `MediaInfo::bit_rate`), `thumbnail`, `VideoReader`,
`AudioReader`, and *new* `convert(input, output, &ConvertConfig, &ffmpeg, progress,
&cancel)`: an MP4 of H.264 (or HEVC) 8-bit 4:2:0 at a constant rate (the source's as a
common rate, or `frame_rate`), deinterlaced where interlaced, turned upright, fitted in
`max_size`, AAC stereo at `audio_rate` (48 kHz); the GPU's encoder where there is one,
else the CPU's; progress about four times a second, `cancel` stops it (an `Interrupted`
error); written as `OUTPUT.part` and renamed when whole; the calling thread at a low
priority. `ConvertResult` says the encoder, size, rate, frames repeated, dropped and
deinterlaced, and `notes` (HDR made SDR without tone mapping, alpha left out).

The programs' logs (§N.42): `GET /api/v1/logs` lists `{programs: [{app, file}]}` (each
program's newest log it told the Node of); `GET /api/v1/logs?app=NAME&lines=N` gives its
last lines (default 200, at most 2000). A host: `NodeCore::program_logs` /
`program_log_tail`.

## 7. IDN Data, listening, watching, Voice

- **IDN Data:** log in with the Node's one-time token (`Node::request(<data_panel/>)` →
  `t`, then `GET http://127.0.0.1:29212/?t=T` sets the session cookie), or show a site
  token (engine `data`); on the network, port 29221 (HTTPS) takes site tokens only. Then
  `GET /api/v1/state`, `library`, `thumb`, `audit`, `POST upload` and `POST
  /api/v1/<command>` (feeds, downloads, the library, sending to machines). It has no push
  channel.
- **Listening on phones:** `GET /api/v1/state` (`title`, `channels` with `listening`
  and `dropped` counts — listeners whose connection was lost, each a `listener_dropped`
  event —, `locked`, and `url`: this server's own address as the asker reached it —
  this machine's network address when asked on the machine itself — for the page to
  build its link and QR code from; no page is served there); `POST /whep/CHANNEL`
  (WHEP: an SDP offer, `201` with the answer and `Location`), `DELETE /whep/session/ID`.
- **Watching:** the same at 29218 with any source (`POST /whep/SOURCE`; its state:
  `sources`, `watching`, `locked`).
- **From a page on another origin** (e.g. a host application's own server): with no password, both
  answer CORS on `/whep/*`, `/api/v1/state` and `/api/v1/report` — the `OPTIONS`
  preflight, `Access-Control-Allow-Origin` (the request's `Origin`), `-Methods` (GET,
  POST, DELETE, OPTIONS), `-Headers` (Content-Type), `-Expose-Headers` (Location). With
  a password (its login a cookie of that server's): same-origin only, no CORS.
- **Voice:** its API under `/voice/api/…` and `/voice/ws` (see `core/idn-voice`). A
  sign-in link (`/voice/api/link/TOKEN`) sets the session cookie and answers `{ok, user}`
  (IDN Industrial, which serves the web client, redirects to it instead).
  Handsets (radios, phone apps: enrolment with a code or a credential at once, Bearer
  sign-in, the certificate's fingerprint to pin), drivers put on handsets by the site's
  program, SOS, resume after a dropped link, and every WebSocket message with its
  fields: [VOICE-CLIENT.md](VOICE-CLIENT.md). Every Voice timing is a site setting
  (`POST /voice/api/settings`, only the fields to change).

## 8. MQTT

The Site Manager's broker (port 1883, logins it holds) and the switcher's link:
`idn/HOST/switcher/…` (state, `cmd/<command>` with a JSON body, results), `idn/site/…`
(machines, events). See the switcher's `MQTT_COMMANDS` for what MQTT may run.

## 9. The display service

One per computer (idn-display.md §D.8): each screen of the computer shows a **feed** (a
source, or a routing source it follows), set through this API; a host application provides
its UI. The Node runs it in the desktop session when `apps.display` is on (`idn-node set
apps-display=on`; by hand `idn display --service`). For each screen given a feed it runs
an `idn-display` full screen on that screen; screens with no feed keep the desktop.

- **Where:** `http://127.0.0.1:29222/api/v1/` on this computer (open, audited as
  `local`); on the network `https://HOST:29223/api/v1/` once the computer is in a site,
  **site tokens only** (engine `display`; a `read` token reads and is refused changes).
  Changes carry `X-IDN-Display: 1`.
- **HTTP `GET`:** `state` (`?after=VERSION`), `schema`, `audit`, `stats`, `events`,
  `thumbnail?screen=N|NAME&width=W` (a JPEG of what the screen shows now, overlays and
  fallback included; width 16–1920, default 480; 404 for a screen showing nothing).
- **State:** `{service, version, build, machine, error, screens: [{id, name, number,
  connected, width, height, refresh_hz, x, y, feed, routed_to, running, error,
  identify, adapter, status: {state, showing, reason, attempts, gave_up, for_s},
  stats, settings, label, feed_by, feed_at, test_pattern, dropped_last_min, modes}],
  problems, sound_cards, lan: {on, available}}`.
  - `label`: the screen's name for people (`label`); `feed_by` / `feed_at`: who set the
    feed (as the audit says them) and when. `modes`: `[{width, height, refresh_hz}]` the
    screen offers. `dropped_last_min`: pictures dropped over the last minute.
  - `sound_cards`: `[{name, id, default, channels}]` that play (null until listed; again
    each minute); `settings.audio.device` takes a `name`.
  - `problems` (as §5) add `screen_smaller` (info: the feed is larger than the screen's
    mode, shown smaller; not for a wall) and `dropping` (warning: 60 or more pictures
    dropped in the last minute).
  - `id`: the screen's name as the desktop gives it (`DP-2`, `\\.\DISPLAY1`); settings
    are kept by it. `number`: from 1, in the desktop's order. A screen saved but not
    plugged in is listed with `connected: false`.
  - `status.state`: `idle` (no feed), `connecting` (a new feed; the picture before it
    held), `playing`, `lost` (the last picture held), `fallback`. `showing`: `feed`,
    `last`, `logo`, `black`, `slideshow`. `reason`: why it lost the feed or shows the
    fallback (`gave up after N attempts` once the cap is reached). `attempts`: attempts
    to reach the feed again since it was lost.
  - `stats`: `bitrate_kbps`, `received`, `shown`, `dropped`, `late`, `fps`, `decode`
    (`gpu`|`cpu`), `source_size` and `screen_size` (`[w, h]`), `frame_rate`,
    `transport` (`squic`, `quic`, `tcp`, `udp`, `multicast`, `machine`, …),
    `encrypted`, `site_trust`, `reconnects`, `decode_errors` (pictures that failed to
    decode since the feed was set), `sync` (`{group, mode, delay_ms, late}` on a wall).
- **Commands** (`POST /api/v1/<command>`, the push channel, `GET /api/v1/schema`):
  - `feed {screen, source}`: `screen` a number or an `id`; `source` `HOST (name)`, a
    routing source (the screen follows wherever it is switched; the state's
    `routed_to` says where), or `address:port`; `""`: nothing (the screen goes back to
    the desktop). The picture on the screen stays until the new feed's first picture:
    no black between, and the screen's window and process stay.
  - `settings {screen, rotation?, overscan?, fit?, audio?, fallback?, wall?, mode?,
    idle?}`: the fields given (each of `audio`, `fallback`, `wall` as a whole):
    - `mode {width, height, refresh_hz}` (one of the screen's `modes`; the nearest
      refresh at that size) or `null` (the desktop's): the screen taken whole in that
      mode while it shows (Windows, X11, macOS; a Wayland desktop keeps its own mode).
    - `idle`: `desktop` (default: a screen with no feed shows the desktop) or
      `fallback` (its logo or slideshow instead: never the desktop).
    - `rotation`: 0, 90, 180, 270 (clockwise); `overscan`: 0–20 (% of each edge the
      screen hides; the picture is kept inside the rest); `fit`: `fit` (default),
      `fill` (edges cut), `stretch`.
    - `audio {device, channels, volume_db, mute}`: `device` a sound card (`""`: the
      system's; null: no sound), `channels` the feed's channels on the card's first
      outputs (default `[0, 1]`, at most 8), `volume_db` −60 to +12.
    - `fallback {after_s, show, logo, slideshow, slide_s, reconnect_cap}`: when the feed
      goes, the last picture is held `after_s` (default 5), then `show`: `logo`
      (default), `last`, `black` or `slideshow`. `logo`: a PNG or JPEG in the
      computer's media folder or the service's folder (empty: the site's, `logo.png` or
      `logo.jpg` at the media folder's top, else black); `slideshow`: a folder there,
      each picture `slide_s` (default 8). `reconnect_cap`: attempts before giving up
      (0, the default: never). The feed's pictures coming back end the fallback at once.
    - `wall {rows, cols, row, col, bezel_mm: {h, v}, screen_mm: {w, h}, group?}` (or
      `null`): this screen's tile of a wall (row and column from 0). The picture is
      placed over the whole wall (`fit` applies to the wall) and this screen shows its
      part; `bezel_mm` (needs `screen_mm`, the picture area of one screen) leaves out
      the part behind the bezels so lines run straight across. The wall's screens
      present each picture together (`sync/1`, group `wall:FEED` unless `group` is
      given; up to a refresh apart without genlock).
  - `identify {screen | "all", seconds?}`: the screen's number, name and feed, large,
    for `seconds` (default 5).
  - `forget {screen}`: drops a screen's saved feed and settings.
  - `label {screen, label}`: its name for people (at most 60 characters; `""`: none);
    shown on identify above its own name, with the computer's name and addresses.
  - `test_pattern {screen | "all", on, seconds?}`: colour bars, a grey ramp, the edges
    and the centre over the whole screen, for `seconds` (1–3600) or until `on: false`;
    `status.showing` is `test_pattern` meanwhile.
  - `restart {screen}`: the screen's process started again (feed and settings kept);
    event `restarted`.
  - `lan {on}`: the API on the network on or off (kept across restarts); off, the
    network port answers 503; on this computer it stays open.
- **Push channel** (`GET /api/v1/events`): `{"type": "state", …}` on every change (not
  the figures); typed events `{"type": "event", "event", "screen", "at", …}`:
  - `lost {feed, reason}`; `reconnecting {feed, attempt, reason}` for each new attempt;
  - `fallback {feed, show, reason, gave_up}`; `recovered {feed, after_s}`;
  - `playing {feed}` (a new feed's first picture); `format {feed, from, to}` (the
    feed's size or frame rate changed, each `{size, frame_rate}`); `routed {feed, to}`;
  - `screens {added, removed}`; `stopped {error}` (the screen's process ended; started
    again after 3 s).

  After `{"subscribe": ["stats"]}`: `{"type": "stats", "screens": [{id, stats, for_s}]}`
  each second; `["audit"]` as the other engines.
- **The screens' own processes:** `idn-display --commands --screen NAME` takes JSON
  lines on its standard input (`{"t": "feed"|"settings"|"identify"|"thumbnail", …}`)
  and answers on its standard output (`ready`, `status`, `thumbnail`); the service
  speaks it, a host needs only the HTTP API.
- **Display power** (HDMI-CEC, DDC-CI) isn't in it.

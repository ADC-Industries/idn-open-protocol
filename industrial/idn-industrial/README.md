# IDN Industrial

The IDN project's machinery as a base for industrial applications
(spec/extensions/idn-industrial.md, roadmap R.20). Two programs:

- **IDN Site Manager | Industrial** (`idn-site-manager-industrial`): one per company, in a
  Docker container beside the company's application server. Machines join it with
  single-use enrolment tokens and keep a link to it (QUIC, or a WebSocket through a
  Cloudflare Tunnel, with IDN's own encryption inside). What they send is stored before
  it is acknowledged and handed to the application server through its **MQTT 5 broker**
  (persistent sessions: an application that was away gets what it missed, in order). Web
  UI at `/`, API under `/api`.
- **IDN Node | Industrial** (`idn-industrial`): on each machine, a service from boot.
  Its local API on `http://127.0.0.1:29208` is how the machine's own programs, in any
  language, send messages, values and files (kept on disk until the Site Manager has
  them), ask the application server, and take its commands. It reports the machine's
  health (memory, disks, temperatures, watched programs and services, pending reboot).

## The Site Manager

One per company, on a mini PC, a server or a VPS. Three ways to run it:

**Linux (Debian 12, Ubuntu 24.04/26.04): the `idn-site-manager-industrial` package**, a
systemd service from boot under its own user:

```sh
sudo apt install ./idn-site-manager-industrial_VER_amd64.deb
sudo nano /etc/idn-site-manager-industrial.env      # IDN_PUBLIC_URLS at least
sudo systemctl restart idn-site-manager-industrial
sudo cat /var/lib/idn-site-manager-industrial/api.key   # for the web UI's first visit
```

Its data is in `/var/lib/idn-site-manager-industrial`. A Node | Industrial on the same
machine: its local MQTT wants 1883 as the Site Manager's broker does; the Node carries on
without it and says so in its log; give it `[mqtt] loopback = "127.0.0.1:1885"` (or the
Site Manager another `IDN_MQTT`). Backup: `sudo systemctl stop idn-site-manager-industrial
&& sudo idn-site-manager-industrial backup site.idnsite && sudo systemctl start
idn-site-manager-industrial` (`restore site.idnsite --replace` the same way).

**Windows:** the Industrial installer's option "IDN Site Manager | Industrial (this
computer is the company's server)": its own service, the address, administrator password
and port asked for, Windows Firewall opened, its data in
`%ProgramData%\IDN Site Manager Industrial` (settings in `settings.env` there). Unattended:
`idn-industrial-setup-…exe /S /SITEMANAGER /ADDRESS=server.example.com [/NONODE]`.

**Docker:** below, the image built from the sources:
`docker build -f installer/docker/site-manager-industrial/Dockerfile -t idn-site-manager-industrial .`
from the repository's root (`--build-arg CARGO_BUILD_JOBS=2` on a small machine).

Point the tunnel at `http://<container>:8080`. `compose.yaml` is a Komodo/compose stack.

| Variable | Default | |
|---|---|---|
| `IDN_DATA` | `/var/lib/idn-site-manager-industrial` (the container: `/data`) | the volume: site CA, tokens, machines, queued messages (back it up) |
| `IDN_HTTP` | `0.0.0.0:8080` | web UI, API, `/idn/link`, `/mqtt` |
| `IDN_QUIC` | `0.0.0.0:29206` | UDP for QUIC links (`off`) |
| `IDN_MQTT` | `0.0.0.0:1883` | MQTT over TCP (`off`) |
| `IDN_PUBLIC_URLS` | | what tokens tell machines |
| `IDN_API_KEY` | made, in `/data/api.key` | the application's API key |
| `IDN_ADMIN_PASSWORD` | | the web UI's password (else set on first visit with the API key) |
| `IDN_VOICE_HTTPS` | `0.0.0.0:29203` | IDN Voice over HTTPS for handsets, and over IDN's link on the same UDP port (`off`: both) |

API (bearer API key): `POST /api/tokens {name, tags, hours}` → `{token}`; `GET
/api/machines`; `POST /api/machines/{fp}/revoke`; `POST /api/machines/{fp}/command {type,
payload}`; `POST /api/mqtt-users {username, password}`.

**Backup and restore** (the whole volume in one file, locked with a passphrase you
choose; the server stopped, as its databases are held open while it runs):

```
docker compose stop site-manager
docker compose run --rm -v "$PWD:/backup" -e IDN_BACKUP_PASSPHRASE site-manager backup /backup/site.idnsite
docker compose start site-manager
```

`restore /backup/site.idnsite` the same way, into an empty volume (`--replace` over one).
The machines stay enrolled: the site's CA and keys come back with it.

### MQTT topics

| Topic | |
|---|---|
| `idn/<machine>/status` | retained: online, transport, tags, version, clock offset |
| `idn/<machine>/v/<point>` | retained values (`health/…` from the health module) |
| `idn/<machine>/m/<type>` | messages (user properties: `id`, `time_ms`, `arrived_ms`) |
| `idn/<machine>/f/<type>` | files |
| `idn/<machine>/req/<type>` | the machine asks: answer on the response topic |
| `idn/<machine>/cmd/<type>` | publish to command the machine; the answer comes to your response topic (user property `status`, 0 = ok) |
| `idn/site/events` | enrolled, revoked, online, offline |

`examples/mqtt_watch` prints what the broker publishes.

### IDN Voice

At `/voice` on the HTTP port (spec §I.18, idn-voice.md). The API key is an
administrator there: the dispatch system provisions handsets (`POST /voice/api/handsets {name,
issue: "credential"}` → the handset's credential), puts drivers on them and into their
channels (`POST /voice/api/driver`, or MQTT `idn/site/voice/cmd/driver`), acknowledges
SOS alerts and changes Voice's settings; it hears `idn/site/voice/presence`, `messages/…`
and `events` (talks, `sos`, `driver_result`). Handset apps sign in with
`Authorization: Bearer <credential>`, over HTTPS only: Voice has its own TLS port,
`IDN_VOICE_HTTPS` (default `0.0.0.0:29203`; `off`), with a self-signed certificate made
once in `<IDN_DATA>/voice/` (kept in the backup; its SHA-256 in the log, the enrolment
answers and QR codes, for handsets to pin). The same address on UDP is Voice over IDN's
link (QUIC, for handsets changing networks; its own key `<IDN_DATA>/voice/link.key`, its
certificate's SHA-256 in the log and the enrolment answers). Open 29203 TCP and UDP on
the plant network's firewall (and publish both from the container: `-p 29203:29203
-p 29203:29203/udp`). Over the plain-HTTP port
(`/voice` on 8080) handset credentials, enrolment and provisioning are refused unless
they come through the tunnel (loopback or `IDN_TRUST_PROXY`, `X-Forwarded-Proto: https`)
or Voice's `plain_handsets` setting is on. The whole client protocol, for app
developers: [impl/docs/VOICE-CLIENT.md](../../docs/VOICE-CLIENT.md). Optional:
`IDN_VOICE_URLS` (addresses told to handsets; `IDN_PUBLIC_URLS`' `wss://` ones are added
as `https://HOST/voice`), `IDN_VOICE_CERT_SHA256` (a proxy's certificate to pin
instead).

## Modules (stage 2)

Every setting below can be made in forms: on the Node's panel
(`http://127.0.0.1:29208/`, the machine's own files, its secrets, a backup and restore)
and on the Site Manager's Modules page (pushed files, by machine or tag). The files are
the same either way. In the Node's state folder: `node.toml` and `modules/*.toml` (the machine's own), and what
the Site Manager pushes (Modules page or `PUT /api/modules/{tag:plant=PD|machine:<fp>}/{name}`),
kept in `site/`. Pushed files can't name actions or make Modbus points writable.

```toml
[health]
processes = ["batching.exe"]
[health.files]               # a file's size and its volume's free space
batch = "D:\\SQL\\Batch.mdf"

[outbox]                     # only in the machine's own files
max_mb = 1024                # what may wait for the Site Manager; by size, never by age

[mqtt]                       # the local MQTT endpoint, on by default at 127.0.0.1:1883
listen = "0.0.0.0:1884"      # also on the plant's network, with a login
username = "plant"
password = "…"

[[modbus]]
name = "plc"
address = "192.168.1.50:502"
[[modbus.points]]
name = "silo/3/level"        # value plc/silo/3/level
address = 100
scale = 0.1
[[modbus.points]]
name = "setpoint"
address = 101
writable = true              # only in the machine's own files
min = 0
max = 500

[[http]]
name = "weather"
url = "https://api.example.com/now"
every_s = 60
headers = { Authorization = "Bearer ${secret:weather_key}" }   # secrets.toml
values = [ { name = "outside/temp_c", pointer = "/current/temp" } ]

[[webhook]]
name = "scale"               # POST 127.0.0.1:29208/webhooks/scale?token=…
type = "scale.weight"
token = "…"

[[action]]                   # only in the machine's own files
name = "export"
run = ["powershell.exe", "-NoProfile", "-File", "C:\\scripts\\export.ps1", "{customer}"]
params = { customer = "[A-Z0-9]{1,10}" }
```

Commands from the application: `idn/<machine>/cmd/modbus/write` `{"device", "point",
"value"}` and `idn/<machine>/cmd/action/run` `{"name", "params"}`; each is reported as
`idn/<machine>/m/audit.modbus` / `audit.action`. Local MQTT: devices publish on the
machine, the application sees `idn/<machine>/local/<topic>` and publishes to
`idn/<machine>/to/<topic>`. `examples/modbus_sim` is a Modbus device for trying it.

### SQL (spec §I.10.6)

Named, read-only queries against a database through ODBC (a batch program's SQL Server
Express, …), each on a schedule or on request, the rows sent as messages of their own
type. The SQL is the site's; the module refuses anything but one SELECT/WITH and rolls
every query back.

```toml
[sql]
zone = "America/New_York"

[[sql.connection]]              # only in the machine's own files
name = "batch"
driver = "ODBC Driver 17 for SQL Server"
server = "localhost\\SQLEXPRESS"
database = "Batch"
as = "user"                     # user (the logged-in Windows user) | system | account
# account = "PLANT\\idnreader"    as = "account": a Windows account, or
# login = "idn_reader"           a SQL login; password = "${secret:batch}"

[[sql.query]]
name = "silos"
connection = "batch"
kind = "snapshot"               # snapshot | latest | cursor | request
type = "scada.silos"
version = 1
every_s = 60
sql = "SELECT …"
```

Each query's state (`ok`, `unavailable`, `timeout`, `error`, `waiting`, `refused`) is on
the panel and sent as `sql/<query>/state`. On request: the command `sql.query`
`{"query", "params"}`. Queries run in `idn-industrial sql-worker` processes, started as
the connection's identity and ended if a query hangs. Off Windows, ODBC needs the `odbc`
feature (unixODBC); `user` and Windows accounts are Windows only.

## The Node

Windows: `idn-industrial-setup-x64.exe` (asks for the token; `/S /TOKEN=IDNI1-…`
unattended). Linux: `idn-industrial.service`. Then `idn-industrial enrol
IDNI1-…` or the panel at `http://127.0.0.1:29208`. Settings in `node.toml` in its state
folder (`name`, `[health] processes = […]`, `services = […]`); the local API is described
in `installer/windows/industrial/README-industrial.txt`.

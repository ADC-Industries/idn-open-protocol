IDN Node | Industrial
=====================

This machine's link to its company's IDN Site Manager | Industrial. It runs as the
Windows service "IDN Node | Industrial" (idn-industrial) from boot.

Joining the site
  The installer asks for an enrolment token (IDNI1-...), minted in the Site Manager's web
  page (Tokens). Each token joins one machine once. Later, or again with a new token:
      the panel at http://127.0.0.1:29208
      or: "C:\Program Files\IDN Node Industrial\idn-industrial.exe" enrol IDNI1-...

For this machine's programs (any language)
  The local API at http://127.0.0.1:29208, with the token in
  C:\ProgramData\IDN Node Industrial\api.token as "Authorization: Bearer <token>":
      POST /messages   {"type": "batch.ticket", "payload": {...}}
      POST /values     [{"name": "silo/3/level", "value": 72.5}]
      POST /files?name=report.pdf   (the file as the body)
      POST /requests   {"type": "customers.lookup", "payload": "C100", "timeout_s": 30}
      GET  /commands   (waits up to 25 s for the next command from the Site Manager)
      POST /commands/<id>   {"ok": true, "payload": ...}
      GET  /status
  Everything sent is kept on disk until the Site Manager has it, across restarts and
  network outages.

Settings
  Every setting is in the panel's forms (http://127.0.0.1:29208, Settings and Secrets);
  they are kept in C:\ProgramData\IDN Node Industrial\node.toml, for example:
      name = "Plant 1 batch"
      [health]
      processes = ["batching.exe"]
      services = ["MSSQLSERVER"]
  The panel says when a change needs the service restarted.

Modules (Modbus TCP, HTTP polling, webhooks, actions, the local MQTT endpoint)
  In C:\ProgramData\IDN Node Industrial\modules\*.toml (this machine's own), and
  what the Site Manager pushes (kept in site\). Only this machine's own files can name
  actions or make Modbus points writable. Local MQTT: 127.0.0.1:1883, no login.
  Secrets for HTTP headers: secrets.toml (NAME = "..."), used as ${secret:NAME}.
  See the IDN Industrial read-me for the settings.

Uninstalling keeps the machine's identity and anything not yet sent in
C:\ProgramData\IDN Node Industrial: installed again, it is the same machine.

IDN Site Manager | Industrial (if chosen in the installer: the company's server)
  The Windows service "IDN Site Manager | Industrial" (idn-site-manager-industrial).
  Its web UI: http://<this computer>:8080/ (the port chosen), the administrator password
  set in the installer. Machines connect on the same port (WebSocket) and UDP 29206
  (QUIC); the application server's MQTT broker is TCP 1883; Voice's calls UDP 29207.
  Windows Firewall lets the program in.
  Its data (the site's CA and keys, machines, waiting items):
      C:\ProgramData\IDN Site Manager Industrial   (back it up; kept when uninstalled)
  Its settings: settings.env there (IDN_PUBLIC_URLS: what tokens tell machines; restart
  the service after a change). The API key for applications: api.key there.
  Backup, with the service stopped (an administrator's command prompt):
      sc stop idn-site-manager-industrial
      "C:\Program Files\IDN Node Industrial\idn-site-manager-industrial.exe" backup site.idnsite
      sc start idn-site-manager-industrial
  ("restore site.idnsite --replace" the same way.) With IDN Node | Industrial on this
  computer too, the Node's local MQTT is 127.0.0.1:1885 (the broker has 1883).

# idn-asio

IDN as an ASIO driver on Windows, and on Linux for cwASIO hosts
(spec/extensions/idn-node.md §N.14): "IDN" in any ASIO program's list of drivers (Reaper,
Cubase, Ableton Live, vMix, …), with no sound card and no kernel driver.

- The program's **outputs** are an IDN source named after the program, `HOST (Reaper)`:
  route them anywhere in the IDN Site Manager's routing matrix (or `idn route`).
- Its **inputs** are a routable receiver of the same name on this machine's IDN Node:
  whatever is routed to it plays there (each transmitter through its own buffer and drift
  correction; by site time where the site clock runs). Without a Node running, the
  outputs still work and the inputs are silent.
- 16 inputs and 16 outputs by default (up to 64 each), 44.1, 48, 88.2 and 96 kHz, 32-bit
  float, buffers of 32 to 2048 frames (preferred 256). The driver's own high-priority
  thread is the clock.

Settings, all optional, in `%LOCALAPPDATA%\idn-open-protocol\asio.json`:

```json
{ "inputs": 8, "outputs": 32, "name": "Studio A", "latency_ms": 10 }
```

`name` defaults to the program's name; `latency_ms` is each input's buffer (sound from the
network). The program's ASIO settings button opens the IDN Node's panel.

## Install

The IDN installer copies `idn_asio.dll` into `C:\Program Files\IDN` and registers it. By
hand, as administrator: `regsvr32 idn_asio.dll` (and `regsvr32 /u idn_asio.dll` to remove
it). It must stay next to `idn-node.exe` for the settings button.

More instances (spec/extensions/asio-interface.md §A.6) on Windows: from a terminal
opened as administrator, `idn asio register "IDN 2"` adds the key
`HKLM\SOFTWARE\ASIO\IDN 2` with IDN's class id; `idn asio unregister "IDN 2"` takes it
off (`--all`: every name of IDN's driver, the installer's "IDN" too). Hosts that name the
instance (cwASIO's `kcwASIOsetInstanceName`) get its settings and source; others get the
default instance under any name. `regsvr32 /u` (uninstalling) takes off every name.

On Linux (cwASIO's conventions, spec/extensions/asio-interface.md §A.5) the driver is
`libidn_asio.so`, for any cwASIO host. The idn-media package installs it in
`/usr/lib/cwASIO/` and lists it as "IDN" in `/etc/cwASIO`. By hand:

```sh
sudo idn asio register            # as "IDN"
sudo idn asio register "IDN 2"    # another instance (§A.6)
idn asio list
sudo idn asio unregister --all    # every listing of IDN's driver
```

The settings file is the same `asio.json`, in `~/.local/state/idn-open-protocol` (or
`$XDG_STATE_HOME/idn-open-protocol`); the settings button runs `idn-node open`. Each
instance other than "IDN" has its own settings under `"instances"`, and its own source
on the network (`PROGRAM NAME`):

```json
{ "inputs": 16, "outputs": 16, "instances": { "IDN 2": { "outputs": 4 } } }
```

## Build

All Rust: the driver object and its class factory (`src/object.rs`) on IDN's own ASIO
interface definitions (`core/idn-asio-abi`, written from cwASIO's MIT headers;
spec/extensions/asio-interface.md). No Steinberg SDK and no C++: build for
`x86_64-pc-windows-gnu`; the DLL needs only Windows' own DLLs. On Linux the same object
is built with cwASIO's entry points (`src/linux.rs`) instead of COM's. Its tests run the
driver with a stand-in program (`tests/driver.rs`), as a cwASIO host sees it
(`tests/linux_cwasio.rs`), and through a Node's routing
(`media/idn-node/tests/routing.rs`).

ASIO is a trademark and software of Steinberg Media Technologies GmbH.

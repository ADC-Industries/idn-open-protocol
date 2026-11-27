# Code conventions

What every crate here follows (set 2026-11-02; `cargo fmt` and clippy cover the rest).

## GPU helpers

One copy of what every program on the GPU needs, in `idn_open_protocol::gpu`
(`gpu/pool.rs`): `block_on` for wgpu's requests, `open_device` for a device on the best
GPU (Direct3D 12 on Windows), `Buffers` for buffers handed out as pictures and taken back
once nothing holds them, `Staging` for staging buffers kept mapped for writing. Use them;
don't write another.

## Errors

- **`io::Result`** for anything that does I/O or touches a device: files, the network, the
  GPU, sound cards, child processes. An error's text says what failed, for a log or a
  person (`io::Error::other(format!(...))`, or the kind that fits).
- **`Result<T, String>`** only where what a person typed or chose is checked (a setting,
  a PIN, a feed's text, a panel request), the `String` being the message shown to them.
- A program's own code may turn an `io::Error` into its message (`.map_err(|e|
  e.to_string())`) where it answers a person; the library doesn't.

## Environment variables

Every one is listed in `idn_common::switches::ALL` (re-exported as
`idn_open_protocol::switches`) with its kind and what it does; a test fails for any
`IDN_*` name in the code that isn't. Switches are read with `switches::on` / `off` and
all take the same values: `0`, `off`, `false` or `no` is off, anything else set is on. A
`Default` switch is on unless set off (for comparing against the old way); an `Opt` one
off unless set on. Names: `IDN_<PROGRAM>_<WHAT>` for one program's, `IDN_<WHAT>` for the
library's.

## Shared helpers

One copy of each small thing the programs kept writing again (consolidation, 2026-11-26).
Use these; don't write another. Below the library, in `idn_common`:

- **Locks**: `sync::LockOk` (`lock_ok()`, `try_lock_ok()`) and `RwLockOk` (`read_ok()`,
  `write_ok()`): never `.lock().unwrap()` or `unwrap_or_else(|e| e.into_inner())`.
- **On or off**: `text::parse_on_off(v)` (on/off, true/false, yes/no, 1/0, any case; `""`
  is "not set", `None`) for anything a person or a file says; `text::on_off_setting(key,
  v)` for a setting (empty is off; the error names the key). Switches:
  `switches::on`/`off`/`says_off`, the same words.
- **Hex**: `hex::hex` (lower case), `hex::unhex`, `hex::hex_upper_colons` (a fingerprint
  as people compare it). Never a `format!("{b:02x}")` loop.
- **`io::Error`s**: `err::other`, `err::invalid_data`, `err::invalid_input` (anything that
  prints). A module that adds a prefix wraps one in a one-line `fn bad`.
- **Time since 1970**: `epoch::secs()`, `epoch::millis()`, `epoch::now()`.
- **Saved files**: `fsutil::write` (whole or not at all, on disk), `write_private`,
  `write_quick`; read back with `fsutil::load_json` (serde) or `fsutil::load_saved` (any
  parser): a damaged file is set aside as `NAME.bad-SECS-PID-N` (the newest 5 kept) and
  the `Err` logged, never read as "none" and then saved over. A file only people write and
  the program never saves (`asio.json`): `fsutil::load_json_readonly`, never moved, the
  `Err` (file, line, column) logged and the defaults used. A secret's file cleared:
  `fsutil::remove_set_asides` too.
- **Network**: `net::local_addr_towards(peer)` / `local_v4_towards` (the address a peer
  reaches this machine by: reply and advertise with it), `net::outgoing_ip()`.
- **Retries**: `backoff::Backoff::new(initial, max)` (`.factor`, `.jitter`; `next()`
  each failure, `reset()` on success) for every wait that grows.
- **Logging**: `log::set_logger` / `log::log` is the one logger (the library's
  `idn_open_protocol::log` is it); `log::log_or_stderr` only for lines that must not
  vanish in a program that installs none. Library code never prints.
- **Windows**: `win::wide` / `wide_os` (NUL-ended UTF-16), `win::reg_sz` / `reg_dword`,
  `win::windows_build()`.
- **Tests**: `testing::TempDir::new(name)` (a folder of the test's own, removed on drop),
  `testing::wait_for(what, within, f)` (fails naming `what`), `wait_until`, `test_port`.

In `idn_open_protocol`:

- **Random**: `random::secure_random::<N>()` is the one source (keys, tokens, ids);
  `random::token_hex(n)`, `code_digits(n)` (uniform: never `% 1_000_000`),
  `code_base32(n)` (no 0/O, 1/I), `below(n)`.
- **SHA-256**: `hash::sha256`, `hash::sha256_hex` (streaming hashers stay `sha2`'s).
- **TLS**: `secure::crypto_provider()` for every rustls config (never
  `ring::default_provider()`); `secure::ed25519_tls13_signatures!(algs)` for a verifier's
  three signature methods.
- **Web** (`web`): `bearer(&headers)` (never `strip_prefix("Bearer ")`), `cookie(&headers,
  name)` (every Cookie header), `set_cookie(name, value)…` for every `Set-Cookie`,
  `split_host` (IPv6 brackets) and `is_loopback_name` (never a list of names), `Logins`
  (failed logins per address and name: count with `attempt`/`attempt_as` before the
  check), `PinLock` (a PIN's wrong tries: `check(|| …)`), `PasswordChecks` (the cap on
  Argon2 checks at once: `run(..).await`, `run_blocking`, `shared()`). These take any
  `web::Headers` (the library's `Request`, an axum `HeaderMap`).
- **Updates**: `update::fetch_package` (with `FetchOptions`: pieces, retries, progress,
  `.part` file, size and SHA-256 checked; pass the piece fetcher) and `release::Store` (a
  Site Manager's kept releases: add, open, list, remove, pieces, uploads,
  `release::safe_version`).

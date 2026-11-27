//! The C ABI of the phone mixing library (`include/idn_iem.h`, idn-iem.md §I.8.6): for
//! Kotlin (through JNI) and Swift. An opaque handle; structured data as JSON (UTF-8,
//! NUL-terminated, freed with `idn_iem_string_free`); results as `IDN_IEM_*` codes (0 OK,
//! negative an error). No call unwinds into C: a panic is `IDN_IEM_PANIC`.
//!
//! Threads: every function may be called from any thread; `idn_iem_render` is for the
//! audio thread (it decodes there, and waits for no network).

use crate::receiver::{Config, Error, Event, Receiver, Transport, Trust};
use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

pub const IDN_IEM_OK: i32 = 0;
pub const IDN_IEM_BAD_ARGUMENT: i32 = -1;
pub const IDN_IEM_IO: i32 = -2;
pub const IDN_IEM_TLS: i32 = -3;
pub const IDN_IEM_HTTP: i32 = -4;
pub const IDN_IEM_TOKEN_NEEDED: i32 = -5;
pub const IDN_IEM_TOKEN_REFUSED: i32 = -6;
pub const IDN_IEM_FULL: i32 = -7;
pub const IDN_IEM_BUSY: i32 = -8;
pub const IDN_IEM_NOT_ALLOWED: i32 = -9;
pub const IDN_IEM_BAD: i32 = -10;
pub const IDN_IEM_NO_FLOW: i32 = -11;
pub const IDN_IEM_NO_MULTICAST: i32 = -12;
pub const IDN_IEM_NO_MIX: i32 = -13;
pub const IDN_IEM_UNKNOWN_CHANNEL: i32 = -14;
pub const IDN_IEM_TIMEOUT: i32 = -15;
pub const IDN_IEM_DESK: i32 = -16;
pub const IDN_IEM_CONFIG: i32 = -17;
pub const IDN_IEM_CLOSED: i32 = -18;
pub const IDN_IEM_PANIC: i32 = -19;
pub const IDN_IEM_UNSUPPORTED: i32 = -20;

/// An error's code.
pub fn code(e: &Error) -> i32 {
    match e {
        Error::Config(_) => IDN_IEM_CONFIG,
        Error::Io(_) => IDN_IEM_IO,
        Error::Tls(_) => IDN_IEM_TLS,
        Error::Http(..) => IDN_IEM_HTTP,
        Error::TokenNeeded(_) => IDN_IEM_TOKEN_NEEDED,
        Error::TokenRefused(_) => IDN_IEM_TOKEN_REFUSED,
        Error::Full(_) => IDN_IEM_FULL,
        Error::Busy(_) => IDN_IEM_BUSY,
        Error::NotAllowed(_) => IDN_IEM_NOT_ALLOWED,
        Error::Bad(_) => IDN_IEM_BAD,
        Error::NoFlow => IDN_IEM_NO_FLOW,
        Error::NoMulticast => IDN_IEM_NO_MULTICAST,
        Error::NoMix(_) => IDN_IEM_NO_MIX,
        Error::UnknownChannel(_) => IDN_IEM_UNKNOWN_CHANNEL,
        Error::Timeout => IDN_IEM_TIMEOUT,
        Error::Desk(_) => IDN_IEM_DESK,
        Error::Closed => IDN_IEM_CLOSED,
    }
}

/// The handle C holds.
pub struct IdnIemReceiver {
    rx: Receiver,
    #[cfg(feature = "play")]
    player: std::sync::Mutex<Option<crate::play::Player>>,
}

/// Runs `f`, a panic as `IDN_IEM_PANIC`.
fn guard(f: impl FnOnce() -> i32) -> i32 {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(IDN_IEM_PANIC)
}

fn result(r: Result<(), Error>) -> i32 {
    match r {
        Ok(()) => IDN_IEM_OK,
        Err(e) => code(&e),
    }
}

/// A C string for the caller (freed with `idn_iem_string_free`).
fn give(s: String) -> *mut c_char {
    CString::new(s.replace('\0', " "))
        .map(CString::into_raw)
        .unwrap_or(std::ptr::null_mut())
}

/// `h` as a reference (null: none).
///
/// # Safety
/// `h` is null or from `idn_iem_connect`, not yet freed.
unsafe fn handle<'a>(h: *const IdnIemReceiver) -> Option<&'a IdnIemReceiver> {
    // SAFETY: as the caller promises.
    unsafe { h.as_ref() }
}

/// # Safety
/// `s` is null or a NUL-terminated string.
unsafe fn text<'a>(s: *const c_char) -> Option<&'a str> {
    if s.is_null() {
        return None;
    }
    // SAFETY: as the caller promises.
    unsafe { CStr::from_ptr(s) }.to_str().ok()
}

/// Connects (blocking up to the configuration's `connect_timeout_ms`, 5 s by default).
/// `config_json`: see `Config::from_json` (`{"desk": "HOST[:PORT]", "token": "…", "mix":
/// "…", "transport": "auto"|"unicast"|"multicast", "talk": true, "buffer_ms": 15, …}`);
/// `ca_der`/`ca_len`: a certificate to pin (the desk's or the site CA's, DER), or null
/// for plain HTTP (the desk's own machine) or `ca_sha256` in the JSON. On failure: null,
/// and the error's code in `*err_code` and its words in `err` (when given).
///
/// # Safety
/// `config_json` NUL-terminated; `ca_der` null or `ca_len` readable bytes; `err` null or
/// `err_len` writable bytes; `err_code` null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_iem_connect(
    config_json: *const c_char,
    ca_der: *const u8,
    ca_len: usize,
    err_code: *mut i32,
    err: *mut c_char,
    err_len: usize,
) -> *mut IdnIemReceiver {
    let say = |code: i32, msg: &str| {
        // SAFETY: as the caller promises.
        unsafe {
            if !err_code.is_null() {
                *err_code = code;
            }
            if !err.is_null() && err_len > 0 {
                let b = msg.as_bytes();
                let n = b.len().min(err_len - 1);
                std::ptr::copy_nonoverlapping(b.as_ptr(), err.cast::<u8>(), n);
                *err.add(n) = 0;
            }
        }
    };
    let r = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: as the caller promises.
        let json = unsafe { text(config_json) }
            .ok_or_else(|| Error::Config("config_json: UTF-8 text".into()))?;
        let v: serde_json::Value =
            serde_json::from_str(json).map_err(|e| Error::Config(format!("config_json: {e}")))?;
        let mut cfg = Config::from_json(&v)?;
        if !ca_der.is_null() && ca_len > 0 {
            // SAFETY: as the caller promises.
            let der = unsafe { std::slice::from_raw_parts(ca_der, ca_len) };
            cfg.trust = Some(Trust::Certificate(der.to_vec()));
        }
        Receiver::connect(cfg)
    }));
    match r {
        Ok(Ok(rx)) => {
            say(IDN_IEM_OK, "");
            Box::into_raw(Box::new(IdnIemReceiver {
                rx,
                #[cfg(feature = "play")]
                player: std::sync::Mutex::new(None),
            }))
        }
        Ok(Err(e)) => {
            say(code(&e), &e.to_string());
            std::ptr::null_mut()
        }
        Err(_) => {
            say(IDN_IEM_PANIC, "panic");
            std::ptr::null_mut()
        }
    }
}

/// Ends the session and frees the handle.
///
/// # Safety
/// `h` is null or from `idn_iem_connect`, freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_iem_free(h: *mut IdnIemReceiver) {
    if h.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: as the caller promises.
        let b = unsafe { Box::from_raw(h) };
        b.rx.close();
        drop(b);
    }));
}

/// Fills `out` with `frames` frames of the mix: stereo interleaved f32, 48 kHz.
///
/// # Safety
/// `h` valid; `out` writable for `2 * frames` floats.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_iem_render(
    h: *const IdnIemReceiver,
    out: *mut f32,
    frames: usize,
) -> i32 {
    guard(|| {
        // SAFETY: as the caller promises.
        let Some(h) = (unsafe { handle(h) }) else {
            return IDN_IEM_BAD_ARGUMENT;
        };
        if out.is_null() {
            return IDN_IEM_BAD_ARGUMENT;
        }
        // SAFETY: as the caller promises.
        let buf = unsafe { std::slice::from_raw_parts_mut(out, frames * 2) };
        h.rx.render(buf);
        IDN_IEM_OK
    })
}

macro_rules! setter {
    ($name:ident, $t:ty, $method:ident, $conv:expr) => {
        /// # Safety
        /// `h` valid.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(h: *const IdnIemReceiver, ch: u16, v: $t) -> i32 {
            guard(|| {
                // SAFETY: as the caller promises.
                match unsafe { handle(h) } {
                    Some(h) => result(h.rx.$method(ch, $conv(v))),
                    None => IDN_IEM_BAD_ARGUMENT,
                }
            })
        }
    };
}

setter!(idn_iem_set_level, f32, set_level, |v| v);
setter!(idn_iem_set_pan, f32, set_pan, |v| v);
setter!(idn_iem_set_mute, i32, set_mute, |v| v != 0);
setter!(idn_iem_set_solo, i32, set_solo, |v| v != 0);
setter!(idn_iem_set_in_mix, i32, set_in_mix, |v| v != 0);

/// The mix's master level (dB).
///
/// # Safety
/// `h` valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_iem_set_master(h: *const IdnIemReceiver, db: f32) -> i32 {
    guard(|| {
        // SAFETY: as the caller promises.
        match unsafe { handle(h) } {
            Some(h) => result(h.rx.set_master(db)),
            None => IDN_IEM_BAD_ARGUMENT,
        }
    })
}

fn json_of(
    h: *const IdnIemReceiver,
    f: impl FnOnce(&Receiver) -> serde_json::Value,
) -> *mut c_char {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the callers' callers promise `h` valid.
        unsafe { handle(h) }.map_or(std::ptr::null_mut(), |h| give(f(&h.rx).to_string()))
    }))
    .unwrap_or(std::ptr::null_mut())
}

/// The mix as JSON (`{"id", "name", "master_db", "channels": {"<id>": {"level_db",
/// "pan", "mute", "solo"}}, "talkback_input", "talking"}`), or `null` before the desk's
/// state came.
///
/// # Safety
/// `h` valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_iem_mix_json(h: *const IdnIemReceiver) -> *mut c_char {
    json_of(h, |rx| {
        rx.mix().map_or(serde_json::Value::Null, |m| m.to_json())
    })
}

/// The flow's channels as JSON: `[{"id", "name", "color", "pair", "side"}]`.
///
/// # Safety
/// `h` valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_iem_channels_json(h: *const IdnIemReceiver) -> *mut c_char {
    json_of(h, |rx| {
        serde_json::Value::Array(rx.channels().iter().map(|c| c.to_json()).collect())
    })
}

/// The status as JSON (connected, transport, buffer_ms, loss_pct, latency_ms, …).
///
/// # Safety
/// `h` valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_iem_status_json(h: *const IdnIemReceiver) -> *mut c_char {
    json_of(h, |rx| rx.status().to_json())
}

/// The next event as JSON (`{"type": …}`), waiting up to `timeout_ms`; null for none.
///
/// # Safety
/// `h` valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_iem_next_event(
    h: *const IdnIemReceiver,
    timeout_ms: u32,
) -> *mut c_char {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: as the caller promises.
        let Some(h) = (unsafe { handle(h) }) else {
            return std::ptr::null_mut();
        };
        h.rx.next_event(Duration::from_millis(u64::from(timeout_ms)))
            .map_or(std::ptr::null_mut(), |e| give(e.to_json().to_string()))
    }))
    .unwrap_or(std::ptr::null_mut())
}

/// The C side's callback and its pointer.
struct Callback {
    f: extern "C" fn(*mut c_void, *const c_char),
    user: *mut c_void,
}

impl Callback {
    fn call(&self, e: &Event) {
        if let Ok(s) = CString::new(e.to_json().to_string()) {
            (self.f)(self.user, s.as_ptr());
        }
    }
}

// SAFETY: the caller of `idn_iem_set_event_callback` promises the callback may be called
// from any thread with its pointer.
unsafe impl Send for Callback {}
// SAFETY: as above.
unsafe impl Sync for Callback {}

/// Events to `cb(user, json)` as they happen, on the library's threads (the string is
/// valid during the call only); null back to `idn_iem_next_event`'s queue.
///
/// # Safety
/// `h` valid; `cb` callable from any thread with `user` until replaced or freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_iem_set_event_callback(
    h: *const IdnIemReceiver,
    cb: Option<extern "C" fn(*mut c_void, *const c_char)>,
    user: *mut c_void,
) -> i32 {
    guard(|| {
        // SAFETY: as the caller promises.
        let Some(h) = (unsafe { handle(h) }) else {
            return IDN_IEM_BAD_ARGUMENT;
        };
        let f = cb.map(|f| {
            let c = Callback { f, user };
            Box::new(move |e: &Event| c.call(e)) as crate::receiver::EventCallback
        });
        h.rx.set_event_callback(f);
        IDN_IEM_OK
    })
}

/// Push-to-talk pressed.
///
/// # Safety
/// `h` valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_iem_talk_start(h: *const IdnIemReceiver) -> i32 {
    guard(|| {
        // SAFETY: as the caller promises.
        unsafe { handle(h) }.map_or(IDN_IEM_BAD_ARGUMENT, |h| result(h.rx.talk_start()))
    })
}

/// The microphone's samples (`frames` frames of `channels` interleaved f32 at `rate`).
///
/// # Safety
/// `h` valid; `pcm` readable for `frames * channels` floats.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_iem_talk_feed(
    h: *const IdnIemReceiver,
    pcm: *const f32,
    frames: usize,
    rate: u32,
    channels: u16,
) -> i32 {
    guard(|| {
        // SAFETY: as the caller promises.
        let Some(h) = (unsafe { handle(h) }) else {
            return IDN_IEM_BAD_ARGUMENT;
        };
        if pcm.is_null() || channels == 0 {
            return IDN_IEM_BAD_ARGUMENT;
        }
        // SAFETY: as the caller promises.
        let s = unsafe { std::slice::from_raw_parts(pcm, frames * usize::from(channels)) };
        result(h.rx.talk_feed(s, rate, channels))
    })
}

/// Push-to-talk released.
///
/// # Safety
/// `h` valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_iem_talk_stop(h: *const IdnIemReceiver) -> i32 {
    guard(|| {
        // SAFETY: as the caller promises.
        unsafe { handle(h) }.map_or(IDN_IEM_BAD_ARGUMENT, |h| result(h.rx.talk_stop()))
    })
}

/// The sound output's latency (ms), for the status's `latency_ms`.
///
/// # Safety
/// `h` valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_iem_set_output_latency_ms(h: *const IdnIemReceiver, ms: f32) -> i32 {
    guard(|| {
        // SAFETY: as the caller promises.
        unsafe { handle(h) }.map_or(IDN_IEM_BAD_ARGUMENT, |h| {
            h.rx.set_output_latency_ms(ms);
            IDN_IEM_OK
        })
    })
}

/// A new token (the old one expiring).
///
/// # Safety
/// `h` valid; `token` NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_iem_set_token(h: *const IdnIemReceiver, token: *const c_char) -> i32 {
    guard(|| {
        // SAFETY: as the caller promises.
        match (unsafe { handle(h) }, unsafe { text(token) }) {
            (Some(h), Some(t)) => {
                h.rx.set_token(t);
                IDN_IEM_OK
            }
            _ => IDN_IEM_BAD_ARGUMENT,
        }
    })
}

/// 0 auto, 1 unicast, 2 multicast.
///
/// # Safety
/// `h` valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_iem_set_transport(h: *const IdnIemReceiver, transport: i32) -> i32 {
    guard(|| {
        // SAFETY: as the caller promises.
        let Some(h) = (unsafe { handle(h) }) else {
            return IDN_IEM_BAD_ARGUMENT;
        };
        let t = match transport {
            0 => Transport::Auto,
            1 => Transport::Unicast,
            2 => Transport::Multicast,
            _ => return IDN_IEM_BAD_ARGUMENT,
        };
        result(h.rx.set_transport(t))
    })
}

/// Plays the mix on a sound card (`device` null: the default) with the library's own
/// audio output (built with the `play` feature; else `IDN_IEM_UNSUPPORTED`).
///
/// # Safety
/// `h` valid; `device` null or NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_iem_play_start(
    h: *const IdnIemReceiver,
    device: *const c_char,
) -> i32 {
    guard(|| {
        // SAFETY: as the caller promises.
        let Some(h) = (unsafe { handle(h) }) else {
            return IDN_IEM_BAD_ARGUMENT;
        };
        #[cfg(feature = "play")]
        {
            use crate::receiver::LockOk as _;
            // SAFETY: as the caller promises.
            let device = unsafe { text(device) };
            match crate::play::Player::start(&h.rx, device) {
                Ok(p) => {
                    *h.player.lock_ok() = Some(p);
                    IDN_IEM_OK
                }
                Err(_) => IDN_IEM_IO,
            }
        }
        #[cfg(not(feature = "play"))]
        {
            let _ = (h, device);
            IDN_IEM_UNSUPPORTED
        }
    })
}

/// Stops `idn_iem_play_start`'s output.
///
/// # Safety
/// `h` valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_iem_play_stop(h: *const IdnIemReceiver) -> i32 {
    guard(|| {
        // SAFETY: as the caller promises.
        let Some(h) = (unsafe { handle(h) }) else {
            return IDN_IEM_BAD_ARGUMENT;
        };
        #[cfg(feature = "play")]
        {
            use crate::receiver::LockOk as _;
            h.player.lock_ok().take();
        }
        #[cfg(not(feature = "play"))]
        let _ = h;
        IDN_IEM_OK
    })
}

/// Frees a string the library gave.
///
/// # Safety
/// `s` null or from this library, freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_iem_string_free(s: *mut c_char) {
    if !s.is_null() {
        // SAFETY: as the caller promises.
        drop(unsafe { CString::from_raw(s) });
    }
}

/// The library's version (static; not freed).
#[unsafe(no_mangle)]
pub extern "C" fn idn_iem_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

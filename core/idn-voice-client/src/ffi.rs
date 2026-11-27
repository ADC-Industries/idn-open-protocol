//! The C ABI (`include/idn_voice_client.h`): for an app's JNI wrapper. An opaque handle;
//! the configuration and commands as JSON; events to a callback as JSON; sound as 16-bit
//! samples. Results: `IDN_VOICE_OK` (0) or a negative `IDN_VOICE_*` code. No call
//! unwinds into C: a panic is `IDN_VOICE_PANIC` (or a null handle).
//!
//! Threads: every function may be called from any thread, at the same time as any other,
//! except `idn_voice_close`, which must be the handle's last call (no other on it under
//! way or after). `idn_voice_pull_speaker_i16` is for the audio output thread. The
//! callback runs on the library's threads, one event at a time; it must not block, and
//! its string is valid only during the call.

use crate::client::Client;
use crate::config::Config;
use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};

pub const IDN_VOICE_OK: i32 = 0;
pub const IDN_VOICE_BAD_ARGUMENT: i32 = -1;
pub const IDN_VOICE_BAD_JSON: i32 = -2;
pub const IDN_VOICE_BAD_RATE: i32 = -3;
pub const IDN_VOICE_PANIC: i32 = -4;
/// The Opus encoder failed (not the caller's input).
pub const IDN_VOICE_CODEC: i32 = -5;

/// The callback: `user_data` as given, and one event's JSON (UTF-8, NUL-terminated; valid
/// during the call only).
pub type IdnVoiceEventCallback =
    Option<unsafe extern "C" fn(user_data: *mut c_void, event_json: *const c_char)>;

/// The handle C holds.
pub struct IdnVoiceClient {
    client: Client,
}

/// The app's callback and its pointer, called from the library's threads (the app says
/// they may be, by giving them).
struct Callback {
    f: unsafe extern "C" fn(*mut c_void, *const c_char),
    user: *mut c_void,
}

// SAFETY: the C side promises the callback and its data may be used from any thread
// (the header says so); the library calls it one event at a time.
unsafe impl Send for Callback {}
// SAFETY: as above.
unsafe impl Sync for Callback {}

impl Callback {
    fn call(&self, json: &str) {
        let Ok(s) = CString::new(json.replace('\0', " ")) else {
            return;
        };
        // SAFETY: the function and pointer the app gave; the string lives through the call.
        unsafe { (self.f)(self.user, s.as_ptr()) }
    }
}

fn guard(f: impl FnOnce() -> i32) -> i32 {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(IDN_VOICE_PANIC)
}

/// # Safety
/// `h` is null or from `idn_voice_connect`, not yet closed.
unsafe fn handle<'a>(h: *const IdnVoiceClient) -> Option<&'a Client> {
    // SAFETY: as the caller promises.
    unsafe { h.as_ref() }.map(|h| &h.client)
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

/// Starts a client (returns at once; it connects in the background). `config_json`: see
/// `Config::from_json`. Events go to `callback` (may be null: none). On a configuration
/// that can't work: null, and the callback is called once, before this returns, with
/// `{"event": "error", "what": "config", "error": "…"}`.
///
/// # Safety
/// `config_json` NUL-terminated; `callback` null or a function that may be called from
/// any thread with `user_data` until `idn_voice_close` returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_voice_connect(
    config_json: *const c_char,
    callback: IdnVoiceEventCallback,
    user_data: *mut c_void,
) -> *mut IdnVoiceClient {
    let cb = callback.map(|f| Callback { f, user: user_data });
    let r = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: as the caller promises.
        let cfg = match unsafe { text(config_json) } {
            Some(t) => Config::from_json(t),
            None => Err("config: null or not UTF-8".to_owned()),
        };
        let cfg = match cfg {
            Ok(c) => c,
            Err(e) => {
                if let Some(cb) = &cb {
                    let ev = serde_json::json!({"event": "error", "what": "config", "error": e});
                    cb.call(&ev.to_string());
                }
                return std::ptr::null_mut();
            }
        };
        let events = move |e: crate::Event| {
            if let Some(cb) = &cb {
                cb.call(&e.to_json().to_string());
            }
        };
        match Client::connect(cfg, events) {
            Ok(client) => Box::into_raw(Box::new(IdnVoiceClient { client })),
            Err(_) => std::ptr::null_mut(),
        }
    }));
    r.unwrap_or(std::ptr::null_mut())
}

/// Any command as JSON (`{"t": "join", "channel": 3}`; docs/VOICE-CLIENT.md §4.3).
///
/// # Safety
/// `h` from `idn_voice_connect`, not closed; `json` NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_voice_send_json(h: *const IdnVoiceClient, json: *const c_char) -> i32 {
    guard(|| {
        // SAFETY: as the caller promises.
        let (Some(c), Some(j)) = (unsafe { handle(h) }, unsafe { text(json) }) else {
            return IDN_VOICE_BAD_ARGUMENT;
        };
        match c.send_json(j) {
            Ok(()) => IDN_VOICE_OK,
            Err(_) => IDN_VOICE_BAD_JSON,
        }
    })
}

/// Push-to-talk on a channel: `on` non-zero starts talking there, zero stops (the channel
/// is then not used).
///
/// # Safety
/// `h` from `idn_voice_connect`, not closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_voice_talk(h: *const IdnVoiceClient, channel: u32, on: i32) -> i32 {
    guard(|| {
        // SAFETY: as the caller promises.
        let Some(c) = (unsafe { handle(h) }) else {
            return IDN_VOICE_BAD_ARGUMENT;
        };
        if on != 0 {
            c.talk_start(channel);
        } else {
            c.talk_stop();
        }
        IDN_VOICE_OK
    })
}

/// The microphone: `samples` mono 16-bit samples at `rate` (48000; or 8000, 12000,
/// 16000, 24000). Sent while talking (held while the link is down, `hold_mic_s`, and sent
/// after the resume), dropped otherwise.
///
/// # Safety
/// `h` from `idn_voice_connect`, not closed; `pcm` null (with 0 samples) or `samples`
/// readable samples.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_voice_push_mic_i16(
    h: *const IdnVoiceClient,
    pcm: *const i16,
    samples: usize,
    rate: u32,
) -> i32 {
    guard(|| {
        // SAFETY: as the caller promises.
        let Some(c) = (unsafe { handle(h) }) else {
            return IDN_VOICE_BAD_ARGUMENT;
        };
        let pcm = if samples == 0 {
            &[][..]
        } else if pcm.is_null() {
            return IDN_VOICE_BAD_ARGUMENT;
        } else {
            // SAFETY: as the caller promises.
            unsafe { std::slice::from_raw_parts(pcm, samples) }
        };
        match c.push_mic_i16(pcm, rate) {
            Ok(()) => IDN_VOICE_OK,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => IDN_VOICE_BAD_RATE,
            Err(_) => IDN_VOICE_CODEC,
        }
    })
}

/// The speaker: fills `out` with `samples` mono 16-bit samples at 48 kHz (silence when
/// nobody talks). For the audio output thread.
///
/// # Safety
/// `h` from `idn_voice_connect`, not closed; `out` `samples` writable samples.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_voice_pull_speaker_i16(
    h: *const IdnVoiceClient,
    out: *mut i16,
    samples: usize,
) -> i32 {
    guard(|| {
        // SAFETY: as the caller promises.
        let Some(c) = (unsafe { handle(h) }) else {
            return IDN_VOICE_BAD_ARGUMENT;
        };
        if samples == 0 {
            return IDN_VOICE_OK;
        }
        if out.is_null() {
            return IDN_VOICE_BAD_ARGUMENT;
        }
        // SAFETY: as the caller promises.
        let out = unsafe { std::slice::from_raw_parts_mut(out, samples) };
        c.pull_speaker_i16(out);
        IDN_VOICE_OK
    })
}

/// The device's network changed (Android's `ConnectivityManager.NetworkCallback`).
///
/// # Safety
/// `h` from `idn_voice_connect`, not closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_voice_network_changed(h: *const IdnVoiceClient) -> i32 {
    guard(|| {
        // SAFETY: as the caller promises.
        let Some(c) = (unsafe { handle(h) }) else {
            return IDN_VOICE_BAD_ARGUMENT;
        };
        c.network_changed();
        IDN_VOICE_OK
    })
}

/// The session to come back to after a restart, as JSON `{"resume", "have"}` (merge it
/// into the next configuration), or null while there is no token (none in the configuration
/// and no `hello` yet). Freed with
/// `idn_voice_free_string`.
///
/// # Safety
/// `h` from `idn_voice_connect`, not closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_voice_resume_state(h: *const IdnVoiceClient) -> *mut c_char {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: as the caller promises.
        let r = unsafe { handle(h) }.and_then(Client::resume_state);
        match r {
            Some(r) => {
                let v = serde_json::json!({"resume": r.token, "have": r.have});
                CString::new(v.to_string())
                    .map(CString::into_raw)
                    .unwrap_or(std::ptr::null_mut())
            }
            None => std::ptr::null_mut(),
        }
    }))
    .unwrap_or(std::ptr::null_mut())
}

/// Signs off and frees the handle: the handle's last call. No callback comes after it
/// returns. It may be called from the callback (it then doesn't wait for the `bye`).
///
/// # Safety
/// `h` null or from `idn_voice_connect`, not closed, and no other call on it under way.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_voice_close(h: *mut IdnVoiceClient) {
    if h.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: as the caller promises: from idn_voice_connect, closed once.
        let b = unsafe { Box::from_raw(h) };
        b.client.close();
        drop(b);
    }));
}

/// Frees a string the library returned.
///
/// # Safety
/// `s` null or from this library, freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn idn_voice_free_string(s: *mut c_char) {
    if !s.is_null() {
        // SAFETY: as the caller promises.
        drop(unsafe { CString::from_raw(s) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LockOk as _;
    use std::sync::Mutex;

    static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());

    unsafe extern "C" fn record(_: *mut c_void, json: *const c_char) {
        // SAFETY: the library's string, valid during the call.
        let s = unsafe { CStr::from_ptr(json) }
            .to_string_lossy()
            .into_owned();
        SEEN.lock_ok().push(s);
    }

    #[test]
    fn a_bad_configuration_is_null_and_says_why_and_null_handles_are_refused() {
        let cfg = CString::new(r#"{"credential": "c"}"#).unwrap();
        // SAFETY: valid strings and a callback that may run anywhere.
        let h = unsafe { idn_voice_connect(cfg.as_ptr(), Some(record), std::ptr::null_mut()) };
        assert!(h.is_null());
        let seen = SEEN.lock_ok().clone();
        assert!(
            seen.iter().any(|s| s.contains("\"what\":\"config\"")),
            "{seen:?}"
        );
        // SAFETY: null handles, checked.
        unsafe {
            assert_eq!(
                idn_voice_talk(std::ptr::null(), 3, 1),
                IDN_VOICE_BAD_ARGUMENT
            );
            assert_eq!(
                idn_voice_send_json(std::ptr::null(), cfg.as_ptr()),
                IDN_VOICE_BAD_ARGUMENT
            );
            assert!(idn_voice_resume_state(std::ptr::null()).is_null());
            idn_voice_close(std::ptr::null_mut());
            idn_voice_free_string(std::ptr::null_mut());
        }
    }

    #[test]
    fn a_handle_takes_commands_and_sound_before_it_connects_and_closes() {
        // Nothing listens there: it keeps trying; every call still answers.
        let cfg = CString::new(
            r#"{"credential": "idnv1.x.y", "urls": ["ws://127.0.0.1:9/voice"], "transport": "websocket"}"#,
        )
        .unwrap();
        // SAFETY: valid strings and a callback that may run anywhere.
        let h = unsafe { idn_voice_connect(cfg.as_ptr(), Some(record), std::ptr::null_mut()) };
        assert!(!h.is_null());
        let join = CString::new(r#"{"t": "join", "channel": 3}"#).unwrap();
        let not_json = CString::new("join 3").unwrap();
        let mut out = [1i16; 960];
        // SAFETY: the handle just made, buffers of the sizes given.
        unsafe {
            assert_eq!(idn_voice_send_json(h, join.as_ptr()), IDN_VOICE_OK);
            assert_eq!(
                idn_voice_send_json(h, not_json.as_ptr()),
                IDN_VOICE_BAD_JSON
            );
            assert_eq!(idn_voice_talk(h, 3, 1), IDN_VOICE_OK);
            assert_eq!(
                idn_voice_push_mic_i16(h, [0i16; 960].as_ptr(), 960, 48_000),
                IDN_VOICE_OK
            );
            assert_eq!(
                idn_voice_push_mic_i16(h, [0i16; 441].as_ptr(), 441, 44_100),
                IDN_VOICE_BAD_RATE
            );
            assert_eq!(
                idn_voice_pull_speaker_i16(h, out.as_mut_ptr(), out.len()),
                IDN_VOICE_OK
            );
            assert_eq!(idn_voice_talk(h, 3, 0), IDN_VOICE_OK);
            assert_eq!(idn_voice_network_changed(h), IDN_VOICE_OK);
            idn_voice_close(h);
        }
        assert!(out.iter().all(|&s| s == 0), "silence");
    }
}

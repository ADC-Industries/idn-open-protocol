//! ASIO sound cards (`IDN_AUDIO_HOST=asio`, Windows and Linux): IDN's own host, a cpal
//! custom host on idn-asio-abi (spec/extensions/asio-interface.md §A.4; from cwASIO's
//! headers, no Steinberg SDK). The drivers are the installed ones (Windows:
//! `HKLM\SOFTWARE\ASIO`; Linux: cwASIO's `/etc/cwASIO`), IDN's own ASIO driver left out
//! (its `init` joins the site: listing would make it come and go).
//!
//! ASIO calls its host back through plain functions with no context, so one driver runs at
//! a time in a program (as ASIO hosts do). One thread (the engine) loads it and makes every
//! call to it (Windows: COM, its apartment; window messages pumped for drivers that post
//! them); the
//! buffer switch, on the driver's own thread, converts between the driver's samples and
//! f32 for the streams. The driver's notices (a reset, a rate, latencies) and a panicking
//! stream only set atomics (nothing that locks or allocates on the audio thread), tagged
//! with the run they belong to; the engine looks at them every 20 ms and acts with the
//! driver stopped. An input and an output stream on the same driver share its buffers:
//! opening or closing one sets the buffers up again for both.

// The interface's own names (ST_Int16LSB, kAsio…), as cwASIO spells them.
#![allow(non_upper_case_globals)]

use crate::asio_samples;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    BufferSize, ChannelCount, Data, DeviceDescription, DeviceDescriptionBuilder, DeviceId, Error,
    ErrorKind, FrameCount, InputCallbackInfo, InputStreamTimestamp, OutputCallbackInfo,
    OutputStreamTimestamp, SampleFormat, SampleRate, StreamConfig, StreamInstant,
    SupportedBufferSize, SupportedStreamConfig, SupportedStreamConfigRange,
};
use idn_asio_abi::*;
use idn_common::sync::LockOk as _;
use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::{c_long, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{
    AtomicBool, AtomicI64, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The rates asked of a driver (it says which it can).
const RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];
/// How long a driver's probe is kept (it is loaded to be asked); a failure or a driver
/// with no channels, briefly (a card just plugged in is asked again soon).
const PROBE_KEPT: Duration = Duration::from_secs(60);
const FAILURE_KEPT: Duration = Duration::from_secs(5);
/// How long a call to the engine is waited for.
const ENGINE_WAIT: Duration = Duration::from_secs(20);
/// How long a stop waits for a buffer switch in progress (a callback stuck longer: its
/// state is let go of, not freed).
const SWITCH_WAIT: Duration = Duration::from_secs(5);
/// IDN's own ASIO driver (media/idn-asio): never loaded here. On Windows its class id; on
/// Linux its library's file name (`libidn_asio.so`, wherever it is installed).
const IDN_DRIVER: Guid = Guid {
    data1: 0x3567_6bb1,
    data2: 0xb34f,
    data3: 0x4615,
    data4: [0x92, 0x9e, 0x85, 0xfc, 0x4b, 0xd4, 0x0d, 0x3b],
};
const IDN_LIBRARY: &str = "libidn_asio.so";

/// Whether `id` is IDN's own driver (a versioned file name, `libidn_asio.so.1`, too).
fn is_idn_driver(id: &DriverId) -> bool {
    match id {
        DriverId::Clsid(g) => *g == IDN_DRIVER,
        // By its name, or (a link, a copy renamed by a link) the file's it leads to.
        DriverId::Library { path: p, .. } => {
            let named = |p: &std::path::Path| {
                p.file_name().is_some_and(|n| {
                    let n = n.to_string_lossy();
                    n == IDN_LIBRARY || n.starts_with(&format!("{IDN_LIBRARY}."))
                })
            };
            named(p) || std::fs::canonicalize(p).is_ok_and(|c| named(&c))
        }
    }
}
/// Drivers that wrap other sound devices: a default only after the cards' own drivers.
const WRAPPERS: [&str; 5] = [
    "asio4all",
    "flexasio",
    "generic low latency",
    "fl studio asio",
    "asio2wasapi",
];
/// "Another driver runs in this program": a probe's answer that isn't kept.
const OTHER_RUNNING: &str = "another ASIO driver is running in this program";

thread_local! {
    /// This thread must not wait on the engine: the engine itself, or the driver's buffer
    /// switch (a stream dropped there closes without waiting).
    static NO_WAIT: Cell<bool> = const { Cell::new(false) };
    /// Listing only ([`listing`]): a driver's last answer is enough, however old; one is
    /// loaded only if it was never asked.
    static LISTING: Cell<bool> = const { Cell::new(false) };
}

/// `f` (a listing of the sound cards) without loading drivers already asked once: a
/// listing made every minute (the display service's) doesn't load every driver every
/// minute, which some wrappers (ASIO4ALL) notice (bug run 2026-11-25).
pub fn listing<T>(f: impl FnOnce() -> T) -> T {
    let was = LISTING.replace(true);
    let r = f();
    LISTING.set(was);
    r
}

/// The host, for idn-audio's `host()`.
pub fn host() -> cpal::Host {
    cpal::Host::from(cpal::platform::CustomHost::from_host(AsioHost))
}

/// What a driver offers.
#[derive(Clone, Debug)]
struct Probe {
    inputs: usize,
    outputs: usize,
    rates: Vec<u32>,
    rate: u32,
    /// (min, max, preferred) frames a buffer, and the granularity (-1: powers of two).
    frames: (u32, u32, u32),
    granularity: i64,
}

fn err(kind: ErrorKind, m: impl Into<String>) -> Error {
    Error::with_message(kind, m.into())
}

fn instant(d: Duration) -> StreamInstant {
    StreamInstant::new(d.as_secs(), d.subsec_nanos())
}

type InCb = Box<dyn FnMut(&Data, &InputCallbackInfo) + Send>;
type OutCb = Box<dyn FnMut(&mut Data, &OutputCallbackInfo) + Send>;
type ErrCb = Box<dyn FnMut(Error) + Send>;

enum Callback {
    In(InCb),
    Out(OutCb),
}

/// A stream's side in the buffer switch.
struct Slot {
    id: u64,
    channels: usize,
    paused: Arc<AtomicBool>,
    cb: Callback,
    /// Its callback panicked: never called again (nor dropped on the driver's thread).
    dead: bool,
}

struct Open {
    id: DriverId,
    input: bool,
    channels: usize,
    rate: SampleRate,
    frames: Option<u32>,
    slot: Slot,
    error: ErrCb,
}

mod device;
mod engine;
mod probe;
mod switch;

use device::*;
use engine::*;
pub use probe::is_running;
use probe::*;
use switch::*;

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(min: u32, max: u32, preferred: u32, granularity: i64) -> Probe {
        Probe {
            inputs: 2,
            outputs: 2,
            rates: vec![48_000],
            rate: 48_000,
            frames: (min, max, preferred),
            granularity,
        }
    }

    #[test]
    fn buffer_sizes_snap_to_the_driver() {
        // Powers of two.
        assert_eq!(snap(240, &probe(64, 2048, 256, -1)), 256);
        assert_eq!(snap(4096, &probe(64, 2048, 256, -1)), 2048);
        assert_eq!(snap(1, &probe(64, 2048, 256, -1)), 64);
        // Multiples of 48 from 48.
        assert_eq!(snap(130, &probe(48, 960, 480, 48)), 96);
        // Its preferred size only.
        assert_eq!(snap(128, &probe(256, 256, 256, 0)), 256);
    }

    #[test]
    fn idn_s_own_driver_is_known_on_both_systems() {
        assert!(is_idn_driver(&DriverId::Clsid(IDN_DRIVER)));
        assert!(!is_idn_driver(&DriverId::Clsid(Guid::default())));
        let lib = |p: &str| DriverId::Library {
            name: "IDN 2".into(),
            path: p.into(),
        };
        assert!(is_idn_driver(&lib("/usr/lib/cwASIO/libidn_asio.so")));
        assert!(is_idn_driver(&lib(
            "/usr/lib/x86_64-linux-gnu/cwASIO/libidn_asio.so.1"
        )));
        assert!(!is_idn_driver(&lib("/usr/lib/cwASIO/libidn_asio_other.so")));
    }
}

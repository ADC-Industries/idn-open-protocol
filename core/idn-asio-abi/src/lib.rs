//! The ASIO driver interface (spec/extensions/asio-interface.md): its types, the driver
//! object's vtable, and (Windows, Linux) the drivers listed and loaded. Written from the public
//! headers of cwASIO (MIT, © 2024 Stefan Heinzmann; NOTICE-cwASIO.txt), an independent
//! implementation of the interface; nothing of Steinberg's SDK. ASIO is a trademark of
//! Steinberg Media Technologies GmbH.
//!
//! Used both ways: by IDN's own ASIO driver (idn-asio, the object hosts call) and by
//! idn-audio's ASIO host (sound cards' ASIO drivers, called).

#![allow(non_upper_case_globals)]

use std::ffi::{c_char, c_long, c_ulong, c_void};

/// `ASIOBool`, `ASIOError`, `ASIOSampleType`: C's `long` (32 bits on Windows).
pub type AsioBool = c_long;
pub type AsioError = c_long;
pub type SampleType = c_long;

pub const ASIO_FALSE: AsioBool = 0;
pub const ASIO_TRUE: AsioBool = 1;

/// Errors.
pub const ASE_OK: AsioError = 0;
/// `future()`'s success.
pub const ASE_SUCCESS: AsioError = 0x3f48_47a0;
pub const ASE_NotPresent: AsioError = -1000;
pub const ASE_HWMalfunction: AsioError = -999;
pub const ASE_InvalidParameter: AsioError = -998;
pub const ASE_InvalidMode: AsioError = -997;
pub const ASE_SPNotAdvancing: AsioError = -996;
pub const ASE_NoClock: AsioError = -995;
pub const ASE_NoMemory: AsioError = -994;

/// Sample types (the little-endian ones IDN converts; the rest are refused).
pub const ST_Int16MSB: SampleType = 0;
pub const ST_Int24MSB: SampleType = 1;
pub const ST_Int32MSB: SampleType = 2;
pub const ST_Float32MSB: SampleType = 3;
pub const ST_Float64MSB: SampleType = 4;
pub const ST_Int16LSB: SampleType = 16;
pub const ST_Int24LSB: SampleType = 17;
pub const ST_Int32LSB: SampleType = 18;
pub const ST_Float32LSB: SampleType = 19;
pub const ST_Float64LSB: SampleType = 20;
/// 32-bit containers holding 16, 18, 20 or 24 significant bits at the bottom.
pub const ST_Int32LSB16: SampleType = 24;
pub const ST_Int32LSB18: SampleType = 25;
pub const ST_Int32LSB20: SampleType = 26;
pub const ST_Int32LSB24: SampleType = 27;

/// `asioMessage` selectors.
pub const kAsioSelectorSupported: c_long = 1;
pub const kAsioEngineVersion: c_long = 2;
pub const kAsioResetRequest: c_long = 3;
pub const kAsioBufferSizeChange: c_long = 4;
pub const kAsioResyncRequest: c_long = 5;
pub const kAsioLatenciesChanged: c_long = 6;
pub const kAsioSupportsTimeInfo: c_long = 7;
pub const kAsioSupportsTimeCode: c_long = 8;
pub const kAsioOverload: c_long = 15;

/// `future()` selectors.
pub const kAsioCanTimeInfo: c_long = 10;
/// cwASIO's own (spec/extensions/asio-interface.md §A.6): the instance's name (its
/// registration name, a NUL-terminated string of at most 31 bytes), set before `init`.
/// Answered `ASE_SUCCESS` by a driver with instances; a null or empty name only asks.
pub const kcwASIOsetInstanceName: c_long = 0x7F00_0001;
pub const kAsioCanTimeCode: c_long = 11;
pub const kAsioCanReportOverload: c_long = 0x2404_2012;

/// Time info flags.
pub const kSystemTimeValid: c_ulong = 1;
pub const kSamplePositionValid: c_ulong = 2;
pub const kSampleRateValid: c_ulong = 4;
pub const kSpeedValid: c_ulong = 8;
pub const kSampleRateChanged: c_ulong = 16;
pub const kClockSourceChanged: c_ulong = 32;

/// A 64-bit count (`ASIOSamples`, `ASIOTimeStamp`): on Windows two 32-bit halves, the
/// high one first; elsewhere a native integer.
#[cfg(windows)]
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Int64 {
    pub hi: c_ulong,
    pub lo: c_ulong,
}

#[cfg(not(windows))]
#[repr(transparent)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Int64(pub i64);

impl Int64 {
    pub fn new(v: u64) -> Int64 {
        #[cfg(windows)]
        {
            Int64 {
                hi: (v >> 32) as c_ulong,
                lo: (v & 0xffff_ffff) as c_ulong,
            }
        }
        #[cfg(not(windows))]
        {
            Int64(v as i64)
        }
    }

    pub fn get(self) -> u64 {
        #[cfg(windows)]
        {
            ((self.hi as u64) << 32) | (self.lo as u64 & 0xffff_ffff)
        }
        #[cfg(not(windows))]
        {
            self.0 as u64
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ClockSource {
    pub index: c_long,
    pub associated_channel: c_long,
    pub associated_group: c_long,
    pub is_current_source: AsioBool,
    pub name: [c_char; 32],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ChannelInfo {
    /// In: the channel.
    pub channel: c_long,
    /// In: an input's (else an output's).
    pub is_input: AsioBool,
    pub is_active: AsioBool,
    pub channel_group: c_long,
    pub sample_type: SampleType,
    pub name: [c_char; 32],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BufferInfo {
    /// In: an input's buffers (else an output's), of channel `channel_num`.
    pub is_input: AsioBool,
    pub channel_num: c_long,
    /// Out: the two halves' buffers.
    pub buffers: [*mut c_void; 2],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TimeInfo {
    pub speed: f64,
    pub system_time: Int64,
    pub sample_position: Int64,
    pub sample_rate: f64,
    pub flags: c_ulong,
    pub reserved: [c_char; 12],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TimeCode {
    pub speed: f64,
    pub time_code_samples: Int64,
    pub flags: c_ulong,
    pub future: [c_char; 64],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AsioTime {
    pub reserved: [c_long; 4],
    pub time_info: TimeInfo,
    pub time_code: TimeCode,
}

impl AsioTime {
    pub fn zeroed() -> AsioTime {
        // SAFETY: plain numbers and byte arrays: all zero is a valid value.
        unsafe { std::mem::zeroed() }
    }
}

/// The host's callbacks (plain C functions, not the driver's convention).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Callbacks {
    pub buffer_switch: Option<unsafe extern "C" fn(index: c_long, direct: AsioBool)>,
    pub sample_rate_did_change: Option<unsafe extern "C" fn(rate: f64)>,
    pub asio_message: Option<
        unsafe extern "C" fn(
            selector: c_long,
            value: c_long,
            message: *mut c_void,
            opt: *mut f64,
        ) -> c_long,
    >,
    pub buffer_switch_time_info: Option<
        unsafe extern "C" fn(
            params: *mut AsioTime,
            index: c_long,
            direct: AsioBool,
        ) -> *mut AsioTime,
    >,
}

/// A GUID, as COM lays it out (the same 16 bytes on Linux).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl Guid {
    /// From its text, `{35676BB1-B34F-4615-929E-85FC4BD40D3B}` (braces optional).
    pub fn parse(s: &str) -> Option<Guid> {
        let h: String = s
            .trim()
            .trim_start_matches('{')
            .trim_end_matches('}')
            .chars()
            .filter(|c| *c != '-')
            .collect();
        if h.len() != 32 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let b = |i: usize| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).ok();
        let mut data4 = [0u8; 8];
        for (k, d) in data4.iter_mut().enumerate() {
            *d = b(8 + k)?;
        }
        Some(Guid {
            data1: u32::from_str_radix(&h[0..8], 16).ok()?,
            data2: u16::from_str_radix(&h[8..12], 16).ok()?,
            data3: u16::from_str_radix(&h[12..16], 16).ok()?,
            data4,
        })
    }
}

impl std::fmt::Display for Guid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let d = &self.data4;
        write!(
            f,
            "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
            self.data1, self.data2, self.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
        )
    }
}

/// The driver object: a pointer to its vtable first.
#[repr(C)]
pub struct Driver {
    pub vtbl: *const DriverVtbl,
}

/// The driver's methods, in their order; `this` first, the platform's convention
/// (`extern "system"`: stdcall on 32-bit Windows, C elsewhere).
#[repr(C)]
pub struct DriverVtbl {
    pub query_interface: unsafe extern "system" fn(
        this: *mut Driver,
        iid: *const Guid,
        out: *mut *mut c_void,
    ) -> c_long,
    pub add_ref: unsafe extern "system" fn(this: *mut Driver) -> c_ulong,
    pub release: unsafe extern "system" fn(this: *mut Driver) -> c_ulong,
    pub init: unsafe extern "system" fn(this: *mut Driver, sys_handle: *mut c_void) -> AsioBool,
    pub get_driver_name: unsafe extern "system" fn(this: *mut Driver, name: *mut c_char),
    pub get_driver_version: unsafe extern "system" fn(this: *mut Driver) -> c_long,
    pub get_error_message: unsafe extern "system" fn(this: *mut Driver, text: *mut c_char),
    pub start: unsafe extern "system" fn(this: *mut Driver) -> AsioError,
    pub stop: unsafe extern "system" fn(this: *mut Driver) -> AsioError,
    pub get_channels: unsafe extern "system" fn(
        this: *mut Driver,
        inputs: *mut c_long,
        outputs: *mut c_long,
    ) -> AsioError,
    pub get_latencies: unsafe extern "system" fn(
        this: *mut Driver,
        input: *mut c_long,
        output: *mut c_long,
    ) -> AsioError,
    pub get_buffer_size: unsafe extern "system" fn(
        this: *mut Driver,
        min: *mut c_long,
        max: *mut c_long,
        preferred: *mut c_long,
        granularity: *mut c_long,
    ) -> AsioError,
    pub can_sample_rate: unsafe extern "system" fn(this: *mut Driver, rate: f64) -> AsioError,
    pub get_sample_rate: unsafe extern "system" fn(this: *mut Driver, rate: *mut f64) -> AsioError,
    pub set_sample_rate: unsafe extern "system" fn(this: *mut Driver, rate: f64) -> AsioError,
    pub get_clock_sources: unsafe extern "system" fn(
        this: *mut Driver,
        clocks: *mut ClockSource,
        num: *mut c_long,
    ) -> AsioError,
    pub set_clock_source:
        unsafe extern "system" fn(this: *mut Driver, reference: c_long) -> AsioError,
    pub get_sample_position: unsafe extern "system" fn(
        this: *mut Driver,
        pos: *mut Int64,
        stamp: *mut Int64,
    ) -> AsioError,
    pub get_channel_info:
        unsafe extern "system" fn(this: *mut Driver, info: *mut ChannelInfo) -> AsioError,
    pub create_buffers: unsafe extern "system" fn(
        this: *mut Driver,
        infos: *mut BufferInfo,
        channels: c_long,
        size: c_long,
        callbacks: *const Callbacks,
    ) -> AsioError,
    pub dispose_buffers: unsafe extern "system" fn(this: *mut Driver) -> AsioError,
    pub control_panel: unsafe extern "system" fn(this: *mut Driver) -> AsioError,
    pub future: unsafe extern "system" fn(
        this: *mut Driver,
        selector: c_long,
        params: *mut c_void,
    ) -> AsioError,
    pub output_ready: unsafe extern "system" fn(this: *mut Driver) -> AsioError,
}

/// A C string from a fixed buffer (to its first NUL).
pub fn text(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// `s` into a fixed buffer, cut to fit with its NUL.
pub fn put_text(buf: &mut [c_char], s: &str) {
    let n = s.len().min(buf.len().saturating_sub(1));
    for (d, b) in buf.iter_mut().zip(s.as_bytes()[..n].iter()) {
        *d = *b as c_char;
    }
    if let Some(end) = buf.get_mut(n) {
        *end = 0;
    }
}

/// What loads a driver: its COM class id (Windows), or its registration's name and its
/// library's path (Linux, as cwASIO lists it: one library may be listed under several
/// names, each an instance of the driver, told its name before `init`). Its text
/// ([`Display`](std::fmt::Display)) parses back ([`DriverId::parse`]), so a host can keep
/// it as text.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DriverId {
    Clsid(Guid),
    Library {
        name: String,
        path: std::path::PathBuf,
    },
}

impl DriverId {
    /// From its text: a GUID (`{…}`, braces optional) is a class id; `NAME:/PATH` a
    /// registration and its library (a name never has a '/', the path starts with one).
    pub fn parse(s: &str) -> Option<DriverId> {
        if let Some(g) = Guid::parse(s) {
            return Some(DriverId::Clsid(g));
        }
        let (name, path) = s.split_once(":/")?;
        (!name.is_empty()).then(|| DriverId::Library {
            name: name.to_owned(),
            path: format!("/{path}").into(),
        })
    }
}

impl std::fmt::Display for DriverId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriverId::Clsid(g) => std::fmt::Display::fmt(g, f),
            DriverId::Library { name, path } => write!(f, "{name}:{}", path.to_string_lossy()),
        }
    }
}

/// One installed driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverEntry {
    /// Its name (the registry key's on Windows, the listing folder's on Linux).
    pub name: String,
    pub id: DriverId,
    /// Its description (the name when it has none).
    pub description: String,
}

#[cfg(any(windows, target_os = "linux"))]
mod loaded;
#[cfg(any(windows, target_os = "linux"))]
pub use loaded::Loaded;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{drivers, register, unregister};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{REGISTRY, drivers, drivers_in, register, registry_dir, same_library, unregister};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guids_and_64_bit_counts() {
        let g = Guid::parse("{35676BB1-B34F-4615-929E-85FC4BD40D3B}").unwrap();
        assert_eq!(g.data1, 0x35676bb1);
        assert_eq!(g.data4, [0x92, 0x9e, 0x85, 0xfc, 0x4b, 0xd4, 0x0d, 0x3b]);
        assert_eq!(g.to_string(), "{35676BB1-B34F-4615-929E-85FC4BD40D3B}");
        assert_eq!(Guid::parse("35676bb1-b34f-4615-929e-85fc4bd40d3b"), Some(g));
        assert!(Guid::parse("{nope}").is_none());
        let v = 0x1_2345_6789u64;
        assert_eq!(Int64::new(v).get(), v);
        let mut name = [0 as c_char; 8];
        put_text(&mut name, "a long name");
        assert_eq!(text(&name), "a long ");
    }

    #[test]
    fn driver_ids_parse_back_from_their_text() {
        let g = Guid::parse("{35676BB1-B34F-4615-929E-85FC4BD40D3B}").unwrap();
        let c = DriverId::Clsid(g);
        assert_eq!(DriverId::parse(&c.to_string()), Some(c.clone()));
        assert_eq!(
            DriverId::parse("35676bb1-b34f-4615-929e-85fc4bd40d3b"),
            Some(c)
        );
        let l = DriverId::Library {
            name: "IDN 2: stage".into(),
            path: "/usr/lib/cwASIO/libidn_asio.so".into(),
        };
        assert_eq!(l.to_string(), "IDN 2: stage:/usr/lib/cwASIO/libidn_asio.so");
        assert_eq!(DriverId::parse(&l.to_string()), Some(l));
        assert_eq!(DriverId::parse("  "), None);
        assert_eq!(DriverId::parse("/no/name.so"), None);
        assert_eq!(DriverId::parse("relative.so"), None);
    }

    #[test]
    fn layouts() {
        // The sizes the interface fixes (Windows: long is 4 bytes).
        let long = std::mem::size_of::<c_long>();
        assert_eq!(std::mem::size_of::<ChannelInfo>(), 5 * long + 32);
        assert_eq!(
            std::mem::size_of::<DriverVtbl>(),
            24 * std::mem::size_of::<usize>()
        );
        assert_eq!(
            std::mem::size_of::<Callbacks>(),
            4 * std::mem::size_of::<usize>()
        );
        assert_eq!(std::mem::size_of::<ClockSource>(), 4 * long + 32);
        let ptr = std::mem::size_of::<usize>();
        assert_eq!(
            std::mem::size_of::<BufferInfo>(),
            (2 * long).next_multiple_of(ptr) + 2 * ptr
        );
        if cfg!(all(windows, target_pointer_width = "64")) {
            assert_eq!(std::mem::size_of::<Int64>(), 8);
            assert_eq!(std::mem::size_of::<TimeInfo>(), 48);
            assert_eq!(std::mem::size_of::<TimeCode>(), 88);
            assert_eq!(std::mem::size_of::<AsioTime>(), 152);
            assert_eq!(std::mem::offset_of!(AsioTime, time_info), 16);
        }
        // Linux LP64: long is 8 bytes; the 64-bit counts are native.
        if cfg!(all(target_os = "linux", target_pointer_width = "64")) {
            assert_eq!(std::mem::size_of::<Int64>(), 8);
            assert_eq!(std::mem::size_of::<TimeInfo>(), 56);
            assert_eq!(std::mem::size_of::<TimeCode>(), 88);
            assert_eq!(std::mem::size_of::<AsioTime>(), 176);
            assert_eq!(std::mem::offset_of!(AsioTime, time_info), 32);
        }
    }
}

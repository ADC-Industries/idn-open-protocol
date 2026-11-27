//! The FFmpeg calls, kept in one place. Every pointer here is owned by the struct that
//! holds it and freed in its `Drop`.

use ffmpeg_sys_next as ff;
use std::ffi::{CStr, CString, c_int};
use std::io;
use std::ptr;

/// A pixel format number as the bindings' enum: `AV_PIX_FMT_NONE` for one they don't know
/// (a newer FFmpeg's, or a bad value), as a plain conversion of it would be undefined.
pub fn pix_fmt(v: c_int) -> ff::AVPixelFormat {
    if (-1..ff::AVPixelFormat::AV_PIX_FMT_NB as c_int).contains(&v) {
        // SAFETY: the enum is `repr(i32)` with every value from -1 below NB (no gaps).
        unsafe { std::mem::transmute::<c_int, ff::AVPixelFormat>(v) }
    } else {
        ff::AVPixelFormat::AV_PIX_FMT_NONE
    }
}

/// As [`pix_fmt`], for sample formats.
pub fn sample_fmt(v: c_int) -> ff::AVSampleFormat {
    if (-1..ff::AVSampleFormat::AV_SAMPLE_FMT_NB as c_int).contains(&v) {
        // SAFETY: the enum is `repr(i32)` with every value from -1 below NB (no gaps).
        unsafe { std::mem::transmute::<c_int, ff::AVSampleFormat>(v) }
    } else {
        ff::AVSampleFormat::AV_SAMPLE_FMT_NONE
    }
}

/// `AVERROR(EAGAIN)`.
pub const EAGAIN: c_int = -11;

pub fn err(code: c_int, what: &str) -> io::Error {
    let mut buf = [0 as std::ffi::c_char; 128];
    // SAFETY: the buffer and its length are ours; av_strerror NUL-terminates within it.
    unsafe { ff::av_strerror(code, buf.as_mut_ptr(), buf.len()) };
    // SAFETY: NUL-terminated by av_strerror above.
    let msg = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy();
    io::Error::other(format!("{what}: {msg}"))
}

pub fn check(code: c_int, what: &str) -> io::Result<c_int> {
    if code < 0 {
        Err(err(code, what))
    } else {
        Ok(code)
    }
}

/// Fills `pkt` with a copy of `data`. The length is checked into FFmpeg's `int`: one
/// that wrapped would allocate a short packet and copy past its end.
pub(crate) unsafe fn fill_packet(
    pkt: *mut ff::AVPacket,
    data: &[u8],
    what: &str,
) -> io::Result<()> {
    let n =
        c_int::try_from(data.len()).map_err(|_| io::Error::other(format!("{what}: too large")))?;
    // SAFETY: `pkt` is the caller's packet (this fn's contract); av_new_packet allocates `n`
    // bytes, `n` = `data.len()` checked to fit.
    unsafe {
        check(ff::av_new_packet(pkt, n), what)?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), (*pkt).data, data.len());
    }
    Ok(())
}

/// A packet's bytes (an empty packet has no data pointer: never a slice from null).
pub(crate) unsafe fn packet_bytes(p: *const ff::AVPacket) -> Vec<u8> {
    // SAFETY: `p` is the caller's packet; `size` bytes at `data` when it is set.
    unsafe {
        if (*p).data.is_null() || (*p).size <= 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts((*p).data, (*p).size as usize).to_vec()
        }
    }
}

pub(crate) fn oom<T>(p: *mut T) -> io::Result<*mut T> {
    if p.is_null() {
        Err(io::Error::other("out of memory"))
    } else {
        Ok(p)
    }
}

/// Quiet FFmpeg's own logging (errors still come back as results).
pub fn quiet() {
    // SAFETY: a plain setting.
    unsafe { ff::av_log_set_level(ff::AV_LOG_FATAL) };
}

/// swscale's area-averaging flag: a plain define up to FFmpeg 7, an enum in 8, the same
/// value in both (the plugin builds against OBS's FFmpeg 7.1 on Windows).
const SWS_AREA: c_int = 0x20;

/// A reference-counted FFmpeg buffer (device or frames context).
pub struct BufferRef(pub *mut ff::AVBufferRef);

// SAFETY: buffer references count atomically, and the device and frames contexts behind
// them are set up once, then shared as FFmpeg shares them (its pools lock).
unsafe impl Send for BufferRef {}
unsafe impl Sync for BufferRef {}

impl Drop for BufferRef {
    fn drop(&mut self) {
        // SAFETY: our reference (or null, which unref takes), released once.
        unsafe { ff::av_buffer_unref(&mut self.0) };
    }
}

impl BufferRef {
    /// A new reference to the same buffer (never null: out of memory ends the program, as
    /// Rust's own allocations do).
    pub fn new_ref(&self) -> *mut ff::AVBufferRef {
        // SAFETY: a live reference of ours; the new one is the caller's.
        let r = unsafe { ff::av_buffer_ref(self.0) };
        if r.is_null() {
            std::alloc::handle_alloc_error(std::alloc::Layout::new::<ff::AVBufferRef>());
        }
        r
    }
}

mod avdevice;
mod decoder;
mod demux;
mod devices;
mod encoder;
mod frame;
mod scale;

pub use avdevice::*;
pub use decoder::*;
pub use demux::*;
pub use devices::*;
pub use encoder::*;
pub use frame::*;
pub use scale::*;

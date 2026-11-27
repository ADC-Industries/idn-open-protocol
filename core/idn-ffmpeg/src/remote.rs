//! Screen capture as H.264 for IDN Industrial's remote support (spec/idn-industrial.md
//! §I.11.3). A machine's monitors, and a pull API that turns the captured screen into
//! H.264 access units (the GPU's encoder where there is one, else the CPU's), ready to
//! relay to a browser that decodes them with WebCodecs.
//!
//! The capture devices are the Node's (`ddagrab` on Windows, `x11grab` on Linux, §R.19);
//! this adds the low-latency H.264 encode and a blocking `next()` so the caller paces it.

use crate::Ffmpeg;
use crate::capture::{CaptureConfig, Source};
use crate::ffi::{Demuxer, FileDecoder, Frame, Scaler};
use idn_open_protocol::codec::{Codec, EncoderConfig, VideoEncoder};
use std::io;
use std::sync::Arc;
use std::time::Instant;

/// A monitor a machine has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
    pub index: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// One encoded picture.
pub struct RemotePicture {
    /// H.264 Annex B; parameter sets are prepended on keyframes.
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub pts_ms: i64,
}

/// The machine's monitors.
pub fn monitors() -> io::Result<Vec<Monitor>> {
    #[cfg(windows)]
    {
        windows_monitors()
    }
    #[cfg(not(windows))]
    {
        // X11 captures the whole display as one framebuffer; its size comes once the
        // capture opens, so report the display with the size unknown (0). Per-monitor
        // selection on Linux is an open item (as is Wayland).
        let d = std::env::var("DISPLAY").unwrap_or_default();
        let name = if d.is_empty() {
            "Primary display".to_owned()
        } else {
            format!("X display {d}")
        };
        Ok(vec![Monitor {
            index: 0,
            name,
            width: 0,
            height: 0,
            primary: true,
        }])
    }
}

#[cfg(windows)]
fn windows_monitors() -> io::Result<Vec<Monitor>> {
    use windows_sys::Win32::Foundation::{LPARAM, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
    };
    // The "primary monitor" flag (windows-sys doesn't re-export the constant).
    const MONITORINFOF_PRIMARY: u32 = 1;
    // The EnumDisplayMonitors callback returns BOOL (i32); non-zero continues.
    const CONTINUE: i32 = 1;

    unsafe extern "system" fn cb(mon: HMONITOR, _dc: HDC, _r: *mut RECT, lp: LPARAM) -> i32 {
        // SAFETY: `lp` is the Vec passed to EnumDisplayMonitors below, alive for the call.
        let out = unsafe { &mut *(lp as *mut Vec<Monitor>) };
        // SAFETY: MONITORINFOEXW is plain data; all zeros is a valid value.
        let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        // SAFETY: a monitor handle from the enumeration and a MONITORINFOEXW of the size `cbSize`
        // says.
        if unsafe { GetMonitorInfoW(mon, &mut info as *mut _ as *mut MONITORINFO) } == 0 {
            return CONTINUE;
        }
        let rc = info.monitorInfo.rcMonitor;
        let end = info.szDevice.iter().position(|&c| c == 0).unwrap_or(0);
        let dev = String::from_utf16_lossy(&info.szDevice[..end]);
        let i = out.len() as u32;
        out.push(Monitor {
            index: i,
            name: if dev.is_empty() {
                format!("Monitor {}", i + 1)
            } else {
                dev
            },
            width: (rc.right - rc.left).max(0) as u32,
            height: (rc.bottom - rc.top).max(0) as u32,
            primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        });
        CONTINUE
    }

    let mut out: Vec<Monitor> = Vec::new();
    // SAFETY: the callback above, with a pointer to `out`, which outlives the call (the
    // enumeration is synchronous).
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(cb),
            &mut out as *mut _ as LPARAM,
        );
    }
    if out.is_empty() {
        return Err(io::Error::other("no monitors found"));
    }
    Ok(out)
}

/// A running screen-to-H.264 capture; stops when dropped.
pub struct ScreenCapture {
    demux: Demuxer,
    dec: FileDecoder,
    enc: Box<dyn VideoEncoder>,
    stream: usize,
    size: (u32, u32),
    scaler: Option<Scaler>,
    scaled: Option<Frame>,
    start: Instant,
    want_key: bool,
    backend: crate::Backend,
    /// Access units an encode gave beyond its first (an encoder working ahead, VA-API
    /// above 1080p, after a stall): handed out next, in order (they
    /// were dropped, and the browser's picture broke up until the next keyframe).
    queued: std::collections::VecDeque<RemotePicture>,
}

/// Opens a capture of `monitor` (or the primary) at `fps` and `bitrate`, with the mouse
/// pointer if `pointer`.
pub fn capture(
    monitor: Option<u32>,
    fps: u32,
    bitrate: u64,
    pointer: bool,
) -> io::Result<ScreenCapture> {
    let ff = Arc::new(Ffmpeg::probe());
    // ddagrab counts monitors from 0; the Node's `Screen(String)` counts from 1.
    let which = (monitor.unwrap_or(0) + 1).to_string();
    let cfg = CaptureConfig {
        fps: fps.clamp(1, 60),
        size: None,
        pointer,
        encoding: None,
        format: None,
        control: false,
        game: false,
    };
    let (format, url, opts) = crate::capture::capture_input(&Source::Screen(which), &cfg)?;
    let o: Vec<(&str, &str)> = opts.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let demux = Demuxer::open_device(format, &url, &o)?;
    let stream = demux
        .streams
        .iter()
        .position(|s| s.video)
        .ok_or_else(|| io::Error::other(format!("{url}: no video")))?;
    let (w, h) = (
        demux.streams[stream].width as u32,
        demux.streams[stream].height as u32,
    );
    let dec = FileDecoder::open_live(&demux.streams[stream])?;
    let fps = fps.clamp(1, 60);
    let cfg = EncoderConfig {
        codec: Codec::H264,
        width: w as usize,
        height: h as usize,
        frame_rate: (fps, 1),
        bitrate: bitrate.clamp(300_000, 40_000_000),
        // Two seconds between keyframes; want_keyframe() asks for more when a viewer joins.
        keyframe_interval: (fps * 2).max(1),
    };
    let (enc, backend) = ff.h264(&cfg)?;
    idn_open_protocol::log::log(&format!(
        "remote: capturing {format} {url} ({w}x{h}) as H.264 on {backend:?} at {:.1} Mbit/s",
        cfg.bitrate as f64 / 1e6
    ));
    Ok(ScreenCapture {
        demux,
        dec,
        enc,
        stream,
        size: (w, h),
        scaler: None,
        scaled: None,
        start: Instant::now(),
        want_key: true,
        backend,
        queued: Default::default(),
    })
}

impl ScreenCapture {
    /// The next encoded picture; blocks until one is ready (about the frame rate, paced
    /// by the capture device itself).
    #[allow(clippy::should_implement_trait)] // a blocking pull, not an Iterator.
    pub fn next(&mut self) -> io::Result<RemotePicture> {
        if let Some(p) = self.queued.pop_front() {
            return Ok(p);
        }
        loop {
            let Some(p) = self.demux.read()? else {
                return Err(io::Error::other("the screen capture stopped"));
            };
            if p.stream != self.stream {
                continue;
            }
            self.dec.send(&p)?;
            while let Some((frame, _)) = self.dec.receive()? {
                let planar =
                    crate::player::to_planar(frame, &mut self.scaler, &mut self.scaled, None)?;
                let pts = self.start.elapsed().as_millis() as i64;
                let force = std::mem::take(&mut self.want_key);
                let out = self.enc.encode(&planar.planes, planar.layout, pts, force)?;
                for e in out {
                    let mut data = e.data;
                    if e.keyframe && !e.extra.is_empty() && !has_sps(&data) {
                        let mut d = e.extra;
                        d.extend_from_slice(&data);
                        data = d;
                    }
                    self.queued.push_back(RemotePicture {
                        data,
                        keyframe: e.keyframe,
                        pts_ms: pts,
                    });
                }
                if let Some(p) = self.queued.pop_front() {
                    return Ok(p);
                }
            }
        }
    }

    /// Ask for a keyframe on the next picture (a viewer joined, so it can start decoding).
    pub fn want_keyframe(&mut self) {
        self.want_key = true;
    }

    /// The picture size (width, height).
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Which encoder is in use.
    pub fn backend(&self) -> crate::Backend {
        self.backend
    }
}

/// Whether an access unit already carries an SPS NAL (type 7).
fn has_sps(au: &[u8]) -> bool {
    au.windows(4)
        .take(64)
        .any(|w| w[..3] == [0, 0, 1] && w[3] & 0x1f == 7)
}

//! Screen capture for remote support (§I.11.3): the monitors a machine has, and a
//! capture that yields encoded H.264 pictures at a frame rate and bit rate the session
//! sets.
//!
//! The real capture (Desktop Duplication on Windows, X11 on Linux) and its H.264 encoder
//! live in `idn-ffmpeg`, which links the system's FFmpeg; it is behind the `capture`
//! feature so the Site Manager's own build (and a Node without FFmpeg) needs neither.
//! A synthetic backend is always built: it draws a moving pattern and "encodes" it as
//! opaque bytes, so the relay, the protocol, roles and the audit are testable with no
//! display and no FFmpeg.

use std::io;
use std::time::Duration;

/// One monitor a machine has.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Monitor {
    /// 0-based index, as the capture backend counts them.
    pub index: u32,
    /// A name to show (the OS's, or `Monitor N`).
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// The primary monitor.
    #[serde(default)]
    pub primary: bool,
}

/// How a session wants a screen captured.
#[derive(Debug, Clone)]
pub struct ScreenParams {
    /// The monitor index, or `None` for all side by side.
    pub monitor: Option<u32>,
    pub fps: u32,
    pub bitrate: u64,
    /// Draw the mouse pointer into the picture.
    pub pointer: bool,
}

impl Default for ScreenParams {
    fn default() -> Self {
        ScreenParams {
            monitor: Some(0),
            fps: 15,
            bitrate: 4_000_000,
            pointer: true,
        }
    }
}

/// One encoded picture from a capture.
pub struct Picture {
    /// The encoded bytes (H.264 Annex B from the real backend; opaque from the synthetic
    /// one).
    pub data: Vec<u8>,
    /// A keyframe (an IDR, decodable on its own): the first picture a late viewer needs.
    pub keyframe: bool,
    /// Milliseconds since the capture started.
    pub pts_ms: i64,
}

/// A running screen capture: pictures until it is dropped.
pub trait Capture: Send {
    /// The next picture, or an error if the capture ended (the device went away, say).
    /// Blocks until one is ready (at about the frame rate).
    fn next(&mut self) -> io::Result<Picture>;
    /// Ask for a keyframe on the next picture (a new viewer joined).
    fn want_keyframe(&mut self);
    /// The size the pictures decode to (width, height).
    fn size(&self) -> (u32, u32);
}

/// The machine's monitors. Without the `capture` feature, or where it finds none, it
/// reports one synthetic 1280×720 monitor so sessions still work for testing.
pub fn monitors() -> Vec<Monitor> {
    #[cfg(feature = "capture")]
    {
        match idn_ffmpeg::remote::monitors() {
            Ok(v) if !v.is_empty() => {
                return v
                    .into_iter()
                    .map(|m| Monitor {
                        index: m.index,
                        name: m.name,
                        width: m.width,
                        height: m.height,
                        primary: m.primary,
                    })
                    .collect();
            }
            Ok(_) => {}
            Err(e) => idn_open_protocol::log::log(&format!("remote: listing monitors: {e}")),
        }
    }
    vec![Monitor {
        index: 0,
        name: "Monitor 0".into(),
        width: 1280,
        height: 720,
        primary: true,
    }]
}

/// Opens a capture for `params`. The real backend when `capture` is on and it succeeds,
/// otherwise the synthetic one.
pub fn open(params: &ScreenParams) -> Box<dyn Capture> {
    #[cfg(feature = "capture")]
    {
        match idn_ffmpeg::remote::capture(
            params.monitor,
            params.fps,
            params.bitrate,
            params.pointer,
        ) {
            Ok(c) => return Box::new(FfmpegCapture(c)),
            Err(e) => idn_open_protocol::log::log(&format!(
                "remote: screen capture fell back to the synthetic backend: {e}"
            )),
        }
    }
    Box::new(Synthetic::new(params))
}

#[cfg(feature = "capture")]
struct FfmpegCapture(idn_ffmpeg::remote::ScreenCapture);

#[cfg(feature = "capture")]
impl Capture for FfmpegCapture {
    fn next(&mut self) -> io::Result<Picture> {
        let p = self.0.next()?;
        Ok(Picture {
            data: p.data,
            keyframe: p.keyframe,
            pts_ms: p.pts_ms,
        })
    }
    fn want_keyframe(&mut self) {
        self.0.want_keyframe();
    }
    fn size(&self) -> (u32, u32) {
        self.0.size()
    }
}

/// A test capture: a frame counter as little-endian bytes with a one-byte keyframe flag,
/// at the asked frame rate. Carries no real pixels; it exercises the stream and relay.
struct Synthetic {
    w: u32,
    h: u32,
    period: Duration,
    start: std::time::Instant,
    n: u64,
    want_key: bool,
}

impl Synthetic {
    fn new(p: &ScreenParams) -> Self {
        Synthetic {
            w: 1280,
            h: 720,
            period: Duration::from_secs_f64(1.0 / p.fps.max(1) as f64),
            start: std::time::Instant::now(),
            n: 0,
            want_key: true,
        }
    }
}

impl Capture for Synthetic {
    fn next(&mut self) -> io::Result<Picture> {
        // Keep roughly to the frame rate.
        let due = self.period.saturating_mul(self.n as u32);
        if let Some(wait) = due.checked_sub(self.start.elapsed()) {
            std::thread::sleep(wait);
        }
        let keyframe = self.want_key || self.n.is_multiple_of(60);
        self.want_key = false;
        let mut data = vec![keyframe as u8];
        data.extend_from_slice(&self.n.to_le_bytes());
        // A little filler so a picture isn't trivially small.
        data.resize(64, 0);
        self.n += 1;
        Ok(Picture {
            data,
            keyframe,
            pts_ms: self.start.elapsed().as_millis() as i64,
        })
    }
    fn want_keyframe(&mut self) {
        self.want_key = true;
    }
    fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }
}

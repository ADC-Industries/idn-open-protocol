//! An OMT receiver (spec/extensions/idn-omt.md §O.4): two TCP connections to the source,
//! one for video and metadata, one for audio, each subscribing to what it carries; the
//! video decoded from VMX to UYVY. Connects again by itself (finding the source again by
//! name each time).

use crate::vmx::{Profile, Vmx};
use crate::wire::{self, Frame, msg};
use idn_common::sync::LockOk as _;
use std::io::{self, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver as Rx, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A decoded picture: UYVY (8-bit 4:2:2, limited range), its alpha plane after it when
/// `alpha`. An interlaced picture is a whole frame, its two fields woven.
#[derive(Debug, Clone)]
pub struct Picture {
    pub width: usize,
    pub height: usize,
    pub rate: (u32, u32),
    pub alpha: bool,
    /// The alpha is premultiplied (the sender said so); straight otherwise.
    pub premultiplied: bool,
    pub interlaced: bool,
    /// Its colour matrix as the sender gave it: 601 or 709 (a sender saying nothing: 709
    /// from 720 lines, 601 below, as VMX codes it).
    pub color_space: u16,
    /// A preview (an eighth of each side: [`Receiver::set_preview`]), not the whole
    /// picture.
    pub preview: bool,
    /// 100 ns.
    pub timestamp: i64,
    pub uyvy: Vec<u8>,
    /// The frame's own metadata, if it had some.
    pub metadata: Option<String>,
}

/// What arrives.
#[derive(Debug, Clone)]
pub enum Event {
    Connected,
    Disconnected(String),
    Video(Picture),
    Audio {
        sample_rate: u32,
        channels: Vec<Vec<f32>>,
        timestamp: i64,
    },
    /// The sender's metadata (who it is: `<OMTInfo …/>`, or the application's).
    Metadata(String),
}

/// Where the source is: "HOST (NAME)" (found on mDNS), or `omt://host:port` / `host:port`.
pub fn resolve(source: &str) -> io::Result<SocketAddr> {
    let s = source.trim();
    let hostport = s.strip_prefix("omt://").unwrap_or(s);
    if !hostport.contains('(')
        && let Ok(mut a) = hostport.to_socket_addrs()
        && let Some(a) = a.next()
    {
        return Ok(a);
    }
    let found = idn_open_protocol::discovery::service::find(wire::SERVICE, Duration::from_secs(2));
    found
        .into_iter()
        .find(|f| f.instance.eq_ignore_ascii_case(s))
        .map(|f| SocketAddr::from((f.addr, f.port)))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no OMT source {s:?}")))
}

/// The OMT sources on the network (their full names, sorted).
pub fn sources(timeout: Duration) -> Vec<String> {
    let mut v: Vec<String> = idn_open_protocol::discovery::service::find(wire::SERVICE, timeout)
        .into_iter()
        .map(|f| f.instance)
        .collect();
    v.sort();
    v.dedup();
    v
}

struct Shared {
    source: String,
    events: SyncSender<Event>,
    /// The video connection, for sending tally and metadata.
    video_out: Mutex<Option<TcpStream>>,
    tally: Mutex<(bool, bool)>,
    /// Previews asked for ([`Receiver::set_preview`]).
    preview: AtomicBool,
    stop: AtomicBool,
    /// Decoded pictures waiting to be taken: at most [`PICTURES_WAITING`] (16 of 8K
    /// pictures was gigabytes; bug round E, E1-2).
    pictures: std::sync::atomic::AtomicUsize,
}

/// Decoded pictures that may wait at once (more are dropped: the program is behind).
const PICTURES_WAITING: usize = 2;
/// The largest picture taken, in pixels (8192 × 4320, as IDN's own SpeedHQ).
const MAX_PIXELS: usize = 8192 * 4320;

/// A receiver of one OMT source; stops when dropped.
pub struct Receiver {
    sh: Arc<Shared>,
    events: Rx<Event>,
}

impl Receiver {
    /// Receives `source` ([`resolve`]): its video (decoded) and metadata, and its audio
    /// when `audio`.
    pub fn start(source: &str, audio: bool) -> io::Result<Receiver> {
        let (tx, rx) = sync_channel(16);
        let sh = Arc::new(Shared {
            source: source.to_owned(),
            events: tx,
            video_out: Mutex::new(None),
            tally: Mutex::new((false, false)),
            preview: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            pictures: std::sync::atomic::AtomicUsize::new(0),
        });
        let s = sh.clone();
        std::thread::Builder::new()
            .name("idn-omt-recv-video".into())
            .spawn(move || run(&s, true))?;
        if audio {
            let s = sh.clone();
            // (The video thread already running is stopped if this one can't start: it
            // reconnected for ever with no receiver to stop it; E1-6.)
            if let Err(e) = std::thread::Builder::new()
                .name("idn-omt-recv-audio".into())
                .spawn(move || run(&s, false))
            {
                sh.stop.store(true, Ordering::Relaxed);
                return Err(e);
            }
        }
        Ok(Receiver { sh, events: rx })
    }

    /// The next event, waiting up to `timeout`.
    pub fn recv(&self, timeout: Duration) -> Option<Event> {
        let e = self.events.recv_timeout(timeout).ok()?;
        if matches!(e, Event::Video(_)) {
            self.sh.pictures.fetch_sub(1, Ordering::Relaxed);
        }
        Some(e)
    }

    /// Tells the source whether it is on program and/or preview (its tally light).
    pub fn set_tally(&self, program: bool, preview: bool) {
        // (The connection held while the value changes: one being made reads the values
        // under the same lock, so a change in between isn't lost; bug run 2026-11-25.)
        let mut out = self.sh.video_out.lock_ok();
        let mut t = self.sh.tally.lock_ok();
        if *t != (program, preview) {
            *t = (program, preview);
            drop(t);
            if let Some(s) = out.as_mut() {
                let _ = Frame::metadata(msg::tally(program, preview), 0).write(s, None);
            }
        }
    }

    /// Asks the source for previews only (an eighth of each side, from the start of each
    /// coded frame: thumbnails at a fraction of the bandwidth and decoding), or the whole
    /// pictures again. Kept across reconnects. A source that doesn't send previews goes on
    /// sending whole pictures; [`Picture::preview`] says which came.
    pub fn set_preview(&self, on: bool) {
        let mut out = self.sh.video_out.lock_ok();
        if self.sh.preview.swap(on, Ordering::Relaxed) != on
            && let Some(s) = out.as_mut()
        {
            let m = if on {
                msg::PREVIEW_ON
            } else {
                msg::PREVIEW_OFF
            };
            let _ = Frame::metadata(m, 0).write(s, None);
        }
    }

    /// Sends a metadata message to the source.
    pub fn send_metadata(&self, xml: &str) {
        send(&self.sh, xml);
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.sh.stop.store(true, Ordering::Relaxed);
        if let Some(s) = self.sh.video_out.lock_ok().take() {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }
}

fn send(sh: &Shared, xml: &str) {
    if let Some(s) = sh.video_out.lock_ok().as_mut() {
        let _ = Frame::metadata(xml, 0).write(s, None);
    }
}

fn emit(sh: &Shared, e: Event) {
    // Pictures are dropped when the program falls behind; everything else waits.
    match e {
        Event::Video(_) => {
            if sh.pictures.fetch_add(1, Ordering::Relaxed) >= PICTURES_WAITING
                || sh.events.try_send(e).is_err()
            {
                sh.pictures.fetch_sub(1, Ordering::Relaxed);
            }
        }
        e => {
            let _ = sh.events.send(e);
        }
    }
}

/// One connection (video and metadata, or audio), connected again after a failure.
fn run(sh: &Arc<Shared>, video: bool) {
    let mut codec: Option<Vmx> = None;
    // Connections that ended within a second running (a source at its limit refusing):
    // the wait grows, 1 s doubling to 16 s (lab 523: refused receivers kept trying).
    let mut quick = 0u32;
    while !sh.stop.load(Ordering::Relaxed) {
        // From the connection, not before the lookup: a named source's lookup takes its
        // 2 s whatever happens (bug run I5-2: such receivers never backed off), and a
        // source that is down (no connection) is tried every second as before.
        let mut connected: Option<std::time::Instant> = None;
        let r = (|| -> io::Result<()> {
            let addr = resolve(&sh.source)?;
            let s = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
            connected = Some(std::time::Instant::now());
            s.set_nodelay(true)?;
            let mut w = s.try_clone()?;
            w.set_write_timeout(Some(Duration::from_secs(5)))?;
            if video {
                // The handshake and the connection put in place under one lock with what
                // set_preview / set_tally change.
                let mut out = sh.video_out.lock_ok();
                // (Previews asked for before the video, as OMT's own receiver does.)
                if sh.preview.load(Ordering::Relaxed) {
                    Frame::metadata(msg::PREVIEW_ON, 0).write(&mut w, None)?;
                }
                for m in [msg::SUBSCRIBE_METADATA, msg::SUBSCRIBE_VIDEO] {
                    Frame::metadata(m, 0).write(&mut w, None)?;
                }
                let t = *sh.tally.lock_ok();
                Frame::metadata(msg::tally(t.0, t.1), 0).write(&mut w, None)?;
                *out = Some(w.try_clone()?);
                drop(out);
                emit(sh, Event::Connected);
            } else {
                Frame::metadata(msg::SUBSCRIBE_AUDIO, 0).write(&mut w, None)?;
            }
            w.flush()?;
            let mut r = s;
            r.set_read_timeout(Some(Duration::from_secs(10)))?;
            while !sh.stop.load(Ordering::Relaxed) {
                let Some(f) = Frame::read(&mut r)? else {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionReset,
                        "the source closed",
                    ));
                };
                match f.kind {
                    wire::METADATA => {
                        if let Some(x) = f.text() {
                            // The source's combined tally is the protocol's, not news.
                            if msg::tally_of(&x).is_none() {
                                emit(sh, Event::Metadata(x));
                            }
                        }
                    }
                    wire::VIDEO => {
                        let Some(v) = f.video.filter(|v| v.codec == wire::VMX1) else {
                            continue;
                        };
                        // A preview: taken only when asked for (one in flight after turning
                        // them off is dropped).
                        let preview = v.flags & wire::FLAG_PREVIEW != 0;
                        if preview && !sh.preview.load(Ordering::Relaxed) {
                            continue;
                        }
                        let (w_, h_) = (v.width.max(0) as usize, v.height.max(0) as usize);
                        if w_.saturating_mul(h_) > MAX_PIXELS {
                            continue;
                        }
                        if codec.as_ref().is_none_or(|c| c.size() != (w_, h_)) {
                            let fps = (v.rate_n / v.rate_d.max(1)).max(1) as u32;
                            codec = Vmx::new(w_, h_, Profile::Standard, v.color_space, fps);
                        }
                        let alpha = v.flags & wire::FLAG_ALPHA != 0;
                        let interlaced = v.flags & wire::FLAG_INTERLACED != 0;
                        let Some(c) = codec.as_mut() else { continue };
                        let decoded = if preview {
                            c.decode_preview(f.payload(), alpha, interlaced)
                        } else {
                            c.decode(f.payload(), alpha).map(|u| (u, w_, h_))
                        };
                        if let Some((uyvy, pw, ph)) = decoded {
                            let metadata = (f.meta_len > 0).then(|| {
                                String::from_utf8_lossy(&f.data[f.data.len() - f.meta_len..])
                                    .trim_end_matches('\0')
                                    .to_owned()
                            });
                            emit(
                                sh,
                                Event::Video(Picture {
                                    width: pw,
                                    height: ph,
                                    rate: (v.rate_n.max(1) as u32, v.rate_d.max(1) as u32),
                                    alpha,
                                    premultiplied: alpha && v.flags & wire::FLAG_PREMULTIPLIED != 0,
                                    interlaced,
                                    color_space: match v.color_space {
                                        601 => 601,
                                        709 => 709,
                                        _ if h_ >= 720 => 709,
                                        _ => 601,
                                    },
                                    preview,
                                    timestamp: f.timestamp,
                                    uyvy,
                                    metadata,
                                }),
                            );
                        }
                    }
                    wire::AUDIO => {
                        let Some(a) = f.audio.filter(|a| a.codec == wire::FPA1) else {
                            continue;
                        };
                        if let Some(channels) = wire::fpa1_decode(
                            f.payload(),
                            a.channels.max(0) as usize,
                            a.samples.max(0) as usize,
                            a.active,
                        ) {
                            emit(
                                sh,
                                Event::Audio {
                                    sample_rate: a.sample_rate.max(0) as u32,
                                    channels,
                                    timestamp: f.timestamp,
                                },
                            );
                        }
                    }
                    _ => {}
                }
            }
            Ok(())
        })();
        if video {
            *sh.video_out.lock_ok() = None;
            if let Err(e) = &r
                && !sh.stop.load(Ordering::Relaxed)
            {
                emit(sh, Event::Disconnected(e.to_string()));
            }
        }
        quick = if connected.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            (quick + 1).min(5)
        } else {
            0
        };
        // In short steps, ending when the receiver is dropped (bug run K7-4: up to 16 s
        // with its threads and connection state kept).
        let until = std::time::Instant::now() + Duration::from_secs(1 << quick.saturating_sub(1));
        while !sh.stop.load(Ordering::Relaxed) && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

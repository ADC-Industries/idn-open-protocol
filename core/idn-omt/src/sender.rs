//! An OMT source (spec/extensions/idn-omt.md §O.3): a TCP listener on a port from 6400,
//! announced as `_omt._tcp` "HOST (NAME)"; each connection says what it wants (video,
//! audio, metadata), its tally, whether it wants previews and the quality it suggests.
//! Each connection has its own queue: a slow receiver loses frames, the others don't wait.

use crate::vmx::{Profile, Vmx};
use crate::wire::{self, Frame, msg};
use idn_common::sync::LockOk as _;
use idn_open_protocol::discovery::service::ServiceAnnouncer;
use std::collections::VecDeque;
use std::io::{self, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const WANT_VIDEO: u8 = 1;
const WANT_AUDIO: u8 = 2;
const WANT_METADATA: u8 = 4;

/// Frames queued for one connection, at most (then they are dropped).
const QUEUE: usize = 6;

/// Settings for [`Sender::start`].
#[derive(Debug, Clone)]
pub struct SenderConfig {
    /// The source's name ("HOST (NAME)" on the network).
    pub name: String,
    /// The TCP port; `None`: the first free one from 6400.
    pub port: Option<u16>,
    /// Announced on mDNS (`_omt._tcp`).
    pub announce: bool,
    /// The bitrate when no receiver suggests one.
    pub profile: Profile,
    /// What each connection is told about the sender: (product, manufacturer, version).
    pub info: Option<(String, String, String)>,
}

impl SenderConfig {
    pub fn new(name: &str) -> SenderConfig {
        SenderConfig {
            name: name.to_owned(),
            port: None,
            announce: true,
            profile: Profile::Standard,
            info: Some((
                "IDN Open Protocol".into(),
                "ADC Industries".into(),
                env!("CARGO_PKG_VERSION").into(),
            )),
        }
    }
}

struct Conn {
    tx: SyncSender<Arc<Vec<u8>>>,
    wants: AtomicU8,
    preview: AtomicBool,
    /// Its tally (program, preview) and suggested profile.
    tally: Mutex<(bool, bool)>,
    quality: Mutex<Option<Profile>>,
    alive: AtomicBool,
}

struct Shared {
    cfg: SenderConfig,
    /// The combined tally last told to every receiver.
    told: Mutex<(bool, bool)>,
    conns: Mutex<Vec<Arc<Conn>>>,
    codec: Mutex<Option<Vmx>>,
    metadata: Mutex<VecDeque<String>>,
    /// Its alpha is premultiplied ([`Sender::set_premultiplied`]).
    premultiplied: AtomicBool,
    /// The colour matrix its pictures are in: 601, 709, or 0 (by the height;
    /// [`Sender::set_color_space`]).
    color_space: std::sync::atomic::AtomicU16,
    stop: AtomicBool,
}

/// An OMT source; withdrawn when dropped.
pub struct Sender {
    sh: Arc<Shared>,
    port: u16,
    _announcer: Option<ServiceAnnouncer>,
}

impl Sender {
    pub fn start(cfg: SenderConfig) -> io::Result<Sender> {
        let listener = match cfg.port {
            Some(p) => TcpListener::bind(("0.0.0.0", p))?,
            None => (wire::PORT_FIRST..=wire::PORT_LAST)
                .find_map(|p| TcpListener::bind(("0.0.0.0", p)).ok())
                .ok_or_else(|| io::Error::new(io::ErrorKind::AddrInUse, "no free OMT port"))?,
        };
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        let announcer = if cfg.announce {
            let a = ServiceAnnouncer::start(wire::SERVICE)?;
            let host = idn_open_protocol::discovery::hostname().to_uppercase();
            a.set(vec![(format!("{host} ({})", cfg.name), port)]);
            Some(a)
        } else {
            None
        };
        let sh = Arc::new(Shared {
            cfg,
            told: Mutex::new((false, false)),
            conns: Mutex::new(Vec::new()),
            codec: Mutex::new(None),
            metadata: Mutex::new(VecDeque::new()),
            premultiplied: AtomicBool::new(false),
            color_space: std::sync::atomic::AtomicU16::new(0),
            stop: AtomicBool::new(false),
        });
        let s2 = sh.clone();
        std::thread::Builder::new()
            .name("idn-omt-accept".into())
            .spawn(move || accept_loop(listener, &s2))?;
        Ok(Sender {
            sh,
            port,
            _announcer: announcer,
        })
    }

    /// The TCP port it listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Receivers connected now (each makes one or two connections).
    pub fn connections(&self) -> usize {
        self.sh.conns.lock_ok().len()
    }

    /// The tally receivers give it: (on program, on preview).
    pub fn tally(&self) -> (bool, bool) {
        let c = self.sh.conns.lock_ok();
        c.iter()
            .filter(|c| c.alive.load(Ordering::Relaxed))
            .fold((false, false), |(p, v), c| {
                let t = *c.tally.lock_ok();
                (p || t.0, v || t.1)
            })
    }

    /// Metadata receivers sent since the last call (protocol messages left out).
    pub fn recv_metadata(&self) -> Vec<String> {
        self.sh.metadata.lock_ok().drain(..).collect()
    }

    fn wanting(&self, w: u8) -> Vec<Arc<Conn>> {
        let mut c = self.sh.conns.lock_ok();
        c.retain(|c| c.alive.load(Ordering::Relaxed));
        c.iter()
            .filter(|c| c.wants.load(Ordering::Relaxed) & w != 0)
            .cloned()
            .collect()
    }

    /// Says whether this source's alpha is premultiplied (straight by default): its
    /// pictures with alpha are marked so from now on.
    pub fn set_premultiplied(&self, on: bool) {
        self.sh.premultiplied.store(on, Ordering::Relaxed);
    }

    /// Says which colour matrix this source's pictures were made with: 601 or 709 (BT.601,
    /// BT.709), marked on each picture from now on; `None`: by the height, as VMX does
    /// (709 from 720 lines, 601 below; the default). For a host that converts every size
    /// with one matrix (its small BT.709 outputs were marked 601 before).
    pub fn set_color_space(&self, matrix: Option<u16>) {
        let v = match matrix {
            Some(601) => 601,
            Some(709) => 709,
            _ => 0,
        };
        self.sh.color_space.store(v, Ordering::Relaxed);
    }

    /// Sends a UYVY picture (alpha plane after it when `alpha`), coded with VMX once for
    /// every receiver that wants video (none: not coded at all). `rate`: (n, d) frames a
    /// second; `timestamp` in 100 ns.
    #[allow(clippy::too_many_arguments)]
    pub fn send_video(
        &self,
        uyvy: &mut [u8],
        width: usize,
        height: usize,
        rate: (u32, u32),
        alpha: bool,
        interlaced: bool,
        timestamp: i64,
    ) -> io::Result<()> {
        let to = self.wanting(WANT_VIDEO);
        if to.is_empty() {
            return Ok(());
        }
        // The highest quality any receiver suggests, else the sender's own.
        let profile = to
            .iter()
            .filter_map(|c| *c.quality.lock_ok())
            .max_by_key(|p| *p as i32)
            .unwrap_or(self.sh.cfg.profile);
        let fps = rate.0 / rate.1.max(1);
        let cs = match self.sh.color_space.load(Ordering::Relaxed) {
            0 if height >= 720 => 709,
            0 => 601,
            v => i32::from(v),
        };
        let mut codec = self.sh.codec.lock_ok();
        // Made again for a new size, or a profile the receivers now ask for (a later
        // request went unheard until the size changed; E1-4).
        let fresh = codec.as_ref().is_none_or(|c| {
            c.size() != (width, height) || c.profile() != profile || c.color_space() != cs
        });
        if fresh {
            let q = codec.as_ref().map(Vmx::quality);
            *codec = Vmx::new(width, height, profile, cs, fps);
            if let (Some(c), Some(q)) = (codec.as_mut(), q) {
                c.set_quality(q);
            }
        }
        let c = codec
            .as_mut()
            .ok_or_else(|| idn_common::err::invalid_input("OMT: no coder for that size"))?;
        let (data, preview) = c
            .encode(uyvy, alpha, interlaced)
            .ok_or_else(|| io::Error::other("OMT: the picture didn't code"))?;
        drop(codec);
        let f = Frame {
            kind: wire::VIDEO,
            timestamp,
            video: Some(wire::Video {
                codec: wire::VMX1,
                width: width as i32,
                height: height as i32,
                rate_n: rate.0 as i32,
                rate_d: rate.1 as i32,
                aspect: width as f32 / height as f32,
                flags: (if alpha { wire::FLAG_ALPHA } else { 0 })
                    | (if alpha && self.sh.premultiplied.load(Ordering::Relaxed) {
                        wire::FLAG_PREMULTIPLIED
                    } else {
                        0
                    })
                    | (if interlaced { wire::FLAG_INTERLACED } else { 0 }),
                color_space: cs,
            }),
            audio: None,
            data,
            meta_len: 0,
        };
        // Each form made only when a client takes it (both were made every picture).
        let (mut full, mut small) = (None::<Arc<Vec<u8>>>, None::<Arc<Vec<u8>>>);
        for c in to {
            let b = if c.preview.load(Ordering::Relaxed) {
                small.get_or_insert_with(|| Arc::new(f.encode(Some(preview))))
            } else {
                full.get_or_insert_with(|| Arc::new(f.encode(None)))
            };
            queue(&c, b.clone());
        }
        Ok(())
    }

    /// Sends planar float audio (silent channels left out on the wire).
    pub fn send_audio(&self, channels: &[Vec<f32>], sample_rate: u32, timestamp: i64) {
        let to = self.wanting(WANT_AUDIO);
        if to.is_empty() || channels.is_empty() {
            return;
        }
        let samples = channels[0].len();
        let (data, active) = wire::fpa1_encode(channels);
        let f = Frame {
            kind: wire::AUDIO,
            timestamp,
            video: None,
            audio: Some(wire::Audio {
                codec: wire::FPA1,
                sample_rate: sample_rate as i32,
                samples: samples as i32,
                channels: channels.len().min(32) as i32,
                active,
            }),
            data,
            meta_len: 0,
        };
        let b = Arc::new(f.encode(None));
        for c in to {
            queue(&c, b.clone());
        }
    }

    /// Sends a metadata message to every receiver that wants metadata.
    pub fn send_metadata(&self, xml: &str) {
        let b = Arc::new(Frame::metadata(xml, 0).encode(None));
        for c in self.wanting(WANT_METADATA) {
            queue(&c, b.clone());
        }
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.sh.stop.store(true, Ordering::Relaxed);
        for c in self.sh.conns.lock_ok().iter() {
            c.alive.store(false, Ordering::Relaxed);
        }
    }
}

fn queue(c: &Conn, b: Arc<Vec<u8>>) {
    if let Err(TrySendError::Disconnected(_)) = c.tx.try_send(b) {
        c.alive.store(false, Ordering::Relaxed);
    }
}

fn accept_loop(listener: TcpListener, sh: &Arc<Shared>) {
    let mut refused = idn_open_protocol::log::Throttle::new(Duration::from_secs(10));
    while !sh.stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((s, from)) => {
                let _ = s.set_nonblocking(false);
                let _ = s.set_nodelay(true);
                if let Err(e) = start_conn(s, sh) {
                    // Said here (OMT has no word for a refusal: the receiver sees only its
                    // connection closed; lab 488-B), at most once each 10 s.
                    refused.log(
                        &format!(
                            "OMT {}: a receiver at {} refused: {e}",
                            sh.cfg.name,
                            from.ip()
                        ),
                        "refused",
                    );
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
    refused.flush();
}

/// Receivers served at once (more are refused): each holds two connections (video and metadata, and
/// audio), so the limit is twice this in connections (lab 527(b): 64 connections served
/// only about 32 receivers).
const MAX_RECEIVERS: usize = 64;
const MAX_CONNECTIONS: usize = 2 * MAX_RECEIVERS;

fn start_conn(s: TcpStream, sh: &Arc<Shared>) -> io::Result<()> {
    // Bounded, and read with a timeout: every connection took two threads, none was ever
    // refused or timed out (bug round E, E1-3). A
    // receiver that says nothing within 10 s of connecting is let go (one that has spoken
    // may then stay quiet).
    {
        let mut conns = sh.conns.lock_ok();
        conns.retain(|c| c.alive.load(Ordering::Relaxed));
        if conns.len() >= MAX_CONNECTIONS {
            return Err(io::Error::other(format!(
                "{MAX_CONNECTIONS} connections already ({MAX_RECEIVERS} receivers), the most it serves"
            )));
        }
    }
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    let (tx, rx) = sync_channel::<Arc<Vec<u8>>>(QUEUE);
    let c = Arc::new(Conn {
        tx,
        wants: AtomicU8::new(0),
        preview: AtomicBool::new(false),
        tally: Mutex::new((false, false)),
        quality: Mutex::new(None),
        alive: AtomicBool::new(true),
    });
    let mut w = s.try_clone()?;
    w.set_write_timeout(Some(Duration::from_secs(5)))?;
    let wc = c.clone();
    std::thread::Builder::new()
        .name("idn-omt-send".into())
        .spawn(move || {
            while wc.alive.load(Ordering::Relaxed) {
                match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(b) => {
                        if w.write_all(&b).is_err() {
                            break;
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(_) => break,
                }
            }
            wc.alive.store(false, Ordering::Relaxed);
            let _ = w.shutdown(std::net::Shutdown::Both);
        })?;
    // Who this is, first.
    if let Some((p, m, v)) = &sh.cfg.info {
        let e = idn_open_protocol::xml::Element::new("OMTInfo")
            .with_attr("ProductName", p)
            .with_attr("Manufacturer", m)
            .with_attr("Version", v);
        queue(&c, Arc::new(Frame::metadata(&e.to_xml(), 0).encode(None)));
    }
    // The combined tally, as every receiver has it (OMT 1.0, "Tally Commands").
    // (Under `told` until it is listed: a change in between went to the others only and
    // this one kept the old tally; bug run 2026-11-25.)
    let told = sh.told.lock_ok();
    queue(
        &c,
        Arc::new(Frame::metadata(msg::tally(told.0, told.1), 0).encode(None)),
    );
    sh.conns.lock_ok().push(c.clone());
    drop(told);
    let (sh2, mut r) = (sh.clone(), s);
    std::thread::Builder::new()
        .name("idn-omt-read".into())
        .spawn(move || {
            let mut first = true;
            while c.alive.load(Ordering::Relaxed) {
                match Frame::read(&mut r) {
                    Ok(Some(f)) => {
                        if std::mem::replace(&mut first, false) {
                            let _ = r.set_read_timeout(None);
                        }
                        if let Some(x) = f.text() {
                            on_metadata(&c, &sh2, x);
                        }
                    }
                    _ => break,
                }
            }
            c.alive.store(false, Ordering::Relaxed);
            // Its tally gone with it: the others told (it stayed lit; E1-5).
            sh2.conns
                .lock_ok()
                .retain(|x| x.alive.load(Ordering::Relaxed));
            retell(&sh2);
        })?;
    Ok(())
}

/// The combined tally of the receivers connected, told to every receiver when it changes.
fn retell(sh: &Shared) {
    // (Worked out under `told`: two receivers' changes at once could store the older
    // result last, the tally lamps left wrong; bug run 2026-11-25.)
    let mut told = sh.told.lock_ok();
    let all = sh
        .conns
        .lock_ok()
        .iter()
        .filter(|c| c.alive.load(Ordering::Relaxed))
        .fold((false, false), |(p, v), c| {
            let t = *c.tally.lock_ok();
            (p || t.0, v || t.1)
        });
    if *told != all {
        *told = all;
        let b = Arc::new(Frame::metadata(msg::tally(all.0, all.1), 0).encode(None));
        for c in sh.conns.lock_ok().iter() {
            queue(c, b.clone());
        }
    }
}

/// A receiver's message: a subscription, tally, preview, quality, else the application's.
fn on_metadata(c: &Conn, sh: &Shared, x: String) {
    let set = |w: u8| {
        c.wants.fetch_or(w, Ordering::Relaxed);
    };
    match x.as_str() {
        msg::SUBSCRIBE_VIDEO => set(WANT_VIDEO),
        msg::SUBSCRIBE_AUDIO => set(WANT_AUDIO),
        msg::SUBSCRIBE_METADATA => set(WANT_METADATA),
        msg::PREVIEW_ON => c.preview.store(true, Ordering::Relaxed),
        msg::PREVIEW_OFF => c.preview.store(false, Ordering::Relaxed),
        t if msg::tally_of(t).is_some() => {
            *c.tally.lock_ok() = msg::tally_of(t).unwrap();
            retell(sh);
        }
        q if q.starts_with(msg::QUALITY_PREFIX) => {
            *c.quality.lock_ok() = if q.contains("\"High\"") {
                Some(Profile::High)
            } else if q.contains("\"Medium\"") || q.contains("\"Default\"") {
                // (Default is 166, Standard: §O.3; E1-4.)
                Some(Profile::Standard)
            } else if q.contains("\"Low\"") {
                Some(Profile::Low)
            } else {
                None
            };
        }
        // Kept for the application, but within bounds: any client may send it (60 ×
        // 10 MiB was held otherwise).
        _ if x.len() <= 64 << 10 => {
            let mut m = sh.metadata.lock_ok();
            m.push_back(x);
            while m.len() > 60 || m.iter().map(|x| x.len()).sum::<usize>() > 1 << 20 {
                m.pop_front();
            }
        }
        _ => {}
    }
}

//! The worker's plumbing: its inbox, the watcher of a source's rings on this machine, the
//! decoding lanes, finding a source by name, and small helpers (text out, colour
//! conversion, hiding the media token).

use super::*;

impl Drop for MachineIn {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Watches a source's control page: each new picture's and block's number to the worker,
/// as the doorbell rings. Pictures a long way behind are skipped (only the slots' worth can
/// still be read); every block of sound is passed on in order.
pub(super) fn watch_machine(
    control: crate::machine::Control,
    inbox: Inbox,
    stop: Arc<AtomicBool>,
    epoch: u64,
    // The heads the worker took when it opened the rings: items written since (before
    // this thread ran) are passed on too.
    mut seen: (u64, u64),
) {
    while !stop.load(Ordering::Relaxed) {
        let now = control.wait(seen, Duration::from_millis(100));
        let first_pic =
            (seen.0 + 1).max(now.0.saturating_sub(crate::machine::PIC_SLOTS as u64 - 1));
        for seq in first_pic..=now.0 {
            if inbox.send(Incoming::Machine(epoch, false, seq)).is_err() {
                return;
            }
        }
        let first_snd =
            (seen.1 + 1).max(now.1.saturating_sub(crate::machine::AUDIO_SLOTS as u64 - 1));
        for seq in first_snd..=now.1 {
            if inbox.send(Incoming::Machine(epoch, true, seq)).is_err() {
                return;
            }
        }
        seen = now;
    }
}

impl Incoming {
    pub(super) fn is_video(&self) -> bool {
        matches!(self, Incoming::Machine(_, false, _))
            || matches!(self,
            Incoming::Tcp(_, Ok(Some(f))) | Incoming::Quic(QEvent::Message(_, f)) | Incoming::Media(_, _, Ok(f), _)
                if f.kind == kind::VIDEO)
    }

    /// A sound message from a media transport (counted against [`MAX_AUDIO_WAITING`]).
    pub(super) fn is_audio(&self) -> bool {
        matches!(self, Incoming::Media(_, _, Ok(f), _) if f.kind == kind::AUDIO)
    }
}

impl Inbox {
    pub(super) fn send(&self, m: Incoming) -> Result<(), mpsc::SendError<Incoming>> {
        let video = m.is_video();
        let audio = m.is_audio();
        if audio && self.audio_waiting.fetch_add(1, Ordering::Relaxed) >= MAX_AUDIO_WAITING {
            self.audio_waiting.fetch_sub(1, Ordering::Relaxed);
            self.queue.stats.lock_ok().late += 1;
            return Ok(());
        }
        if video && self.video_waiting.fetch_add(1, Ordering::Relaxed) >= MAX_VIDEO_WAITING {
            self.video_waiting.fetch_sub(1, Ordering::Relaxed);
            self.video_lost.store(true, Ordering::Relaxed);
            self.queue.stats.lock_ok().late += 1;
            return Ok(());
        }
        let r = self.tx.send(m);
        if r.is_err() && video {
            self.video_waiting.fetch_sub(1, Ordering::Relaxed);
        }
        if r.is_err() && audio {
            self.audio_waiting.fetch_sub(1, Ordering::Relaxed);
        }
        r
    }
}

pub(super) fn hostname() -> String {
    crate::discovery::hostname()
}

/// Resolves `ip[:port]`, `host:port`, or an NDI name via the Discovery Server or mDNS.
#[doc(hidden)]
pub fn resolve(
    target: &str,
    iface: Option<Ipv4Addr>,
    server: Option<&str>,
) -> io::Result<(SocketAddr, String)> {
    resolve_within(target, iface, server, Duration::from_secs(5))
}

/// As [`resolve`], waiting at most `timeout` for mDNS or the Discovery Server.
#[doc(hidden)]
pub fn resolve_within(
    target: &str,
    iface: Option<Ipv4Addr>,
    server: Option<&str>,
    timeout: Duration,
) -> io::Result<(SocketAddr, String)> {
    resolve_hinted(target, iface, server, timeout).map(|(a, how, _)| (a, how))
}

/// As [`resolve_within`], also telling whether the source's discovery hint offers the
/// IDN link (`None` for an address given directly: unknown, §E.11.3).
pub(super) fn resolve_hinted(
    target: &str,
    iface: Option<Ipv4Addr>,
    server: Option<&str>,
    timeout: Duration,
) -> io::Result<(SocketAddr, String, Option<bool>)> {
    use std::net::ToSocketAddrs;
    let link = |s: &crate::discovery::Source| Some(s.offers("link"));
    if let Ok(ip) = target.parse::<std::net::IpAddr>() {
        return Ok((SocketAddr::new(ip, 5961), String::new(), None));
    }
    if let Some(a) = target.to_socket_addrs().ok().and_then(|mut i| i.next()) {
        return Ok((a, String::new(), None));
    }
    if let Some(sv) = server {
        let list =
            crate::discovery::ds::query_sources(crate::discovery::ds::server_addr(sv)?, timeout)?;
        return match list
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(target))
            .and_then(|s| Some((s.socket_addr()?, link(s))))
        {
            Some((a, l)) => Ok((
                a,
                format!("found \"{target}\" at {a} via discovery server"),
                l,
            )),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no source named \"{target}\" on discovery server {sv}"),
            )),
        };
    }
    let opts = crate::discovery::BrowseOptions { interface: iface };
    match crate::discovery::find(target, timeout, &opts)? {
        Some(s) => {
            let a = s.socket_addr().unwrap();
            Ok((a, format!("found \"{}\" at {a} via mDNS", s.name), link(&s)))
        }
        None => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no source named \"{target}\" found via mDNS"),
        )),
    }
}

pub(super) fn send_text(sock: &mut TcpStream, k: u16, xml: &str) -> io::Result<()> {
    let tc = if k == kind::METADATA {
        crate::frame::TIMECODE_SYNTHESIZE
    } else {
        0
    };
    let r = sock.write_all(&Frame::text(k, tc, xml).encode());
    if r.is_err() {
        // Part of a frame may have gone (the write timeout): the next would be read from
        // its middle. Ended here instead: the reader sees the loss and reconnects (as the
        // sender does, F5-5; bug run 2026-11-24).
        let _ = sock.shutdown(std::net::Shutdown::Both);
    }
    r
}

impl Ahead {
    pub(super) fn new() -> Self {
        Ahead {
            lanes: None,
            results: mpsc::channel(),
            in_flight: VecDeque::new(),
            done: Default::default(),
            next: None,
            seq: 0,
        }
    }

    /// Runs `job` on one of the two decoding threads.
    pub(super) fn run(&mut self, job: Box<dyn FnOnce() + Send>) {
        let lanes = self.lanes.get_or_insert_with(|| {
            let (tx, rx) = mpsc::channel::<Box<dyn FnOnce() + Send>>();
            let rx = Arc::new(Mutex::new(rx));
            for i in 0..AHEAD {
                let rx = rx.clone();
                // Without the thread, the other one does the work.
                let _ = std::thread::Builder::new()
                    .name(format!("input-decode-{i}"))
                    .spawn(move || {
                        loop {
                            let job = rx.lock_ok().recv();
                            match job {
                                Ok(job) => job(),
                                Err(_) => return, // the input is gone
                            }
                        }
                    });
            }
            tx
        });
        // No decoding thread could be started (the receiving end is gone with them): done
        // here, or the input would wait for its result for ever.
        if let Err(mpsc::SendError(job)) = lanes.send(job) {
            job();
        }
    }
}

/// How to convert a received picture to RGBA: what the sender announced (§E.7
/// `x_idn_color` for range and wire alpha, §3.10 `ndi_color_info` for the matrix), and
/// the input's own [`InputConfig::color`] for whatever it didn't say.
/// The RGBA conversion for a frame: the local configuration, with the matrix when the
/// source named one, range and alpha when `announced` (an IDN sender's `x_idn_color`
/// or a compressed stream's own tags), and the transfer function and primaries whenever
/// the frame's colour metadata says something other than the SDR default (§3.10.4,
/// §7.1.2): PQ/HLG frames are then tone-mapped.
pub(crate) fn conversion_config(
    local: &ColorConfig,
    color: &crate::ext::ColorInfo,
    announced: bool,
    matrix: Option<&str>,
) -> ColorConfig {
    use crate::color::{Primaries, Transfer};
    let mut c = *local;
    if let Some(m) = matrix.and_then(crate::color::Matrix::from_name) {
        c.matrix = m;
    }
    if announced {
        match color.range.as_str() {
            "full" => c.limited = false,
            "limited" => c.limited = true,
            _ => {}
        }
        match color.alpha.as_str() {
            "premultiplied" => c.wire_alpha = AlphaMode::Premultiplied,
            "straight" => c.wire_alpha = AlphaMode::Straight,
            _ => {}
        }
    }
    let d = crate::ext::ColorInfo::default();
    if color.transfer != d.transfer {
        c.transfer = Transfer::from_name(&color.transfer);
    }
    if color.primaries != d.primaries {
        c.primaries = Primaries::from_name(&color.primaries);
    }
    c
}

/// Hides the secure/1 media token (a per-session secret, §E.8.4) in log lines.
pub(super) fn redact_token(xml: &str) -> String {
    match xml.find("x_idn_media_token=\"") {
        Some(i) => {
            let start = i + "x_idn_media_token=\"".len();
            let end = xml[start..].find('"').map_or(xml.len(), |e| start + e);
            format!("{}…{}", &xml[..start], &xml[end..])
        }
        None => xml.to_owned(),
    }
}

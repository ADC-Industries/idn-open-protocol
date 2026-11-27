//! The audio itself (§5): transmit flows sent from the program's sound as the media clock
//! reaches each packet, receive flows' packets written into it at their play-out time,
//! keepalives both ways.
//!
//! What it costs is what it does per packet (lab 528(i): the first version made a system
//! call or more per packet and polled every socket every 0.25 ms, 36 % of a core for 64
//! channels each way). So:
//! - one socket sends every transmit flow; each wake gathers every packet now due (flows
//!   share packet boundaries) and sends them in one `sendmmsg`; keepalives coming back
//!   are read every 50 ms and matched to their flow by where they come from;
//! - the receiving thread sleeps in `poll` on every receive flow's socket until a packet
//!   comes, then reads each ready socket in batches (`recvmmsg`).
//!
//! Windows has neither batch call: there it sends and reads one datagram per call, and
//! waits in `WSAPoll` (more of a core for many channels; the same timing).

use crate::audio;
use crate::receiver::{KEEPALIVE, KEEPALIVE_EVERY};
use idn_common::sync::LockOk as _;
use std::collections::BTreeMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A unicast transmit flow is stopped this long after its last keepalive (§5.6).
pub const TX_EXPIRES: Duration = Duration::from_secs(4);
/// Keepalives to transmit flows are looked for this often.
const KEEPALIVES_READ_EVERY: Duration = Duration::from_millis(50);
/// Packets read from one socket at a time.
const BATCH: usize = 32;
/// The largest datagram taken (a jumbo frame's worth).
const DATAGRAM: usize = 9000;
/// How late past its play-out point a packet is still written: 2 ms, in samples at `rate`
/// (bug run K2-9: a fixed 96 was 1 ms at 96 kHz). The rings are read a few milliseconds
/// behind the clock, so a packet a little late still plays (lab 529(d): dropping at the
/// play-out point lost 3.6 times as many once the reading thread woke at most every
/// 0.5 ms).
fn late_taken(rate: u32) -> u64 {
    u64::from(rate) / 500
}

/// The media clock: samples since the media epoch, `None` while there is no usable clock
/// (then nothing is sent, §8.1).
pub trait MediaClock: Send + Sync {
    fn now(&self) -> Option<u64>;
}

/// The program's sound: transmit channels read at a media time, receive channels written
/// at one (the shared rings, indexed by media time).
pub trait Sound: Send + Sync {
    fn read(&self, tx_channel: usize, at: u64, out: &mut [f32]);
    fn write(&self, rx_channel: usize, at: u64, samples: &[f32]);

    /// As [`Sound::read`], as left-justified 32-bit values (the wire's and the rings'
    /// form): the engine's path, with no float in between (lab 528(i)).
    fn read_raw(&self, tx_channel: usize, at: u64, out: &mut [i32]) {
        let mut f = vec![0.0f32; out.len()];
        self.read(tx_channel, at, &mut f);
        for (o, v) in out.iter_mut().zip(f) {
            *o = audio::to_i32(v);
        }
    }

    fn write_raw(&self, rx_channel: usize, at: u64, samples: &[i32]) {
        let f: Vec<f32> = samples
            .iter()
            .map(|&v| (v as f64 / 2_147_483_648.0) as f32)
            .collect();
        self.write(rx_channel, at, &f);
    }

    /// One slot of a packet's audio (`channels` a frame, `bytes` a sample) straight into
    /// receive channel `rx_channel` at `at`: by default through [`Sound::write_raw`]; the
    /// rings take it with no buffer between.
    fn write_slot(
        &self,
        rx_channel: usize,
        at: u64,
        audio: &[u8],
        channels: usize,
        bytes: usize,
        slot: usize,
    ) {
        let mut v = Vec::new();
        audio::slot_i32(audio, channels, bytes, slot, &mut v);
        self.write_raw(rx_channel, at, &v);
    }
}

/// A transmit flow being sent.
pub struct TxRun {
    pub to: SocketAddr,
    /// TX channel per slot (1-based; 0 silence).
    pub channels: Vec<u16>,
    pub bytes: usize,
    pub fpp: u64,
    pub multicast: bool,
    /// The next packet's first frame; `None` until it starts (then aligned to "now",
    /// §6.6, §5.3).
    next: Option<u64>,
    last_keepalive: Instant,
}

impl TxRun {
    pub fn new(
        to: SocketAddr,
        channels: Vec<u16>,
        bytes: usize,
        fpp: u16,
        multicast: bool,
    ) -> TxRun {
        TxRun {
            to,
            channels,
            bytes,
            fpp: u64::from(fpp.max(1)),
            multicast,
            next: None,
            last_keepalive: Instant::now(),
        }
    }
}

/// A receive flow being received.
pub struct RxRun {
    pub socket: UdpSocket,
    /// Per slot, the RX channels (1-based) that take it.
    pub slots: Vec<Vec<usize>>,
    pub bytes: usize,
    pub latency: u64,
    pub multicast: bool,
    /// The transmitter's address, when known (a unicast flow: the device that set it up);
    /// packets from elsewhere passed over (bug run N1-1: anyone's packets to the port were
    /// played, and took its keepalives).
    pub source: Option<std::net::IpAddr>,
    /// Where its packets come from (its keepalives go there), once one came.
    from: Option<SocketAddr>,
    last_keepalive: Instant,
    /// When its last packet came, and its largest now − T since the last heartbeat
    /// (§5.4), for the heartbeat and the scan.
    pub last_packet: Arc<Mutex<Option<Instant>>>,
    pub most_late: Arc<AtomicU64>,
}

impl RxRun {
    pub fn new(
        socket: UdpSocket,
        slots: Vec<Vec<usize>>,
        bytes: usize,
        latency: u64,
        multicast: bool,
    ) -> RxRun {
        let _ = socket.set_nonblocking(true);
        // Room for the packets that come between two reads, and a stall besides.
        let _ = socket2::SockRef::from(&socket).set_recv_buffer_size(1 << 20);
        RxRun {
            socket,
            slots,
            bytes,
            latency,
            multicast,
            source: None,
            from: None,
            last_keepalive: Instant::now(),
            last_packet: Arc::default(),
            most_late: Arc::default(),
        }
    }
}

/// Counters for the program's state.
#[derive(Debug, Default)]
pub struct Counts {
    pub sent: AtomicU64,
    pub received: AtomicU64,
    /// Packets dropped: a timestamp too far behind the play-out position or more than a second
    /// ahead (§5.4's advice).
    pub dropped: AtomicU64,
    /// Transmit flows stopped for want of keepalives.
    pub expired: AtomicU64,
    /// Those flows (number, where they went), for the program to free them in the
    /// device's flow table too ([`Engine::take_expired`]; bug run I1-1: their numbers
    /// stayed taken and the 33rd receiver was refused).
    expired_flows: Mutex<Vec<(u16, SocketAddr)>>,
}

/// The engine: flows added and removed by the program, run by two threads.
pub struct Engine {
    pub tx: Arc<Mutex<BTreeMap<u16, TxRun>>>,
    pub rx: Arc<Mutex<BTreeMap<u16, RxRun>>>,
    pub counts: Arc<Counts>,
    stop: Arc<AtomicBool>,
}

/// The transmit socket on the interface `ip`: multicast out through it too.
fn tx_socket(ip: Ipv4Addr) -> io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.bind(&SocketAddr::from((ip, 0)).into())?;
    if !ip.is_unspecified() {
        s.set_multicast_if_v4(&ip)?;
    }
    s.set_multicast_ttl_v4(32)?;
    s.set_nonblocking(true)?;
    // Room for a burst of packets in one call.
    let _ = s.set_send_buffer_size(1 << 20);
    let s: UdpSocket = s.into();
    no_udp_resets(&s);
    Ok(s)
}

impl Engine {
    /// The transmit flows stopped for want of keepalives since the last call.
    pub fn take_expired(&self) -> Vec<(u16, SocketAddr)> {
        std::mem::take(&mut *self.counts.expired_flows.lock_ok())
    }

    /// The engine on the interface `ip` (its transmit flows sent from there).
    pub fn start(
        clock: Arc<dyn MediaClock>,
        sound: Arc<dyn Sound>,
        rate: u32,
        ip: Ipv4Addr,
    ) -> io::Result<Engine> {
        let e = Engine {
            tx: Arc::default(),
            rx: Arc::default(),
            counts: Arc::default(),
            stop: Arc::default(),
        };
        let socket = tx_socket(ip)?;
        let (tx, st, c, ck, so) = (
            e.tx.clone(),
            e.stop.clone(),
            e.counts.clone(),
            clock.clone(),
            sound.clone(),
        );
        std::thread::Builder::new()
            .name("idn-aoip-send".into())
            .spawn(move || {
                idn_common::rt::audio_this_thread();
                send_loop(&socket, &tx, &st, &c, ck.as_ref(), so.as_ref(), rate)
            })?;
        let (rx, st, c) = (e.rx.clone(), e.stop.clone(), e.counts.clone());
        std::thread::Builder::new()
            .name("idn-aoip-receive".into())
            .spawn(move || {
                idn_common::rt::audio_this_thread();
                receive_loop(&rx, &st, &c, clock.as_ref(), sound.as_ref(), rate)
            })?;
        Ok(e)
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Sends `packets` (datagram, destination) from `s`: in one `sendmmsg` on Linux (as many
/// calls as it takes for all), one by one elsewhere. How many went.
fn send_all(s: &UdpSocket, packets: &[(Vec<u8>, SocketAddr)]) -> usize {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let mut addrs: Vec<libc::sockaddr_in> = Vec::with_capacity(packets.len());
        let mut iovs: Vec<libc::iovec> = Vec::with_capacity(packets.len());
        for (d, to) in packets {
            let SocketAddr::V4(v4) = to else { continue };
            addrs.push(libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: v4.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(v4.ip().octets()),
                },
                sin_zero: [0; 8],
            });
            iovs.push(libc::iovec {
                iov_base: d.as_ptr() as *mut libc::c_void,
                iov_len: d.len(),
            });
        }
        let mut msgs: Vec<libc::mmsghdr> = (0..iovs.len())
            .map(|i| {
                // SAFETY: a zeroed mmsghdr is a valid empty one; its pointers are set next.
                let mut m: libc::mmsghdr = unsafe { std::mem::zeroed() };
                m.msg_hdr.msg_name = &mut addrs[i] as *mut libc::sockaddr_in as *mut libc::c_void;
                m.msg_hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
                m.msg_hdr.msg_iov = &mut iovs[i];
                m.msg_hdr.msg_iovlen = 1;
                m
            })
            .collect();
        let (mut at, mut sent) = (0, 0);
        while at < msgs.len() {
            // SAFETY: `msgs[at..]` point into `addrs` and `iovs`, which point into
            // `packets`; all live for this call.
            let n = unsafe {
                libc::sendmmsg(
                    s.as_raw_fd(),
                    msgs[at..].as_mut_ptr(),
                    (msgs.len() - at) as libc::c_uint,
                    0,
                )
            };
            if n > 0 {
                at += n as usize;
                sent += n as usize;
                continue;
            }
            // -1: the message at `at` failed (sendmmsg says so only for the first). A full
            // buffer: the rest of this wake's packets are dropped (by the next wake they
            // would be late; bug run K2-10: the comment said they waited). Anything else is that one packet's
            // (a flow to port 0, a broadcast, an unreachable or too large one): skipped,
            // the flows after it still sent (bug run I2-2: they all went silent).
            let e = io::Error::last_os_error();
            if n == 0
                || matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                )
                || e.raw_os_error() == Some(libc::ENOBUFS)
            {
                break;
            }
            at += 1;
        }
        sent
    }
    #[cfg(not(target_os = "linux"))]
    {
        packets
            .iter()
            .filter(|(d, to)| s.send_to(d, to).is_ok())
            .count()
    }
}

fn send_loop(
    socket: &UdpSocket,
    tx: &Mutex<BTreeMap<u16, TxRun>>,
    stop: &AtomicBool,
    counts: &Counts,
    clock: &dyn MediaClock,
    sound: &dyn Sound,
    rate: u32,
) {
    let mut samples: Vec<i32> = Vec::new();
    let mut chan: Vec<i32> = Vec::new();
    // Datagrams of this wake, their buffers kept for the next.
    let mut out: Vec<(Vec<u8>, SocketAddr)> = Vec::new();
    let mut spare: Vec<Vec<u8>> = Vec::new();
    let lead = audio::lead(rate);
    // A jump back larger than this re-aligns (§9: 192 000 at 48 kHz, 4 s).
    let back = u64::from(rate) * 4;
    let mut keepalives_read = Instant::now();
    // Any datagram back counts (§5.6), whatever its size: a 64-byte buffer failed larger
    // ones on Windows (WSAEMSGSIZE, no source; bug run K2-7).
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::Relaxed) {
        let Some(now) = clock.now() else {
            // Nothing sent, so no receiver owes a keepalive: the flows' 4 s start again
            // when the clock is back (bug run I2-4: a flow set up meanwhile expired on
            // its first packet).
            for f in tx.lock_ok().values_mut() {
                f.last_keepalive = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(5));
            continue;
        };
        let mut wait = Duration::from_millis(5);
        {
            let mut flows = tx.lock_ok();
            // Keepalives (any datagram back counts, §5.6), now and then.
            if keepalives_read.elapsed() >= KEEPALIVES_READ_EVERY {
                keepalives_read = Instant::now();
                // At most 256 a time: a flood to this port doesn't stop every flow's
                // sending while it lasts (bug run N1-7, N4-1).
                for _ in 0..256 {
                    let Ok((_, from)) = socket.recv_from(&mut buf) else {
                        break;
                    };
                    for f in flows.values_mut().filter(|f| f.to == from) {
                        f.last_keepalive = Instant::now();
                    }
                }
                let gone: Vec<u16> = flows
                    .iter()
                    .filter(|(_, f)| !f.multicast && f.last_keepalive.elapsed() >= TX_EXPIRES)
                    .map(|(n, _)| *n)
                    .collect();
                for n in gone {
                    if let Some(f) = flows.remove(&n) {
                        counts.expired.fetch_add(1, Ordering::Relaxed);
                        let mut e = counts.expired_flows.lock_ok();
                        // Bounded: a program that never takes them keeps the last 64.
                        if e.len() >= 64 {
                            e.remove(0);
                        }
                        e.push((n, f.to));
                    }
                }
            }
            for f in flows.values_mut() {
                let next = *f.next.get_or_insert_with(|| audio::align(now, f.fpp));
                // Far behind (ten packets) or a clock that went back a long way: aligned
                // to now again (§5.5).
                let mut t = if now > next + 10 * f.fpp || next > now + back {
                    audio::align(now, f.fpp)
                } else {
                    next
                };
                // Every packet now due, in order (a late wake sends what it owes).
                while t <= now {
                    let frames = f.fpp as usize;
                    let nch = f.channels.len();
                    samples.clear();
                    samples.resize(frames * nch, 0);
                    for (slot, &ch) in f.channels.iter().enumerate() {
                        if ch == 0 {
                            continue;
                        }
                        chan.clear();
                        chan.resize(frames, 0);
                        sound.read_raw(usize::from(ch), t, &mut chan);
                        for (i, v) in chan.iter().enumerate() {
                            samples[i * nch + slot] = *v;
                        }
                    }
                    let mut packet = spare.pop().unwrap_or_default();
                    // Stamped half a millisecond early (§5.3).
                    audio::encode_i32(t.saturating_sub(lead), rate, f.bytes, &samples, &mut packet);
                    out.push((packet, f.to));
                    t += f.fpp;
                }
                f.next = Some(t);
                let due = Duration::from_nanos((t - now) * 1_000_000_000 / u64::from(rate.max(1)));
                wait = wait.min(due);
            }
        }
        if !out.is_empty() {
            let n = send_all(socket, &out);
            counts.sent.fetch_add(n as u64, Ordering::Relaxed);
            spare.extend(out.drain(..).map(|(p, _)| p));
        }
        std::thread::sleep(wait.max(Duration::from_micros(100)));
    }
}

/// Reads up to [`BATCH`] datagrams from `s` into `bufs`: their lengths and sources.
fn read_batch(s: &UdpSocket, bufs: &mut [Vec<u8>], got: &mut Vec<(usize, SocketAddr)>) {
    got.clear();
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let n = bufs.len();
        // SAFETY: zeroed sockaddr_in and mmsghdr are valid empty values.
        let mut addrs: Vec<libc::sockaddr_in> = vec![unsafe { std::mem::zeroed() }; n];
        let mut iovs: Vec<libc::iovec> = bufs
            .iter_mut()
            .map(|b| libc::iovec {
                iov_base: b.as_mut_ptr() as *mut libc::c_void,
                iov_len: b.len(),
            })
            .collect();
        let mut msgs: Vec<libc::mmsghdr> = (0..n)
            .map(|i| {
                let mut m: libc::mmsghdr = unsafe { std::mem::zeroed() };
                m.msg_hdr.msg_name = &mut addrs[i] as *mut libc::sockaddr_in as *mut libc::c_void;
                m.msg_hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
                m.msg_hdr.msg_iov = &mut iovs[i];
                m.msg_hdr.msg_iovlen = 1;
                m
            })
            .collect();
        // SAFETY: every message points into `addrs`, `iovs` and `bufs`, live for the call.
        let r = unsafe {
            libc::recvmmsg(
                s.as_raw_fd(),
                msgs.as_mut_ptr(),
                n as libc::c_uint,
                libc::MSG_DONTWAIT,
                std::ptr::null_mut(),
            )
        };
        for (i, m) in msgs.iter().enumerate().take(r.max(0) as usize) {
            let a = &addrs[i];
            let ip = Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr));
            got.push((
                m.msg_len as usize,
                SocketAddr::from((ip, u16::from_be(a.sin_port))),
            ));
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Windows reports a receiver that went away (an ICMP "port unreachable" for a
        // packet sent from this socket) as "connection reset" on the next read: skipped,
        // the datagrams behind it still read.
        let mut i = 0;
        let mut resets = 0;
        while i < bufs.len() {
            match s.recv_from(&mut bufs[i]) {
                Ok((n, from)) => {
                    got.push((n, from));
                    i += 1;
                }
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset && resets < BATCH => {
                    resets += 1
                }
                Err(_) => break,
            }
        }
    }
}

/// Windows: a socket that sends to a receiver gone away (an ICMP "port unreachable")
/// isn't told so as an error on its next read, one per packet sent there. Without this a
/// transmit socket still sending to a gone receiver reads its errors instead of the other
/// receivers' keepalives. Elsewhere such errors aren't given to unconnected sockets.
pub fn no_udp_resets(s: &UdpSocket) {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock::{SIO_UDP_CONNRESET, WSAIoctl};
        let off: u32 = 0;
        let mut returned: u32 = 0;
        // SAFETY: an open socket; a 4-byte input it only reads, no output, no overlapped
        // call. A failure leaves the socket as it was (its errors then skipped on read).
        unsafe {
            WSAIoctl(
                s.as_raw_socket() as usize,
                SIO_UDP_CONNRESET,
                (&off as *const u32).cast(),
                4,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
                None,
            );
        }
    }
    #[cfg(not(windows))]
    let _ = s;
}

/// A socket as the system's wait calls take it.
#[cfg(unix)]
type RawSock = std::os::fd::RawFd;
#[cfg(windows)]
type RawSock = std::os::windows::io::RawSocket;
#[cfg(not(any(unix, windows)))]
type RawSock = ();

fn raw_sock(s: &UdpSocket) -> RawSock {
    #[cfg(unix)]
    {
        std::os::fd::AsRawFd::as_raw_fd(s)
    }
    #[cfg(windows)]
    {
        std::os::windows::io::AsRawSocket::as_raw_socket(s)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = s;
    }
}

/// Waits up to `timeout` for any of the sockets `fds` to have a packet (Linux: `poll`;
/// Windows: `WSAPoll`; elsewhere a short sleep); `ready[i]`: socket i may have one (all
/// where the wait can't tell).
fn wait_readable(fds: &[RawSock], timeout: Duration, ready: &mut Vec<bool>) {
    ready.clear();
    ready.resize(fds.len(), true);
    #[cfg(target_os = "linux")]
    {
        let mut p: Vec<libc::pollfd> = fds
            .iter()
            .map(|&fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        // SAFETY: `p` holds `p.len()` valid pollfd entries (a descriptor closed meanwhile
        // comes back as POLLNVAL, harmless).
        let r = unsafe {
            libc::poll(
                p.as_mut_ptr(),
                p.len() as libc::nfds_t,
                timeout.as_millis().max(1) as i32,
            )
        };
        // Only what poll said (an error or a hang-up is read too: the read says what).
        if r >= 0 {
            for (r, q) in ready.iter_mut().zip(&p) {
                *r = q.revents != 0;
            }
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Networking::WinSock::{POLLRDNORM, WSAPOLLFD, WSAPoll};
        let mut p: Vec<WSAPOLLFD> = fds
            .iter()
            .map(|&fd| WSAPOLLFD {
                fd: fd as usize,
                events: POLLRDNORM,
                revents: 0,
            })
            .collect();
        // SAFETY: `p` holds `p.len()` valid entries for open sockets (one closed meanwhile
        // is reported in `revents`, harmless); a failure is a plain return.
        let r = unsafe {
            WSAPoll(
                p.as_mut_ptr(),
                p.len() as u32,
                timeout.as_millis().max(1) as i32,
            )
        };
        if r < 0 {
            std::thread::sleep(timeout.min(Duration::from_micros(500)));
        } else {
            for (r, q) in ready.iter_mut().zip(&p) {
                *r = q.revents != 0;
            }
        }
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = fds;
        std::thread::sleep(timeout.min(Duration::from_micros(500)));
    }
}

/// The least time between two reads of the receive sockets: packets wait in their
/// buffers meanwhile, so a wake takes several flows' packets (well inside the latency).
const RX_WAKE_EVERY: Duration = Duration::from_micros(500);

fn receive_loop(
    rx: &Mutex<BTreeMap<u16, RxRun>>,
    stop: &AtomicBool,
    counts: &Counts,
    clock: &dyn MediaClock,
    sound: &dyn Sound,
    rate: u32,
) {
    let mut bufs: Vec<Vec<u8>> = (0..BATCH).map(|_| vec![0u8; DATAGRAM]).collect();
    let mut got: Vec<(usize, SocketAddr)> = Vec::with_capacity(BATCH);
    let mut fds: Vec<RawSock> = Vec::new();
    // The flows waited on, in `fds`' order, and which of them have packets.
    let mut ids: Vec<u16> = Vec::new();
    let mut ready: Vec<bool> = Vec::new();
    // A quarter second: well past the largest latency (40 ms), well inside what the rings
    // keep ahead of the clock (bug run I2-7: a second, and the clearing half a ring ahead
    // erased what was kept 0.68–1 s ahead).
    let ahead = u64::from(rate) / 4;
    let late_taken = late_taken(rate);
    let mut last_wake = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        // Until a packet comes on any flow (or 5 ms, for keepalives and new flows), and
        // no sooner than RX_WAKE_EVERY after the last wake.
        if let Some(w) = RX_WAKE_EVERY.checked_sub(last_wake.elapsed()) {
            std::thread::sleep(w);
        }
        let any = {
            let flows = rx.lock_ok();
            fds.clear();
            ids.clear();
            for (&id, f) in flows.iter() {
                fds.push(raw_sock(&f.socket));
                ids.push(id);
            }
            !flows.is_empty()
        };
        if !any {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        wait_readable(&fds, Duration::from_millis(5), &mut ready);
        last_wake = Instant::now();
        let now = clock.now();
        let mut flows = rx.lock_ok();
        for (id, f) in flows.iter_mut() {
            // Only the sockets with packets (a read of the others would come back empty: a
            // system call each, about a quarter of the reads at 32-sample packets, profile
            // round 14). A flow added since the wait is read.
            let has = match ids.binary_search(id) {
                Ok(i) => ready.get(i).copied().unwrap_or(true),
                Err(_) => true,
            };
            let mut batches = 0;
            loop {
                if !has {
                    break;
                }
                read_batch(&f.socket, &mut bufs, &mut got);
                if got.is_empty() {
                    break;
                }
                batches += 1;
                let mut taken = false;
                for (k, &(n, from)) in got.iter().enumerate() {
                    if f.source.is_some_and(|ip| ip != from.ip()) {
                        continue;
                    }
                    let Some((secs, sample, data)) = audio::header(&bufs[k][..n]) else {
                        continue;
                    };
                    // Keepalives go where its audio comes from, taken or not (a sender
                    // kept while this side's clock is out).
                    f.from = Some(from);
                    // This side's clock out: not played, but the flow is alive (bug run
                    // K2-4: every flow was dropped as lost and set up again every 6–8 s
                    // of an outage).
                    let Some(now) = now else {
                        taken = true;
                        continue;
                    };
                    let t = audio::join(secs, sample, rate, now);
                    // Past its play-out point by more than `late_taken`, or further ahead
                    // than any latency: dropped.
                    if t + f.latency + late_taken < now || t > now + ahead {
                        counts.dropped.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    f.most_late
                        .fetch_max(now.saturating_sub(t), Ordering::Relaxed);
                    counts.received.fetch_add(1, Ordering::Relaxed);
                    taken = true;
                    let nch = f.slots.len();
                    for (slot, rxs) in f.slots.iter().enumerate() {
                        if rxs.is_empty() {
                            continue;
                        }
                        for &ch in rxs {
                            sound.write_slot(ch, t + f.latency, data, nch, f.bytes, slot);
                        }
                    }
                }
                // Alive by what it played, not what came (bug run I2-8: a sender on
                // another clock, all dropped, looked "receiving").
                if taken {
                    *f.last_packet.lock_ok() = Some(Instant::now());
                }
                // At most a few batches a socket a wake: a flood to one port doesn't hold
                // the thread (and the other flows) for as long as it lasts (bug run N4-1).
                if got.len() < BATCH || batches >= 4 {
                    break;
                }
            }
            // Keepalives on unicast flows, to where its packets come from (§5.6).
            if !f.multicast
                && let Some(to) = f.from
                && f.last_keepalive.elapsed() >= KEEPALIVE_EVERY
            {
                let _ = f.socket.send_to(&KEEPALIVE, to);
                f.last_keepalive = Instant::now();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clock that runs at 48 kHz from a start.
    struct Clock(Instant, u64);
    impl MediaClock for Clock {
        fn now(&self) -> Option<u64> {
            Some(self.1 + self.0.elapsed().as_nanos() as u64 * 48 / 1_000_000)
        }
    }

    /// Sound that sends a ramp on every channel (its media time) and records writes.
    #[derive(Default)]
    struct Rec(Mutex<Vec<(usize, u64, Vec<f32>)>>);
    impl Sound for Rec {
        fn read(&self, ch: usize, at: u64, out: &mut [f32]) {
            for (i, v) in out.iter_mut().enumerate() {
                *v = (((at + i as u64) % 1000) as f32 / 1000.0) * if ch == 2 { -1.0 } else { 1.0 };
            }
        }
        fn write(&self, ch: usize, at: u64, s: &[f32]) {
            self.0.lock_ok().push((ch, at, s.to_vec()));
        }
    }

    #[test]
    fn a_flow_from_sender_to_receiver() {
        let clock = Arc::new(Clock(Instant::now(), 1_700_000_000 * 48_000));
        let tx_sound = Arc::new(Rec::default());
        let rx_sound = Arc::new(Rec::default());
        let sender = Engine::start(clock.clone(), tx_sound, 48_000, Ipv4Addr::LOCALHOST).unwrap();
        let receiver =
            Engine::start(clock.clone(), rx_sound.clone(), 48_000, Ipv4Addr::LOCALHOST).unwrap();
        // Two flows to two receive sockets (sent together, received apart).
        let rs = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let rs2 = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let (to, to2) = (rs.local_addr().unwrap(), rs2.local_addr().unwrap());
        // RX 5 takes slot 1 (TX 1), RX 6 slot 2 (TX 2); 2 ms latency. RX 7 from flow 2.
        receiver
            .rx
            .lock_ok()
            .insert(1, RxRun::new(rs, vec![vec![5], vec![6]], 3, 96, false));
        receiver
            .rx
            .lock_ok()
            .insert(2, RxRun::new(rs2, vec![vec![7]], 3, 96, false));
        sender
            .tx
            .lock_ok()
            .insert(1, TxRun::new(to, vec![1, 2], 3, 32, false));
        sender
            .tx
            .lock_ok()
            .insert(2, TxRun::new(to2, vec![1], 3, 32, false));
        std::thread::sleep(Duration::from_millis(300));
        let got = rx_sound.0.lock_ok().clone();
        assert!(got.len() > 50, "{} writes", got.len());
        assert!(got.iter().any(|w| w.0 == 7));
        assert!(receiver.counts.received.load(Ordering::Relaxed) > 25);
        // The samples are where they were sent (stamped 24 early, played 96 later).
        let (ch, at, s) = got.iter().find(|w| w.0 == 6).unwrap().clone();
        assert_eq!(ch, 6);
        let sent_at = at - 96 + 24;
        let want = -((sent_at % 1000) as f32 / 1000.0);
        assert!((s[0] - want).abs() < 1e-4, "{} vs {want}", s[0]);
        // Keepalives reached the sender (matched to each flow by where they came from).
        std::thread::sleep(Duration::from_millis(100));
        let t = sender.tx.lock_ok();
        assert!(
            t.values()
                .all(|f| f.last_keepalive.elapsed() < Duration::from_secs(1))
        );
    }
}

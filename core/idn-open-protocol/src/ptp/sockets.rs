//! The site clock's sockets: datagrams in (timed by the kernel where it can) and out, the
//! version filter, and each port's timers.

use super::{ClockConfig, Protocol, Site, is_v1, mono_ns};
use idn_common::sync::LockOk as _;
use statime::port::{PortAction, PortActionIterator, TimestampContext};
use statime::time::Time;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

pub(super) fn socket(cfg: &ClockConfig, port: u16) -> io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    #[cfg(unix)]
    s.set_reuse_port(true)?;
    s.bind(&SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)).into())?;
    use super::Protocol as P;
    if cfg.protocol != P::V1 {
        s.join_multicast_v4(&cfg.group, &cfg.interface)?;
    }
    if cfg.protocol == P::V1 || (cfg.protocol == P::Both && cfg.v1_group() != cfg.group) {
        s.join_multicast_v4(&cfg.v1_group(), &cfg.interface)?;
    }
    s.set_multicast_loop_v4(true)?;
    s.set_multicast_ttl_v4(1)?;
    if !cfg.interface.is_unspecified() {
        s.set_multicast_if_v4(&cfg.interface)?;
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let on: libc::c_int = 1;
        // The kernel's receive time, so a late thread wake-up doesn't count as path delay.
        // SAFETY: `on` is a c_int of the size given, alive through the call.
        unsafe {
            libc::setsockopt(
                s.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_TIMESTAMPNS,
                &on as *const _ as *const libc::c_void,
                std::mem::size_of_val(&on) as libc::socklen_t,
            )
        };
    }
    Ok(s.into())
}

/// Whether the port takes this datagram: only its own PTP version (a network may carry
/// both: Dante-compatible equipment with AES67 on sends v1 and v2), not our own messages
/// back through the multicast loop.
pub(super) fn wanted(protocol: Protocol, data: &[u8], me: &[u8; 8]) -> bool {
    match protocol {
        Protocol::V2 => data.len() >= 34 && !is_v1(data) && data[20..28] != *me,
        // v1: a 40-byte header; the sender's UUID at 22..28 (ours: the identity's first 6).
        Protocol::V1 => data.len() >= 40 && is_v1(data) && data[22..28] != me[..6],
        Protocol::Both => wanted(Protocol::V1, data, me) || wanted(Protocol::V2, data, me),
    }
}

pub(super) fn site_at(clock: &Site, mono: i64) -> Time {
    clock
        .lock_ok()
        .time_from_underlying(Time::from_nanos(mono as u64))
}

/// SO_TIMESTAMPNS on a socket, for [`recv_timed`] (tests' stand-in leaders).
#[doc(hidden)]
pub fn timestamps_on(sock: &UdpSocket) {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let on: libc::c_int = 1;
        // SAFETY: `on` is a c_int of the size given, alive through the call.
        unsafe {
            libc::setsockopt(
                sock.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_TIMESTAMPNS,
                &on as *const _ as *const libc::c_void,
                std::mem::size_of_val(&on) as libc::socklen_t,
            )
        };
    }
    #[cfg(not(target_os = "linux"))]
    let _ = sock;
}

/// A datagram and when it arrived (monotonic): the kernel's time where it gives one (the
/// socket must have SO_TIMESTAMPNS on, [`timestamps_on`]; tests use it too).
#[doc(hidden)]
pub fn recv_timed(sock: &UdpSocket, buf: &mut [u8]) -> io::Result<(usize, i64)> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut libc::c_void,
            iov_len: buf.len(),
        };
        let mut control = [0u64; 16];
        // SAFETY: msghdr is plain data; zeroed is valid (its buffers are set below).
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = std::mem::size_of_val(&control) as _;
        // SAFETY: `msg` points at `iov` (all of `buf`) and `control`, both alive through the call.
        let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mono = mono_ns();
        // SAFETY: `msg` as the kernel left it; the CMSG macros stay within `control`.
        let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
        while !cmsg.is_null() {
            let c = unsafe { &*cmsg };
            if c.cmsg_level == libc::SOL_SOCKET && c.cmsg_type == libc::SCM_TIMESTAMPNS {
                // SAFETY: SCM_TIMESTAMPNS carries a timespec; it is the only option set on this
                // socket, so its one message fits in the 128 bytes of `control`.
                let ts: libc::timespec = unsafe {
                    std::ptr::read_unaligned(libc::CMSG_DATA(cmsg) as *const libc::timespec)
                };
                // The kernel's stamp is on the real-time clock: how long ago, in monotonic time.
                let mut now = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                };
                // SAFETY: fills a timespec of our own.
                unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut now) };
                let ago = (now.tv_sec - ts.tv_sec) as i64 * 1_000_000_000
                    + (now.tv_nsec - ts.tv_nsec) as i64;
                return Ok((n as usize, mono - ago.clamp(0, 100_000_000)));
            }
            // SAFETY: `cmsg` is a header within `msg`'s control buffer (above).
            cmsg = unsafe { libc::CMSG_NXTHDR(&msg, cmsg) };
        }
        Ok((n as usize, mono))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let (n, _) = sock.recv_from(buf)?;
        Ok((n, mono_ns()))
    }
}

pub(super) struct Packet {
    pub(super) data: Vec<u8>,
    pub(super) at: Time,
    pub(super) is_event: bool,
}

/// A port's timers, and the event it sent last (its send time, for statime).
#[derive(Default)]
pub(super) struct Timers {
    pub(super) announce: Option<Instant>,
    pub(super) sync: Option<Instant>,
    pub(super) delay_req: Option<Instant>,
    pub(super) announce_receipt: Option<Instant>,
    pub(super) filter: Option<Instant>,
    pub(super) sent: Option<(TimestampContext, Time)>,
}

impl Timers {
    pub(super) fn next_deadline(&self) -> Option<Instant> {
        [
            self.announce,
            self.sync,
            self.delay_req,
            self.announce_receipt,
            self.filter,
        ]
        .into_iter()
        .flatten()
        .min()
    }
}

/// The sockets (shared by the ports: v1 and v2 use the same ports and group) and each
/// port's timers.
pub(super) struct Io {
    pub(super) event: UdpSocket,
    pub(super) general: UdpSocket,
    /// Each port's group (v1's subdomain has its own).
    pub(super) groups: Vec<Ipv4Addr>,
    pub(super) event_port: u16,
    pub(super) general_port: u16,
    pub(super) ports: Vec<Timers>,
}

impl Io {
    pub(super) fn new(event: UdpSocket, general: UdpSocket, ports: usize) -> Io {
        Io {
            event,
            general,
            groups: vec![Ipv4Addr::UNSPECIFIED; ports],
            event_port: 0,
            general_port: 0,
            ports: (0..ports).map(|_| Timers::default()).collect(),
        }
    }

    /// Port `k`'s actions.
    pub(super) fn handle(&mut self, k: usize, actions: PortActionIterator<'_>, clock: &Site) {
        let at = |d: Duration| Some(Instant::now() + d);
        for action in actions {
            let t = &mut self.ports[k];
            match action {
                PortAction::SendEvent { context, data, .. } => {
                    // The start of the call: a network card sends soon after it, and on one
                    // machine the packet is delivered (and stamped by the receiving kernel)
                    // inside it, so a later stamp made the path delay negative. The same
                    // in both directions, so the offset is unaffected.
                    let mono = mono_ns();
                    let _ = self
                        .event
                        .send_to(data, SocketAddrV4::new(self.groups[k], self.event_port));
                    t.sent = Some((context, site_at(clock, mono)));
                }
                PortAction::SendGeneral { data, .. } => {
                    let _ = self
                        .general
                        .send_to(data, SocketAddrV4::new(self.groups[k], self.general_port));
                }
                PortAction::ResetAnnounceTimer { duration } => t.announce = at(duration),
                PortAction::ResetSyncTimer { duration } => t.sync = at(duration),
                PortAction::ResetDelayRequestTimer { duration } => t.delay_req = at(duration),
                PortAction::ResetAnnounceReceiptTimer { duration } => {
                    t.announce_receipt = at(duration)
                }
                PortAction::ResetFilterUpdateTimer { duration } => t.filter = at(duration),
                PortAction::ForwardTLV { .. } => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_protocol_takes_only_its_own_messages() {
        let me = [1, 2, 3, 4, 5, 6, 7, 8];
        let mut v2 = [0u8; 44];
        v2[1] = 0x02; // versionPTP 2
        let mut v1 = [0u8; 124];
        v1[1] = 1; // versionPTP 1
        assert!(wanted(Protocol::V2, &v2, &[9; 8]));
        assert!(!wanted(Protocol::V2, &v1, &[9; 8]));
        assert!(wanted(Protocol::V1, &v1, &me));
        assert!(!wanted(Protocol::V1, &v2, &me));
        // Both, as a bridge: either version.
        assert!(wanted(Protocol::Both, &v1, &me) && wanted(Protocol::Both, &v2, &[9; 8]));
        // A v1 Delay_Req from a follower: taken (a v1 clock may lead).
        v1[32] = 1;
        assert!(wanted(Protocol::V1, &v1, &me));
        // Our own messages back through the loop (v2: the identity; v1: its first six
        // bytes as the UUID); too short.
        v2[20..28].copy_from_slice(&me);
        assert!(!wanted(Protocol::V2, &v2, &me));
        v1[22..28].copy_from_slice(&me[..6]);
        assert!(!wanted(Protocol::V1, &v1, &me) && !wanted(Protocol::Both, &v1, &me));
        assert!(!wanted(Protocol::V1, &v1[..39], &[9; 8]));
    }
}

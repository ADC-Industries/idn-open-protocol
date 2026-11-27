//! Media transports other than QUIC (spec chapter 6).

pub mod receiver;
pub mod tcp;
pub mod udp;

/// Turns on TCP keepalive: probes after `idle` without traffic, then every second, three
/// times, so a peer that vanished without closing (power loss, cable pull, sleep) is
/// noticed within about `idle` + 3 s instead of never (an idle connection otherwise
/// stays "open" forever).
pub(crate) fn keepalive(s: &std::net::TcpStream, idle: std::time::Duration) {
    let ka = socket2::TcpKeepalive::new()
        .with_time(idle)
        .with_interval(std::time::Duration::from_secs(1));
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    let ka = ka.with_retries(3);
    let _ = socket2::SockRef::from(s).set_tcp_keepalive(&ka);
}

/// How long a write to a peer may block before the peer counts as gone: a receiver or
/// server that stops reading must not stall the thread (and the locks) writing to it.
pub(crate) const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether a read error is just the socket's read timeout expiring (try again), as
/// opposed to a dead connection. On Unix the timeout is `WouldBlock` (EAGAIN) and
/// `TimedOut` (ETIMEDOUT) means TCP keepalive or retransmission gave up: treating that as
/// "try again" hid dead peers. Windows reports the read timeout itself as `TimedOut`.
/// `Interrupted` (EINTR) is a try-again too: on Linux a read on a socket with a read
/// timeout is never restarted after a signal or a stop (a debugger stopping every thread,
/// SIGSTOP/SIGCONT), and taking it for a dead connection dropped trust sessions and the
/// pairings waiting on them under load.
#[doc(hidden)]
pub fn is_poll_timeout(e: &std::io::Error) -> bool {
    let k = e.kind();
    k == std::io::ErrorKind::WouldBlock
        || k == std::io::ErrorKind::Interrupted
        || (cfg!(windows) && k == std::io::ErrorKind::TimedOut)
}

/// Sends datagrams to `to`: on Linux in batches of up to 64 per system call
/// (`sendmmsg`), elsewhere one by one. A 1 MB video message is about 800 packets, so
/// this is ~13 calls instead of ~800 (R1-26). Partial sends are retried from where they
/// stopped; the first error is returned after trying the rest.
pub(crate) fn send_batch(
    sock: &std::net::UdpSocket,
    to: std::net::SocketAddr,
    packets: &[Vec<u8>],
) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let addr = socket2::SockAddr::from(to);
        let mut first_err = None;
        for chunk in packets.chunks(64) {
            let mut iov: Vec<libc::iovec> = chunk
                .iter()
                .map(|p| libc::iovec {
                    iov_base: p.as_ptr() as *mut libc::c_void,
                    iov_len: p.len(),
                })
                .collect();
            let mut msgs: Vec<libc::mmsghdr> = iov
                .iter_mut()
                .map(|v| {
                    // SAFETY: a zeroed msghdr is valid; the fields set below point into
                    // `addr` and `iov`, which outlive the call.
                    let mut h: libc::mmsghdr = unsafe { std::mem::zeroed() };
                    h.msg_hdr.msg_name = addr.as_ptr() as *mut libc::c_void;
                    h.msg_hdr.msg_namelen = addr.len();
                    h.msg_hdr.msg_iov = v as *mut libc::iovec;
                    h.msg_hdr.msg_iovlen = 1;
                    h
                })
                .collect();
            let mut done = 0;
            while done < msgs.len() {
                // SAFETY: `msgs[done..]` are valid headers (see above).
                let n = unsafe {
                    libc::sendmmsg(
                        sock.as_raw_fd(),
                        msgs[done..].as_mut_ptr(),
                        (msgs.len() - done) as libc::c_uint,
                        0,
                    )
                };
                if n < 0 {
                    let e = std::io::Error::last_os_error();
                    if e.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    // This packet is lost (FEC or the next frame covers it): go on.
                    first_err.get_or_insert(e);
                    done += 1;
                } else {
                    done += n as usize;
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let mut first_err = None;
        for p in packets {
            if let Err(e) = sock.send_to(p, to) {
                first_err.get_or_insert(e);
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

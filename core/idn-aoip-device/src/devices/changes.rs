//! The devices' change notifications (§0.4): a sender re-read at once.

use super::*;

/// The notifications group and port (§0.4).
pub(crate) const CHANGES: (Ipv4Addr, u16) = (Ipv4Addr::new(224, 0, 0, 231), 8702);

/// Hears the devices' change notifications (§0.4): one that names a change of channels,
/// subscriptions, names or settings has its sender read again at once (else within 30 s).
pub(crate) fn changes(ip: Ipv4Addr, devices: &Mutex<BTreeMap<String, Entry>>) {
    let socket = match changes_socket(ip) {
        Ok(s) => s,
        Err(e) => {
            idn_common::log::log_or_stderr(&format!(
                "idn-aoip: network devices: changes not heard ({e}); read every 30 s"
            ));
            return;
        }
    };
    let mut buf = vec![0u8; 2048];
    loop {
        let Ok((n, from)) = socket.recv_from(&mut buf) else {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };
        if !names_a_change(&buf[..n]) && !is_status(&buf[..n]) {
            continue;
        }
        let mut d = devices.lock_ok();
        for e in d.values_mut() {
            if e.seen.as_ref().is_some_and(|s| s.arc.ip() == from.ip()) {
                e.wanted = true;
            }
        }
    }
}

/// The notifications' socket: the port shared (other programs on this machine may listen
/// on it too), the group joined on the interface.
pub(crate) fn changes_socket(ip: Ipv4Addr) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    #[cfg(unix)]
    s.set_reuse_port(true)?;
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, CHANGES.1)).into())?;
    s.join_multicast_v4(&CHANGES.0, &ip)?;
    Ok(s.into())
}

/// A device's channel-change notification on the notification port (idn-aoip-device.md §4.7:
/// protocol 0xFFFF, a 32-byte header, its length right, type `07 xx 01 02`; lab 522(b):
/// devices send the change in this form, not the ARC one). Only that type: board,
/// product, network and clock status go to the same port whenever a controller asks
/// (bug run I3-7: a controller polling the clock had every device read every second).
pub(crate) fn is_status(d: &[u8]) -> bool {
    d.len() >= 32
        && d[..2] == [0xFF, 0xFF]
        && usize::from(u16::from_be_bytes([d[2], d[3]])) == d.len()
        && d[24] == 0x07
        && d[26..28] == [0x01, 0x02]
}

/// Whether a notification names a change that makes a controller read the device again
/// (§0.4: protocol 0x27FF, its length right, the ID at offset 26 one of 16, 256–262,
/// 288, 4110).
pub(crate) fn names_a_change(d: &[u8]) -> bool {
    let word = |at: usize| d.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
    word(0) == Some(inv::LEGACY)
        && word(2).map(usize::from) == Some(d.len())
        && word(26).is_some_and(|id| matches!(id, 16 | 256..=262 | 288 | 4110))
}

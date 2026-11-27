//! IPv4 interface enumeration, so mDNS can be sent on every interface explicitly
//! instead of relying on a multicast route.

use std::net::Ipv4Addr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    pub addr: Ipv4Addr,
    pub netmask: Ipv4Addr,
    pub loopback: bool,
}

impl Interface {
    /// Whether `ip` is on this interface's subnet.
    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        let m = u32::from(self.netmask);
        u32::from(self.addr) & m == u32::from(ip) & m
    }
}

/// Up, multicast-capable IPv4 interfaces. Loopback is included only if nothing else is.
#[cfg(unix)]
pub fn ipv4_multicast_interfaces() -> Vec<Interface> {
    let mut out = Vec::new();
    // SAFETY: getifaddrs/freeifaddrs as documented; the list is only read before freeing.
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 {
            return out;
        }
        let mut p = head;
        while !p.is_null() {
            let ifa = &*p;
            p = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || (*ifa.ifa_addr).sa_family as i32 != libc::AF_INET {
                continue;
            }
            let flags = ifa.ifa_flags as i32;
            if flags & libc::IFF_UP == 0 {
                continue;
            }
            let loopback = flags & libc::IFF_LOOPBACK != 0;
            if !loopback && flags & libc::IFF_MULTICAST == 0 {
                continue;
            }
            let v4 = |a: *const libc::sockaddr| {
                Ipv4Addr::from(u32::from_be(
                    (*(a as *const libc::sockaddr_in)).sin_addr.s_addr,
                ))
            };
            let addr = v4(ifa.ifa_addr);
            let netmask = if ifa.ifa_netmask.is_null() {
                Ipv4Addr::BROADCAST
            } else {
                v4(ifa.ifa_netmask)
            };
            let name = std::ffi::CStr::from_ptr(ifa.ifa_name)
                .to_string_lossy()
                .into_owned();
            out.push(Interface {
                name,
                addr,
                netmask,
                loopback,
            });
        }
        libc::freeifaddrs(head);
    }
    if out.iter().any(|i| !i.loopback) {
        out.retain(|i| !i.loopback);
    }
    out
}

/// Not on Unix (Windows): the interface the default route uses, found by connecting a
/// UDP socket to the mDNS group, as one interface with a /24. Enough to publish and
/// browse on the usual single-network machine; the full adapter list is a later item.
#[cfg(not(unix))]
pub fn ipv4_multicast_interfaces() -> Vec<Interface> {
    let addr = idn_common::net::local_v4_towards(Ipv4Addr::new(224, 0, 0, 251));
    match addr {
        Some(addr) => vec![Interface {
            name: "default".into(),
            addr,
            netmask: Ipv4Addr::new(255, 255, 255, 0),
            loopback: addr.is_loopback(),
        }],
        None => Vec::new(),
    }
}

/// Whether `ip` is on one of this host's IPv4 subnets (or is loopback): a sender on the
/// local link, as mDNS requires before answering by unicast (RFC 6762 §11).
#[cfg(unix)]
pub fn is_on_link(ip: Ipv4Addr) -> bool {
    if ip.is_loopback() {
        return true;
    }
    let mut on = false;
    // SAFETY: getifaddrs/freeifaddrs as documented; the list is only read before freeing, and an
    // address is read as a sockaddr_in only after its family is checked.
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 {
            return false;
        }
        let mut p = head;
        while !p.is_null() && !on {
            let ifa = &*p;
            p = ifa.ifa_next;
            if ifa.ifa_addr.is_null()
                || ifa.ifa_netmask.is_null()
                || (*ifa.ifa_addr).sa_family as i32 != libc::AF_INET
                || ifa.ifa_flags as i32 & libc::IFF_UP == 0
            {
                continue;
            }
            let v4 = |a: *const libc::sockaddr| {
                u32::from_be((*(a as *const libc::sockaddr_in)).sin_addr.s_addr)
            };
            let (addr, mask) = (v4(ifa.ifa_addr), v4(ifa.ifa_netmask));
            on = addr & mask == u32::from(ip) & mask;
        }
        libc::freeifaddrs(head);
    }
    on
}

#[cfg(not(unix))]
pub fn is_on_link(ip: Ipv4Addr) -> bool {
    ip.is_loopback() || ip.is_private() || ip.is_link_local()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_addresses_are_on_link_and_public_ones_are_not() {
        assert!(is_on_link(Ipv4Addr::LOCALHOST));
        for i in ipv4_multicast_interfaces() {
            assert!(is_on_link(i.addr), "{i:?}");
        }
        assert!(!is_on_link(Ipv4Addr::new(192, 0, 2, 77))); // TEST-NET-1, never local
    }
}

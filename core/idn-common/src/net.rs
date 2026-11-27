//! The machine's network links, for monitoring (N5, idn-node.md §N.30): each interface
//! with an IPv4 address, whether its link is up (a cable in, a Wi-Fi association), and
//! its speed when the system says.

use std::net::Ipv4Addr;

/// One network interface's link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// The interface's name (`eth0`, `Ethernet 2`).
    pub name: String,
    /// Its IPv4 address.
    pub addr: Ipv4Addr,
    /// The link is up: connected and carrying.
    pub up: bool,
    /// Its speed in Mbit/s, when known (Wi-Fi and virtual links often don't say).
    pub speed_mbps: Option<u64>,
    /// A network card (wired or Wi-Fi), not a virtual link (a container bridge, a VPN
    /// tunnel): only these are watched for being down or slow.
    pub physical: bool,
    /// A Wi-Fi card.
    pub wireless: bool,
    /// Its hardware (MAC) address, when the system says (a network-audio device reports
    /// it to controllers: lab lt18 showed 00:00:00:00:00:00).
    pub mac: Option<[u8; 6]>,
}

/// What lies between this machine and another (roadmap R.30): where a receiver starts its
/// latency before it has measured the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Path {
    /// The other end is this machine.
    Local,
    /// Through a wired network card.
    Wired,
    /// Through a Wi-Fi card.
    Wireless,
    /// Through a virtual link (a VPN, an overlay network): the internet, perhaps.
    Tunnel,
    /// Not known (no route, an interface not listed).
    Unknown,
}

impl Path {
    /// The latency a receiver starts at on this path, ms (it then adapts to what it
    /// measures): this machine 5, wired 10, Wi-Fi 40, a tunnel 60, unknown 20.
    pub fn start_ms(self) -> u32 {
        match self {
            Path::Local => 5,
            Path::Wired => 10,
            Path::Wireless => 40,
            Path::Tunnel => 60,
            Path::Unknown => 20,
        }
    }

    /// Its name, for logs and states.
    pub fn name(self) -> &'static str {
        match self {
            Path::Local => "local",
            Path::Wired => "wired",
            Path::Wireless => "wi-fi",
            Path::Tunnel => "tunnel",
            Path::Unknown => "unknown",
        }
    }
}

/// The path to `peer`: this machine (a loopback address or one of its own), else the link
/// the system's routes would send through (a UDP socket connected to it says which local
/// address: nothing is sent).
pub fn path_to(peer: std::net::IpAddr) -> Path {
    let links = links();
    path_with(peer, &links, |ip| match local_addr_towards(ip)? {
        std::net::IpAddr::V4(a) => Some(a),
        _ => None,
    })
}

/// This machine's address that `peer` is reached from, IPv4 or IPv6 as `peer` is: the one
/// the system's routes choose (a UDP socket connected to it says which; nothing is sent).
/// `None` with no route. On a machine with two networks, the first interface's address
/// is another network's: reply and advertise with this one.
pub fn local_addr_towards(peer: std::net::IpAddr) -> Option<std::net::IpAddr> {
    let any: std::net::IpAddr = if peer.is_ipv4() {
        Ipv4Addr::UNSPECIFIED.into()
    } else {
        std::net::Ipv6Addr::UNSPECIFIED.into()
    };
    let s = std::net::UdpSocket::bind((any, 0)).ok()?;
    s.connect((peer, 9)).ok()?;
    Some(s.local_addr().ok()?.ip()).filter(|a| !a.is_unspecified())
}

/// [`local_addr_towards`] for an IPv4 peer, as the IPv4 address it gives.
pub fn local_v4_towards(peer: Ipv4Addr) -> Option<Ipv4Addr> {
    match local_addr_towards(peer.into())? {
        std::net::IpAddr::V4(a) => Some(a),
        _ => None,
    }
}

/// The address this machine reaches the internet from: [`local_addr_towards`] an address
/// past the default route (192.0.2.1, documentation-only, RFC 5737: never answers).
/// `None` on a machine with no default route.
pub fn outgoing_ip() -> Option<std::net::IpAddr> {
    local_addr_towards(Ipv4Addr::new(192, 0, 2, 1).into())
}

/// [`path_to`] with the links and the route lookup given (for tests).
pub fn path_with(
    peer: std::net::IpAddr,
    links: &[Link],
    route: impl Fn(std::net::IpAddr) -> Option<Ipv4Addr>,
) -> Path {
    if peer.is_loopback() || links.iter().any(|l| std::net::IpAddr::V4(l.addr) == peer) {
        return Path::Local;
    }
    let Some(local) = route(peer) else {
        return Path::Unknown;
    };
    match links.iter().find(|l| l.addr == local) {
        Some(l) if l.physical && l.wireless => Path::Wireless,
        Some(l) if l.physical => Path::Wired,
        Some(_) => Path::Tunnel,
        None => Path::Unknown,
    }
}

/// Names of virtual links, for systems that don't say: container and
/// VM bridges, VPN tunnels.
pub fn virtual_name(name: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "docker",
        "br-",
        "veth",
        "virbr",
        "vnet",
        "wt",
        "wg",
        "tun",
        "tap",
        "zt",
        "tailscale",
        "vEthernet",
        "lxc",
        "lxd",
        "cni",
        "flannel",
        "podman",
        "vmnet",
        "vboxnet",
    ];
    PREFIXES.iter().any(|p| name.starts_with(p))
}

/// The machine's links, loopback left out.
pub fn links() -> Vec<Link> {
    #[cfg(target_os = "linux")]
    {
        linux()
    }
    #[cfg(windows)]
    {
        windows()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        Vec::new()
    }
}

#[cfg(target_os = "linux")]
fn linux() -> Vec<Link> {
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
            if flags & libc::IFF_LOOPBACK != 0 {
                continue;
            }
            let addr = Ipv4Addr::from(u32::from_be(
                (*(ifa.ifa_addr as *const libc::sockaddr_in))
                    .sin_addr
                    .s_addr,
            ));
            let name = std::ffi::CStr::from_ptr(ifa.ifa_name)
                .to_string_lossy()
                .into_owned();
            // An address's label (`eth0:1`) has no folder in /sys: its card's (bug run J3-7).
            let card = name.split(':').next().unwrap_or(&name).to_owned();
            // The kernel's speed (Mbit/s); -1 or missing where it doesn't know.
            let speed_mbps = std::fs::read_to_string(format!("/sys/class/net/{card}/speed"))
                .ok()
                .and_then(|s| s.trim().parse::<i64>().ok())
                .filter(|&s| s > 0)
                .map(|s| s as u64);
            // A network card has a device behind it; bridges and tunnels don't.
            let physical = std::path::Path::new(&format!("/sys/class/net/{card}/device")).exists()
                && !virtual_name(&name);
            // A Wi-Fi card has its wireless directory (or its phy80211 link).
            let wireless = std::path::Path::new(&format!("/sys/class/net/{card}/wireless"))
                .exists()
                || std::path::Path::new(&format!("/sys/class/net/{card}/phy80211")).exists();
            let mac = std::fs::read_to_string(format!("/sys/class/net/{card}/address"))
                .ok()
                .and_then(|s| parse_mac(s.trim()));
            out.push(Link {
                name,
                addr,
                up: flags & libc::IFF_UP != 0 && flags & libc::IFF_RUNNING != 0,
                speed_mbps,
                physical,
                wireless,
                mac,
            });
        }
        libc::freeifaddrs(head);
    }
    out
}

#[cfg(windows)]
fn windows() -> Vec<Link> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
        GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};
    // IfOperStatusUp, and the loopback's interface type.
    const UP: i32 = 1;
    const LOOPBACK: u32 = 24;
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size: u32 = 16 * 1024;
    let mut buf: Vec<u64> = Vec::new();
    // SAFETY: the buffer is sized as the call asks (retried once when too small) and only
    // read within it, as the call filled it.
    unsafe {
        let mut filled = false;
        for _ in 0..3 {
            buf.resize((size as usize).div_ceil(8), 0);
            let r = GetAdaptersAddresses(
                AF_INET as u32,
                flags,
                std::ptr::null(),
                buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
                &mut size,
            );
            // ERROR_BUFFER_OVERFLOW: again with the size asked for.
            if r == 111 {
                continue;
            }
            if r != 0 {
                return Vec::new();
            }
            filled = true;
            break;
        }
        // Still too small after three tries: nothing filled in to walk.
        if !filled {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut p = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while !p.is_null() {
            let a = &*p;
            p = a.Next;
            if a.IfType == LOOPBACK {
                continue;
            }
            // Every IPv4 address of the card, each a link with its MAC (bug run J3-7: only
            // the first; a second address on the audio network had none).
            let mut u = a.FirstUnicastAddress;
            let mut addrs = Vec::new();
            while !u.is_null() && addrs.len() < 64 {
                let sa = (*u).Address.lpSockaddr;
                if !sa.is_null() && (*sa).sa_family == AF_INET {
                    let sin = &*(sa as *const SOCKADDR_IN);
                    addrs.push(Ipv4Addr::from(u32::from_be(sin.sin_addr.S_un.S_addr)));
                }
                u = (*u).Next;
            }
            if addrs.is_empty() {
                continue;
            }
            let name = {
                let w = a.FriendlyName;
                let mut n = 0;
                while !w.is_null() && *w.add(n) != 0 && n < 256 {
                    n += 1;
                }
                if w.is_null() {
                    String::new()
                } else {
                    String::from_utf16_lossy(std::slice::from_raw_parts(w, n))
                }
            };
            let bits = a.TransmitLinkSpeed;
            // Ethernet (6) or Wi-Fi (71), not a tunnel; Hyper-V's switches are named.
            let physical = matches!(a.IfType, 6 | 71) && a.TunnelType == 0 && !virtual_name(&name);
            let mac = (a.PhysicalAddressLength == 6)
                .then(|| {
                    let mut m = [0u8; 6];
                    m.copy_from_slice(&a.PhysicalAddress[..6]);
                    m
                })
                .filter(|m| *m != [0; 6]);
            for (k, addr) in addrs.into_iter().enumerate() {
                out.push(Link {
                    // The second and later addresses named apart (as Linux's aliases).
                    name: if k == 0 {
                        name.clone()
                    } else {
                        format!("{name} ({})", k + 1)
                    },
                    addr,
                    up: a.OperStatus == UP,
                    speed_mbps: (bits > 0 && bits != u64::MAX).then_some(bits / 1_000_000),
                    // Each address of a network card is one (as Linux's aliases; bug run
                    // L1-10: the second shown "virtual" in the panel, last).
                    physical,
                    // IEEE 802.11 (71).
                    wireless: a.IfType == 71,
                    mac,
                });
            }
        }
        out
    }
}

/// `aa:bb:cc:dd:ee:ff` as six bytes (`None` for anything else, or all zeros).
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let mut m = [0u8; 6];
    let mut parts = s.split(':');
    for b in &mut m {
        *b = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    (parts.next().is_none() && m != [0; 6]).then_some(m)
}

/// The hardware address of the interface with the IPv4 address `addr`, when known.
pub fn mac_of(addr: Ipv4Addr) -> Option<[u8; 6]> {
    links()
        .into_iter()
        .find(|l| l.addr == addr)
        .and_then(|l| l.mac)
}

/// A port free now for both TCP and UDP, for a program to bind and bind again later (a
/// helper restarted on the same port): picked at random from 20000–31999 outside IDN's
/// own block 29200–29499 (which IDN hands out itself; F1-2), below the
/// range the system hands out by itself (32768–60999 on Linux, from 49152 on Windows), so
/// no socket another program opens meanwhile is given it. A port asked of the system
/// (`bind(…:0)`) and let go could be.
pub fn kept_port() -> u16 {
    use std::net::{TcpListener, UdpSocket};
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    loop {
        let t = crate::epoch::now().as_nanos() as u64;
        // splitmix64 of the time, the process and a count.
        let mut x = t
            ^ (u64::from(std::process::id()) << 32)
            ^ N.fetch_add(1, Ordering::Relaxed)
                .wrapping_mul(0x9E37_79B9_7F4A_7C15);
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^= x >> 31;
        let port = 20_000 + (x % 12_000) as u16;
        if (29_200..=29_499).contains(&port) {
            continue;
        }
        if TcpListener::bind(("0.0.0.0", port)).is_ok()
            && UdpSocket::bind(("0.0.0.0", port)).is_ok()
        {
            return port;
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn this_machine_lists_its_links_without_loopback() {
        let l = super::links();
        assert!(l.iter().all(|x| !x.addr.is_loopback()), "{l:?}");
        assert!(
            l.iter()
                .all(|x| !x.physical || !super::virtual_name(&x.name))
        );
        assert!(super::virtual_name("docker0") && super::virtual_name("br-1a2b"));
        assert!(!super::virtual_name("eno1") && !super::virtual_name("Ethernet 2"));
        assert_eq!(
            super::parse_mac("78:55:36:02:65:3b"),
            Some([0x78, 0x55, 0x36, 0x02, 0x65, 0x3b])
        );
        assert_eq!(super::parse_mac("00:00:00:00:00:00"), None);
        // A machine running the tests has some network (CI containers too: their veth).
        #[cfg(target_os = "linux")]
        assert!(!l.is_empty());
    }

    #[test]
    fn paths_by_the_link_they_take() {
        use super::{Link, Path, path_with};
        use std::net::{IpAddr, Ipv4Addr};
        let link = |a: [u8; 4], physical, wireless| Link {
            name: "x".into(),
            addr: Ipv4Addr::from(a),
            up: true,
            speed_mbps: None,
            physical,
            wireless,
            mac: None,
        };
        let links = [
            link([192, 168, 1, 5], true, false),
            link([10, 0, 0, 7], true, true),
            link([100, 64, 0, 2], false, false),
        ];
        let ip = |a: [u8; 4]| IpAddr::V4(Ipv4Addr::from(a));
        let via = |a: [u8; 4]| move |_| Some(Ipv4Addr::from(a));
        assert_eq!(
            path_with(ip([127, 0, 0, 1]), &links, via([0, 0, 0, 0])),
            Path::Local
        );
        assert_eq!(
            path_with(ip([192, 168, 1, 5]), &links, via([0, 0, 0, 0])),
            Path::Local
        );
        assert_eq!(
            path_with(ip([192, 168, 1, 9]), &links, via([192, 168, 1, 5])),
            Path::Wired
        );
        assert_eq!(
            path_with(ip([10, 0, 0, 9]), &links, via([10, 0, 0, 7])),
            Path::Wireless
        );
        assert_eq!(
            path_with(ip([100, 64, 0, 9]), &links, via([100, 64, 0, 2])),
            Path::Tunnel
        );
        assert_eq!(path_with(ip([8, 8, 8, 8]), &links, |_| None), Path::Unknown);
        assert_eq!(
            (Path::Wired.start_ms(), Path::Wireless.start_ms()),
            (10, 40)
        );
        // This machine's own route to itself.
        assert_eq!(super::path_to(ip([127, 0, 0, 1])), Path::Local);
    }

    #[test]
    fn the_address_towards_loopback_is_loopback() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
        assert_eq!(
            super::local_addr_towards(Ipv4Addr::LOCALHOST.into()),
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST))
        );
        assert_eq!(
            super::local_v4_towards(Ipv4Addr::LOCALHOST),
            Some(Ipv4Addr::LOCALHOST)
        );
        // IPv6 where the machine has it (some containers don't).
        if let Some(a) = super::local_addr_towards(Ipv6Addr::LOCALHOST.into()) {
            assert_eq!(a, IpAddr::V6(Ipv6Addr::LOCALHOST));
        }
        // With a default route, never an unspecified or loopback address.
        if let Some(a) = super::outgoing_ip() {
            assert!(!a.is_unspecified() && !a.is_loopback(), "{a}");
        }
    }
}

//! IDN's own ports (never NDI's or Dante's, so IDN installs beside
//! them on a desktop). One block, 29200–29499, unregistered at IANA (checked 2026-11-10)
//! and clear of NDI (5353, 5959, 5960, 5961+, 6960+ receiving, 7960+ sending), Dante
//! (319/320, 4321, 4440/4444/4455, 8700–8708, 8751, 8800, 9998, 14336–14591), OMT
//! (6400–6600) and every system's ephemeral range (Linux 32768+, Windows and macOS
//! 49152+):
//!
//! - 29200–29249: IDN's services, one fixed port each (below);
//! - 29250–29259: multicast, one fixed port per kind of stream, each stream on a group
//!   address of its own (as Dante's 4321 and AES67's 5004);
//! - 29300–29499: media, one port per connection, the next free one.
//!
//! Kept where they are: NDI's own protocol ports (5353 mDNS, 5959 Discovery Server, 5960
//! listing), taken only when free; AES67's (5004, SAP 9875); PTP's 319/320 (fixed by the
//! standard); MQTT's 1883 for the brokers devices connect to.

/// The IDN Node's panel on this machine (loopback).
pub const NODE_PANEL: u16 = 29200;
/// The IDN Node's panel on the network (HTTPS, with its LAN password).
pub const NODE_LAN_PANEL: u16 = 29201;
/// The IDN Site Manager's panel (HTTPS).
pub const SITE_MANAGER: u16 = 29202;
/// The Site Managers' Voice (HTTPS): Media's panel port plus one; Industrial's default
/// (`IDN_VOICE_HTTPS`). Media's Voice over IDN's link takes the same number on UDP.
pub const SITE_VOICE: u16 = 29203;
/// Linking two computers without a site (TCP).
pub const PAIR_LINK: u16 = 29204;
/// The remote site link (UDP; a gateway's is forwarded on its router).
pub const SITE_LINK: u16 = 29205;
/// The Industrial Site Manager's QUIC link (UDP).
pub const INDUSTRIAL_QUIC: u16 = 29206;
/// Voice's calls (WebRTC, UDP).
pub const VOICE_RTC: u16 = 29207;
/// The Industrial Node's local API and panel (loopback).
pub const INDUSTRIAL_API: u16 = 29208;
/// The switcher's panel and API (loopback).
pub const SWITCHER: u16 = 29210;
/// The switcher's panel on the network (HTTPS).
pub const SWITCHER_LAN: u16 = 29211;
/// IDN Data's panel and API (loopback).
pub const DATA: u16 = 29212;
/// The links that play library items directly (HTTP).
pub const MEDIA_LINK: u16 = 29213;
/// The desk's panel and API (loopback).
pub const DESK: u16 = 29214;
/// The desk's panel on the network (HTTPS).
pub const DESK_LAN: u16 = 29215;
/// The listening page (phones).
pub const LISTEN: u16 = 29216;
/// The OBS plugin's panel.
pub const OBS_PANEL: u16 = 29217;
/// The viewing page (WHEP).
pub const VIEW: u16 = 29218;
/// The desk run on a network-audio device's rings by the Node (R.33): its panel and API
/// (loopback), beside a desk of the session's own (bug run I4-3: both took DESK).
pub const NET_DESK: u16 = 29219;
/// That desk's panel on the network (HTTPS).
pub const NET_DESK_LAN: u16 = 29220;
/// IDN Data's API on the network (HTTPS, site tokens; idn-node.md §N.31.5).
pub const DATA_LAN: u16 = 29221;
/// The display service's API (loopback; idn-display.md §D.8).
pub const DISPLAY: u16 = 29222;
/// The display service's API on the network (HTTPS, site tokens only).
pub const DISPLAY_LAN: u16 = 29223;
/// The desk's IEM flow for phones (UDP; idn-iem.md §I.5): sessions, talkback, unicast
/// audio.
pub const IEM: u16 = 29224;
/// The same for the desk in a network-audio device's rings (R.33).
pub const NET_DESK_IEM: u16 = 29225;

/// Multicast, the proxy video (`vmclq`).
pub const MCAST_PROXY: u16 = 29250;
/// Multicast, the full video (`vmchq`).
pub const MCAST_VIDEO: u16 = 29251;
/// Multicast, the sound (`amc`).
pub const MCAST_AUDIO: u16 = 29252;
/// Multicast, the desk's IEM flow for phones (Opus; idn-iem.md §I.5).
pub const MCAST_IEM: u16 = 29253;

/// Media ports, one per connection: the first and the last.
pub const MEDIA_FIRST: u16 = 29300;
pub const MEDIA_LAST: u16 = 29499;

/// Every service's port (one each).
pub const SERVICES: &[u16] = &[
    NODE_PANEL,
    NODE_LAN_PANEL,
    SITE_MANAGER,
    SITE_VOICE,
    PAIR_LINK,
    SITE_LINK,
    INDUSTRIAL_QUIC,
    VOICE_RTC,
    INDUSTRIAL_API,
    SWITCHER,
    SWITCHER_LAN,
    DATA,
    MEDIA_LINK,
    DESK,
    DESK_LAN,
    LISTEN,
    OBS_PANEL,
    VIEW,
    NET_DESK,
    NET_DESK_LAN,
    DATA_LAN,
    DISPLAY,
    DISPLAY_LAN,
    IEM,
    NET_DESK_IEM,
];

/// The media range this process uses: [`MEDIA_FIRST`]..=[`MEDIA_LAST`] unless an
/// application chose another ([`set_media_range`]) or `IDN_MEDIA_PORTS` says so
/// (`FIRST-LAST`, or `off`: the system picks). `None`: the system picks.
pub fn media_range() -> Option<(u16, u16)> {
    if let Some(r) = *lock(&RANGE) {
        return r;
    }
    match crate::switches::value("IDN_MEDIA_PORTS") {
        Some(v) if crate::switches::says_off(&v) => None,
        Some(v) => parse_range(&v).or_else(|| {
            // (A typo isn't "off": media on ports a firewall blocks, and nothing said;
            // F1-6. Said once.)
            static SAID: std::sync::Once = std::sync::Once::new();
            SAID.call_once(|| {
                crate::log::log_or_stderr(&format!(
                    "IDN_MEDIA_PORTS={v:?}: not FIRST-LAST or off; IDN's own {MEDIA_FIRST}-{MEDIA_LAST} used"
                ))
            });
            Some((MEDIA_FIRST, MEDIA_LAST))
        }),
        None => Some((MEDIA_FIRST, MEDIA_LAST)),
    }
}

/// For an application using IDN as a library: its own media ports (`Some((first, last))`)
/// or the system's choice (`None`), for every connection made after this.
pub fn set_media_range(range: Option<(u16, u16)>) {
    *lock(&RANGE) = Some(range.map(|(a, b)| (a.min(b), a.max(b))));
}

/// The multicast ports, (proxy video, video, sound): [`MCAST_PROXY`], [`MCAST_VIDEO`],
/// [`MCAST_AUDIO`] unless an application chose others ([`set_multicast_ports`]).
pub fn multicast_ports() -> (u16, u16, u16) {
    lock(&MCAST).unwrap_or((MCAST_PROXY, MCAST_VIDEO, MCAST_AUDIO))
}

/// For an application using IDN as a library: its own multicast ports.
pub fn set_multicast_ports(proxy: u16, video: u16, audio: u16) {
    *lock(&MCAST) = Some((proxy, video, audio));
}

static RANGE: std::sync::Mutex<Option<Option<(u16, u16)>>> = std::sync::Mutex::new(None);
static MCAST: std::sync::Mutex<Option<(u16, u16, u16)>> = std::sync::Mutex::new(None);

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    crate::sync::LockOk::lock_ok(m)
}

/// `FIRST-LAST` → the range; anything else `None`.
fn parse_range(v: &str) -> Option<(u16, u16)> {
    let (a, b) = v.trim().split_once('-')?;
    let (a, b): (u16, u16) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
    (a > 0 && b > 0).then_some((a.min(b), a.max(b)))
}

/// A TCP listener on the next free media port on `ip` (the range of [`media_range`]),
/// else (all taken, or no range) one the system picks.
pub fn media_tcp(ip: std::net::IpAddr) -> std::io::Result<std::net::TcpListener> {
    for p in media_ports() {
        if let Ok(l) = std::net::TcpListener::bind((ip, p)) {
            return Ok(l);
        }
    }
    std::net::TcpListener::bind((ip, 0))
}

/// A UDP socket on the next free media port (as [`media_tcp`]).
pub fn media_udp(ip: std::net::IpAddr) -> std::io::Result<std::net::UdpSocket> {
    for p in media_ports() {
        if let Ok(s) = std::net::UdpSocket::bind((ip, p)) {
            return Ok(s);
        }
    }
    std::net::UdpSocket::bind((ip, 0))
}

/// The media ports in the order they are tried: from a point that moves on with each
/// call, starting at a point of the process's own (each program started from 29300 and
/// walked the same ports: a program restarted gave its old ports to other outputs, where
/// receivers reconnecting fast found another source; F1-1, F1-5). Empty with no range.
pub fn media_ports() -> impl Iterator<Item = u16> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: std::sync::OnceLock<AtomicU32> = std::sync::OnceLock::new();
    let next = NEXT.get_or_init(|| {
        let t = crate::epoch::now().subsec_nanos();
        AtomicU32::new(t ^ std::process::id().wrapping_mul(0x9E37_79B9))
    });
    let (first, last) = media_range().unwrap_or((1, 0));
    let n = (last as u32 + 1).saturating_sub(first as u32);
    let start = if n == 0 {
        0
    } else {
        next.fetch_add(1, Ordering::Relaxed) % n
    };
    (0..n).map(move |i| (first as u32 + (start + i) % n) as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every IDN port is in its block and clear of NDI's and Dante's (as published
    /// 2026-11-10), and no two services share one.
    #[test]
    fn idn_ports_stay_clear_of_ndi_and_dante() {
        let mut seen = std::collections::BTreeSet::new();
        for p in SERVICES.iter().copied().chain([
            MCAST_PROXY,
            MCAST_VIDEO,
            MCAST_AUDIO,
            MCAST_IEM,
            MEDIA_FIRST,
            MEDIA_LAST,
        ]) {
            assert!((29200..=29499).contains(&p), "{p}");
            assert!(seen.insert(p), "{p} twice");
        }
        let ndi = |p: u16| p == 5353 || (5959..=8959).contains(&p);
        let dante = |p: u16| {
            matches!(
                p,
                319 | 320 | 4321 | 4440 | 4444 | 4455 | 8751 | 8800 | 9998
            ) || (8700..=8708).contains(&p)
                || (14336..=14600).contains(&p)
        };
        for p in 29200..=29499 {
            assert!(!ndi(p) && !dante(p), "{p}");
        }
        let all: Vec<u16> = media_ports().collect();
        assert_eq!(all.len(), 200);
        assert!(all.iter().all(|p| (MEDIA_FIRST..=MEDIA_LAST).contains(p)));
        assert_eq!(SITE_VOICE, SITE_MANAGER + 1);
        assert_eq!(parse_range("30000-30010"), Some((30000, 30010)));
        assert_eq!(parse_range("30010 - 30000"), Some((30000, 30010)));
        assert_eq!(parse_range("off"), None);
        assert_eq!(parse_range("29300"), None);
    }
}

//! The device's address followed (lab Alpha84 537-B; bug run M2-1): when DHCP gives the
//! card it runs on another address, the program starts again on it.

use std::net::Ipv4Addr;

/// The card an address is on, by its MAC and name (an alias's base name).
pub struct Card {
    mac: Option<[u8; 6]>,
    name: String,
}

impl Card {
    /// The card that has `ip` now.
    pub fn of(ip: Ipv4Addr) -> Option<Card> {
        idn_common::net::links()
            .into_iter()
            .find(|l| l.addr == ip)
            .map(|l| Card {
                mac: l.mac,
                name: l.name.split(':').next().unwrap_or("").to_owned(),
            })
    }

    /// Whether `ip` is gone and this card has another address: a new one from DHCP. Gone
    /// with none (the cable out, the switch restarting): not moved, the old binding works
    /// again when the address comes back (M2-1: it restarted on Wi-Fi or a container
    /// bridge, for good).
    pub fn moved_from(&self, ip: Ipv4Addr) -> bool {
        let links = idn_common::net::links();
        !links.iter().any(|l| l.addr == ip)
            && links.iter().any(|l| {
                l.up && l.addr != ip
                    && (self.mac.is_some() && l.mac == self.mac
                        || l.name.split(':').next() == Some(self.name.as_str()))
            })
    }
}

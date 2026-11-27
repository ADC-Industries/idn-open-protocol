//! Machines' certificates for their HTTPS ports (§E.10.11): asked for over an enrolled
//! device's trust connection, for its own device key, with the names and addresses it
//! gives and the address the authority sees it at.

use super::*;
use crate::trust::pki::dns_name_ok;

/// The most names and addresses a request may carry.
const MAX_NAMES: usize = 8;
const MAX_IPS: usize = 16;

/// The least time between two certificates on one trust connection.
pub(super) const LAN_EVERY: Duration = Duration::from_secs(10);

/// Whether `ip` may be in a certificate: an address a client could reach.
fn ip_ok(ip: &IpAddr) -> bool {
    !ip.is_unspecified() && !ip.is_multicast() && !matches!(ip, IpAddr::V4(v4) if v4.is_broadcast())
}

impl Authority {
    /// The certificate for the HTTPS ports of the enrolled device whose trust connection
    /// presented `cert` (its device certificate, `info` what it says), as `m` asks
    /// (`<idn_lan_cert><name v=".."/><ip v=".."/></idn_lan_cert>`), `peer` the address
    /// the connection comes from (added to the addresses).
    pub(super) fn issue_lan(
        &self,
        cert: &[u8],
        info: &CertInfo,
        m: &Element,
        peer: Option<IpAddr>,
    ) -> io::Result<Vec<u8>> {
        let mut names: Vec<String> = Vec::new();
        for n in m.children_named("name").filter_map(|c| c.attr("v")) {
            let n = n.trim().to_ascii_lowercase();
            if !dns_name_ok(&n) {
                return Err(err(format!("not a host name: {n:?}")));
            }
            if !names.contains(&n) {
                names.push(n);
            }
        }
        let mut ips: Vec<IpAddr> = Vec::new();
        for v in m.children_named("ip").filter_map(|c| c.attr("v")) {
            let ip: IpAddr = v
                .trim()
                .parse()
                .map_err(|_| err(format!("not an address: {v:?}")))?;
            if !ip_ok(&ip) {
                return Err(err(format!("not an address for a certificate: {ip}")));
            }
            if !ips.contains(&ip) {
                ips.push(ip);
            }
        }
        // The address the site sees it at, mapped back from IPv6 where it was IPv4.
        if let Some(p) = peer.map(|p| p.to_canonical())
            && ip_ok(&p)
            && !ips.contains(&p)
        {
            ips.push(p);
        }
        if names.len() > MAX_NAMES || ips.len() > MAX_IPS {
            return Err(err(format!(
                "at most {MAX_NAMES} names and {MAX_IPS} addresses"
            )));
        }
        if names.is_empty() && ips.is_empty() {
            return Err(err("no names or addresses"));
        }
        let (_, c) = x509_parser::parse_x509_certificate(cert).map_err(err)?;
        let spki = c.public_key().raw.to_vec();
        self.0
            .ca
            .issue_lan(&spki, &display_name(&info.name), &names, &ips)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_addresses_checked() {
        assert!(dns_name_ok("booth-pc"));
        assert!(dns_name_ok("booth-pc.local"));
        assert!(dns_name_ok("localhost"));
        assert!(!dns_name_ok("*.local"));
        assert!(!dns_name_ok("-x"));
        assert!(!dns_name_ok("a..b"));
        assert!(!dns_name_ok("a b"));
        assert!(!dns_name_ok(&"a".repeat(64)));
        assert!(ip_ok(&"192.168.1.20".parse().unwrap()));
        assert!(ip_ok(&"127.0.0.1".parse().unwrap()));
        assert!(!ip_ok(&"0.0.0.0".parse().unwrap()));
        assert!(!ip_ok(&"239.1.2.3".parse().unwrap()));
        assert!(!ip_ok(&"255.255.255.255".parse().unwrap()));
    }
}

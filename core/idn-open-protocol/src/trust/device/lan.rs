//! The machine's certificate for its HTTPS ports (§E.10.11): asked of the authority over
//! the trust connection (or given by the IDN Node, which holds the membership), kept
//! beside the device certificate, renewed when half its life is left or when the
//! machine's names or addresses change.

use super::super::pki::{DeviceCreds, LAN_CERT_LIFE, LanCert};
use super::*;
use std::net::IpAddr;

/// How often the certificate is looked at during a trust session.
const CHECK_EVERY: Duration = Duration::from_secs(30);
/// The same request again (an authority that refused part of it, or doesn't know it)
/// after this; one for other names or addresses after [`CHANGED_AGAIN`].
const SAME_AGAIN: Duration = Duration::from_secs(600);
const CHANGED_AGAIN: Duration = Duration::from_secs(15);

/// The certificate, the CA it chains to (its fingerprint), and the asking.
#[derive(Default)]
pub(super) struct Held {
    cert: RwLock<Option<(Arc<LanCert>, String)>>,
    ask: Mutex<Ask>,
}

#[derive(Default)]
struct Ask {
    /// No look before this.
    next_check: Option<Instant>,
    /// The last request: when, and for what.
    last: Option<(Instant, String)>,
}

/// The names and addresses this machine's HTTPS ports are reached by: its host name
/// (and `.local`), `localhost`, its IPv4 addresses and the loopback ones.
pub(crate) fn wanted() -> (Vec<String>, Vec<IpAddr>) {
    let host = crate::discovery::hostname().to_ascii_lowercase();
    let mut names = Vec::new();
    if crate::trust::pki::dns_name_ok(&host) && host != "localhost" {
        names.push(format!("{host}.local"));
        names.insert(0, host);
    }
    names.push("localhost".to_owned());
    let mut ips: Vec<IpAddr> = crate::discovery::ifaces::ipv4_multicast_interfaces()
        .into_iter()
        .filter(|i| !i.loopback)
        .map(|i| IpAddr::V4(i.addr))
        .collect();
    ips.sort();
    ips.dedup();
    ips.truncate(12);
    ips.push(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    ips.push(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST));
    (names, ips)
}

impl Enrollment {
    /// This machine's certificate for its HTTPS ports (§E.10.11), signed by the site CA
    /// for the device key: once in a site and issued, while valid and of the site the
    /// machine is in now. Present it with the CA certificate
    /// ([`DeviceCreds::anchor`](super::super::pki::DeviceCreds)) as the chain
    /// (`web::lan` does).
    pub fn lan_cert(&self) -> Option<Arc<LanCert>> {
        let creds = self.creds()?;
        let held = self.lan.cert.read_ok();
        let (c, ca) = held.as_ref()?;
        (*ca == creds.anchor.ca_fp && c.valid_for(Duration::ZERO)).then(|| c.clone())
    }

    /// The device key (the key of [`lan_cert`](Self::lan_cert)).
    pub(crate) fn device_key(&self) -> &KeyFile {
        &self.key
    }

    fn set_lan(&self, creds: &DeviceCreds, der: &[u8]) -> io::Result<()> {
        let c = creds.anchor.verify_lan(der, &self.key.public)?;
        *self.lan.cert.write_ok() = Some((Arc::new(c), creds.anchor.ca_fp.clone()));
        Ok(())
    }

    /// The certificate kept from the last run, if it still holds.
    pub(super) fn load_lan_cert(&self, creds: &DeviceCreds) {
        if self.node.is_some() {
            return;
        }
        if let Ok(der) = std::fs::read(self.path("lan.crt")) {
            let _ = self.set_lan(creds, &der);
        }
    }

    pub(super) fn forget_lan_cert(&self) {
        *self.lan.cert.write_ok() = None;
        if self.node.is_none() {
            let _ = std::fs::remove_file(self.path("lan.crt"));
        }
    }

    /// The authority's answer to `idn_lan_cert`.
    pub(super) fn take_lan_cert(&self, creds: &DeviceCreds, m: &Element) {
        if let Some(e) = m.attr("error") {
            crate::log::log(&format!("trust: no HTTPS certificate from the site: {e}"));
            return;
        }
        let Some(der) = m.attr("cert").and_then(unb64) else {
            return;
        };
        match self.set_lan(creds, &der) {
            Ok(()) => {
                let _ = super::super::pki::write_private(&self.path("lan.crt"), &der);
            }
            Err(e) => crate::log::log(&format!("trust: the site's HTTPS certificate: {e}")),
        }
    }

    /// The IDN Node's `identity` gave the certificate it holds for the machine (or none).
    pub(super) fn apply_node_lan_cert(&self, id: &Element) {
        let der = id.attr("lan_cert").and_then(unb64);
        let same = {
            let held = self.lan.cert.read_ok();
            match (&*held, &der) {
                (Some((c, _)), Some(d)) => c.der == *d,
                (None, None) => true,
                _ => false,
            }
        };
        if same {
            return;
        }
        match (self.creds(), der) {
            (Some(creds), Some(der)) => {
                if let Err(e) = self.set_lan(&creds, &der) {
                    crate::log::log(&format!("IDN Node: the HTTPS certificate: {e}"));
                }
            }
            _ => *self.lan.cert.write_ok() = None,
        }
    }

    /// A trust session began: looked at at once.
    pub(super) fn lan_session_started(&self) {
        *self.lan.ask.lock_ok() = Ask::default();
        self.lan_tick();
    }

    /// Asks the authority for a certificate when there is none, half its life is gone,
    /// or the machine's names or addresses aren't all in it; at most every
    /// [`CHECK_EVERY`], the same request again only after [`SAME_AGAIN`].
    pub(super) fn lan_tick(&self) {
        if self.node.is_some() || self.creds().is_none() {
            return;
        }
        {
            let mut ask = self.lan.ask.lock_ok();
            let now = Instant::now();
            if ask.next_check.is_some_and(|t| now < t) {
                return;
            }
            ask.next_check = Some(now + CHECK_EVERY);
        }
        let (names, ips) = wanted();
        let fits = self.lan_cert().is_some_and(|c| {
            c.valid_for(LAN_CERT_LIFE / 2)
                && names.iter().all(|n| c.names.contains(n))
                && ips.iter().all(|i| c.ips.contains(i))
        });
        if fits {
            return;
        }
        let what = format!("{names:?} {ips:?}");
        {
            let mut ask = self.lan.ask.lock_ok();
            if let Some((t, w)) = &ask.last {
                let wait = if *w == what {
                    SAME_AGAIN
                } else {
                    CHANGED_AGAIN
                };
                if t.elapsed() < wait {
                    return;
                }
            }
            ask.last = Some((Instant::now(), what));
        }
        let mut e = Element::new("idn_lan_cert");
        for n in &names {
            e.children.push(Element::new("name").with_attr("v", n));
        }
        for ip in &ips {
            e.children
                .push(Element::new("ip").with_attr("v", &ip.to_string()));
        }
        self.trust_send(&e.to_xml());
    }
}

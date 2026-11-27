//! Encryption settings: trusted devices (sticky secure), `SecureConfig`, a session's credentials.

use super::*;

/// Devices this one has encrypted with before ("trusted devices").
///
/// Once two devices have encrypted, each refuses an unencrypted connection to the other
/// from then on. Otherwise an attacker could strip the encryption offer and get the
/// plain stream. When a device is replaced or reset, call
/// [`forget_all`](StickyStore::forget_all) (a "reset trusted devices" button).
///
/// [`in_memory`](StickyStore::in_memory) forgets everything at exit; for real
/// installations use [`with_file`](StickyStore::with_file).
#[derive(Debug, Default)]
pub struct StickyStore {
    seen: Mutex<BTreeSet<(String, String)>>,
    path: Option<PathBuf>,
}

impl StickyStore {
    /// A store that lives as long as the process (the default in [`SecureConfig::new`]).
    pub fn in_memory() -> Arc<Self> {
        Arc::default()
    }

    /// A store kept in a text file (one `keyid name` line per device), loaded now and
    /// rewritten whenever a new device is added. A missing or unreadable file starts
    /// empty.
    pub fn with_file(path: PathBuf) -> Arc<Self> {
        let seen = match std::fs::read_to_string(&path) {
            Ok(t) => t
                .lines()
                .filter_map(|l| l.split_once(' ').map(|(k, n)| (k.to_owned(), n.to_owned())))
                .collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Default::default(),
            // Not silently: downgrade protection (§E.8.5) is off for every peer until
            // they are seen again (R1-113).
            Err(e) => {
                crate::log::log(&format!(
                    "secure: can't read the sticky file {}: {e}; starting with none remembered",
                    path.display()
                ));
                Default::default()
            }
        };
        Arc::new(StickyStore {
            seen: Mutex::new(seen),
            path: Some(path),
        })
    }

    #[doc(hidden)]
    pub fn contains(&self, key_id: &str, peer: &str) -> bool {
        self.seen
            .lock_ok()
            .contains(&(key_id.to_owned(), peer.to_owned()))
    }

    #[doc(hidden)]
    pub fn remember(&self, key_id: &str, peer: &str) {
        let mut s = self.seen.lock_ok();
        if s.insert((key_id.to_owned(), peer.to_owned()))
            && let Some(p) = &self.path
        {
            let text: String = s.iter().map(|(k, n)| format!("{k} {n}\n")).collect();
            // Replaced whole or not at all (a power cut mid-write used to leave it empty).
            if let Err(e) = crate::trust::pki::write_private(p, text.as_bytes()) {
                crate::log::log(&format!(
                    "secure: can't write the sticky file {}: {e}",
                    p.display()
                ));
            }
        }
    }

    /// Forgets every device (and deletes the file). Afterwards, devices that no longer
    /// encrypt are accepted again, and the next encrypted session is remembered anew.
    pub fn forget_all(&self) {
        self.seen.lock_ok().clear();
        if let Some(p) = &self.path {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Encryption settings for a publisher or an input. Clone it to share one
/// configuration (and one trusted-device store) between them.
///
/// Two ways to encrypt, usable together: a site **passphrase** ([`SecureConfig::new`],
/// `secure/1`), and **site trust** with per-device certificates
/// ([`SecureConfig::with_site`], `secure/2`, see [`trust`](crate::trust)). A session uses
/// site trust when both devices are enrolled in the same site, else the passphrase when
/// both share it, else no encryption (unless [`require`](SecureConfig::require) is set).
#[derive(Debug, Clone)]
pub struct SecureConfig {
    /// The key derived from the site passphrase (`secure/1`), if one is set.
    pub key: Option<Arc<SiteKey>>,
    /// This device's site membership (`secure/2`), if any.
    pub site: Option<Arc<crate::trust::Enrollment>>,
    /// "Require encryption": refuse every peer that can't encrypt with this device, NDI
    /// devices included. On a publisher it applies to all its outputs. Off by default
    /// ("encrypt when possible").
    pub require: bool,
    /// Devices encrypted with before; see [`StickyStore`].
    pub sticky: Arc<StickyStore>,
    /// The input whose site options apply (its receiver name), set by
    /// [`Input::open`](crate::input::Input::open); `None`: the device's.
    #[doc(hidden)]
    pub input_scope: Option<String>,
}

impl SecureConfig {
    /// "Encrypt when possible" with this passphrase, remembering trusted devices in
    /// memory only. Takes about 0.1 s on a modern PC (see [`SiteKey::from_passphrase`]).
    pub fn new(passphrase: &str) -> io::Result<Self> {
        Ok(SecureConfig {
            key: Some(SiteKey::from_passphrase(passphrase)?),
            site: None,
            require: false,
            sticky: StickyStore::in_memory(),
            input_scope: None,
        })
    }

    /// "Encrypt when possible" with site trust: device certificates once `enrollment` is
    /// enrolled, and its passphrase (if it has one) with devices outside the site.
    pub fn with_site(enrollment: Arc<crate::trust::Enrollment>) -> Self {
        SecureConfig {
            key: None,
            site: Some(enrollment),
            require: false,
            sticky: StickyStore::in_memory(),
            input_scope: None,
        }
    }

    /// Whether "require encryption" is on now: [`require`](Self::require), the IDN Node's
    /// setting for the machine (live), or the site's option `secure.require` (§E.20),
    /// read at each connection, so that a change on the
    /// authority applies at once, without a restart. The site's `true` wins
    /// over a local `false`: it is the site's security policy.
    pub fn requires(&self) -> bool {
        self.require
            || self.site.as_ref().is_some_and(|e| e.node_requires())
            || self.site.as_ref().is_some_and(|e| {
                let (o, fp) = (e.options(), e.fingerprint());
                let view = match &self.input_scope {
                    Some(name) => o.for_input(&fp, name),
                    None => o.for_device(&fp),
                };
                view.bool("secure.require") == Some(true)
            })
    }

    /// The passphrase key in use: this configuration's own, else the site membership's
    /// (which the IDN Node may change while the program runs).
    pub fn passphrase_key(&self) -> Option<Arc<SiteKey>> {
        self.key
            .clone()
            .or_else(|| self.site.as_ref().and_then(|s| s.site_key()))
    }

    /// The `ndi_version` to send: announcing what this device can do right now. A device
    /// with a site certificate offers only `secure/2`: otherwise a revoked device that knows
    /// the passphrase could keep connecting with `secure/1`, and revocation would mean
    /// nothing (§E.10.7).
    #[doc(hidden)]
    pub fn version(&self, sender: bool) -> String {
        let ca = self.site.as_ref().and_then(|s| s.ca_id());
        let pass = self.passphrase_key();
        let key_id = if ca.is_some() {
            None
        } else {
            pass.as_ref().map(|k| k.key_id.as_str())
        };
        let pair = self.pair_offer();
        crate::ext::ndi_version_offer(
            sender,
            key_id,
            ca.as_deref(),
            pair.as_ref().map(|(f, p)| (f.as_str(), p.as_str())),
        )
    }

    /// This machine's short fingerprint and its linked computers', when it has some and
    /// isn't in a site (§E.28: a machine in a site uses its site only).
    fn pair_offer(&self) -> Option<(String, String)> {
        let site = self.site.as_ref()?;
        if site.creds().is_some() {
            return None;
        }
        let pairs = site.pairs();
        if pairs.is_empty() {
            return None;
        }
        let me = site.open_identity()?;
        let list: Vec<&str> = pairs
            .iter()
            .map(|l| crate::trust::pair::short(&l.fp))
            .collect();
        Some((
            crate::trust::pair::short(&me.fingerprint()).to_owned(),
            list.join(" "),
        ))
    }

    /// The credentials for a session, from what both sides announced (§E.10.7): `secure/2`
    /// if both list it with the same `x_idn_ca`, else `secure/1` with the same key id.
    pub(super) fn choose(
        &self,
        ours: &crate::control::Element,
        peer: &crate::control::Element,
    ) -> Option<Creds> {
        let lists = |e: &crate::control::Element, t: &str| {
            e.attr("x_idn_ext")
                .is_some_and(|l| l.split_whitespace().any(|x| x == t))
        };
        if let Some(site) = &self.site
            && lists(ours, "secure/2")
            && lists(peer, "secure/2")
            && ours.attr("x_idn_ca").is_some()
            && ours.attr("x_idn_ca") == peer.attr("x_idn_ca")
            && let Some(c) = site.creds()
        {
            return Some(Creds::Device(c, site.clone()));
        }
        // Linked computers (§E.28): each lists the other.
        if let Some(site) = &self.site
            && lists(ours, "pair/1")
            && lists(peer, "pair/1")
            && let (Some(ours_fp), Some(theirs_fp), Some(their_pairs)) = (
                ours.attr("x_idn_fp"),
                peer.attr("x_idn_fp"),
                peer.attr("x_idn_pairs"),
            )
            && their_pairs.split_whitespace().any(|p| p == ours_fp)
            && site.creds().is_none()
            && let Some(me) = site.open_identity()
        {
            let allowed: Vec<String> = site
                .pairs()
                .iter()
                .filter(|l| crate::trust::pair::short(&l.fp) == theirs_fp)
                .map(|l| l.fp.clone())
                .collect();
            if let Some(fp) = allowed.first() {
                let lease = site.pair_lease(fp);
                return Some(Creds::Pair(me, Arc::new(allowed), Some(lease)));
            }
        }
        match &self.passphrase_key() {
            Some(k)
                if lists(ours, "secure/1") && crate::ext::Active::secure_with(peer, &k.key_id) =>
            {
                Some(Creds::Passphrase(k.clone()))
            }
            _ => None,
        }
    }

    /// Whether a plain-text session with this peer must be refused because this device
    /// is in a site: an IDN peer that can't authenticate
    /// with the site's certificates is refused; NDI devices (no IDN extensions) are not,
    /// unless "require encryption" is on.
    pub(super) fn site_refuses(&self, peer: Option<&crate::control::Element>) -> bool {
        self.site.as_ref().is_some_and(|s| s.creds().is_some())
            && peer.is_some_and(|p| p.attr("x_idn_ext").is_some_and(|l| !l.trim().is_empty()))
    }

    /// Sticky records this device keeps (passphrase key id, site CA).
    fn sticky_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .passphrase_key()
            .iter()
            .map(|k| k.key_id.clone())
            .collect();
        if let Some(ca) = self.site.as_ref().and_then(|s| s.ca_id()) {
            v.push(format!("ca:{ca}"));
        }
        if self.pair_offer().is_some() {
            v.push("pair".into());
        }
        v
    }

    /// Whether a plain-text session with `peer` must be refused because it encrypted
    /// before (§E.8.5).
    #[doc(hidden)]
    pub fn sticky_refuses(&self, peer: &str) -> bool {
        self.sticky_ids()
            .iter()
            .any(|id| self.sticky.contains(id, peer))
    }
}

/// The credentials one encrypted session uses.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub enum Creds {
    /// The passphrase-derived site certificate (`secure/1`).
    Passphrase(Arc<SiteKey>),
    /// This device's certificate from the site authority (`secure/2`).
    Device(
        Arc<crate::trust::pki::DeviceCreds>,
        Arc<crate::trust::Enrollment>,
    ),
    /// This machine's device key, with a linked computer's fingerprint (`pair/1`, §E.28),
    /// and the session's hold on the link (a link for this time only ends with its last).
    Pair(
        Arc<crate::link::Identity>,
        Arc<Vec<String>>,
        Option<Arc<crate::trust::PairLease>>,
    ),
}

impl Creds {
    /// TLS 1.3 client configuration with this ALPN.
    pub fn client_config(&self, alpn: &[u8]) -> io::Result<rustls::ClientConfig> {
        match self {
            Creds::Passphrase(k) => {
                let mut c = k.client_config()?;
                c.alpn_protocols = vec![alpn.to_vec()];
                Ok(c)
            }
            Creds::Device(d, _) => d.client_config(alpn),
            Creds::Pair(me, allowed, _) => crate::trust::pair::client_config(me, allowed, alpn),
        }
    }

    /// TLS 1.3 server configuration with this ALPN.
    pub fn server_config(&self, alpn: &[u8]) -> io::Result<rustls::ServerConfig> {
        match self {
            Creds::Passphrase(k) => {
                let mut c = k.server_config()?;
                c.alpn_protocols = vec![alpn.to_vec()];
                Ok(c)
            }
            Creds::Device(d, _) => d.server_config(alpn),
            Creds::Pair(me, allowed, _) => crate::trust::pair::server_config(me, allowed, alpn),
        }
    }

    /// `secure/1`, `secure/2` or `pair/1`.
    pub fn mode(&self) -> &'static str {
        match self {
            Creds::Passphrase(_) => "secure/1",
            Creds::Device(..) => "secure/2",
            Creds::Pair(..) => "pair/1",
        }
    }

    pub(super) fn sticky_id(&self) -> String {
        match self {
            Creds::Passphrase(k) => k.key_id.clone(),
            Creds::Device(d, _) => format!("ca:{}", d.anchor.ca_id()),
            Creds::Pair(..) => "pair".into(),
        }
    }

    /// TLS on `net` as client or server; `secure/2` sessions are registered so that a
    /// revocation closes them (§E.10.6).
    /// The TLS session, and the peer's key fingerprint (SHA-256 of its SPKI).
    pub(super) fn tls(
        &self,
        net: TcpStream,
        client: bool,
    ) -> io::Result<(TcpStream, Option<String>)> {
        let raw = net.try_clone()?;
        let conn = if client {
            let name = ServerName::try_from("idn-site").map_err(io::Error::other)?;
            rustls::Connection::Client(
                rustls::ClientConnection::new(Arc::new(self.client_config(CONTROL_ALPN)?), name)
                    .map_err(io::Error::other)?,
            )
        } else {
            rustls::Connection::Server(
                rustls::ServerConnection::new(Arc::new(self.server_config(CONTROL_ALPN)?))
                    .map_err(io::Error::other)?,
            )
        };
        let (s, peer) = wrap_with_peer(net, conn)?;
        let fp = peer
            .as_deref()
            .and_then(ed25519_public_key)
            .map(|k| crate::trust::pki::fingerprint(&crate::trust::pki::spki(&k)));
        if let (Creds::Device(_, e), Some(cert)) = (self, peer) {
            e.track_session(&cert, &raw, false)?; // under TLS: nothing plain may be written
        }
        // A linked computer's session ends when the link is removed (E3-2).
        if let Creds::Pair(_, _, Some(lease)) = self {
            lease.track(&raw);
        }
        Ok((s, fp))
    }
}

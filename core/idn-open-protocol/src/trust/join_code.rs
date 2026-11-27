//! Joining a site with a code typed on the device (§E.10.10): the site's join mode, the
//! authority's codes (issued, single use, 10 minutes, 5 tries, kept across restarts),
//! and the password-authenticated exchange (SPAKE2) that checks a typed code without
//! letting anyone who sees, or plays, one side learn the code offline.

use super::proto;
use idn_common::hex::{hex, unhex};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use std::collections::BTreeMap;
use std::io;
use std::net::IpAddr;
use std::path::Path;

/// How machines join the site (§E.10.10), the site option `join.mode` (§E.20.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JoinMode {
    /// A machine asks, both screens show the comparison code, an operator approves
    /// (§E.10.4). A site without the option is in this mode.
    #[default]
    Approval,
    /// A machine that types a code the Site Manager issued joins at once; one without a
    /// code waits for approval as in [`JoinMode::Approval`].
    CodeOrApproval,
    /// A machine must type a code, then still waits for approval.
    CodeAndApproval,
}

impl JoinMode {
    /// The option's value: `approval`, `code_or_approval` or `code_and_approval`.
    pub fn as_str(self) -> &'static str {
        match self {
            JoinMode::Approval => "approval",
            JoinMode::CodeOrApproval => "code_or_approval",
            JoinMode::CodeAndApproval => "code_and_approval",
        }
    }

    /// From the option's value.
    pub fn parse(v: &str) -> Option<Self> {
        match v.trim() {
            "approval" => Some(JoinMode::Approval),
            "code_or_approval" => Some(JoinMode::CodeOrApproval),
            "code_and_approval" => Some(JoinMode::CodeAndApproval),
            _ => None,
        }
    }

    /// Whether a typed code is taken.
    pub fn takes_codes(self) -> bool {
        self != JoinMode::Approval
    }
}

/// The site option that holds the join mode (§E.20.3).
pub const JOIN_MODE_KEY: &str = "join.mode";
/// How long a code is good for, seconds.
pub const CODE_LIFE_S: u64 = 600;
/// Wrong tries a code takes before it is burned.
pub const CODE_TRIES: u32 = 5;
/// Unused codes a site holds at once; issuing another burns the oldest. Each is one run
/// of the exchange, and so one guess a rogue authority gets per typed code (§E.10.10).
pub const MAX_CODES: usize = 4;
/// Failed code tries from one address within [`ADDRESS_WINDOW_S`] before that address is
/// refused code joins for the rest of the window.
pub const ADDRESS_FAILS: usize = 10;
/// See [`ADDRESS_FAILS`].
pub const ADDRESS_WINDOW_S: u64 = 600;

/// The file the authority keeps its codes in, beside its keys (mode 0600).
pub(crate) const CODES_FILE: &str = "join_codes.tsv";

/// A code just issued (`POST /api/v1/join_code`): shown to the operator once, then only
/// its expiry and tries are listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedCode {
    /// Its id, as [`JoinCodeInfo::id`].
    pub id: u64,
    /// Six digits, e.g. `482913`; shown `482 913` ([`show_code`](super::show_code)).
    pub code: String,
    /// When it expires, Unix seconds.
    pub expires: u64,
}

/// A code waiting to be typed, as listed: not the code itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinCodeInfo {
    /// Its id (counts up; for an operator's "cancel").
    pub id: u64,
    /// When it expires, Unix seconds.
    pub expires: u64,
    /// Wrong tries it takes before it is burned.
    pub tries_left: u32,
    /// Who issued it (a Site Manager user, or empty).
    pub issuer: String,
}

/// What happened to a code, for the authority's events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Burned {
    /// Its tries ran out.
    Tries(u64),
    /// A newer code took its place ([`MAX_CODES`]).
    Replaced(u64),
}

#[derive(Debug, Clone)]
struct Code {
    id: u64,
    code: String,
    expires: u64,
    /// Tries left, a try taken as each exchange starts (given back if another code
    /// matched).
    tries: u32,
    issuer: String,
}

/// The authority's codes and the failed tries by address.
#[derive(Debug, Default)]
pub(crate) struct Codes {
    list: Vec<Code>,
    next_id: u64,
    /// Failed tries by address: their times (Unix seconds), the last window's.
    fails: BTreeMap<IpAddr, Vec<u64>>,
}

impl Codes {
    /// The codes kept in `dir` (none if the file is missing or unreadable lines).
    pub(crate) fn load(dir: &Path) -> Self {
        let text = std::fs::read_to_string(dir.join(CODES_FILE)).unwrap_or_default();
        let list: Vec<Code> = text
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.splitn(5, '\t').collect();
                let [id, code, expires, tries, issuer] = f.as_slice() else {
                    return None;
                };
                let code = String::from_utf8(unhex(code)?).ok()?;
                Some(Code {
                    id: id.parse().ok()?,
                    code: normalize(&code)?,
                    expires: expires.parse().ok()?,
                    tries: tries.parse().ok()?,
                    issuer: (*issuer).to_owned(),
                })
            })
            .collect();
        let next_id = list.iter().map(|c| c.id).max().unwrap_or(0) + 1;
        Codes {
            list,
            next_id,
            fails: BTreeMap::new(),
        }
    }

    /// Keeps the codes in `dir` (readable by this account only).
    pub(crate) fn save(&self, dir: &Path) -> io::Result<()> {
        let t: String = self
            .list
            .iter()
            .map(|c| {
                format!(
                    "{}\t{}\t{}\t{}\t{}\n",
                    c.id,
                    // Hex: the file never shows the digits at a glance.
                    hex(c.code.as_bytes()),
                    c.expires,
                    c.tries,
                    c.issuer.replace(['\t', '\n', '\r'], " ")
                )
            })
            .collect();
        super::pki::write_private(&dir.join(CODES_FILE), t.as_bytes())
    }

    /// Drops the codes expired at `now`; whether any was.
    pub(crate) fn prune(&mut self, now: u64) -> bool {
        let n = self.list.len();
        self.list.retain(|c| c.expires > now && c.tries > 0);
        n != self.list.len()
    }

    /// A new code from `issuer`, good until `now` + 10 minutes. Past [`MAX_CODES`] the
    /// oldest is burned.
    pub(crate) fn issue(&mut self, issuer: &str, now: u64) -> (IssuedCode, Option<Burned>) {
        self.prune(now);
        let replaced = (self.list.len() >= MAX_CODES).then(|| {
            let old = self.list.remove(0);
            Burned::Replaced(old.id)
        });
        let code = loop {
            let c = crate::random::code_digits(6);
            if !self.list.iter().any(|x| x.code == c) {
                break c;
            }
        };
        let id = self.next_id;
        self.next_id += 1;
        let expires = now + CODE_LIFE_S;
        self.list.push(Code {
            id,
            code: code.clone(),
            expires,
            tries: CODE_TRIES,
            issuer: issuer.chars().take(64).collect(),
        });
        (IssuedCode { id, code, expires }, replaced)
    }

    /// Removes the code `id`; whether it was there.
    pub(crate) fn cancel(&mut self, id: u64) -> bool {
        let n = self.list.len();
        self.list.retain(|c| c.id != id);
        n != self.list.len()
    }

    /// The codes waiting, oldest first.
    pub(crate) fn info(&self, now: u64) -> Vec<JoinCodeInfo> {
        self.list
            .iter()
            .filter(|c| c.expires > now && c.tries > 0)
            .map(|c| JoinCodeInfo {
                id: c.id,
                expires: c.expires,
                tries_left: c.tries,
                issuer: c.issuer.clone(),
            })
            .collect()
    }

    /// The codes an exchange starting now runs against, each with one try taken: (id,
    /// code). Empty: no code waits.
    pub(crate) fn take_tries(&mut self, now: u64) -> Vec<(u64, String)> {
        self.list
            .iter_mut()
            .filter(|c| c.expires > now && c.tries > 0)
            .map(|c| {
                c.tries -= 1;
                (c.id, c.code.clone())
            })
            .collect()
    }

    /// The exchange matched `used`: it is gone (single use), and the other codes it ran
    /// against get their try back.
    pub(crate) fn matched(&mut self, used: u64, others: &[u64]) {
        self.list.retain(|c| c.id != used);
        for c in self.list.iter_mut().filter(|c| others.contains(&c.id)) {
            c.tries = (c.tries + 1).min(CODE_TRIES);
        }
    }

    /// The exchange over `ids` failed: their tries stay taken. Returns the most tries any
    /// of them has left, and those burned (none left).
    pub(crate) fn failed(&mut self, ids: &[u64]) -> (u32, Vec<Burned>) {
        let left = self
            .list
            .iter()
            .filter(|c| ids.contains(&c.id))
            .map(|c| c.tries)
            .max()
            .unwrap_or(0);
        let burned: Vec<Burned> = self
            .list
            .iter()
            .filter(|c| ids.contains(&c.id) && c.tries == 0)
            .map(|c| Burned::Tries(c.id))
            .collect();
        self.list.retain(|c| c.tries > 0);
        (left, burned)
    }

    /// Seconds until `addr` may try a code again, if it is refused now.
    pub(crate) fn limited(&mut self, addr: Option<IpAddr>, now: u64) -> Option<u64> {
        let addr = addr?;
        let f = self.fails.get_mut(&addr)?;
        f.retain(|t| now < t + ADDRESS_WINDOW_S);
        if f.is_empty() {
            self.fails.remove(&addr);
            return None;
        }
        (f.len() >= ADDRESS_FAILS).then(|| f[0] + ADDRESS_WINDOW_S - now)
    }

    /// A failed try from `addr`.
    pub(crate) fn note_fail(&mut self, addr: Option<IpAddr>, now: u64) {
        let Some(addr) = addr else { return };
        // Bounded: the addresses of the last window, at most 4096 of them.
        if self.fails.len() >= 4096 && !self.fails.contains_key(&addr) {
            self.fails
                .retain(|_, f| f.last().is_some_and(|t| now < t + ADDRESS_WINDOW_S));
            if self.fails.len() >= 4096 {
                return;
            }
        }
        let f = self.fails.entry(addr).or_default();
        f.push(now);
        if f.len() > ADDRESS_FAILS {
            f.remove(0);
        }
    }
}

/// A typed code as kept: six digits, spaces dropped (`"482 913"` → `"482913"`); `None` if
/// it isn't six digits.
pub fn normalize(typed: &str) -> Option<String> {
    let bare: String = typed.chars().filter(|c| !c.is_whitespace()).collect();
    (bare.len() == 6 && bare.bytes().all(|b| b.is_ascii_digit())).then_some(bare)
}

// ---- the exchange (§E.10.10)

/// A random stream from a 32-byte seed (SHA-512 in counter mode): the device starts
/// SPAKE2 again from the same seed for each run it is offered, with the same secret.
struct Seeded {
    seed: [u8; 32],
    counter: u64,
    buf: [u8; 64],
    pos: usize,
}

impl Seeded {
    fn new(seed: [u8; 32]) -> Self {
        Seeded {
            seed,
            counter: 0,
            buf: [0; 64],
            pos: 64,
        }
    }
}

impl rand_08::RngCore for Seeded {
    fn next_u32(&mut self) -> u32 {
        let mut b = [0; 4];
        self.fill_bytes(&mut b);
        u32::from_le_bytes(b)
    }
    fn next_u64(&mut self) -> u64 {
        let mut b = [0; 8];
        self.fill_bytes(&mut b);
        u64::from_le_bytes(b)
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        for d in dest {
            if self.pos == 64 {
                let h = sha2::Sha512::new()
                    .chain_update(b"IDN join code rng")
                    .chain_update(self.seed)
                    .chain_update(self.counter.to_be_bytes())
                    .finalize();
                self.buf.copy_from_slice(&h);
                self.counter += 1;
                self.pos = 0;
            }
            *d = self.buf[self.pos];
            self.pos += 1;
        }
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_08::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

impl rand_08::CryptoRng for Seeded {}

/// SPAKE2's side A (the device) or B (the authority), with the code as the password and
/// the two keys as the identities.
fn start(
    side_a: bool,
    code: &str,
    device_spki: &[u8],
    ca_spki: &[u8],
    seed: [u8; 32],
) -> (Spake2<Ed25519Group>, Vec<u8>) {
    let (pw, a, b) = (
        Password::new(code.as_bytes()),
        Identity::new(device_spki),
        Identity::new(ca_spki),
    );
    if side_a {
        Spake2::<Ed25519Group>::start_a_with_rng(&pw, &a, &b, Seeded::new(seed))
    } else {
        Spake2::<Ed25519Group>::start_b_with_rng(&pw, &a, &b, Seeded::new(seed))
    }
}

/// What one exchange binds (§E.10.10): `SHA-256("IDN join code" || each field as a 4-byte
/// big-endian length and its bytes)`, over the device's and the CA's SPKI, the join's
/// `name` and `kind` as sent, both nonces and both SPAKE2 messages.
#[allow(clippy::too_many_arguments)]
pub(crate) fn transcript(
    device_spki: &[u8],
    ca_spki: &[u8],
    name: &str,
    kind: &str,
    nd: &[u8],
    na: &[u8],
    x: &[u8],
    y: &[u8],
) -> [u8; 32] {
    let mut h = Sha256::new().chain_update(b"IDN join code");
    for f in [
        device_spki,
        ca_spki,
        name.as_bytes(),
        kind.as_bytes(),
        nd,
        na,
        x,
        y,
    ] {
        h.update((f.len() as u32).to_be_bytes());
        h.update(f);
    }
    h.finalize().into()
}

/// The two confirmations from SPAKE2's key `k` and the transcript `t`:
/// `HMAC-SHA256(k, "authority" || t)` and `HMAC-SHA256(k, "device" || t)`.
pub(crate) fn confirmations(k: &[u8], t: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, k);
    let mac = |who: &[u8]| -> [u8; 32] {
        let mut c = ring::hmac::Context::with_key(&key);
        c.update(who);
        c.update(t);
        c.sign()
            .as_ref()
            .try_into()
            .expect("HMAC-SHA256 is 32 bytes")
    };
    (mac(b"authority"), mac(b"device"))
}

/// The device's side: its SPAKE2 message `X` and nonce `Nd`, and what it needs to check
/// the authority's runs.
pub(crate) struct DeviceSide {
    seed: [u8; 32],
    code: String,
    pub(crate) x: Vec<u8>,
    pub(crate) nd: [u8; 32],
}

impl DeviceSide {
    pub(crate) fn new(code: &str, device_spki: &[u8], ca_spki: &[u8]) -> Self {
        let seed = crate::random::secure_random::<32>();
        let (_, x) = start(true, code, device_spki, ca_spki, seed);
        DeviceSide {
            seed,
            code: code.to_owned(),
            x,
            nd: crate::random::secure_random::<32>(),
        }
    }

    /// The authority's runs `(Y, its confirmation)` with its nonce `na`: the device's
    /// confirmation for the run that proves the code (and so the CA), if one does. Every
    /// run is checked, so the time doesn't tell which.
    pub(crate) fn check(
        &self,
        device_spki: &[u8],
        ca_spki: &[u8],
        name: &str,
        kind: &str,
        na: &[u8],
        runs: &[(Vec<u8>, Vec<u8>)],
    ) -> Option<[u8; 32]> {
        let mut found = None;
        for (y, ca_conf) in runs {
            let (st, _) = start(true, &self.code, device_spki, ca_spki, self.seed);
            let Ok(k) = st.finish(y) else { continue };
            let t = transcript(device_spki, ca_spki, name, kind, &self.nd, na, &self.x, y);
            let (a, d) = confirmations(&k, &t);
            if proto::same(&a, ca_conf) && found.is_none() {
                found = Some(d);
            }
        }
        found
    }
}

/// One run of the authority's side for one code: its `Y`, its confirmation, and the
/// device's confirmation it expects. `None`: the device's `X` is malformed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn authority_run(
    code: &str,
    device_spki: &[u8],
    ca_spki: &[u8],
    name: &str,
    kind: &str,
    nd: &[u8],
    na: &[u8],
    x: &[u8],
) -> Option<(Vec<u8>, [u8; 32], [u8; 32])> {
    let (st, y) = start(
        false,
        code,
        device_spki,
        ca_spki,
        crate::random::secure_random::<32>(),
    );
    let k = st.finish(x).ok()?;
    let t = transcript(device_spki, ca_spki, name, kind, nd, na, x, &y);
    let (a, d) = confirmations(&k, &t);
    Some((y, a, d))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEV: &[u8] = b"device spki";
    const CA: &[u8] = b"ca spki";

    fn exchange(dev_code: &str, site_codes: &[&str], ca_dev: &[u8], ca_site: &[u8]) -> bool {
        let d = DeviceSide::new(dev_code, DEV, ca_dev);
        let na = [7u8; 32];
        let runs: Vec<_> = site_codes
            .iter()
            .map(|c| authority_run(c, DEV, ca_site, "Cam", "app", &d.nd, &na, &d.x).unwrap())
            .collect();
        let offered: Vec<_> = runs
            .iter()
            .map(|(y, a, _)| (y.clone(), a.to_vec()))
            .collect();
        match d.check(DEV, ca_dev, "Cam", "app", &na, &offered) {
            Some(conf) => runs.iter().any(|(_, _, want)| proto::same(want, &conf)),
            None => false,
        }
    }

    #[test]
    fn the_right_code_matches_and_a_wrong_one_doesnt() {
        assert!(exchange("482913", &["482913"], CA, CA));
        assert!(!exchange("482914", &["482913"], CA, CA));
        // One of several codes waiting.
        assert!(exchange("111111", &["222222", "111111", "333333"], CA, CA));
        assert!(!exchange("444444", &["222222", "111111", "333333"], CA, CA));
    }

    #[test]
    fn another_ca_never_matches_even_with_the_code() {
        // A rogue authority relaying the device's message to the real site: the device
        // binds the CA it was shown, the site its own.
        assert!(!exchange("482913", &["482913"], b"rogue ca", CA));
        assert!(!exchange("482913", &["482913"], CA, b"rogue ca"));
    }

    #[test]
    fn the_transcript_binds_every_field() {
        let base = transcript(DEV, CA, "n", "k", b"nd", b"na", b"x", b"y");
        for other in [
            transcript(b"d2", CA, "n", "k", b"nd", b"na", b"x", b"y"),
            transcript(DEV, b"c2", "n", "k", b"nd", b"na", b"x", b"y"),
            transcript(DEV, CA, "n2", "k", b"nd", b"na", b"x", b"y"),
            transcript(DEV, CA, "n", "k2", b"nd", b"na", b"x", b"y"),
            transcript(DEV, CA, "n", "k", b"n2", b"na", b"x", b"y"),
            transcript(DEV, CA, "n", "k", b"nd", b"n2", b"x", b"y"),
            transcript(DEV, CA, "n", "k", b"nd", b"na", b"x2", b"y"),
            transcript(DEV, CA, "n", "k", b"nd", b"na", b"x", b"y2"),
            // Lengths are part of it: moving a byte between fields changes it.
            transcript(DEV, CA, "nk", "", b"nd", b"na", b"x", b"y"),
        ] {
            assert_ne!(other, base);
        }
    }

    #[test]
    fn codes_are_single_use_expire_and_burn() {
        let mut c = Codes::default();
        let (a, _) = c.issue("op", 1000);
        assert_eq!(a.code.len(), 6);
        assert_eq!(a.expires, 1000 + CODE_LIFE_S);
        // Five failed exchanges burn it.
        for i in 0..CODE_TRIES {
            let ids: Vec<u64> = c.take_tries(1001).iter().map(|x| x.0).collect();
            assert_eq!(ids, vec![a.id]);
            let (left, burned) = c.failed(&ids);
            assert_eq!(left, CODE_TRIES - i - 1);
            assert_eq!(burned.is_empty(), i + 1 < CODE_TRIES);
        }
        assert!(c.take_tries(1002).is_empty());
        // Expired.
        let (b, _) = c.issue("op", 2000);
        assert!(c.take_tries(2000 + CODE_LIFE_S).is_empty());
        assert!(c.info(2000 + CODE_LIFE_S).is_empty());
        let _ = b;
        // Used once; the other code's try given back.
        let (x, _) = c.issue("op", 3000);
        let (y, _) = c.issue("op", 3000);
        let taken = c.take_tries(3001);
        assert_eq!(taken.len(), 2);
        c.matched(x.id, &[y.id]);
        let left = c.info(3001);
        assert_eq!(left.len(), 1);
        assert_eq!((left[0].id, left[0].tries_left), (y.id, CODE_TRIES));
        // Past MAX_CODES, the oldest goes.
        for _ in 0..MAX_CODES {
            c.issue("op", 3002);
        }
        assert!(!c.info(3002).iter().any(|i| i.id == y.id));
        assert_eq!(c.info(3002).len(), MAX_CODES);
    }

    #[test]
    fn addresses_with_many_failures_wait() {
        let mut c = Codes::default();
        let a: Option<IpAddr> = Some("10.0.0.9".parse().unwrap());
        for t in 0..ADDRESS_FAILS as u64 {
            assert_eq!(c.limited(a, 100 + t), None);
            c.note_fail(a, 100 + t);
        }
        assert_eq!(c.limited(a, 110), Some(ADDRESS_WINDOW_S - 10));
        assert_eq!(c.limited(Some("10.0.0.8".parse().unwrap()), 110), None);
        assert_eq!(c.limited(a, 100 + ADDRESS_WINDOW_S), None);
    }

    #[test]
    fn typed_codes_with_or_without_the_space() {
        assert_eq!(normalize("482 913").as_deref(), Some("482913"));
        assert_eq!(normalize(" 482913\n").as_deref(), Some("482913"));
        assert_eq!(normalize("48291"), None);
        assert_eq!(normalize("48291x"), None);
        assert_eq!(
            JoinMode::parse("code_or_approval"),
            Some(JoinMode::CodeOrApproval)
        );
        assert_eq!(JoinMode::default().as_str(), "approval");
    }
}

//! Linked computers (`pair/1`, spec/extensions/idn-extensions.md §E.28): two machines of
//! IDN Media linked without a site. One (B) asks; both show the same six-digit code, made
//! from both keys and a nonce each side commits to before seeing the other's, so someone
//! in the middle can't choose keys that give matching codes. The person at the asked one
//! (A) compares the codes and accepts (remembered, or for this time only) or rejects. Each
//! then keeps the other's fingerprint, and sessions between them use TLS with their device
//! keys, each checking the other's fingerprint is a linked one. A machine in a site neither
//! links nor uses its links.

use super::pki;
use super::proto::{b64, recv_by, same, send, unb64};
use crate::xml::Element;
use idn_common::err::invalid_data as bad;
use rustls::DistinguishedName;
use rustls::client::danger::{ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use sha2::{Digest, Sha256};
use std::io;
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The service a machine that takes links announces on mDNS (instance: its name).
pub const SERVICE: &str = "_idn-pair._tcp.local";
/// How long the asked machine's person has to answer.
pub const ANSWER_WITHIN: Duration = Duration::from_secs(120);
/// How long each message of the exchange may take (but the answer).
const STEP: Duration = Duration::from_secs(10);
/// The largest message of the exchange.
const MAX: usize = 4096;

/// A linked computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Linked {
    /// Its device key's fingerprint (§E.10.1), 64 hex digits.
    pub fp: String,
    /// Its name, as it gave it when linked.
    pub name: String,
    /// When it was linked (Unix time, seconds).
    pub since: u64,
    /// For this time only (a visiting preacher's laptop): forgotten once the last session
    /// between the two ends, never saved.
    pub once: bool,
}

impl Linked {
    /// As the IDN Node gives it to programs and keeps it: `<pair fp name since once/>`.
    pub fn to_element(&self) -> Element {
        let e = Element::new("pair")
            .with_attr("fp", &self.fp)
            .with_attr("name", &self.name)
            .with_attr("since", &self.since.to_string());
        if self.once {
            e.with_attr("once", "true")
        } else {
            e
        }
    }

    /// Inverse of [`to_element`](Self::to_element).
    pub fn from_element(e: &Element) -> Option<Linked> {
        let fp = e.attr("fp")?;
        if fp.len() != 64 || !fp.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        Some(Linked {
            fp: fp.to_ascii_lowercase(),
            name: e.attr("name").unwrap_or("").to_owned(),
            since: e.attr("since").and_then(|s| s.parse().ok()).unwrap_or(0),
            once: e.attr("once") == Some("true"),
        })
    }
}

/// The first 16 hex digits of a fingerprint: how offers name machines (`x_idn_fp`).
pub fn short(fp: &str) -> &str {
    &fp[..fp.len().min(16)]
}

/// The code both machines show: six digits from `SHA-256("IDN pair code" || A's key ||
/// B's key || Na || Nb)`, its first 8 bytes modulo 1 000 000, as the site join code. Kept
/// as `"482913"`; shown as `"482 913"` ([`show_code`](super::show_code)).
pub fn code(a_spki: &[u8], b_spki: &[u8], na: &[u8], nb: &[u8]) -> String {
    super::proto::six_digits(
        &Sha256::new()
            .chain_update(b"IDN pair code")
            .chain_update(a_spki)
            .chain_update(b_spki)
            .chain_update(na)
            .chain_update(nb)
            .finalize(),
    )
}

/// The other machine: its name and key.
#[derive(Debug, Clone)]
pub struct Peer {
    /// Its name, as it gave it.
    pub name: String,
    /// Its device key (DER SubjectPublicKeyInfo).
    pub spki: Vec<u8>,
}

impl Peer {
    /// Its fingerprint.
    pub fn fingerprint(&self) -> String {
        pki::fingerprint(&self.spki)
    }
}

fn next_by(s: &mut TcpStream, name: &str, within: Duration) -> io::Result<Element> {
    let e = recv_by(s, MAX, Instant::now() + within)?
        .ok_or_else(|| bad("the other computer closed the connection"))?;
    if e.name == "idn_pair_no" {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            e.attr("reason").unwrap_or("refused").to_owned(),
        ));
    }
    if e.name != name {
        return Err(bad(format!("expected {name}, got {}", e.name)));
    }
    Ok(e)
}

fn next(s: &mut TcpStream, name: &str) -> io::Result<Element> {
    next_by(s, name, STEP)
}

fn key_of(e: &Element) -> io::Result<Vec<u8>> {
    e.attr("key")
        .and_then(unb64)
        .filter(|k| pki::public_from_spki(k).is_some())
        .ok_or_else(|| bad("no usable key"))
}

fn name_of(e: &Element) -> String {
    e.attr("name")
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(100)
        .collect()
}

fn nonce32(e: &Element, k: &str) -> io::Result<Vec<u8>> {
    e.attr(k)
        .and_then(unb64)
        .filter(|n| n.len() == 32)
        .ok_or_else(|| bad("no usable nonce"))
}

/// Says no (busy, requests off, in a site, rejected).
pub fn refuse(s: &mut TcpStream, reason: &str) {
    let _ = send(
        s,
        &Element::new("idn_pair_no")
            .with_attr("reason", reason)
            .to_xml(),
    );
}

/// The first message of a connection to a machine that takes links.
pub fn first(s: &mut TcpStream) -> io::Result<Element> {
    recv_by(s, MAX, Instant::now() + STEP)?
        .ok_or_else(|| bad("the other computer closed the connection"))
}

/// A request heard, on the asked side (A): who asks and the code to compare. Answer it
/// with [`accept`] or [`refuse`] on the same connection.
#[derive(Debug)]
pub struct Heard {
    /// Who asks.
    pub peer: Peer,
    /// The code to compare with the one the asking machine shows. After the accept, the
    /// asking machine says whether its own person confirmed it too ([`confirmed`]).
    pub code: String,
}

/// The asked side (A): the exchange after the request `hello` (`idn_pair`, version 3):
/// A commits to its nonce, B sends its own, A reveals; both compute the code. After A's
/// accept comes B's last word, `idn_pair_done ok="true|false"` ([`confirmed`]): A keeps
/// the link only once B's person confirmed the code too (lab 484-A and 499-A).
pub fn hear(
    s: &mut TcpStream,
    hello: &Element,
    our_name: &str,
    our_spki: &[u8],
) -> io::Result<Heard> {
    if hello.name != "idn_pair" || hello.attr("version") != Some(VERSION) {
        refuse(
            s,
            "this computer links with version 3 only (update the other one)",
        );
        return Err(bad("another version"));
    }
    let peer = Peer {
        name: name_of(hello),
        spki: key_of(hello)?,
    };
    if peer.spki == our_spki {
        refuse(s, "that is this computer");
        return Err(bad("this computer"));
    }
    let na = crate::random::secure_random::<32>();
    send(
        s,
        &Element::new("idn_pair_commit")
            .with_attr("name", our_name)
            .with_attr("key", &b64(our_spki))
            .with_attr("c", &b64(&Sha256::digest(na)))
            .to_xml(),
    )?;
    let nb = nonce32(&next(s, "idn_pair_nonce")?, "n")?;
    send(
        s,
        &Element::new("idn_pair_reveal")
            .with_attr("n", &b64(&na))
            .to_xml(),
    )?;
    let code = code(our_spki, &peer.spki, &na, &nb);
    Ok(Heard { peer, code })
}

/// The asked side (A), after its [`accept`]: the asking machine's
/// last word, within the time its person has. `Ok(true)`: confirmed there; `Ok(false)`:
/// cancelled there; an error: it went away or said nothing in time (not confirmed).
pub fn confirmed(s: &mut TcpStream) -> io::Result<bool> {
    let e = next_by(s, "idn_pair_done", ANSWER_WITHIN + STEP)?;
    Ok(e.attr("ok") == Some("true"))
}

/// The asked side's yes: remembered, or for this time only (`once`).
pub fn accept(s: &mut TcpStream, once: bool) -> io::Result<()> {
    send(
        s,
        &Element::new("idn_pair_accept")
            .with_attr("once", if once { "true" } else { "false" })
            .to_xml(),
    )
}

/// The asking side (B), the codes computed: the other machine and the code to show while
/// its person answers ([`Asking::wait`]).
#[derive(Debug)]
pub struct Asking {
    s: TcpStream,
    /// The machine asked.
    pub peer: Peer,
    /// The code to show (the asked machine shows the same one).
    pub code: String,
}

/// The linking exchange's version (§E.28: version 3, with the asker's last word); a
/// request of any other is refused.
const VERSION: &str = "3";

/// The asking side (B): asks the machine at `addr` to link.
pub fn ask(addr: SocketAddr, our_name: &str, our_spki: &[u8]) -> io::Result<Asking> {
    let mut s = TcpStream::connect_timeout(&addr, STEP)?;
    s.set_write_timeout(Some(STEP))?;
    send(
        &mut s,
        &Element::new("idn_pair")
            .with_attr("version", VERSION)
            .with_attr("name", our_name)
            .with_attr("key", &b64(our_spki))
            .to_xml(),
    )?;
    let commit = next(&mut s, "idn_pair_commit")?;
    let peer = Peer {
        name: name_of(&commit),
        spki: key_of(&commit)?,
    };
    let c = commit
        .attr("c")
        .and_then(unb64)
        .ok_or_else(|| bad("no commitment"))?;
    let nb = crate::random::secure_random::<32>();
    send(
        &mut s,
        &Element::new("idn_pair_nonce")
            .with_attr("n", &b64(&nb))
            .to_xml(),
    )?;
    let na = nonce32(&next(&mut s, "idn_pair_reveal")?, "n")?;
    if !same(&Sha256::digest(&na), &c) {
        return Err(bad(
            "the other computer's reveal doesn't match its commitment: possible attack",
        ));
    }
    let code = code(&peer.spki, our_spki, &na, &nb);
    Ok(Asking { s, peer, code })
}

impl Asking {
    /// Waits for the other machine's person: `Ok(once)` when accepted (for this time only,
    /// or remembered), an error of kind `PermissionDenied` when rejected.
    pub fn wait(&mut self) -> io::Result<bool> {
        let e = next_by(&mut self.s, "idn_pair_accept", ANSWER_WITHIN + STEP)?;
        Ok(e.attr("once") == Some("true"))
    }

    /// After an accept: this machine's person's word, told to the other machine (which
    /// keeps the link only on `true`). An error:
    /// the word didn't go, so the other machine won't keep the link (bug run I5-3: this
    /// one kept it anyway).
    pub fn done(mut self, ok: bool) -> io::Result<()> {
        send(
            &mut self.s,
            &Element::new("idn_pair_done")
                .with_attr("ok", if ok { "true" } else { "false" })
                .to_xml(),
        )
        .map(|_| ())
    }
}

/// Accepts a peer whose device key is one of the linked ones, in both directions.
#[derive(Debug)]
pub(crate) struct PairVerifier {
    allowed: Arc<Vec<String>>,
    algs: WebPkiSupportedAlgorithms,
}

impl PairVerifier {
    pub(crate) fn new(allowed: Arc<Vec<String>>, p: &CryptoProvider) -> Self {
        PairVerifier {
            allowed,
            algs: p.signature_verification_algorithms,
        }
    }

    fn check(&self, cert: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        let fp = crate::secure::ed25519_public_key(cert).map(|k| pki::fingerprint(&pki::spki(&k)));
        match fp {
            Some(fp) if self.allowed.contains(&fp) => Ok(()),
            _ => Err(rustls::Error::General("not a linked computer".into())),
        }
    }
}

impl ServerCertVerifier for PairVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|_| ServerCertVerified::assertion())
    }
    crate::secure::ed25519_tls13_signatures!(algs);
}

impl ClientCertVerifier for PairVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|_| ClientCertVerified::assertion())
    }
    crate::secure::ed25519_tls13_signatures!(algs);
}

/// TLS 1.3 client configuration: this machine's device certificate, linked peers only.
pub(crate) fn client_config(
    me: &crate::link::Identity,
    allowed: &Arc<Vec<String>>,
    alpn: &[u8],
) -> io::Result<rustls::ClientConfig> {
    let provider = crate::secure::crypto_provider();
    let mut c = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(io::Error::other)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PairVerifier::new(allowed.clone(), &provider)))
        .with_client_cert_resolver(me.certified(&provider)?);
    c.alpn_protocols = vec![alpn.to_vec()];
    Ok(c)
}

/// TLS 1.3 server configuration: this machine's device certificate, a linked peer's
/// required.
pub(crate) fn server_config(
    me: &crate::link::Identity,
    allowed: &Arc<Vec<String>>,
    alpn: &[u8],
) -> io::Result<rustls::ServerConfig> {
    let provider = crate::secure::crypto_provider();
    let mut c = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(io::Error::other)?
        .with_client_cert_verifier(Arc::new(PairVerifier::new(allowed.clone(), &provider)))
        .with_cert_resolver(me.certified(&provider)?);
    c.alpn_protocols = vec![alpn.to_vec()];
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spki() -> Vec<u8> {
        pki::KeyFile::generate().unwrap().spki()
    }

    /// Both sides on loopback; `answer` decides on A: Some(once) accepts, None rejects.
    fn exchange(answer: Option<bool>) -> (io::Result<Heard>, String, io::Result<bool>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let (a_key, b_key) = (spki(), spki());
        let a = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let hello = first(&mut s).unwrap();
            let h = hear(&mut s, &hello, "Alpha", &a_key);
            if h.is_ok() {
                match answer {
                    Some(once) => accept(&mut s, once).unwrap(),
                    None => refuse(&mut s, "rejected"),
                }
            }
            h
        });
        let mut asking = ask(addr, "Bravo", &b_key).unwrap();
        assert_eq!(asking.peer.name, "Alpha");
        let code = asking.code.clone();
        let r = asking.wait();
        (a.join().unwrap(), code, r)
    }

    /// Version 3: after A's accept, B's last word reaches A; `word`: what B's person
    /// says (`None`: B goes away without a word).
    fn last_word(word: Option<bool>) -> io::Result<bool> {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let (a_key, b_key) = (spki(), spki());
        let a = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let hello = first(&mut s).unwrap();
            hear(&mut s, &hello, "Alpha", &a_key).unwrap();
            accept(&mut s, false).unwrap();
            confirmed(&mut s)
        });
        let mut asking = ask(addr, "Bravo", &b_key).unwrap();
        asking.wait().unwrap();
        match word {
            Some(ok) => asking.done(ok).unwrap(),
            None => drop(asking),
        }
        a.join().unwrap()
    }

    #[test]
    fn the_asked_side_hears_the_askers_last_word() {
        assert!(last_word(Some(true)).unwrap());
        assert!(!last_word(Some(false)).unwrap());
        // Gone without a word: not confirmed.
        assert!(last_word(None).is_err());
    }

    #[test]
    fn both_sides_show_the_same_code_and_the_answer_decides() {
        let (heard, code, r) = exchange(Some(false));
        let heard = heard.unwrap();
        assert_eq!(heard.code, code);
        assert!(code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()));
        assert_eq!(heard.peer.name, "Bravo");
        assert!(!r.unwrap(), "remembered");
        let (_, _, r) = exchange(Some(true));
        assert!(r.unwrap(), "once");
        let (_, _, r) = exchange(None);
        assert_eq!(r.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn codes_depend_on_every_part_and_links_round_trip() {
        let c = code(b"a", b"b", &[1; 32], &[2; 32]);
        assert_ne!(c, code(b"a", b"c", &[1; 32], &[2; 32]));
        assert_ne!(c, code(b"a", b"b", &[3; 32], &[2; 32]));
        for once in [false, true] {
            let l = Linked {
                fp: "ab".repeat(32),
                name: "Desk".into(),
                since: 7,
                once,
            };
            assert_eq!(Linked::from_element(&l.to_element()), Some(l));
        }
    }
}

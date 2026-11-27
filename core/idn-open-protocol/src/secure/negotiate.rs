//! The plain-text start of a control connection (§E.8.3): offers, refusals, the TLS upgrade.

use super::*;

/// The result of the plain-text start of a control connection (§E.8.3).
#[doc(hidden)]
pub struct Negotiated {
    /// The stream to continue on: the TLS plaintext pipe if `secure`, else the socket.
    pub stream: TcpStream,
    pub secure: bool,
    /// The credentials of an encrypted session (for secure QUIC, §E.8.4).
    pub creds: Option<Creds>,
    /// The peer's `ndi_version`, if it sent one in time.
    pub peer_version: Option<crate::control::Element>,
    /// Frames already read from the peer (its `ndi_version` at least), to be processed
    /// as if they had just arrived.
    pub pre: Vec<crate::frame::Frame>,
    /// An encrypted session's peer: its key's fingerprint.
    pub peer_fp: Option<String>,
}

pub(super) fn write_text(s: &mut TcpStream, xml: &str) -> io::Result<()> {
    s.write_all(&crate::frame::Frame::text(crate::frame::kind::CONTROL, 0, xml).encode())
}

/// Reads frames (unbuffered, so nothing beyond them is consumed) until one whose root
/// element is `name`, or until `timeout`. Returns all frames read and the match. The
/// socket's read timeout is cleared on every exit: on the plain path the socket goes on
/// to be the session's control connection.
pub(super) fn read_until(
    s: &mut TcpStream,
    name: &str,
    timeout: std::time::Duration,
) -> io::Result<(Vec<crate::frame::Frame>, Option<crate::control::Element>)> {
    let r = read_until_deadline(s, name, timeout);
    let reset = s.set_read_timeout(None);
    let r = r?;
    reset?;
    Ok(r)
}

/// How long a frame, once its first byte has arrived, may take to arrive whole.
const FRAME_COMPLETION: std::time::Duration = std::time::Duration::from_secs(5);
/// Before negotiation the peer is unknown and its frames are all kept: each at most this
/// long (control text, not pictures), and all together at most `PRE_TOTAL` (the default
/// 64 MiB limit let a LAN host make a sender keep hundreds of MB).
const PRE_FRAME: u32 = 1 << 20;
const PRE_TOTAL: usize = 4 << 20;

fn read_until_deadline(
    s: &mut TcpStream,
    name: &str,
    timeout: std::time::Duration,
) -> io::Result<(Vec<crate::frame::Frame>, Option<crate::control::Element>)> {
    let end = std::time::Instant::now() + timeout;
    let mut got = Vec::new();
    let mut kept = 0usize;
    loop {
        let left = end.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Ok((got, None));
        }
        // The deadline applies to the start of a frame only: wait for its first byte
        // without consuming anything, then read it whole. A deadline hit mid-frame would
        // lose the bytes already read and leave the connection out of step.
        s.set_read_timeout(Some(left))?;
        match s.peek(&mut [0u8; 1]) {
            // Closed: with the reason, if the peer gave one (§E.10.7).
            Ok(0) => {
                let why = got.iter().find_map(|f| {
                    let e = f
                        .text_payload()
                        .ok()
                        .and_then(crate::control::parse_element)?;
                    (e.name == "x_idn_refused").then(|| e.attr("reason").unwrap_or("").to_owned())
                });
                return Err(match why {
                    Some(r) => {
                        io::Error::new(io::ErrorKind::PermissionDenied, format!("refused: {r}"))
                    }
                    None => io::ErrorKind::UnexpectedEof.into(),
                });
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if crate::transport::is_poll_timeout(&e) => return Ok((got, None)),
            Err(e) => return Err(e),
        }
        s.set_read_timeout(Some(FRAME_COMPLETION))?;
        let limits = crate::frame::Limits {
            max_data_len: PRE_FRAME,
            ..Default::default()
        };
        let f = crate::frame::read_frame(s, &limits)?.ok_or(io::ErrorKind::UnexpectedEof)?;
        kept += f.ext.len() + f.payload.len();
        if kept > PRE_TOTAL {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "too much before negotiation",
            ));
        }
        let hit = f
            .text_payload()
            .ok()
            .and_then(crate::control::parse_element)
            .filter(|e| e.name == name);
        got.push(f);
        if hit.is_some() {
            return Ok((got, hit));
        }
    }
}

/// How long a sender keeps a refused connection open before closing it.
#[doc(hidden)]
pub const REFUSAL_HOLD: std::time::Duration = std::time::Duration::from_secs(2);

/// Why a device in a site refuses this peer (§E.10.7), from what it announced: enrolled
/// with another site, or not a member of this one.
fn site_reason(peer: Option<&crate::control::Element>, who: &str) -> String {
    let lists_s2 = peer
        .and_then(|p| p.attr("x_idn_ext"))
        .is_some_and(|l| l.split_whitespace().any(|x| x == "secure/2"));
    if lists_s2 && peer.and_then(|p| p.attr("x_idn_ca")).is_some() {
        format!("{who} is enrolled with another site authority")
    } else {
        format!("{who} is not a member of this site (not approved yet, revoked, or not enrolled)")
    }
}

/// Tells the peer why it is refused, in a control message it prints (`x_idn_refused`),
/// before the connection closes. Only to IDN peers.
pub(crate) fn tell_refused(mut s: &TcpStream, reason: &str) {
    let e = crate::xml::Element::new("x_idn_refused").with_attr("reason", reason);
    let _ = s.write_all(
        &crate::frame::Frame::text(crate::frame::kind::CONTROL, 0, &e.to_xml()).encode(),
    );
    let _ = s.flush();
}

fn refuse(m: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, m.to_owned())
}

/// Why no credentials matched the peer's `ndi_version`: it offers no encryption at all,
/// or it offers `secure/1` under another key id (a different passphrase, not to be
/// reported as "does not offer encryption").
fn no_match(peer: Option<&crate::control::Element>, who: &str) -> String {
    let lists = |t: &str| {
        peer.and_then(|p| p.attr("x_idn_ext"))
            .is_some_and(|l| l.split_whitespace().any(|x| x == t))
    };
    if lists("secure/1") {
        format!("{who}'s passphrase is not ours (its key id differs)")
    } else if lists("secure/2") {
        format!("{who} is enrolled with another site authority")
    } else {
        format!("{who} does not offer encryption")
    }
}

/// Exchanges and checks the transcript inside TLS (§E.8.3, with `x_idn_ca` §E.10.7).
fn transcript(
    tls: &mut TcpStream,
    ours: &crate::control::Element,
    peer: &crate::control::Element,
) -> io::Result<()> {
    let f = |e: &crate::control::Element| {
        (
            e.attr("x_idn_ext").unwrap_or("").to_owned(),
            e.attr("x_idn_keyid").unwrap_or("").to_owned(),
            e.attr("x_idn_ca").unwrap_or("").to_owned(),
        )
    };
    // Linked computers' offer too (§E.28): rewritten in plain
    // text, it could make two linked machines that share a passphrase use the passphrase.
    let pair = |e: &crate::control::Element| {
        format!(
            "{}|{}",
            e.attr("x_idn_fp").unwrap_or(""),
            e.attr("x_idn_pairs").unwrap_or("")
        )
    };
    let (se, sk, sc) = f(ours);
    let (ge, gk, gc) = f(peer);
    let (sp, gp) = (pair(ours), pair(peer));
    write_text(
        tls,
        &crate::ext::secure_hello_pair([&se, &sk, &sc, &sp], [&ge, &gk, &gc, &gp]),
    )?;
    let (_, hello) = read_until(tls, "x_idn_secure_hello", std::time::Duration::from_secs(3))?;
    match hello {
        Some(h)
            if crate::ext::check_secure_hello_ca(&h, (&se, &sk, &sc), (&ge, &gk, &gc))
                && h.attr("pair").unwrap_or("") == gp
                && h.attr("seen_pair").unwrap_or("") == sp =>
        {
            Ok(())
        }
        _ => Err(refuse(
            "transcript check failed: the plain-text offer was altered",
        )),
    }
}

/// Receiver side (TLS client) of §E.8.3. `source` is the name the receiver connects to,
/// used for sticky secure.
#[doc(hidden)]
pub fn negotiate_receiver(
    mut s: TcpStream,
    cfg: &SecureConfig,
    our_version: &str,
    source: &str,
) -> io::Result<Negotiated> {
    let ours = crate::control::parse_element(our_version)
        .ok_or_else(|| io::Error::other("bad version"))?;
    write_text(&mut s, our_version)?;
    let (pre, peer) = read_until(&mut s, "ndi_version", std::time::Duration::from_secs(1))?;
    let Some(creds) = peer.as_ref().and_then(|p| cfg.choose(&ours, p)) else {
        if cfg.requires() {
            return Err(refuse(&format!(
                "{}, and \"require encryption\" is on",
                no_match(peer.as_ref(), "the source")
            )));
        }
        if cfg.sticky_refuses(source) {
            return Err(refuse(
                "this source used encryption before; refusing an unencrypted connection (reset in settings if it changed)",
            ));
        }
        if cfg.site_refuses(peer.as_ref()) {
            tell_refused(
                &s,
                "this receiver is in a site and takes IDN connections only from its members",
            );
            return Err(refuse(&format!(
                "{}; this device is in a site and takes IDN connections only with the site's certificates",
                site_reason(peer.as_ref(), "the source")
            )));
        }
        return Ok(Negotiated {
            stream: s,
            secure: false,
            creds: None,
            peer_version: peer,
            pre,
            peer_fp: None,
        });
    };
    write_text(&mut s, "<x_idn_starttls/>")?;
    let (more, ok) = read_until(
        &mut s,
        "x_idn_starttls_ok",
        std::time::Duration::from_secs(3),
    )?;
    if ok.is_none() {
        return Err(refuse("the source did not answer x_idn_starttls"));
    }
    // Frames that arrived in plain text are not part of the encrypted session: anyone on
    // the path could have written them (an ndi_routing redirect, tally, metadata). Only
    // the peer's ndi_version is kept, and its offer is checked by the transcript.
    drop(more);
    let pre = only_version(pre);
    let (mut tls, peer_fp) = creds.tls(s, true)?;
    transcript(
        &mut tls,
        &ours,
        peer.as_ref().expect("a chosen peer announced itself"),
    )?;
    cfg.sticky.remember(&creds.sticky_id(), source);
    Ok(Negotiated {
        stream: tls,
        secure: true,
        creds: Some(creds),
        peer_version: peer,
        pre,
        peer_fp,
    })
}

/// Sender side (TLS server) of §E.8.3. Sticky secure needs the receiver's name, which
/// only arrives later (`ndi_identify`); the caller checks it then.
#[doc(hidden)]
pub fn negotiate_sender(
    mut s: TcpStream,
    cfg: &SecureConfig,
    our_version: &str,
) -> io::Result<Negotiated> {
    let ours = crate::control::parse_element(our_version)
        .ok_or_else(|| io::Error::other("bad version"))?;
    write_text(&mut s, our_version)?;
    let (pre, peer) = read_until(&mut s, "ndi_version", std::time::Duration::from_secs(1))?;
    let Some(creds) = peer.as_ref().and_then(|p| cfg.choose(&ours, p)) else {
        if cfg.requires() {
            // Hold the refused connection a while before closing (§E.8.5): NDI receivers
            // retry at once, and this turns a storm of attempts into one every ~2 s.
            std::thread::sleep(REFUSAL_HOLD);
            return Err(refuse(&format!(
                "{}; refused (\"require encryption\" is on)",
                no_match(peer.as_ref(), "the receiver")
            )));
        }
        if cfg.site_refuses(peer.as_ref()) {
            tell_refused(
                &s,
                "this source is in a site and takes IDN connections only from its members",
            );
            std::thread::sleep(REFUSAL_HOLD);
            return Err(refuse(&format!(
                "{}; refused: this device is in a site and takes IDN connections only with the site's certificates",
                site_reason(peer.as_ref(), "the receiver")
            )));
        }
        return Ok(Negotiated {
            stream: s,
            secure: false,
            creds: None,
            peer_version: peer,
            pre,
            peer_fp: None,
        });
    };
    let (_, start) = read_until(&mut s, "x_idn_starttls", std::time::Duration::from_secs(3))?;
    if start.is_none() {
        return Err(refuse(
            "the receiver offered encryption but did not start TLS",
        ));
    }
    write_text(&mut s, "<x_idn_starttls_ok/>")?;
    // As on the receiver side: of the plain-text frames, only the peer's ndi_version
    // (transcript-checked) belongs to the encrypted session.
    let pre = only_version(pre);
    let (mut tls, peer_fp) = creds.tls(s, false)?;
    transcript(
        &mut tls,
        &ours,
        peer.as_ref().expect("a chosen peer announced itself"),
    )?;
    Ok(Negotiated {
        stream: tls,
        secure: true,
        creds: Some(creds),
        peer_version: peer,
        pre,
        peer_fp,
    })
}

/// Of the frames read in plain text, the peer's `ndi_version` only (the last one).
fn only_version(pre: Vec<crate::frame::Frame>) -> Vec<crate::frame::Frame> {
    pre.into_iter()
        .rfind(|f| {
            f.text_payload()
                .ok()
                .and_then(crate::control::parse_element)
                .is_some_and(|e| e.name == "ndi_version")
        })
        .into_iter()
        .collect()
}

/// Remembers that `peer` encrypted with these credentials (sticky secure, §E.8.5).
#[doc(hidden)]
pub fn remember_peer(cfg: &SecureConfig, creds: &Creds, peer: &str) {
    cfg.sticky.remember(&creds.sticky_id(), peer);
}

//! Trust-connection messages and the join cryptography (§E.10.2, §E.10.4).

use sha2::{Digest, Sha256};

/// ALPN of the TLS trust connection.
pub const TRUST_ALPN: &[u8] = b"idn-trust/1";

/// Base64 (RFC 4648 §4, with padding).
pub fn b64(d: &[u8]) -> String {
    idn_common::text::base64(d)
}

/// Inverse of [`b64`].
pub fn unb64(s: &str) -> Option<Vec<u8>> {
    idn_common::text::unbase64(s.trim().as_bytes())
}

/// `SHA-256(Na)`: the authority's commitment.
pub fn commit(na: &[u8; 32]) -> [u8; 32] {
    Sha256::digest(na).into()
}

/// The confirmation code: six decimal digits, the first 8 bytes of
/// `SHA-256("IDN trust code" || device SPKI || CA SPKI || Na || Nd)` as a big-endian
/// integer modulo 1 000 000 (§E.10.4). Kept as `"482913"`; shown as `"482 913"`
/// ([`show_code`]).
pub fn code(device_spki: &[u8], ca_spki: &[u8], na: &[u8], nd: &[u8]) -> String {
    six_digits(
        &Sha256::new()
            .chain_update(b"IDN trust code")
            .chain_update(device_spki)
            .chain_update(ca_spki)
            .chain_update(na)
            .chain_update(nd)
            .finalize(),
    )
}

/// Six decimal digits from a hash: its first 8 bytes (big-endian) modulo 1 000 000. The
/// bias of the modulo is below 10^-13: none worth caring about.
pub(crate) fn six_digits(h: &[u8]) -> String {
    let n = u64::from_be_bytes(h[..8].try_into().expect("a hash has 8 bytes"));
    format!("{:06}", n % 1_000_000)
}

/// A comparison code as people see it: six digits in two groups, `"482 913"` (the join
/// code and the linked computers' code alike). Anything else is shown as it is.
pub fn show_code(code: &str) -> String {
    let c = code.trim();
    if c.len() == 6 && c.bytes().all(|b| b.is_ascii_digit()) {
        format!("{} {}", &c[..3], &c[3..])
    } else {
        c.to_owned()
    }
}

/// Whether a code someone typed or sent (`"482 913"`, `"482913"`) is `code`: spaces
/// don't count.
pub fn same_code(typed: &str, code: &str) -> bool {
    let bare = |c: &str| c.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    !code.is_empty() && bare(typed) == bare(code)
}

/// Writes one NUL-terminated XML message (§8.1 framing).
pub fn send(w: &mut impl std::io::Write, xml: &str) -> std::io::Result<()> {
    w.write_all(xml.as_bytes())?;
    w.write_all(&[0])
}

/// Reads one NUL-terminated message without reading past it, so that a TLS handshake
/// right after it is not consumed. `Ok(None)` at end of stream. Messages are capped at
/// 1 MiB (CRLs, inside TLS).
pub fn recv(r: &mut impl std::io::Read) -> std::io::Result<Option<crate::xml::Element>> {
    recv_into(r, &mut Vec::new(), MAX_MESSAGE)
}

/// The largest trust message (a CRL, inside TLS).
pub const MAX_MESSAGE: usize = 1 << 20;
/// The largest plain-text message (before TLS and authentication): hellos and joins are a
/// few hundred bytes, and a megabyte read byte by byte was a cheap way to burn CPU
/// (R1-117).
pub const MAX_PLAIN: usize = 64 << 10;

/// As [`recv`], at most `max` bytes, keeping what was read in `partial` if the read times
/// out mid-message (call again with the same buffer to go on).
/// [`recv_into`] a fresh message from `s`, whole by `deadline`: a peer sending one byte at
/// a time under the read timeout doesn't hold the thread for as long as it likes (64 KiB
/// at 30 s a byte).
pub fn recv_by(
    s: &mut std::net::TcpStream,
    max: usize,
    deadline: std::time::Instant,
) -> std::io::Result<Option<crate::xml::Element>> {
    struct By<'a>(&'a mut std::net::TcpStream, std::time::Instant);
    impl std::io::Read for By<'_> {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            let left = self
                .1
                .checked_duration_since(std::time::Instant::now())
                .filter(|d| !d.is_zero())
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::TimedOut))?;
            self.0.set_read_timeout(Some(left))?;
            self.0.read(b)
        }
    }
    recv_into(&mut By(s, deadline), &mut Vec::new(), max)
}

pub fn recv_into(
    r: &mut impl std::io::Read,
    partial: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<Option<crate::xml::Element>> {
    let mut b = [0u8; 1];
    loop {
        // EINTR (a signal, a debugger's stop): read again, the message so far kept.
        let n = match r.read(&mut b) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            n => n?,
        };
        if n == 0 {
            return Ok(None);
        }
        if b[0] == 0 {
            break;
        }
        partial.push(b[0]);
        if partial.len() > max {
            return Err(std::io::Error::other("trust message too long"));
        }
    }
    let text = String::from_utf8(std::mem::take(partial))
        .map_err(|_| std::io::Error::other("trust message is not UTF-8"))?;
    crate::xml::parse(&text).map(Some).ok_or_else(|| {
        // The peer's text, cut short and on one line: it goes to the log.
        let shown: String = text
            .chars()
            .take(200)
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        std::io::Error::other(format!("unparsable trust message: {shown}"))
    })
}

/// Constant-time equality.
pub fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    /// EINTR in the middle of a message (a debugger's stop, a signal): read on, nothing
    /// lost; and a poll timeout to every caller (sessions dropped before).
    #[test]
    fn an_interrupted_read_goes_on() {
        struct Flaky(std::io::Cursor<Vec<u8>>, u32);
        impl std::io::Read for Flaky {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                self.1 += 1;
                if self.1.is_multiple_of(3) {
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                self.0.read(b)
            }
        }
        let mut r = Flaky(
            std::io::Cursor::new(b"<idn_trust_ok ca=\"x\"/>\0".to_vec()),
            0,
        );
        let m = super::recv_into(&mut r, &mut Vec::new(), 1024)
            .unwrap()
            .unwrap();
        assert_eq!(m.name, "idn_trust_ok");
        assert!(crate::transport::is_poll_timeout(
            &std::io::ErrorKind::Interrupted.into()
        ));
    }

    use super::*;

    #[test]
    fn code_is_six_digits_and_depends_on_everything() {
        let c = code(b"dev", b"ca", &[1; 32], &[2; 32]);
        assert_eq!(c.len(), 6);
        assert!(c.bytes().all(|b| b.is_ascii_digit()));
        assert_eq!(c, code(b"dev", b"ca", &[1; 32], &[2; 32]));
        for other in [
            code(b"dex", b"ca", &[1; 32], &[2; 32]),
            code(b"dev", b"cb", &[1; 32], &[2; 32]),
            code(b"dev", b"ca", &[3; 32], &[2; 32]),
            code(b"dev", b"ca", &[1; 32], &[4; 32]),
        ] {
            assert_ne!(other, c);
        }
        // The first 8 bytes of the hash modulo 10^6: against a direct computation.
        let h = Sha256::new()
            .chain_update(b"IDN trust code")
            .chain_update(b"dev")
            .chain_update(b"ca")
            .chain_update([1u8; 32])
            .chain_update([2u8; 32])
            .finalize();
        let n = u64::from_be_bytes(h[..8].try_into().unwrap()) % 1_000_000;
        assert_eq!(c.parse::<u64>().unwrap(), n);
        // Leading zeros kept.
        assert_eq!(six_digits(&[0; 8]), "000000");
        assert_eq!(six_digits(&[0, 0, 0, 0, 0, 0, 0, 42]), "000042");
        assert_eq!(unb64(&b64(&[0, 255, 7])), Some(vec![0, 255, 7]));
        assert!(same(b"abc", b"abc") && !same(b"abc", b"abd") && !same(b"ab", b"abc"));
    }

    #[test]
    fn codes_show_in_two_groups_and_compare_with_or_without_the_space() {
        assert_eq!(show_code("482913"), "482 913");
        assert_eq!(show_code("004213"), "004 213");
        assert_eq!(show_code(" 482913\n"), "482 913");
        // Not six digits: as it is (an older peer's code, an empty one).
        assert_eq!(show_code("K7Q2"), "K7Q2");
        assert_eq!(show_code(""), "");
        assert!(same_code("482913", "482913"));
        assert!(same_code("482 913", "482913"));
        assert!(same_code(" 482 913 ", "482 913"));
        assert!(!same_code("482914", "482913"));
        assert!(!same_code("48291", "482913"));
        assert!(!same_code("", ""));
    }
}

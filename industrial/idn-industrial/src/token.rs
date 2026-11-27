//! Enrolment tokens (spec/extensions/idn-industrial.md §I.5): single use, carrying the
//! Site Manager's addresses, the site CA's fingerprint and a secret, written as
//! `IDNI1-<base32>` so an installer can take it from a person or a command line.

use idn_open_protocol::random::secure_random;
use std::io;

/// The prefix of a token's text form; `1` is the format's version.
const PREFIX: &str = "IDNI1-";
const VERSION: u8 = 1;

/// A token as the Site Manager mints it and a Node redeems it.
#[derive(Clone, PartialEq, Eq)]
pub struct Token {
    /// Names the token at the Site Manager (not secret).
    pub id: [u8; 8],
    /// Proves the holder may join; never sent, only used in MACs.
    pub secret: [u8; 32],
    /// The site CA's fingerprint (SHA-256 of its SPKI): the Node trusts that CA and no
    /// other for this site.
    pub ca_fingerprint: [u8; 32],
    /// Where the Site Manager is: `idn://host:port` (QUIC) and/or `wss://host/idn/link`
    /// (`ws://` for a local test).
    pub urls: Vec<String>,
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the secret.
        write!(f, "Token({}, {:?})", self.id_hex(), self.urls)
    }
}

fn bad(m: &str) -> io::Error {
    idn_common::err::invalid_data(format_args!("enrolment token: {m}"))
}

impl Token {
    /// A new token for the site with this CA fingerprint (64 hex digits).
    pub fn new(ca_fingerprint_hex: &str, urls: Vec<String>) -> io::Result<Token> {
        let fp = idn_common::hex::unhex(ca_fingerprint_hex)
            .and_then(|v| <[u8; 32]>::try_from(v).ok())
            .ok_or_else(|| bad("not a CA fingerprint"))?;
        Ok(Token {
            id: secure_random(),
            secret: secure_random(),
            ca_fingerprint: fp,
            urls,
        })
    }

    /// The id as 16 hex digits, as the Site Manager lists it.
    pub fn id_hex(&self) -> String {
        idn_common::hex::hex(&self.id)
    }

    /// The CA fingerprint as 64 hex digits.
    pub fn ca_hex(&self) -> String {
        idn_common::hex::hex(&self.ca_fingerprint)
    }

    /// The text form, `IDNI1-…`.
    pub fn encode(&self) -> String {
        let mut b = vec![VERSION];
        b.extend_from_slice(&self.id);
        b.extend_from_slice(&self.secret);
        b.extend_from_slice(&self.ca_fingerprint);
        b.push(self.urls.len().min(8) as u8);
        for u in self.urls.iter().take(8) {
            let u = &u.as_bytes()[..u.len().min(1024)];
            b.extend_from_slice(&(u.len() as u16).to_le_bytes());
            b.extend_from_slice(u);
        }
        format!("{PREFIX}{}", data_encoding::BASE32_NOPAD.encode(&b))
    }

    /// Reads a token's text form (case, spaces and line breaks don't matter).
    pub fn decode(text: &str) -> io::Result<Token> {
        let t: String = text
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_ascii_uppercase();
        let body = t
            .strip_prefix(&PREFIX.to_ascii_uppercase())
            .ok_or_else(|| bad("doesn't start with IDNI1-"))?;
        let b = data_encoding::BASE32_NOPAD
            .decode(body.as_bytes())
            .map_err(|_| bad("not readable (a character mistyped?)"))?;
        let mut r = Reader(&b);
        if r.take(1)?[0] != VERSION {
            return Err(bad("made by a newer Site Manager"));
        }
        let id = r.take(8)?.try_into().unwrap();
        let secret = r.take(32)?.try_into().unwrap();
        let ca_fingerprint = r.take(32)?.try_into().unwrap();
        let n = r.take(1)?[0] as usize;
        let mut urls = Vec::with_capacity(n);
        for _ in 0..n {
            let len = u16::from_le_bytes(r.take(2)?.try_into().unwrap()) as usize;
            let u = std::str::from_utf8(r.take(len)?).map_err(|_| bad("an address isn't text"))?;
            urls.push(u.to_owned());
        }
        if !r.0.is_empty() {
            return Err(bad("trailing bytes"));
        }
        Ok(Token {
            id,
            secret,
            ca_fingerprint,
            urls,
        })
    }

    /// `HMAC-SHA256(secret, label || device SPKI || CA SPKI)`: `label` is `device` (the
    /// Node's proof) or `authority` (the Site Manager's).
    pub fn mac(&self, label: &str, device_spki: &[u8], ca_spki: &[u8]) -> Vec<u8> {
        mac(&self.secret, label, device_spki, ca_spki)
    }
}

/// [`Token::mac`] with the secret alone, as the Site Manager keeps it.
pub fn mac(secret: &[u8; 32], label: &str, device_spki: &[u8], ca_spki: &[u8]) -> Vec<u8> {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
    let mut ctx = ring::hmac::Context::with_key(&key);
    ctx.update(label.as_bytes());
    ctx.update(device_spki);
    ctx.update(ca_spki);
    ctx.sign().as_ref().to_vec()
}

/// Constant-time equality.
pub use idn_open_protocol::trust::proto::same;

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(bad("too short (cut off?)"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_read_back_whatever_the_case_and_spacing() {
        let t = Token::new(
            &"ab".repeat(32),
            vec![
                "idn://hub.example:29206".into(),
                "wss://hub.example/idn/link".into(),
            ],
        )
        .unwrap();
        let text = t.encode();
        assert!(text.starts_with("IDNI1-"));
        assert_eq!(Token::decode(&text).unwrap(), t);
        let messy = format!(" {} \n", text.to_ascii_lowercase());
        assert_eq!(Token::decode(&messy).unwrap(), t);
        // A character changed: refused, not misread.
        let mut cut = text.clone();
        cut.truncate(text.len() - 10);
        assert!(Token::decode(&cut).is_err());
        assert!(Token::decode("IDNI1-!!!").is_err());
        assert!(!format!("{t:?}").contains(&idn_common::hex::hex(&t.secret)));
    }

    #[test]
    fn macs_bind_the_label_the_device_and_the_ca() {
        let t = Token::new(&"00".repeat(32), vec![]).unwrap();
        let m = t.mac("device", b"dev", b"ca");
        assert!(same(&m, &mac(&t.secret, "device", b"dev", b"ca")));
        assert!(!same(&m, &t.mac("authority", b"dev", b"ca")));
        assert!(!same(&m, &t.mac("device", b"dev2", b"ca")));
        assert!(!same(&m, &t.mac("device", b"dev", b"ca2")));
    }
}

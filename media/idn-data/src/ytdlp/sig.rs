//! yt-dlp's release signature (bug round E, E4-3): each release's
//! `SHA2-256SUMS` is signed with yt-dlp's signing key (OpenPGP, RSA 4096, Simon Sawicki,
//! fingerprint `AC0C BBE6 848D 6A87 3464 AF4E 57CF 6593 3B5A 7581`, published as
//! `public.key` in yt-dlp's repository). The key is pinned here, so a release changed by
//! someone else (its checksums too) is refused, and the yt-dlp already here is kept.
//!
//! Only what yt-dlp uses is read: one version 4 signature over a binary document, RSA
//! (PKCS #1 v1.5) with SHA-256 or SHA-512, issued by the pinned key. Anything else is
//! refused rather than guessed at.

/// The pinned key: its OpenPGP public-key packet (version 4, RSA), as yt-dlp publishes it.
const KEY_PACKET: &[u8] = include_bytes!("signing-key.bin");
/// Its fingerprint (SHA-1 of the packet, RFC 4880 §12.2).
pub const KEY_FINGERPRINT: [u8; 20] = [
    0xac, 0x0c, 0xbb, 0xe6, 0x84, 0x8d, 0x6a, 0x87, 0x34, 0x64, 0xaf, 0x4e, 0x57, 0xcf, 0x65, 0x93,
    0x3b, 0x5a, 0x75, 0x81,
];

/// The key's (modulus, exponent).
fn key() -> Result<(&'static [u8], &'static [u8]), String> {
    let k = KEY_PACKET;
    if k.len() < 6 || k[0] != 4 || k[5] != 1 {
        return Err("the pinned key isn't a version 4 RSA key".into());
    }
    let (n, rest) = mpi(&k[6..])?;
    let (e, _) = mpi(rest)?;
    Ok((n, e))
}

/// An OpenPGP multiprecision integer: (its bytes, what follows).
fn mpi(b: &[u8]) -> Result<(&[u8], &[u8]), String> {
    if b.len() < 2 {
        return Err("a number cut short".into());
    }
    let bits = u16::from_be_bytes([b[0], b[1]]) as usize;
    let len = bits.div_ceil(8);
    if b.len() < 2 + len {
        return Err("a number cut short".into());
    }
    Ok((&b[2..2 + len], &b[2 + len..]))
}

/// The one packet in `b`: (tag, body).
fn packet(b: &[u8]) -> Result<(u8, &[u8]), String> {
    let first = *b.first().ok_or("an empty signature")?;
    if first & 0x80 == 0 {
        return Err("not an OpenPGP packet".into());
    }
    let (tag, start, len) = if first & 0x40 != 0 {
        // New format (RFC 4880 §4.2.2).
        let tag = first & 0x3f;
        match b.get(1).copied() {
            Some(l @ 0..=191) => (tag, 2, l as usize),
            Some(l @ 192..=223) => {
                let l2 = *b.get(2).ok_or("cut short")? as usize;
                (tag, 3, ((l as usize - 192) << 8) + l2 + 192)
            }
            Some(255) => {
                let l = b.get(2..6).ok_or("cut short")?;
                (tag, 6, u32::from_be_bytes(l.try_into().unwrap()) as usize)
            }
            _ => return Err("a packet length not taken".into()),
        }
    } else {
        // Old format (§4.2.1).
        let tag = (first >> 2) & 0x0f;
        match first & 3 {
            0 => (tag, 2, *b.get(1).ok_or("cut short")? as usize),
            1 => {
                let l = b.get(1..3).ok_or("cut short")?;
                (tag, 3, u16::from_be_bytes(l.try_into().unwrap()) as usize)
            }
            2 => {
                let l = b.get(1..5).ok_or("cut short")?;
                (tag, 5, u32::from_be_bytes(l.try_into().unwrap()) as usize)
            }
            _ => return Err("a packet length not taken".into()),
        }
    };
    let body = b.get(start..start + len).ok_or("a packet cut short")?;
    if start + len != b.len() {
        return Err("more than one packet".into());
    }
    Ok((tag, body))
}

/// Subpackets (§5.2.3.1): (type without the critical bit, data).
fn subpackets(mut b: &[u8]) -> Result<Vec<(u8, &[u8])>, String> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let (len, skip) = match b[0] {
            l @ 0..=191 => (l as usize, 1),
            l @ 192..=254 => {
                let l2 = *b.get(1).ok_or("cut short")? as usize;
                (((l as usize - 192) << 8) + l2 + 192, 2)
            }
            255 => {
                let l = b.get(1..5).ok_or("cut short")?;
                (u32::from_be_bytes(l.try_into().unwrap()) as usize, 5)
            }
        };
        let sp = b.get(skip..skip + len).ok_or("a subpacket cut short")?;
        let (&t, data) = sp.split_first().ok_or("an empty subpacket")?;
        out.push((t & 0x7f, data));
        b = &b[skip + len..];
    }
    Ok(out)
}

/// Checks that `sig` (a detached binary signature, as `SHA2-256SUMS.sig`) is the pinned
/// key's over `data`: the time it was made (Unix seconds, from its signed part), so an
/// older release can be refused (bug round F, F2-1).
pub fn verify(data: &[u8], sig: &[u8]) -> Result<u64, String> {
    use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RSA_PKCS1_2048_8192_SHA512};
    let (tag, s) = packet(sig)?;
    if tag != 2 {
        return Err("not a signature".into());
    }
    // version, type, key algorithm, hash, hashed subpackets' length.
    if s.len() < 6 || s[0] != 4 {
        return Err("not a version 4 signature".into());
    }
    if s[1] != 0x00 {
        return Err(format!(
            "signature type {:#04x}, not over a binary file",
            s[1]
        ));
    }
    if s[2] != 1 {
        return Err("not an RSA signature".into());
    }
    let alg = match s[3] {
        8 => &RSA_PKCS1_2048_8192_SHA256,
        10 => &RSA_PKCS1_2048_8192_SHA512,
        h => return Err(format!("hash {h} not taken (SHA-256 or SHA-512)")),
    };
    let hashed_len = u16::from_be_bytes([s[4], s[5]]) as usize;
    let hashed = s.get(6..6 + hashed_len).ok_or("cut short")?;
    let rest = &s[6 + hashed_len..];
    let unhashed_len = u16::from_be_bytes([
        *rest.first().ok_or("cut short")?,
        *rest.get(1).ok_or("cut short")?,
    ]) as usize;
    let unhashed = rest.get(2..2 + unhashed_len).ok_or("cut short")?;
    let rest = &rest[2 + unhashed_len..];
    // (Two bytes of the hash, then the signature as a number.)
    let (value, _) = mpi(rest.get(2..).ok_or("cut short")?)?;
    // Issued by the pinned key: its fingerprint (hashed, so signed) or its key id.
    let hashed_sp = subpackets(hashed)?;
    let by_fingerprint = hashed_sp
        .iter()
        .any(|(t, d)| *t == 33 && d.len() == 21 && d[0] == 4 && d[1..] == KEY_FINGERPRINT);
    let by_id = hashed_sp
        .iter()
        .chain(subpackets(unhashed)?.iter())
        .any(|(t, d)| *t == 16 && **d == KEY_FINGERPRINT[12..]);
    if !by_fingerprint && !by_id {
        return Err("signed by another key than yt-dlp's".into());
    }
    // When it was made (§5.2.3.4; required, and in the signed part).
    let created = hashed_sp
        .iter()
        .find(|(t, d)| *t == 2 && d.len() == 4)
        .map(|(_, d)| u32::from_be_bytes([d[0], d[1], d[2], d[3]]) as u64)
        .ok_or("a signature without the time it was made")?;
    // What is signed: the file, the signature's hashed part, then its trailer (§5.2.4).
    let mut msg = Vec::with_capacity(data.len() + 6 + hashed_len + 6);
    msg.extend_from_slice(data);
    msg.extend_from_slice(&s[..6 + hashed_len]);
    msg.extend_from_slice(&[0x04, 0xff]);
    msg.extend_from_slice(&((6 + hashed_len) as u32).to_be_bytes());
    let (n, e) = key()?;
    // (The signature as long as the modulus: leading zeros an MPI leaves out put back.)
    if value.len() > n.len() {
        return Err("a signature longer than the key".into());
    }
    let mut padded = vec![0u8; n.len() - value.len()];
    padded.extend_from_slice(value);
    ring::signature::RsaPublicKeyComponents { n, e }
        .verify(alg, &msg, &padded)
        .map_err(|_| "the signature doesn't match (not signed by yt-dlp's key, or changed)")?;
    Ok(created)
}

/// When the release this was written against (2026.10.26) was signed: no older release
/// is taken, even by a machine that never had one (F2-1).
pub const SIGNED_2026_10_26: u64 = 1_793_053_481;

#[cfg(test)]
mod tests {
    use super::*;

    const SUMS: &[u8] = include_bytes!("../../tests/vectors/yt-dlp-2026.10.26-SHA2-256SUMS");
    const SIG: &[u8] = include_bytes!("../../tests/vectors/yt-dlp-2026.10.26-SHA2-256SUMS.sig");

    /// The pinned packet is yt-dlp's published key: its fingerprint is the one pinned.
    #[test]
    fn the_pinned_key_has_yt_dlps_fingerprint() {
        let mut h = Vec::from([0x99u8]);
        h.extend_from_slice(&(KEY_PACKET.len() as u16).to_be_bytes());
        h.extend_from_slice(KEY_PACKET);
        let fp = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, &h);
        assert_eq!(fp.as_ref(), KEY_FINGERPRINT);
        let (n, e) = key().unwrap();
        assert_eq!((n.len(), e), (512, &[1u8, 0, 1][..]));
    }

    /// A real release (2026.10.26) passes; one byte changed in its checksums, or in the
    /// signature, fails.
    #[test]
    fn a_release_signature_is_checked() {
        assert_eq!(verify(SUMS, SIG).unwrap(), SIGNED_2026_10_26);
        let mut sums = SUMS.to_vec();
        sums[10] ^= 1;
        assert!(verify(&sums, SIG).is_err());
        let mut sig = SIG.to_vec();
        let last = sig.len() - 1;
        sig[last] ^= 1;
        assert!(verify(SUMS, &sig).is_err());
        assert!(verify(SUMS, b"").is_err());
        assert!(verify(SUMS, &SIG[..100]).is_err());
    }
}

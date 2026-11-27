//! The pieces of the QUIC v1 wire format (RFC 9000) the NDI profile's crypto needs
//! (spec chapter 4): varints and the trailer value `K` (§4.2.2). Packets and frames are
//! quinn's.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    Truncated,
    BadPacket,
    BadFrame(u64),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for WireError {}

type R<T> = Result<T, WireError>;

// ---------------------------------------------------------------------------------------
// Variable-length integers (RFC 9000 §16).

pub fn read_varint(b: &mut &[u8]) -> R<u64> {
    let first = *b.first().ok_or(WireError::Truncated)?;
    let len = 1usize << (first >> 6);
    if b.len() < len {
        return Err(WireError::Truncated);
    }
    let mut v = (first & 0x3f) as u64;
    for &x in &b[1..len] {
        v = (v << 8) | x as u64;
    }
    *b = &b[len..];
    Ok(v)
}

pub fn write_varint(out: &mut Vec<u8>, v: u64) {
    if v < 1 << 6 {
        out.push(v as u8);
    } else if v < 1 << 14 {
        out.extend_from_slice(&((v as u16) | 0x4000).to_be_bytes());
    } else if v < 1 << 30 {
        out.extend_from_slice(&((v as u32) | 0x8000_0000).to_be_bytes());
    } else {
        out.extend_from_slice(&(v | 0xC000_0000_0000_0000).to_be_bytes());
    }
}

// ---------------------------------------------------------------------------------------
// Trailer key K (§4.2.2).

/// QUIC v1 initial salt (RFC 9001 §5.2).
pub const INITIAL_SALT: [u8; 20] = [
    0x38, 0x76, 0x2c, 0xf7, 0xf5, 0x59, 0x34, 0xb3, 0x4d, 0x17, 0x9a, 0xe6, 0xa4, 0xc8, 0x0c, 0xad,
    0xcc, 0xbb, 0x7f, 0x0a,
];

/// The 8-byte trailer value for a 32-byte secret.
pub fn trailer_key(secret: &[u8; 32]) -> [u8; 8] {
    let mut k = 1u64.to_le_bytes();
    for (i, &x) in secret.iter().enumerate() {
        k[i % 8] = k[i % 8].wrapping_add(x);
    }
    k
}

/// The Initial-level secret for the client's original destination connection ID.
pub fn initial_secret(odcid: &[u8]) -> [u8; 32] {
    let mut x = [0u8; 32];
    for (i, &s) in INITIAL_SALT.iter().enumerate() {
        x[i % 32] = x[i % 32].wrapping_add(s);
    }
    for (k, &d) in odcid.iter().enumerate() {
        x[(20 + k) % 32] = x[(20 + k) % 32].wrapping_add(d);
    }
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> Vec<u8> {
        idn_common::hex::unhex(s).unwrap()
    }

    #[test]
    fn varint_roundtrip() {
        for v in [
            0,
            63,
            64,
            16383,
            16384,
            (1 << 30) - 1,
            1 << 30,
            (1 << 62) - 1,
        ] {
            let mut b = Vec::new();
            write_varint(&mut b, v);
            let mut s = &b[..];
            assert_eq!(read_varint(&mut s).unwrap(), v);
            assert!(s.is_empty());
        }
    }

    #[test]
    fn trailer_key_vectors() {
        // §4.5 vectors 1, 3, 5.
        let k = |s: &[u8]| idn_common::hex::hex(&trailer_key(&initial_secret(s)));
        assert_eq!(k(&h("c7c4f1cae8a56a27")), "3aedaf0e60e5312a");
        assert_eq!(k(&h("93e81f52870439e7")), "d94c7ece2c095fb2");
        let sec = |s: &str| -> [u8; 32] { h(s).try_into().unwrap() };
        assert_eq!(
            idn_common::hex::hex(&trailer_key(&sec(
                "d394aed40946b9db7288e80eca44bc6eaf8da5ac6b46f3b70dbdb597a16c2be6"
            ))),
            "0266f025df3c93e6"
        );
        assert_eq!(
            idn_common::hex::hex(&trailer_key(&sec(
                "c6d5a28a71edad6464da72fec655883805bca017c55c9d5214e1b0ac1fb372a4"
            ))),
            "444c644b1b514492"
        );
    }
}

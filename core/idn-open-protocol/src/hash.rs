//! SHA-256 in the two forms the code wants: the bytes, and lower-case hex (a package's
//! checksum, a file's version, an id made from a name).

use sha2::{Digest, Sha256};

/// The SHA-256 of `b`.
pub fn sha256(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

/// The SHA-256 of `b` as lower-case hex (64 characters).
pub fn sha256_hex(b: &[u8]) -> String {
    idn_common::hex::hex(&sha256(b))
}

#[cfg(test)]
mod tests {
    /// FIPS 180-2's "abc".
    #[test]
    fn abc() {
        assert_eq!(
            super::sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}

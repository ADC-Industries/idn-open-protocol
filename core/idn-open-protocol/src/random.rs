//! Random bytes, tokens and codes: one source ([`secure_random`], the system's
//! cryptographic generator through ring) and one way to make each kind of secret. Codes
//! made with `% 1_000_000` on a random `u32` came out a little more often low than high
//! (bug: the key-request codes); these draw without that bias.

use ring::rand::SecureRandom;

/// Cryptographically random bytes: tokens, keys, codes, nonces.
pub fn secure_random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    fill(&mut b);
    b
}

/// `buf` filled with cryptographically random bytes.
pub fn fill(buf: &mut [u8]) {
    ring::rand::SystemRandom::new()
        .fill(buf)
        .expect("the system's random generator");
}

/// A number from `0..n`, every one as likely (a random `u32` drawn again while it falls
/// in the incomplete last round of `n`). `n` must not be 0.
pub fn below(n: u32) -> u32 {
    assert!(n > 0, "random::below(0)");
    // The largest multiple of n that fits: draws at or above it are drawn again.
    let zone = u32::MAX - u32::MAX % n;
    loop {
        let x = u32::from_le_bytes(secure_random::<4>());
        if x < zone {
            return x % n;
        }
    }
}

/// `n` random bytes as lower-case hex (`2n` characters): an id or a bearer token.
pub fn token_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    fill(&mut b);
    idn_common::hex::hex(&b)
}

/// `n` random decimal digits, every code as likely: a code a person types (a six-digit
/// pairing or key-request code).
pub fn code_digits(n: usize) -> String {
    (0..n).map(|_| char::from(b'0' + below(10) as u8)).collect()
}

/// The letters of [`code_base32`]: digits and capitals without 0/O and 1/I, which are
/// misread from a screen or paper.
pub const CODE_ALPHABET: &[u8; 32] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";

/// `n` random characters of [`CODE_ALPHABET`] (5 bits each): a code a person types that
/// needs more than digits give (a one-time admin code).
pub fn code_base32(n: usize) -> String {
    let mut b = vec![0u8; n];
    fill(&mut b);
    // 32 divides 256: masking a uniform byte keeps it uniform.
    b.iter()
        .map(|x| CODE_ALPHABET[(x & 31) as usize] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes() {
        assert_ne!(secure_random::<16>(), [0u8; 16]);
        let t = token_hex(16);
        assert_eq!(t.len(), 32);
        assert!(
            t.bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_ne!(token_hex(16), t);
        let c = code_digits(6);
        assert_eq!(c.len(), 6);
        assert!(c.bytes().all(|c| c.is_ascii_digit()));
        let b = code_base32(8);
        assert_eq!(b.len(), 8);
        assert!(b.bytes().all(|c| CODE_ALPHABET.contains(&c)));
        assert_eq!(below(1), 0);
    }

    /// Each digit near a tenth of the draws (a sanity check of the rejection, not a
    /// statistical test: the bounds are wide enough never to fail by chance).
    #[test]
    fn digits_come_out_evenly() {
        let mut seen = [0u32; 10];
        for _ in 0..2000 {
            for c in code_digits(10).bytes() {
                seen[(c - b'0') as usize] += 1;
            }
        }
        // 20 000 draws: 2000 expected each, σ ≈ 42.
        assert!(seen.iter().all(|n| (1700..2300).contains(n)), "{seen:?}");
        let mut seen = [0u32; 3];
        for _ in 0..30_000 {
            seen[below(3) as usize] += 1;
        }
        assert!(seen.iter().all(|n| (9_400..10_600).contains(n)), "{seen:?}");
    }
}

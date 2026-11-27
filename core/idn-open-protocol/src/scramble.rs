//! Frame-body scrambling (spec §1.3).
//!
//! Two keyless obfuscation schemes protect the extension header and payload of every
//! frame whose `type` has bit 15 set. Neither is security; both are needed to interoperate.

/// Scheme B XOR table `K` (spec Appendix A).
pub const TABLE_K: [u8; 128] = [
    0x4e, 0x44, 0x49, 0xae, 0x2c, 0x20, 0xa9, 0x32, 0x30, 0x31, 0x37, 0x20, 0x4e, 0x65, 0x77, 0x54,
    0x65, 0x6b, 0x2c, 0x20, 0x50, 0x72, 0x6f, 0x70, 0x72, 0x69, 0x65, 0x74, 0x79, 0x20, 0x61, 0x6e,
    0x64, 0x20, 0x43, 0x6f, 0x6e, 0x66, 0x69, 0x64, 0x65, 0x6e, 0x74, 0x69, 0x61, 0x6c, 0x2e, 0x20,
    0x59, 0x6f, 0x75, 0x20, 0x61, 0x72, 0x65, 0x20, 0x69, 0x6e, 0x20, 0x76, 0x69, 0x6f, 0x6c, 0x61,
    0x74, 0x69, 0x6f, 0x6e, 0x20, 0x6f, 0x66, 0x20, 0x74, 0x68, 0x65, 0x20, 0x4e, 0x44, 0x49, 0xae,
    0x20, 0x53, 0x44, 0x4b, 0x20, 0x6c, 0x69, 0x63, 0x65, 0x6e, 0x73, 0x65, 0x20, 0x61, 0x74, 0x20,
    0x68, 0x74, 0x74, 0x70, 0x3a, 0x2f, 0x2f, 0x6e, 0x65, 0x77, 0x2e, 0x74, 0x6b, 0x2f, 0x6e, 0x64,
    0x69, 0x73, 0x64, 0x6b, 0x5f, 0x6c, 0x69, 0x63, 0x65, 0x6e, 0x73, 0x65, 0x2f, 0x00, 0x00, 0x00,
];

const A_INIT0: u64 = 0xB711_674B_D24F_4B24;
const A_INIT1: u64 = 0xB080_D84F_1FE3_BF44;

const B_G: u64 = 0x9E37_79B9_7F4A_7C15;
const B_G_INV: u64 = 0xF1DE_83E1_9937_733D;
const B_C1: u64 = 0xC42B_D7DE_E627_0F1B;

/// Which scrambling scheme a frame uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    A,
    B,
}

/// Picks the scheme from the message kind (`type & 0x7FFF`) and `version` (§1.3).
pub fn scheme_for(kind: u16, version: u16) -> Scheme {
    match version {
        1 | 2 if kind > 2 => Scheme::B,
        0 if kind > 3 => Scheme::B,
        _ => Scheme::A,
    }
}

/// The seed `S = (hdr_len + data_len) mod 2^32` shared by both schemes.
pub fn seed(hdr_len: u32, data_len: u32) -> u32 {
    hdr_len.wrapping_add(data_len)
}

/// Running Scheme A state (xorshift128+ with plaintext feedback, §1.3.1).
///
/// One state covers a whole frame: the extension header and then the payload, each
/// processed with [`SchemeA::scramble`] / [`SchemeA::descramble`] in turn.
#[derive(Debug, Clone)]
pub struct SchemeA {
    s0: u64,
    s1: u64,
}

impl SchemeA {
    pub fn new(seed: u32) -> Self {
        let s = seed as u64;
        let d = (s << 32) | s;
        SchemeA {
            s0: d ^ A_INIT0,
            s1: d ^ A_INIT1,
        }
    }

    /// Advances one step and returns the key word; `plain` is fed back afterwards.
    fn key(&self) -> (u64, u64) {
        let x = self.s0 ^ (self.s0 << 23);
        let t = x ^ self.s1 ^ (x >> 17) ^ (self.s1 >> 26);
        (t.wrapping_add(self.s1), t)
    }

    fn feed(&mut self, t: u64, plain: u64) {
        self.s0 = self.s1;
        self.s1 = t ^ plain;
    }

    /// Scrambles `buf` in place. Word alignment starts at `buf[0]`.
    pub fn scramble(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let plain = load_padded(chunk);
            let (key, t) = self.key();
            store_partial(chunk, plain ^ key);
            self.feed(t, plain);
        }
    }

    /// Descrambles `buf` in place. Word alignment starts at `buf[0]`.
    pub fn descramble(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let (key, t) = self.key();
            // For a short tail this feeds back (zero-padded ciphertext XOR full key), not
            // the zero-padded plaintext the scrambler used (§1.3.1). The two
            // only agree when the tail is the last word of the frame.
            let plain = load_padded(chunk) ^ key;
            store_partial(chunk, plain);
            self.feed(t, plain);
        }
    }
}

/// Scheme B scrambling of one buffer (§1.3.2). Header and payload are done independently,
/// each starting from the same `seed`.
pub fn scheme_b_scramble(seed: u32, buf: &mut [u8]) {
    let len = buf.len() as u64;
    for (b, k) in buf.iter_mut().zip(TABLE_K.iter()) {
        *b ^= k;
    }
    let mut s = seed as u64;
    for chunk in buf.as_chunks_mut::<8>().0 {
        let p = u64::from_le_bytes(*chunk);
        let c = (p.wrapping_mul(B_G).wrapping_add(B_C1)) ^ scheme_b_mask(s, len);
        *chunk = c.to_le_bytes();
        s = c & 0xFFFF_FFFF;
    }
}

/// Inverse of [`scheme_b_scramble`].
pub fn scheme_b_descramble(seed: u32, buf: &mut [u8]) {
    let len = buf.len() as u64;
    let mut s = seed as u64;
    for chunk in buf.as_chunks_mut::<8>().0 {
        let c = u64::from_le_bytes(*chunk);
        let p = ((c ^ scheme_b_mask(s, len)).wrapping_sub(B_C1)).wrapping_mul(B_G_INV);
        *chunk = p.to_le_bytes();
        s = c & 0xFFFF_FFFF;
    }
    for (b, k) in buf.iter_mut().zip(TABLE_K.iter()) {
        *b ^= k;
    }
}

fn scheme_b_mask(s: u64, len: u64) -> u64 {
    s.wrapping_mul(B_G).wrapping_mul(len).wrapping_add(B_C1)
}

fn load_padded(chunk: &[u8]) -> u64 {
    let mut w = [0u8; 8];
    w[..chunk.len()].copy_from_slice(chunk);
    u64::from_le_bytes(w)
}

fn store_partial(chunk: &mut [u8], word: u64) {
    let n = chunk.len();
    chunk.copy_from_slice(&word.to_le_bytes()[..n]);
}

/// Scrambles a frame body in place: `ext` (extension header) and `payload`.
/// The payload is scrambled only when `version` is 2 (§1.3).
///
/// Senders must not produce a Scheme A frame whose `ext` length is not a multiple of 8
/// when a scrambled payload follows (§1.3.1); this is checked in debug builds.
pub fn scramble_body(kind: u16, version: u16, ext: &mut [u8], payload: &mut [u8]) {
    let s = seed(ext.len() as u32, payload.len() as u32);
    let do_payload = version == 2;
    match scheme_for(kind, version) {
        Scheme::A => {
            debug_assert!(
                !do_payload || ext.len().is_multiple_of(8),
                "Scheme A ext length must be a multiple of 8 before a scrambled payload"
            );
            let mut st = SchemeA::new(s);
            st.scramble(ext);
            if do_payload {
                st.scramble(payload);
            }
        }
        Scheme::B => {
            scheme_b_scramble(s, ext);
            if do_payload {
                scheme_b_scramble(s, payload);
            }
        }
    }
}

/// Inverse of [`scramble_body`].
pub fn descramble_body(kind: u16, version: u16, ext: &mut [u8], payload: &mut [u8]) {
    let s = seed(ext.len() as u32, payload.len() as u32);
    let do_payload = version == 2;
    match scheme_for(kind, version) {
        Scheme::A => {
            let mut st = SchemeA::new(s);
            st.descramble(ext);
            if do_payload {
                st.descramble(payload);
            }
        }
        Scheme::B => {
            scheme_b_descramble(s, ext);
            if do_payload {
                scheme_b_descramble(s, payload);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn table_k_hash() {
        let h = Sha256::digest(TABLE_K);
        assert_eq!(
            idn_common::hex::hex(&h),
            "475ae3fe48ba9e0f9fd99f22e673768d9b8321eb88922318310fdc4f50e8c284"
        );
    }

    #[test]
    fn g_inverse() {
        assert_eq!(B_G.wrapping_mul(B_G_INV), 1);
    }

    #[test]
    fn scheme_selection() {
        assert_eq!(scheme_for(1, 2), Scheme::A);
        assert_eq!(scheme_for(2, 2), Scheme::A);
        assert_eq!(scheme_for(3, 2), Scheme::B);
        assert_eq!(scheme_for(7, 0), Scheme::B);
        assert_eq!(scheme_for(5, 1), Scheme::B);
        assert_eq!(scheme_for(3, 0), Scheme::A);
    }

    #[test]
    fn roundtrip_all_lengths() {
        for (kind, version) in [(1u16, 2u16), (3, 2), (7, 0), (5, 1), (1, 0)] {
            for hl in [0usize, 3, 8, 13] {
                // Forbidden for senders; covered by `scheme_a_partial_header_diverges`.
                if scheme_for(kind, version) == Scheme::A && version == 2 && hl % 8 != 0 {
                    continue;
                }
                for dl in [0usize, 1, 7, 8, 9, 127, 128, 129, 300] {
                    let ext0: Vec<u8> = (0..hl).map(|i| (i * 7 + 1) as u8).collect();
                    let pay0: Vec<u8> = (0..dl).map(|i| (i * 13 + 5) as u8).collect();
                    let (mut ext, mut pay) = (ext0.clone(), pay0.clone());
                    scramble_body(kind, version, &mut ext, &mut pay);
                    descramble_body(kind, version, &mut ext, &mut pay);
                    assert_eq!((ext, pay), (ext0, pay0), "kind {kind} v{version} {hl}/{dl}");
                }
            }
        }
    }

    /// §1.3.1: after a partial word, scrambler and descrambler states diverge. The
    /// partial word itself still descrambles correctly; the data after it does not.
    #[test]
    fn scheme_a_partial_header_diverges() {
        let ext0 = vec![0x11u8; 3];
        let pay0 = vec![0x22u8; 16];
        let s = seed(3, 16);
        let (mut ext, mut pay) = (ext0.clone(), pay0.clone());
        let mut tx = SchemeA::new(s);
        tx.scramble(&mut ext);
        tx.scramble(&mut pay);
        let mut rx = SchemeA::new(s);
        rx.descramble(&mut ext);
        rx.descramble(&mut pay);
        assert_eq!(ext, ext0);
        assert_ne!(pay, pay0);
    }

    /// A partial word at the very end of the frame descrambles the same under either rule.
    #[test]
    fn scheme_a_final_partial_word_roundtrips() {
        let ext0 = 0i64.to_le_bytes().to_vec();
        let pay0: Vec<u8> = (0..33u8).collect();
        let (mut ext, mut pay) = (ext0.clone(), pay0.clone());
        scramble_body(1, 2, &mut ext, &mut pay);
        descramble_body(1, 2, &mut ext, &mut pay);
        assert_eq!((ext, pay), (ext0, pay0));
    }
}

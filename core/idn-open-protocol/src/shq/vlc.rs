//! Variable-length code tables (spec §3.6.1, §3.7.2, §3.7.3, §3.8).

use super::ac_table::{AC_CODES, EOB_CODE, ESC_CODE};
use std::sync::OnceLock;

/// An AC symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcSym {
    Eob,
    Esc,
    /// Run and |level|; a sign bit follows.
    RunLevel(u8, u8),
}

/// A decoded table entry: code length in bits and symbol. Length 0 = no valid code.
#[derive(Debug, Clone, Copy)]
pub struct Entry<T> {
    pub len: u8,
    pub sym: T,
}

/// Lookup table indexed by the next `BITS` stream bits (first bit = bit 0 of the index).
pub struct Lut<T: Copy, const BITS: u32> {
    entries: Vec<Entry<T>>,
}

impl<T: Copy, const BITS: u32> Lut<T, BITS> {
    fn build(codes: &[(&str, T)], fill: T) -> Self {
        let mut entries = vec![Entry { len: 0, sym: fill }; 1 << BITS];
        for &(code, sym) in codes {
            let n = code.len() as u32;
            assert!(n <= BITS);
            let prefix = code
                .bytes()
                .enumerate()
                .fold(0usize, |acc, (i, b)| acc | (((b - b'0') as usize) << i));
            for ext in 0..(1usize << (BITS - n)) {
                let e = &mut entries[prefix | (ext << n)];
                assert_eq!(e.len, 0, "codes are not prefix-free: {code}");
                *e = Entry { len: n as u8, sym };
            }
        }
        Lut { entries }
    }

    #[inline]
    pub fn get(&self, bits: u32) -> Entry<T> {
        self.entries[bits as usize]
    }
}

pub type DcLut = Lut<u8, 10>;

/// The AC table in two levels, small enough for L1 (about 3 KB instead of 256 KB): the
/// first 8 bits resolve every code of up to 8 bits; the few prefixes of longer codes
/// lead to a second table indexed by the next 8 bits.
pub struct AcLut {
    first: [Entry<AcSym>; 256],
    /// For first-level entries of longer codes: their second table (index into `second`,
    /// 256 entries each).
    sub: [u8; 256],
    second: Vec<Entry<AcSym>>,
}

/// Marks a first-level entry whose codes are longer than 8 bits.
const LONG: u8 = 0xff;

impl AcLut {
    fn build(codes: &[(&str, AcSym)]) -> Self {
        let full = Lut::<AcSym, 16>::build(codes, AcSym::Eob);
        let mut t = AcLut {
            first: [Entry {
                len: 0,
                sym: AcSym::Eob,
            }; 256],
            sub: [0; 256],
            second: Vec::new(),
        };
        for p in 0..256usize {
            // The 256 continuations of this 8-bit prefix.
            let cont: Vec<Entry<AcSym>> =
                (0..256).map(|x| full.get((p | (x << 8)) as u32)).collect();
            let short = cont
                .iter()
                .all(|e| e.len == cont[0].len && e.len <= 8 && e.len > 0);
            if short || cont.iter().all(|e| e.len == 0) {
                t.first[p] = cont[0];
            } else {
                t.sub[p] = (t.second.len() / 256) as u8;
                t.first[p] = Entry {
                    len: LONG,
                    sym: AcSym::Eob,
                };
                t.second.extend(cont);
            }
        }
        t
    }

    /// The code at the start of the next 16 stream bits (first bit = bit 0).
    #[inline]
    pub fn get(&self, bits: u32) -> Entry<AcSym> {
        let e = self.first[(bits & 0xff) as usize];
        if e.len != LONG {
            return e;
        }
        let base = self.sub[(bits & 0xff) as usize] as usize * 256;
        self.second[base + ((bits >> 8) & 0xff) as usize]
    }
}

fn ac_codes() -> Vec<(&'static str, AcSym)> {
    let mut codes: Vec<(&str, AcSym)> = AC_CODES
        .iter()
        .map(|&(r, l, c)| (c, AcSym::RunLevel(r, l)))
        .collect();
    codes.push((EOB_CODE, AcSym::Eob));
    codes.push((ESC_CODE, AcSym::Esc));
    codes
}

pub fn ac_lut() -> &'static AcLut {
    static T: OnceLock<AcLut> = OnceLock::new();
    T.get_or_init(|| AcLut::build(&ac_codes()))
}

/// DC size codes, sizes 0..11 (§3.6.1).
const DC_LUMA: [&str; 12] = [
    "100",
    "00",
    "01",
    "101",
    "110",
    "1110",
    "11110",
    "111110",
    "1111110",
    "11111110",
    "111111110",
    "111111111",
];
const DC_CHROMA: [&str; 12] = [
    "00",
    "01",
    "10",
    "110",
    "1110",
    "11110",
    "111110",
    "1111110",
    "11111110",
    "111111110",
    "1111111110",
    "1111111111",
];

fn dc_lut(codes: &[&'static str; 12]) -> DcLut {
    let v: Vec<(&str, u8)> = codes
        .iter()
        .enumerate()
        .map(|(s, &c)| (c, s as u8))
        .collect();
    Lut::build(&v, 0)
}

pub fn dc_luma_lut() -> &'static DcLut {
    static T: OnceLock<DcLut> = OnceLock::new();
    T.get_or_init(|| dc_lut(&DC_LUMA))
}

pub fn dc_chroma_lut() -> &'static DcLut {
    static T: OnceLock<DcLut> = OnceLock::new();
    T.get_or_init(|| dc_lut(&DC_CHROMA))
}

/// MPEG-2 zigzag scan (`alternate_scan` = 0): scan position → raster index `8·v + u`.
pub const ZIGZAG: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// MPEG-2 default intra quantiser matrix, raster order.
pub const INTRA_QUANT: [u8; 64] = [
    8, 16, 19, 22, 26, 27, 29, 34, //
    16, 16, 22, 24, 27, 29, 34, 37, //
    19, 22, 26, 27, 29, 34, 34, 38, //
    22, 22, 26, 27, 29, 34, 37, 40, //
    22, 26, 27, 29, 32, 35, 40, 48, //
    26, 27, 29, 32, 35, 40, 48, 58, //
    26, 27, 29, 34, 38, 46, 56, 69, //
    27, 29, 35, 38, 46, 56, 69, 83,
];

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn ac_table_hash_matches_spec() {
        let mut txt = String::new();
        for (r, l, c) in AC_CODES {
            txt += &format!("{r} {l} {c}\n");
        }
        txt += &format!("EOB {EOB_CODE}\nESC {ESC_CODE}\n");
        assert_eq!(txt.len(), 2101);
        assert_eq!(
            idn_common::hex::hex(&Sha256::digest(txt.as_bytes())),
            "f6f052416b0a12fbfe509c22c8047548bfba0c0badc33170af943a7fc54f50b1"
        );
    }

    #[test]
    fn ac_table_covers_all_but_twelve_zeros() {
        let lut = ac_lut();
        for bits in 0u32..1 << 16 {
            let valid = lut.get(bits).len != 0;
            assert_eq!(valid, bits & 0xfff != 0, "bits {bits:016b}");
        }
    }

    #[test]
    fn two_level_ac_table_equals_the_full_one_and_is_small() {
        let full = Lut::<AcSym, 16>::build(&ac_codes(), AcSym::Eob);
        let lut = ac_lut();
        for bits in 0u32..1 << 16 {
            let (a, b) = (full.get(bits), lut.get(bits));
            assert_eq!((a.len, a.sym), (b.len, b.sym), "bits {bits:016b}");
        }
        let bytes = std::mem::size_of_val(&lut.first) + 256 + lut.second.len() * 4;
        assert!(bytes < 8 << 10, "{bytes} bytes");
    }

    #[test]
    fn max_level_per_run() {
        let mut max = [0u8; 32];
        for (r, l, _) in AC_CODES {
            max[r as usize] = max[r as usize].max(l);
        }
        let want = [40, 20, 11, 5, 4, 3, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2];
        assert_eq!(&max[..17], &want);
        assert!(max[17..].iter().all(|&m| m == 1));
    }

    #[test]
    fn zigzag_orientation() {
        assert_eq!(&ZIGZAG[1..4], &[1, 8, 16]);
        assert_eq!(ZIGZAG[63], 63);
        let mut seen = [false; 64];
        ZIGZAG.iter().for_each(|&k| seen[k as usize] = true);
        assert!(seen.iter().all(|&s| s));
    }
}

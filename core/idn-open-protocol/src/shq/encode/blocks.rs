//! Per-block coding: gathering samples, quantising, and the AC symbols.

use super::*;
use crate::shq::ac_table::AC_CODES;
use std::sync::OnceLock;

/// AC symbols of the blocks of one MB, each one code word (VLC with its sign bit, or
/// escape with run and level), built once and used both for the bit-cost estimate and for
/// writing. Cleared per MB and reused, so blocks stay small and nothing is allocated.
/// Each word is the code (≤ 24 bits) with its length in the top 8 bits.
pub(super) struct SymArena {
    pub(super) words: Box<[u32; SymArena::CAP]>,
    pub(super) n: usize,
}

impl Default for SymArena {
    fn default() -> Self {
        SymArena {
            words: Box::new([0; SymArena::CAP]),
            n: 0,
        }
    }
}

impl SymArena {
    /// Room for one MB: at most 16 blocks (4:4:4:4) of 63 AC symbols.
    const CAP: usize = 16 * 63;

    pub(super) fn clear(&mut self) {
        self.n = 0;
    }

    #[inline(always)]
    fn push(&mut self, code: u32, len: u32) {
        self.words[self.n] = code | len << 24;
        self.n += 1;
    }
}

/// A block's symbols in a [`SymArena`]: where they start, how many, total bits (no EOB).
#[derive(Clone, Copy, Default)]
pub(super) struct AcSymbols {
    pub(super) start: u32,
    pub(super) n: u8,
    pub(super) bits: u32,
}

impl AcSymbols {
    pub(super) const EMPTY: AcSymbols = AcSymbols {
        start: 0,
        n: 0,
        bits: 0,
    };
}

/// (run, |level|) → code word with its length in the top 8 bits; 0 = needs an escape.
/// Every run 0–62 has a row (runs from 32 on are all escapes), so lookups need no check.
fn ac_flat() -> &'static [[u32; 41]; 64] {
    static T: OnceLock<[[u32; 41]; 64]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [[0u32; 41]; 64];
        for &(r, l, c) in AC_CODES.iter() {
            let v = vlc(c);
            t[r as usize][l as usize] = v.0 | (v.1 << 24);
        }
        t
    })
}

/// The AC symbols of quantised levels (transposed layout, as the DCT gives them; index 0
/// ignored), into `arena`. Visits only the non-zero levels, by a bit mask in scan order.
#[inline(always)]
pub(super) fn ac_symbols(lv: &[i32; 64], arena: &mut SymArena) -> AcSymbols {
    let t = ac_flat();
    let mut s = AcSymbols {
        start: arena.n as u32,
        n: 0,
        bits: 0,
    };
    // The non-zero levels in scan order, without gathering all 64 (R1-2): the raster
    // mask, each nibble's bits moved to their scan positions by a table.
    let raster = nonzero_mask(lv);
    let perm = scan_perm();
    let mut mask = (0..16).fold(0u64, |m, c| {
        m | perm[c][((raster >> (4 * c)) & 15) as usize]
    }) & !1;
    let mut last = 0u32;
    while mask != 0 {
        let i = mask.trailing_zeros();
        mask &= mask - 1;
        let run = (i - last - 1) as usize;
        last = i;
        let l = lv[SCAN[i as usize] as usize];
        let a = l.unsigned_abs() as usize;
        let e = if a <= 40 { t[run][a] } else { 0 };
        let (code, len) = if e != 0 {
            let len = e >> 24;
            ((e & 0x00ff_ffff) | (u32::from(l < 0) << len), len + 1)
        } else {
            // ESC, 6-bit run, 12-bit level + 2048: one 24-bit word.
            (
                ESC.0 | ((run as u32) << 6) | (((l + 2048) as u32) << 12),
                24,
            )
        };
        arena.push(code, len);
        s.n += 1;
        s.bits += len;
    }
    s
}

/// For raster nibble `c` (indices 4c..4c+3) and its bits: the same set bits in scan
/// order.
fn scan_perm() -> &'static [[u64; 16]; 16] {
    static T: OnceLock<[[u64; 16]; 16]> = OnceLock::new();
    T.get_or_init(|| {
        let mut pos = [0u32; 64];
        for (i, &k) in SCAN.iter().enumerate() {
            pos[k as usize] = i as u32;
        }
        let mut t = [[0u64; 16]; 16];
        for (c, row) in t.iter_mut().enumerate() {
            for (bits, v) in row.iter_mut().enumerate() {
                for b in 0..4 {
                    if bits & (1 << b) != 0 {
                        *v |= 1u64 << pos[4 * c + b];
                    }
                }
            }
        }
        t
    })
}

/// Bit i set when `v[i]` is non-zero.
#[inline(always)]
fn nonzero_mask(v: &[i32; 64]) -> u64 {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: SSE2 is part of the x86-64 baseline; each load reads v[4c..4c+4].
    unsafe {
        use std::arch::x86_64::*;
        let z = _mm_setzero_si128();
        let mut zero = 0u64;
        for c in 0..16 {
            let x = _mm_loadu_si128(v.as_ptr().add(4 * c).cast());
            zero |= (_mm_movemask_ps(_mm_castsi128_ps(_mm_cmpeq_epi32(x, z))) as u64) << (4 * c);
        }
        !zero
    }
    #[cfg(not(target_arch = "x86_64"))]
    v.iter()
        .enumerate()
        .fold(0u64, |m, (i, &l)| m | u64::from(l != 0) << i)
}

/// One block: DC value `D` (normal mode) and its AC symbols.
pub(super) struct Coded {
    pub(super) d: i32,
    pub(super) sym: AcSymbols,
    pub(super) has_ac: bool,
}
#[inline(always)]
pub(super) fn quantise(
    samples: &[u8; 64],
    q: &QTable,
    short_dc: bool,
    arena: &mut SymArena,
) -> Coded {
    // Uniform block (very common on slides): DC is exactly 8·value, no AC, no DCT needed.
    // Compared eight samples at a time (byte by byte it kept the block in 64 bytes).
    let v0 = samples[0];
    let all = u64::from_ne_bytes([v0; 8]);
    if samples
        .as_chunks::<8>()
        .0
        .iter()
        .all(|r| u64::from_ne_bytes(*r) == all)
    {
        let d = if short_dc { v0 as i32 } else { 8 * v0 as i32 };
        return Coded {
            d,
            sym: AcSymbols::EMPTY,
            has_ac: false,
        };
    }
    let (dc, lv, has_ac) = dct_quant_u8(samples, q);
    let sym = if has_ac {
        ac_symbols(&lv, arena)
    } else {
        AcSymbols::EMPTY
    };
    let d = if short_dc {
        // Short DC: D is the block mean in sample units, filled exactly (§3.6.3).
        (dc / 8.0).round().clamp(0.0, 255.0) as i32
    } else if has_ac {
        dc.round().clamp(0.0, 2047.0) as i32
    } else {
        // No AC: fill truncates D >> 3, so use a multiple of 8 (§3.9.1, §3.12.3 #6).
        8 * (dc / 8.0).round().clamp(0.0, 255.0) as i32
    };
    Coded { d, sym, has_ac }
}

/// High-precision block (samples in 8-bit units with fractions, §3.9.5): as `quantise`
/// in normal DC mode, but a block without AC keeps D's fraction, which receivers with
/// more than 8 bits of output reproduce (8-bit receivers truncate it, as from NDI).
fn quantise_precise(samples: &[f32; 64], q: &QTable, arena: &mut SymArena) -> Coded {
    let v0 = samples[0];
    if samples.iter().all(|&v| v == v0) {
        return Coded {
            d: (8.0 * v0).round().clamp(0.0, 2047.0) as i32,
            sym: AcSymbols::EMPTY,
            has_ac: false,
        };
    }
    let (dc, lv, has_ac) = dct_quant_f32(samples, q);
    let sym = if has_ac {
        ac_symbols(&lv, arena)
    } else {
        AcSymbols::EMPTY
    };
    Coded {
        d: dc.round().clamp(0.0, 2047.0) as i32,
        sym,
        has_ac,
    }
}

fn block16(p: &Plane16, x0: usize, y0: usize) -> [f32; 64] {
    let mut b = [0f32; 64];
    // Interior blocks (nearly all): whole rows, no clamping (vectorises).
    if x0 + 8 <= p.width && y0 + 8 <= p.height {
        for y in 0..8 {
            let o = (y0 + y) * p.width + x0;
            let row: &[u16; 8] = p.data[o..o + 8].try_into().unwrap();
            for (d, &v) in b[8 * y..8 * y + 8].iter_mut().zip(row) {
                *d = v as f32 / 256.0;
            }
        }
        return b;
    }
    for y in 0..8 {
        let yy = (y0 + y).min(p.height - 1);
        for x in 0..8 {
            b[8 * y + x] = p.data[yy * p.width + (x0 + x).min(p.width - 1)] as f32 / 256.0;
        }
    }
    b
}

/// One block of plane `pl` at (x0, y0), from the precise planes when there are any.
pub(super) fn code_block(
    inp: &EncodeInput,
    pl: usize,
    x0: usize,
    y0: usize,
    q: &QTable,
    short_dc: bool,
    arena: &mut SymArena,
) -> Coded {
    match inp.precise {
        Some(p) => quantise_precise(&block16(&p[pl], x0, y0), q, arena),
        None => quantise(&block(&inp.planes[pl], x0, y0), q, short_dc, arena),
    }
}
/// Edge-replicated access to a plane, so partial MBs are coded from sensible samples.
/// The edge case is a function of its own: in one function, its byte-by-byte writes made
/// the compiler keep the whole block as 64 separate bytes, so every block's row copies
/// became 64 single-byte moves through the stack (about half of a 2160p encoder's time).
pub(super) fn block(p: &Plane, x0: usize, y0: usize) -> [u8; 64] {
    if x0 + 8 <= p.width && y0 + 8 <= p.height {
        let mut b = [0u8; 64];
        for y in 0..8 {
            let o = (y0 + y) * p.width + x0;
            b[8 * y..8 * y + 8].copy_from_slice(&p.data[o..o + 8]);
        }
        return b;
    }
    block_at_edge(p, x0, y0)
}

#[cold]
#[inline(never)]
fn block_at_edge(p: &Plane, x0: usize, y0: usize) -> [u8; 64] {
    let mut b = [0u8; 64];
    for y in 0..8 {
        let yy = (y0 + y).min(p.height - 1);
        for x in 0..8 {
            b[8 * y + x] = p.data[yy * p.width + (x0 + x).min(p.width - 1)];
        }
    }
    b
}

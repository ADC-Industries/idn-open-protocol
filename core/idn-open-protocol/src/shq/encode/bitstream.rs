//! The bit writer, the VLC tables, and writing blocks and macroblocks.

use super::*;

/// LSB-first bit writer (§3.2.1) with a 64-bit accumulator.
pub(super) struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl BitWriter {
    pub(super) fn new() -> Self {
        BitWriter {
            out: Vec::with_capacity(4096),
            acc: 0,
            n: 0,
        }
    }

    /// Appends `len` (≤ 32) bits; the first bit sent is bit 0 of `v`.
    #[inline]
    fn bits(&mut self, v: u32, len: u32) {
        debug_assert!(len <= 32 && (len == 32 || v >> len == 0));
        self.acc |= (v as u64) << self.n;
        self.n += len;
        // Flush 32 bits at a time (LSB-first bits = little-endian bytes).
        if self.n >= 32 {
            self.out.extend_from_slice(&(self.acc as u32).to_le_bytes());
            self.acc >>= 32;
            self.n -= 32;
        }
    }

    #[inline]
    pub(super) fn put(&mut self, b: bool) {
        self.bits(b as u32, 1);
    }

    /// A fixed-length field, least significant bit first.
    #[inline]
    fn field(&mut self, v: u32, n: u32) {
        self.bits(v & ((1u64 << n) - 1) as u32, n);
    }

    /// A precomputed VLC.
    #[inline]
    fn vlc(&mut self, c: Vlc) {
        self.bits(c.0, c.1);
    }

    /// A VLC given in transmission order as a '0'/'1' string (used for rare codes).
    pub(super) fn code(&mut self, c: &str) {
        self.vlc(vlc(c));
    }

    /// Pads with zero bits to the next byte (§3.3.2).
    pub(super) fn finish(mut self) -> Vec<u8> {
        // Up to 31 bits are pending; write them out, zero-padded to a byte.
        let bytes = self.n.div_ceil(8) as usize;
        self.out.extend_from_slice(&self.acc.to_le_bytes()[..bytes]);
        self.out
    }

    /// Bits written so far (for tests).
    #[cfg(test)]
    pub(super) fn bit_len(&self) -> usize {
        self.out.len() * 8 + self.n as usize
    }
}

/// A code as (bits in transmission order packed LSB-first, length).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Vlc(pub(super) u32, pub(super) u32);

const fn vlc_bytes(c: &[u8]) -> Vlc {
    let mut v = 0u32;
    let mut i = 0;
    while i < c.len() {
        if c[i] == b'1' {
            v |= 1 << i;
        }
        i += 1;
    }
    Vlc(v, c.len() as u32)
}

pub(super) fn vlc(c: &str) -> Vlc {
    vlc_bytes(c.as_bytes())
}

const EOB: Vlc = vlc_bytes(b"0110");
pub(super) const ESC: Vlc = vlc_bytes(b"000001");

const DC_LUMA: [Vlc; 12] = [
    vlc_bytes(b"100"),
    vlc_bytes(b"00"),
    vlc_bytes(b"01"),
    vlc_bytes(b"101"),
    vlc_bytes(b"110"),
    vlc_bytes(b"1110"),
    vlc_bytes(b"11110"),
    vlc_bytes(b"111110"),
    vlc_bytes(b"1111110"),
    vlc_bytes(b"11111110"),
    vlc_bytes(b"111111110"),
    vlc_bytes(b"111111111"),
];
const DC_CHROMA: [Vlc; 12] = [
    vlc_bytes(b"00"),
    vlc_bytes(b"01"),
    vlc_bytes(b"10"),
    vlc_bytes(b"110"),
    vlc_bytes(b"1110"),
    vlc_bytes(b"11110"),
    vlc_bytes(b"111110"),
    vlc_bytes(b"1111110"),
    vlc_bytes(b"11111110"),
    vlc_bytes(b"111111110"),
    vlc_bytes(b"1111111110"),
    vlc_bytes(b"1111111111"),
];
fn dc_size(diff: i32) -> u32 {
    if diff == 0 {
        0
    } else {
        32 - diff.unsigned_abs().leading_zeros()
    }
}

fn dc_bits(diff: i32, chroma: bool) -> u32 {
    let s = dc_size(diff);
    (if chroma {
        DC_CHROMA[s as usize]
    } else {
        DC_LUMA[s as usize]
    })
    .1 + s
}

/// Bits of a FULL block's AC symbols including EOB.
#[inline]
fn ac_bits(sym: &AcSymbols) -> u32 {
    sym.bits + EOB.1
}

/// A skip run of `n` MBs (§3.5.2): `00000` per 31, then the code for the rest (a run of 0
/// is `1`, so a row never ends in a bare `00000`).
pub(super) fn write_skiprun(w: &mut BitWriter, n: usize) {
    for _ in 0..n / 31 {
        w.code("00000");
    }
    let r = n % 31;
    if r == 0 {
        w.code("1");
        return;
    }
    let k = 31 - ((r + 1) as u32).leading_zeros(); // floor(log2(r + 1)), 1..=4
    for _ in 0..k {
        w.put(false);
    }
    w.put(true);
    w.field((r - ((1 << k) - 1)) as u32, k);
}

fn write_dc(w: &mut BitWriter, diff: i32, chroma: bool) {
    let s = if diff == 0 {
        0
    } else {
        32 - diff.unsigned_abs().leading_zeros()
    };
    debug_assert!(s <= 11, "DC difference out of range");
    w.vlc(if chroma {
        DC_CHROMA[s as usize]
    } else {
        DC_LUMA[s as usize]
    });
    if s > 0 {
        let v = if diff > 0 { diff } else { diff + (1 << s) - 1 };
        w.field(v as u32, s);
    }
}

fn write_ac(w: &mut BitWriter, sym: &AcSymbols, arena: &SymArena) {
    let (a, b) = (sym.start as usize, sym.start as usize + sym.n as usize);
    for &c in &arena.words[a..b] {
        w.bits(c & 0x00ff_ffff, c >> 24);
    }
    w.vlc(EOB);
}

/// Codes one FULL block: DC differential, then AC symbols and EOB.
fn write_full(w: &mut BitWriter, pred: &mut i32, c: &Coded, chroma: bool, arena: &SymArena) {
    // The decoder computes P = P − diff (§3.6.2).
    let diff = *pred - c.d;
    write_dc(w, diff, chroma);
    *pred = c.d;
    if c.has_ac {
        write_ac(w, &c.sym, arena);
    } else {
        w.vlc(EOB);
    }
}
/// Quantised colour and alpha blocks of MB (m, r), in bitstream order (§3.4.1).
/// Quantised blocks with their plane index.
pub(super) type Blocks = Vec<(usize, Coded)>;

/// Codes the colour and alpha blocks of MB (m, r) into `colour` and `alpha`, which are
/// reused across MBs (no allocation per MB).
#[allow(clippy::too_many_arguments)]
pub(super) fn mb_blocks(
    inp: &EncodeInput,
    m: usize,
    r: usize,
    q: &QTable,
    short_dc: bool,
    colour: &mut Blocks,
    alpha: &mut Blocks,
    arena: &mut SymArena,
    preds: &[i32; 4],
) {
    let (cl, al) = inp.fourcc.blocks();
    let (cmw, cmh) = inp.fourcc.chroma_mb();
    let origin = |pl: usize| {
        if pl == 1 || pl == 2 {
            (cmw * m, cmh * r)
        } else {
            (16 * m, 16 * r)
        }
    };
    arena.clear();
    colour.clear();
    alpha.clear();
    // A block wholly outside the picture is discarded by the decoder (§3.4.3): no DCT,
    // just a repeat of the predictor, which costs a couple of bits.
    let mut p = *preds;
    let mut one = |pl: usize, x: usize, y: usize, arena: &mut SymArena| {
        let plane = &inp.planes[pl];
        let c = if x >= plane.width || y >= plane.height {
            Coded {
                d: p[pl],
                sym: AcSymbols::EMPTY,
                has_ac: false,
            }
        } else {
            code_block(inp, pl, x, y, q, short_dc, arena)
        };
        p[pl] = c.d;
        (pl, c)
    };
    for &(pl, dx, dy) in cl {
        let (x0, y0) = origin(pl);
        colour.push(one(pl, x0 + dx, y0 + dy, arena));
    }
    for &(pl, dx, dy) in al {
        let (x0, y0) = origin(pl);
        alpha.push(one(pl, x0 + dx, y0 + dy, arena));
    }
}

/// Block kinds of one MB part (at most 12 blocks: 4:4:4 colour).
#[derive(Clone, Copy)]
struct KindList {
    k: [Kind; 12],
    n: usize,
}

impl std::ops::Deref for KindList {
    type Target = [Kind];
    fn deref(&self) -> &[Kind] {
        &self.k[..self.n]
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Full,
    DcOnly,
    Repeat,
}

/// Block kinds given the predictors at the start of the MB, and the predictors after it.
fn kinds(blocks: &[(usize, Coded)], preds: &[i32; 4]) -> (KindList, [i32; 4]) {
    let mut p = *preds;
    let mut out = KindList {
        k: [Kind::Full; 12],
        n: 0,
    };
    for (pl, c) in blocks {
        out.k[out.n] = if c.has_ac {
            Kind::Full
        } else if c.d == p[*pl] {
            Kind::Repeat
        } else {
            Kind::DcOnly
        };
        out.n += 1;
        p[*pl] = c.d;
    }
    (out, p)
}

/// Writes one coded MB in the new format (§3.5.3–§3.5.4), choosing the cheaper mode.
pub(super) fn write_mb_compact(
    w: &mut BitWriter,
    preds: &mut [i32; 4],
    colour: &[(usize, Coded)],
    alpha: &[(usize, Coded)],
    arena: &SymArena,
) {
    let (ck, after_c) = kinds(colour, preds);
    let (ak, after_a) = kinds(alpha, &after_c);
    let alpha_repeat = !alpha.is_empty() && ak.iter().all(|&k| k == Kind::Repeat);
    // Bit cost of each block written as FULL (mode 1) and with its best kind (mode 0).
    let mut cost = [1u32, 1u32]; // [mode 1, mode 0]: the mode bit
    let mut p = *preds;
    let mut tally = |blocks: &[(usize, Coded)], ks: &[Kind], p: &mut [i32; 4]| {
        for ((pl, c), k) in blocks.iter().zip(ks) {
            let chroma = *pl == 1 || *pl == 2;
            let full =
                dc_bits(p[*pl] - c.d, chroma) + if c.has_ac { ac_bits(&c.sym) } else { EOB.1 };
            cost[0] += full;
            cost[1] += match k {
                Kind::Full => 1 + full,
                Kind::Repeat => 2,
                Kind::DcOnly => 2 + dc_bits(p[*pl] - c.d, chroma),
            };
            p[*pl] = c.d;
        }
    };
    tally(colour, &ck, &mut p);
    if !alpha_repeat {
        tally(alpha, &ak, &mut p);
    }
    let mode1 = cost[0] <= cost[1];
    w.put(mode1);
    let emit = |w: &mut BitWriter, blocks: &[(usize, Coded)], ks: &[Kind], preds: &mut [i32; 4]| {
        for ((pl, c), k) in blocks.iter().zip(ks) {
            let chroma = *pl == 1 || *pl == 2;
            match (mode1, k) {
                (true, _) | (false, Kind::Full) => {
                    if !mode1 {
                        w.put(false); // PREFIX FULL
                    }
                    write_full(w, &mut preds[*pl], c, chroma, arena);
                }
                (false, Kind::Repeat) => w.code("10"),
                (false, Kind::DcOnly) => {
                    w.code("11");
                    write_dc(w, preds[*pl] - c.d, chroma);
                    preds[*pl] = c.d;
                }
            }
        }
    };
    emit(w, colour, &ck, preds);
    if !alpha.is_empty() {
        w.put(alpha_repeat); // alpha switch (§3.5.4)
        if !alpha_repeat {
            emit(w, alpha, &ak, preds);
        }
    }
    debug_assert!(*preds == after_a || alpha_repeat);
}

#[allow(clippy::too_many_arguments)]
pub(super) fn write_mb(
    w: &mut BitWriter,
    inp: &EncodeInput,
    preds: &mut [i32; 4],
    m: usize,
    r: usize,
    q: &QTable,
    new_format: bool,
    arena: &mut SymArena,
) {
    let (colour, alpha) = inp.fourcc.blocks();
    let (cmw, cmh) = inp.fourcc.chroma_mb();
    let origin = |pl: usize| {
        if pl == 1 || pl == 2 {
            (cmw * m, cmh * r)
        } else {
            (16 * m, 16 * r)
        }
    };
    let mut one =
        |w: &mut BitWriter, preds: &mut [i32; 4], &(pl, dx, dy): &(usize, usize, usize)| {
            let (x0, y0) = origin(pl);
            arena.clear();
            let c = if new_format {
                code_block(inp, pl, x0 + dx, y0 + dy, q, false, arena)
            } else {
                quantise(&block(&inp.planes[pl], x0 + dx, y0 + dy), q, false, arena)
            };
            write_full(w, &mut preds[pl], &c, pl == 1 || pl == 2, arena);
        };
    for b in colour {
        one(w, preds, b);
    }
    if !alpha.is_empty() {
        if new_format {
            w.put(false); // alpha switch: blocks follow (§3.5.4)
        }
        for b in alpha {
            one(w, preds, b);
        }
    }
}

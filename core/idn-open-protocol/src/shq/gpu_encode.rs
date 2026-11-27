//! SpeedHQ encoding on the application's GPU (new format, §3.3–§3.10), for pictures that
//! are already there (§E.27): the same bitstream as [`super::encode::encode`] in compact
//! mode, without the picture coming to this memory and without the CPU's cores.
//!
//! Two passes. *Blocks*, one workgroup per 8×8 block, an invocation per sample:
//! gathered (edges repeated, as the CPU does), transformed (the exact separable DCT),
//! quantised (the same tables and rounding), DC chosen by the same rules (flat blocks,
//! short DC, DC-only rounding), the AC levels kept in zigzag order with their code
//! length. *Rows*, one workgroup per macroblock row (rows are independent, §3.3.2), an
//! invocation per macroblock: each one's kind (skipped, REPEAT / DCONLY / FULL and the
//! cheaper mode) and size, prefix sums for the skip runs and the bit offsets, then every
//! macroblock's variable-length codes written LSB-first at its offset in the row's
//! region. The CPU reads back the rows' lengths, then only the bits, and frames them as
//! the CPU encoder does.
//!
//! 4:2:0 and 4:2:2 (SHQ0, SHQ2), 8-bit; anything else, or a row past its region (noise
//! at very high quality), is `None`: the CPU encodes it.

use super::Fourcc;
use super::ac_table::AC_CODES;
use super::encode::EncodeOptions;
use super::vlc::{INTRA_QUANT, ZIGZAG};
use crate::gpu::{GpuHandIn, GpuPicture};

/// Words of a block's record: flags, DC, AC code bits, 32 words of 63 levels (i16 pairs).
const RECORD: u32 = 35;

const SHADER: &str = r#"
// pr: 0 cols, 1 rows, 2 blocks per MB, 3 chroma MB width, 4 chroma MB height, 5 short DC,
// 6 words per row region, 7 -, 8..20 planes (offset, row bytes, width, height) Y Cb Cr,
// 20..44 the MB's blocks (plane, dx, dy).
@group(0) @binding(0) var<storage, read> pic: array<u32>;
@group(0) @binding(1) var<storage, read_write> blk: array<u32>;
@group(0) @binding(2) var<storage, read> pr: array<u32>;
// tf: 0..64 DCT basis b[u][x], 64..128 quantiser factor, 128..192 level limit (raster).
@group(0) @binding(3) var<storage, read> tf: array<f32>;
// tu: 0..64 zigzag, 64..64+64*41 AC codes (code | len << 24, sign not included),
// then DC luma (12), DC chroma (12).
@group(0) @binding(4) var<storage, read> tu: array<u32>;
@group(0) @binding(5) var<storage, read_write> rows_out: array<atomic<u32>>;
@group(0) @binding(6) var<storage, read_write> lens: array<u32>;
// Per macroblock: its predictors after it (Y, Cb, Cr), its size in bits or SKIPPED.
@group(0) @binding(7) var<storage, read_write> mbs: array<i32>;

const REC: u32 = 35u;
const AC0: u32 = 64u;
const DCL: u32 = 2688u;  // 64 + 64 * 41
const DCC: u32 = 2700u;

fn sample(off: u32, row: u32, x: u32, y: u32) -> f32 {
    let i = off + y * row + x;
    return f32((pic[i >> 2u] >> ((i & 3u) * 8u)) & 255u);
}

fn ac_len(run: u32, a: u32) -> u32 {
    if (a <= 40u) {
        let e = tu[AC0 + run * 41u + a];
        if (e != 0u) { return (e >> 24u) + 1u; }
    }
    return 24u;
}

// One workgroup per block, one invocation per sample (then per coefficient): the same
// sums in the same order as one invocation per block did, without its three 64-entry
// arrays spilling to memory (10 ms of a 2160p picture on a Radeon 680M).
var<workgroup> ws: array<f32, 64>;
var<workgroup> wt: array<f32, 64>;
var<workgroup> wf: array<f32, 64>;
var<workgroup> wl: array<i32, 64>;
var<workgroup> w_notflat: atomic<u32>;
var<workgroup> w_any: atomic<u32>;
var<workgroup> w_bits: atomic<u32>;

@compute @workgroup_size(64, 1, 1)
fn blocks(@builtin(workgroup_id) wg: vec3<u32>, @builtin(local_invocation_index) j: u32) {
    let cols = pr[0];
    let nb = pr[2];
    let idx = wg.x + wg.y * 65535u;
    if (idx >= cols * pr[1] * nb) { return; }
    let mb = idx / nb;
    let b = idx % nb;
    let r = mb / cols;
    let m = mb % cols;
    let pl = pr[20u + 3u * b];
    var ox = 16u * m;
    var oy = 16u * r;
    if (pl != 0u) { ox = pr[3] * m; oy = pr[4] * r; }
    let x0 = ox + pr[21u + 3u * b];
    let y0 = oy + pr[22u + 3u * b];
    let off = pr[8u + 4u * pl];
    let row = pr[9u + 4u * pl];
    let pw = pr[10u + 4u * pl];
    let ph = pr[11u + 4u * pl];
    let base = idx * REC;
    if (x0 >= pw || y0 >= ph) {
        if (j == 0u) { blk[base] = 2u; } // outside the picture: the predictor repeated
        return;
    }
    let x = j & 7u;
    let y = j >> 3u;
    let v = sample(off, row, min(x0 + x, pw - 1u), min(y0 + y, ph - 1u));
    ws[j] = v;
    if (j == 0u) {
        atomicStore(&w_notflat, 0u);
        atomicStore(&w_any, 0u);
        atomicStore(&w_bits, 0u);
    }
    workgroupBarrier();
    if (v != ws[0]) { atomicOr(&w_notflat, 1u); }
    // The separable DCT, raster 8·v + u: this invocation's t[y][u = x], then f[v = y][u = x].
    var acc = 0.0;
    for (var k = 0u; k < 8u; k = k + 1u) {
        acc = acc + tf[8u * x + k] * ws[8u * y + k];
    }
    wt[j] = acc;
    workgroupBarrier();
    acc = 0.0;
    for (var k = 0u; k < 8u; k = k + 1u) {
        acc = acc + tf[8u * y + k] * wt[8u * k + x];
    }
    wf[j] = acc;
    workgroupBarrier();
    // This invocation's level: scan position j (1..63).
    var c = 0;
    if (j >= 1u) {
        let k = tu[j];
        let xq = wf[k] * tf[64u + k];
        let mag = min(abs(xq) + 0.5, tf[128u + k]);
        c = i32(mag);
        if (xq < 0.0) { c = -c; }
    }
    wl[j] = c;
    workgroupBarrier();
    if (c != 0) {
        // Its run: the zeros just before it (from position 1).
        var run = 0u;
        var i = j - 1u;
        loop {
            if (i == 0u || wl[i] != 0) { break; }
            run = run + 1u;
            i = i - 1u;
        }
        atomicAdd(&w_bits, ac_len(run, u32(abs(c))));
        atomicOr(&w_any, 1u);
    }
    workgroupBarrier();
    let short = pr[5] != 0u;
    if (atomicLoad(&w_notflat) == 0u) {
        if (j == 0u) {
            var d = 8u * u32(ws[0]);
            if (short) { d = u32(ws[0]); }
            blk[base] = 0u;
            blk[base + 1u] = d;
            blk[base + 2u] = 0u;
        }
        return;
    }
    // Levels in zigzag order, two to a word (position 0's half unused).
    if (j < 32u) {
        blk[base + 3u + j] = (u32(wl[2u * j]) & 0xffffu) | (u32(wl[2u * j + 1u]) << 16u);
    }
    if (j == 0u) {
        let any_ac = atomicLoad(&w_any) != 0u;
        let dc = wf[0];
        var d: i32;
        if (short) {
            d = i32(clamp(floor(dc / 8.0 + 0.5), 0.0, 255.0));
        } else if (any_ac) {
            d = i32(clamp(floor(dc + 0.5), 0.0, 2047.0));
        } else {
            d = 8 * i32(clamp(floor(dc / 8.0 + 0.5), 0.0, 255.0));
        }
        blk[base] = select(0u, 1u, any_ac);
        blk[base + 1u] = u32(d);
        blk[base + 2u] = atomicLoad(&w_bits);
    }
}

// ---- rows ------------------------------------------------------------------------------

// One workgroup per macroblock row, each invocation a run of its macroblocks. A
// macroblock's DC values don't depend on the ones before it (the first block of each
// plane is in the picture), so each one's kind, mode and size come first; then the skip
// runs and every macroblock's bit offset by prefix sums over the invocations; then all
// write their codes at once, into words OR-ed together where two meet.

struct BW { acc: u32, n: u32, w: u32, base: u32 };

fn emit_word(bw: ptr<function, BW>, v: u32) {
    if (v != 0u) { atomicOr(&rows_out[(*bw).base + (*bw).w], v); }
    (*bw).w = (*bw).w + 1u;
}

// `len` (≤ 32) bits of `v`, the first sent in bit 0.
fn put(bw: ptr<function, BW>, v: u32, len: u32) {
    if (len == 0u) { return; }
    let n = (*bw).n;
    (*bw).acc = (*bw).acc | (v << n);
    let t = n + len;
    if (t >= 32u) {
        emit_word(bw, (*bw).acc);
        (*bw).acc = select(v >> (32u - n), 0u, n == 0u);
        (*bw).n = t - 32u;
    } else {
        (*bw).n = t;
    }
}

fn put_code(bw: ptr<function, BW>, e: u32) {
    put(bw, e & 0xffffffu, e >> 24u);
}

fn dc_size(diff: i32) -> u32 {
    if (diff == 0) { return 0u; }
    return 32u - countLeadingZeros(u32(abs(diff)));
}

fn dc_entry(s: u32, chroma: bool) -> u32 {
    return select(tu[DCL + s], tu[DCC + s], chroma);
}

fn dc_bits(diff: i32, chroma: bool) -> u32 {
    let s = dc_size(diff);
    return (dc_entry(s, chroma) >> 24u) + s;
}

fn write_dc(bw: ptr<function, BW>, diff: i32, chroma: bool) {
    let s = dc_size(diff);
    put_code(bw, dc_entry(s, chroma));
    if (s > 0u) {
        var v = diff;
        if (diff <= 0) { v = diff + (1 << s) - 1; }
        put(bw, u32(v) & ((1u << s) - 1u), s);
    }
}

fn skiprun_bits(n: u32) -> u32 {
    let r = n % 31u;
    var k = 0u;
    if (r != 0u) { k = 31u - countLeadingZeros(r + 1u); }
    return 5u * (n / 31u) + 2u * k + 1u;
}

fn write_skiprun(bw: ptr<function, BW>, n: u32) {
    for (var i = 0u; i < n / 31u; i = i + 1u) { put(bw, 0u, 5u); }
    let r = n % 31u;
    if (r == 0u) { put(bw, 1u, 1u); return; }
    let k = 31u - countLeadingZeros(r + 1u);
    put(bw, 0u, k);
    put(bw, 1u, 1u);
    put(bw, r - ((1u << k) - 1u), k);
}

// A block's AC codes then EOB (`0110`, sent first bit first: 6, 4 bits).
fn write_ac(bw: ptr<function, BW>, base: u32) {
    var run = 0u;
    for (var i = 1u; i < 64u; i = i + 1u) {
        let word = blk[base + 3u + i / 2u];
        let half = select(word & 0xffffu, word >> 16u, (i & 1u) == 1u);
        let l = (i32(half << 16u)) >> 16u;
        if (l == 0) { run = run + 1u; continue; }
        let a = u32(abs(l));
        var e = 0u;
        if (a <= 40u) { e = tu[AC0 + run * 41u + a]; }
        if (e != 0u) {
            let len = e >> 24u;
            put(bw, (e & 0xffffffu) | (select(0u, 1u, l < 0) << len), len + 1u);
        } else {
            // ESC (`000001`: 32, 6 bits), the run, the level + 2048 in 12 bits.
            put(bw, 32u | (run << 6u) | (u32(l + 2048) << 12u), 24u);
        }
        run = 0u;
    }
    put(bw, 6u, 4u);
}

// A macroblock's blocks: DC (outside the picture: the predictor so far, from `p`) and
// whether each has AC; `p` ends as the predictors after it.
struct Mb { d: array<i32, 8>, ac: array<bool, 8> };

fn mb_blocks(first: u32, p: ptr<function, array<i32, 4>>) -> Mb {
    var o: Mb;
    for (var b = 0u; b < pr[2]; b = b + 1u) {
        let pl = pr[20u + 3u * b];
        let base = (first + b) * REC;
        let flags = blk[base];
        if ((flags & 2u) != 0u) {
            o.d[b] = (*p)[pl];
            o.ac[b] = false;
        } else {
            o.d[b] = i32(blk[base + 1u]);
            o.ac[b] = (flags & 1u) != 0u;
        }
        (*p)[pl] = o.d[b];
    }
    return o;
}

const SKIPPED: u32 = 0x80000000u;
const RT: u32 = 256u;
var<workgroup> w_last: array<i32, 256>;
var<workgroup> w_sum: array<u32, 256>;
var<workgroup> w_total: u32;

@compute @workgroup_size(256, 1, 1)
fn rows(@builtin(workgroup_id) wg: vec3<u32>, @builtin(local_invocation_index) t: u32) {
    let cols = pr[0];
    let r = wg.x;
    let nb = pr[2];
    let per = (cols + RT - 1u) / RT;
    let m0 = min(t * per, cols);
    let m1 = min(m0 + per, cols);
    let init = select(1024, 128, pr[5] != 0u);
    // Each macroblock's predictors after it.
    for (var m = m0; m < m1; m = m + 1u) {
        var p = array<i32, 4>(init, init, init, init);
        _ = mb_blocks((r * cols + m) * nb, &p);
        let at = 4u * (r * cols + m);
        mbs[at] = p[0];
        mbs[at + 1u] = p[1];
        mbs[at + 2u] = p[2];
    }
    storageBarrier();
    // Each macroblock skipped (nothing new), or its size in bits (without its skip run)
    // in the cheaper mode: 1 (every block FULL) or 0 (each its own kind).
    var last = -1;
    for (var m = m0; m < m1; m = m + 1u) {
        let at = 4u * (r * cols + m);
        var preds = array<i32, 4>(init, init, init, init);
        if (m > 0u) {
            preds = array<i32, 4>(mbs[at - 4u], mbs[at - 3u], mbs[at - 2u], init);
        }
        var p = preds;
        let first = (r * cols + m) * nb;
        let o = mb_blocks(first, &p);
        var skip = true;
        for (var b = 0u; b < nb; b = b + 1u) {
            let pl = pr[20u + 3u * b];
            if (o.ac[b] || o.d[b] != preds[pl]) { skip = false; }
        }
        if (skip) {
            mbs[at + 3u] = i32(SKIPPED);
            continue;
        }
        var cost1 = 1u;
        var cost0 = 1u;
        p = preds;
        for (var b = 0u; b < nb; b = b + 1u) {
            let pl = pr[20u + 3u * b];
            let base = (first + b) * REC;
            let dcb = dc_bits(p[pl] - o.d[b], pl != 0u);
            var full = dcb + 4u;
            if (o.ac[b]) { full = dcb + blk[base + 2u] + 4u; }
            cost1 = cost1 + full;
            if (o.ac[b]) {
                cost0 = cost0 + 1u + full;
            } else if (o.d[b] == p[pl]) {
                cost0 = cost0 + 2u;
            } else {
                cost0 = cost0 + 2u + dcb;
            }
            p[pl] = o.d[b];
        }
        mbs[at + 3u] = i32(min(cost1, cost0));
        last = i32(m);
    }
    // The last macroblock before each invocation's run that isn't skipped (a max scan).
    w_last[t] = last;
    workgroupBarrier();
    for (var s = 1u; s < RT; s = s * 2u) {
        var v = w_last[t];
        if (t >= s) { v = max(v, w_last[t - s]); }
        workgroupBarrier();
        w_last[t] = v;
        workgroupBarrier();
    }
    var prev = -1;
    if (t > 0u) { prev = w_last[t - 1u]; }
    let last_all = w_last[RT - 1u];
    // This run's bits (skip runs included), then the offsets (a sum scan).
    var sum = 0u;
    var q = prev;
    for (var m = m0; m < m1; m = m + 1u) {
        let info = u32(mbs[4u * (r * cols + m) + 3u]);
        if (info == SKIPPED) { continue; }
        sum = sum + skiprun_bits(u32(i32(m) - q - 1)) + info;
        q = i32(m);
    }
    w_sum[t] = sum;
    workgroupBarrier();
    for (var s = 1u; s < RT; s = s * 2u) {
        var v = w_sum[t];
        if (t >= s) { v = v + w_sum[t - s]; }
        workgroupBarrier();
        w_sum[t] = v;
        workgroupBarrier();
    }
    let start = w_sum[t] - sum;
    // The skip run at the end of the row.
    let tail = cols - u32(last_all + 1);
    if (t == 0u) {
        var bits = w_sum[RT - 1u];
        if (tail > 0u) { bits = bits + skiprun_bits(tail); }
        w_total = bits;
    }
    let total = workgroupUniformLoad(&w_total);
    let words = (total + 31u) / 32u;
    let over = words > pr[6];
    if (t == 0u) {
        lens[r] = (total + 7u) / 8u | (select(0u, 1u, over) << 31u);
    }
    if (over) { return; }
    let base = r * pr[6];
    for (var i = t; i < words; i = i + RT) { atomicStore(&rows_out[base + i], 0u); }
    storageBarrier();
    var bw: BW;
    bw.base = base;
    bw.w = start / 32u;
    bw.n = start % 32u;
    bw.acc = 0u;
    q = prev;
    for (var m = m0; m < m1; m = m + 1u) {
        let at = 4u * (r * cols + m);
        if (u32(mbs[at + 3u]) == SKIPPED) { continue; }
        var preds = array<i32, 4>(init, init, init, init);
        if (m > 0u) {
            preds = array<i32, 4>(mbs[at - 4u], mbs[at - 3u], mbs[at - 2u], init);
        }
        var p = preds;
        let first = (r * cols + m) * nb;
        let o = mb_blocks(first, &p);
        write_skiprun(&bw, u32(i32(m) - q - 1));
        q = i32(m);
        // The mode again, as above.
        var cost1 = 1u;
        var cost0 = 1u;
        p = preds;
        for (var b = 0u; b < nb; b = b + 1u) {
            let pl = pr[20u + 3u * b];
            let base = (first + b) * REC;
            let dcb = dc_bits(p[pl] - o.d[b], pl != 0u);
            var full = dcb + 4u;
            if (o.ac[b]) { full = dcb + blk[base + 2u] + 4u; }
            cost1 = cost1 + full;
            if (o.ac[b]) {
                cost0 = cost0 + 1u + full;
            } else if (o.d[b] == p[pl]) {
                cost0 = cost0 + 2u;
            } else {
                cost0 = cost0 + 2u + dcb;
            }
            p[pl] = o.d[b];
        }
        let mode1 = cost1 <= cost0;
        put(&bw, select(0u, 1u, mode1), 1u);
        for (var b = 0u; b < nb; b = b + 1u) {
            let pl = pr[20u + 3u * b];
            let chroma = pl != 0u;
            let base = (first + b) * REC;
            if (mode1 || o.ac[b]) {
                if (!mode1) { put(&bw, 0u, 1u); }
                write_dc(&bw, preds[pl] - o.d[b], chroma);
                preds[pl] = o.d[b];
                if (o.ac[b]) { write_ac(&bw, base); } else { put(&bw, 6u, 4u); }
            } else if (o.d[b] == preds[pl]) {
                put(&bw, 1u, 2u); // REPEAT `10`
            } else {
                put(&bw, 3u, 2u); // DCONLY `11`
                write_dc(&bw, preds[pl] - o.d[b], chroma);
                preds[pl] = o.d[b];
            }
        }
    }
    if (t == 0u && tail > 0u) {
        let end = w_sum[RT - 1u];
        var tb: BW;
        tb.base = base;
        tb.w = end / 32u;
        tb.n = end % 32u;
        tb.acc = 0u;
        write_skiprun(&tb, tail);
        if (tb.n > 0u) { emit_word(&tb, tb.acc); }
    }
    if (bw.n > 0u) { emit_word(&bw, bw.acc); }
}
"#;

/// A code in transmission order ('0'/'1') packed LSB-first, with its length.
fn vlc(c: &str) -> (u32, u32) {
    let v = c
        .bytes()
        .enumerate()
        .fold(0u32, |v, (i, b)| v | (u32::from(b == b'1') << i));
    (v, c.len() as u32)
}

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

/// Buffers sized for one picture size and layout.
struct Sized {
    key: (usize, usize, Fourcc),
    blk: wgpu::Buffer,
    rows_out: wgpu::Buffer,
    mbs: wgpu::Buffer,
    lens: wgpu::Buffer,
    lens_read: wgpu::Buffer,
    out_read: wgpu::Buffer,
    cap_words: u32,
}

/// The encoder on one device; not shared between threads (callers hold it under a lock).
pub struct GpuShqEncoder {
    hand: GpuHandIn,
    layout: wgpu::BindGroupLayout,
    blocks: wgpu::ComputePipeline,
    rows: wgpu::ComputePipeline,
    params: wgpu::Buffer,
    tf: wgpu::Buffer,
    tu: wgpu::Buffer,
    quality: Option<u8>,
    /// Buffers for the sizes encoded lately, the newest last: a picture and its proxy
    /// each frame would else have made them anew twice a frame.
    sized: Vec<Sized>,
}

// SAFETY: wgpu's types are Send and Sync (checked in crate::gpu).
unsafe impl Send for GpuShqEncoder {}

fn wait(hand: &GpuHandIn, sub: wgpu::SubmissionIndex) -> Option<()> {
    hand.device
        .poll(wgpu::PollType::Wait {
            submission_index: Some(sub),
            timeout: Some(std::time::Duration::from_secs(2)),
        })
        .ok()
        .map(|_| ())
}

fn map_read(hand: &GpuHandIn, b: &wgpu::Buffer, len: u64) -> Option<Vec<u8>> {
    let slice = b.slice(..len);
    crate::gpu::map_read(&hand.device, slice, None).ok()?;
    let v = slice.get_mapped_range().ok().map(|r| r.to_vec());
    b.unmap();
    v
}

impl GpuShqEncoder {
    /// The encoder on `hand`'s device, or why the device can't run it (e.g. Windows' FXC
    /// couldn't compile its shader, 2026-11-05): its errors caught here, so the device's
    /// other users (a switcher's compositor) go on and SpeedHQ is encoded on the CPU.
    pub fn new(hand: &GpuHandIn) -> Result<GpuShqEncoder, String> {
        let validation = hand.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let internal = hand.device.push_error_scope(wgpu::ErrorFilter::Internal);
        let e = Self::make(hand);
        let errors = [
            crate::gpu::block_on(internal.pop()),
            crate::gpu::block_on(validation.pop()),
        ];
        match errors.into_iter().flatten().next() {
            Some(err) => Err(err.to_string()),
            None => Ok(e),
        }
    }

    fn make(hand: &GpuHandIn) -> GpuShqEncoder {
        let d = &hand.device;
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("SpeedHQ encode"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let entry = |binding, read_only| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("SpeedHQ encode"),
            entries: &[
                entry(0, true),
                entry(1, false),
                entry(2, true),
                entry(3, true),
                entry(4, true),
                entry(5, false),
                entry(6, false),
                entry(7, false),
            ],
        });
        let pl = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("SpeedHQ encode"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = |e: &str| {
            d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("SpeedHQ encode"),
                layout: Some(&pl),
                module: &module,
                entry_point: Some(e),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let storage = |size: u64, label| {
            d.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        // The code tables (they don't change).
        let mut tu = vec![0u32; 64 + 64 * 41 + 24];
        for (i, &z) in ZIGZAG.iter().enumerate() {
            tu[i] = u32::from(z);
        }
        for &(r, l, c) in AC_CODES.iter() {
            let (v, n) = vlc(c);
            tu[64 + r as usize * 41 + l as usize] = v | (n << 24);
        }
        for (i, c) in DC_LUMA.iter().enumerate() {
            let (v, n) = vlc(c);
            tu[64 + 64 * 41 + i] = v | (n << 24);
        }
        for (i, c) in DC_CHROMA.iter().enumerate() {
            let (v, n) = vlc(c);
            tu[64 + 64 * 41 + 12 + i] = v | (n << 24);
        }
        let tu_buf = storage((tu.len() * 4) as u64, "SpeedHQ codes");
        hand.queue.write_buffer(
            &tu_buf,
            0,
            &tu.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>(),
        );
        GpuShqEncoder {
            hand: hand.clone(),
            blocks: pipeline("blocks"),
            rows: pipeline("rows"),
            layout,
            params: storage(64 * 4, "SpeedHQ params"),
            tf: storage(192 * 4, "SpeedHQ quantiser"),
            tu: tu_buf,
            quality: None,
            sized: Vec::new(),
        }
    }

    /// The basis and the quantiser for quality `q`.
    fn tables(&mut self, q: u8) {
        if self.quality == Some(q) {
            return;
        }
        let mut tf = vec![0f32; 192];
        for u in 0..8 {
            for x in 0..8 {
                let c = if u == 0 {
                    std::f64::consts::FRAC_1_SQRT_2
                } else {
                    1.0
                };
                tf[8 * u + x] = (c / 2.0
                    * ((2 * x + 1) as f64 * u as f64 * std::f64::consts::PI / 16.0).cos())
                    as f32;
            }
        }
        let s_q = 100 - i32::from(q);
        for k in 1..64 {
            let m = i32::from(INTRA_QUANT[k]) * s_q;
            tf[64 + k] = 16.0 / m as f32;
            tf[128 + k] = (32767 / m).min(2047) as f32;
        }
        let bytes: Vec<u8> = tf.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.hand.queue.write_buffer(&self.tf, 0, &bytes);
        self.quality = Some(q);
    }

    /// `pic` (planes Y, Cb, Cr laid out as [`GpuPicture`] says) encoded as `fourcc` at
    /// `width` × `height` with `opt` (new format, compact): the same bytes the CPU
    /// encoder's layout has. `None` where this can't (another format, a row past its
    /// region, a GPU error): encode on the CPU.
    pub fn encode(
        &mut self,
        pic: &GpuPicture,
        fourcc: Fourcc,
        width: usize,
        height: usize,
        opt: &EncodeOptions,
    ) -> Option<Vec<u8>> {
        if !opt.new_format || !opt.compact || opt.high_precision || pic.planes.len() < 3 {
            return None;
        }
        let (nb, cmw, cmh, digit): (u32, u32, u32, u8) = match fourcc {
            Fourcc::Shq0 => (6, 8, 8, 0),
            Fourcc::Shq2 => (8, 8, 16, 2),
            _ => return None,
        };
        let q = opt.quality.clamp(1, 99);
        let short = opt.short_dc && q <= 86;
        self.tables(q);
        let (cols, rows) = (width.div_ceil(16) as u32, height.div_ceil(16) as u32);
        let key = (width, height, fourcc);
        if let Some(k) = self.sized.iter().position(|s| s.key == key) {
            let s = self.sized.remove(k);
            self.sized.push(s);
        } else {
            if self.sized.len() >= 3 {
                self.sized.remove(0);
            }
            let d = &self.hand.device;
            let blocks = u64::from(cols * rows * nb);
            // A row's region: its raw samples and a quarter more, and some to spare.
            let cap_words =
                ((16 * width * if digit == 0 { 3 } else { 4 } / 2) * 5 / 4 / 4 + 1024) as u32;
            let buf = |size: u64, usage, label| {
                d.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(label),
                    size,
                    usage,
                    mapped_at_creation: false,
                })
            };
            use wgpu::BufferUsages as U;
            self.sized.push(Sized {
                key: (width, height, fourcc),
                blk: buf(blocks * u64::from(RECORD) * 4, U::STORAGE, "SpeedHQ blocks"),
                rows_out: buf(
                    u64::from(rows) * u64::from(cap_words) * 4,
                    U::STORAGE | U::COPY_SRC,
                    "SpeedHQ rows",
                ),
                mbs: buf(
                    u64::from(cols * rows) * 16,
                    U::STORAGE,
                    "SpeedHQ macroblocks",
                ),
                lens: buf(
                    u64::from(rows).max(1) * 4,
                    U::STORAGE | U::COPY_SRC,
                    "SpeedHQ lengths",
                ),
                lens_read: buf(
                    u64::from(rows).max(1) * 4,
                    U::MAP_READ | U::COPY_DST,
                    "SpeedHQ lengths read",
                ),
                out_read: buf(
                    u64::from(rows) * u64::from(cap_words) * 4,
                    U::MAP_READ | U::COPY_DST,
                    "SpeedHQ out read",
                ),
                cap_words,
            });
        }
        let s = self.sized.last().expect("made above");
        let mut pr = [0u32; 64];
        pr[..7].copy_from_slice(&[cols, rows, nb, cmw, cmh, u32::from(short), s.cap_words]);
        for (k, p) in pic.planes.iter().take(3).enumerate() {
            pr[8 + 4 * k..12 + 4 * k].copy_from_slice(&[
                p.offset as u32,
                p.row_bytes,
                p.width,
                p.height,
            ]);
        }
        const Y: [(u32, u32, u32); 4] = [(0, 0, 0), (0, 8, 0), (0, 0, 8), (0, 8, 8)];
        let order: Vec<(u32, u32, u32)> = if digit == 0 {
            vec![Y[0], Y[1], Y[2], Y[3], (1, 0, 0), (2, 0, 0)]
        } else {
            vec![
                Y[0],
                Y[1],
                Y[2],
                Y[3],
                (1, 0, 0),
                (2, 0, 0),
                (1, 0, 8),
                (2, 0, 8),
            ]
        };
        for (b, &(pl, dx, dy)) in order.iter().enumerate() {
            pr[20 + 3 * b..23 + 3 * b].copy_from_slice(&[pl, dx, dy]);
        }
        let q_ = &self.hand.queue;
        q_.write_buffer(
            &self.params,
            0,
            &pr.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>(),
        );
        fn entry(binding: u32, b: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
            wgpu::BindGroupEntry {
                binding,
                resource: b.as_entire_binding(),
            }
        }
        let group = self
            .hand
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.layout,
                entries: &[
                    entry(0, &pic.buffer),
                    entry(1, &s.blk),
                    entry(2, &self.params),
                    entry(3, &self.tf),
                    entry(4, &self.tu),
                    entry(5, &s.rows_out),
                    entry(6, &s.lens),
                    entry(7, &s.mbs),
                ],
            });
        let n_blocks = cols * rows * nb;
        let mut enc = self.hand.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_bind_group(0, &group, &[]);
            pass.set_pipeline(&self.blocks);
            pass.dispatch_workgroups(n_blocks.min(65535), n_blocks.div_ceil(65535), 1);
        }
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_bind_group(0, &group, &[]);
            pass.set_pipeline(&self.rows);
            pass.dispatch_workgroups(rows, 1, 1);
        }
        enc.copy_buffer_to_buffer(&s.lens, 0, &s.lens_read, 0, u64::from(rows) * 4);
        wait(&self.hand, q_.submit([enc.finish()]))?;
        let lens: Vec<u32> = map_read(&self.hand, &s.lens_read, u64::from(rows) * 4)?
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect();
        if lens.iter().any(|l| l >> 31 != 0) {
            return None;
        }
        // Each row's used words, one after another.
        let mut enc = self.hand.device.create_command_encoder(&Default::default());
        let mut at = 0u64;
        let mut where_ = Vec::with_capacity(lens.len());
        for (r, &l) in lens.iter().enumerate() {
            let words = u64::from(l.div_ceil(4));
            if words > 0 {
                enc.copy_buffer_to_buffer(
                    &s.rows_out,
                    r as u64 * u64::from(s.cap_words) * 4,
                    &s.out_read,
                    at,
                    words * 4,
                );
            }
            where_.push(at);
            at += words * 4;
        }
        wait(&self.hand, q_.submit([enc.finish()]))?;
        let data = if at > 0 {
            map_read(&self.hand, &s.out_read, at)?
        } else {
            Vec::new()
        };
        let mut out = vec![100 + q];
        out.extend_from_slice(&(width as u16).to_le_bytes());
        out.extend_from_slice(&(height as u16).to_le_bytes());
        out.push(digit << 4);
        out.push(if short { 3 } else { 0 });
        out.push(0);
        out.reserve(data.len() + 3 * lens.len() + 4);
        for (&l, &w) in lens.iter().zip(&where_) {
            let size = 3 + l as usize;
            out.extend_from_slice(&(size as u32).to_le_bytes()[..3]);
            out.extend_from_slice(&data[w as usize..w as usize + l as usize]);
        }
        out.extend_from_slice(&[0; 4]);
        Some(out)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::gpu::block_on;
    use crate::gpu::slot_layout;
    use crate::shq::encode::{EncodeInput, encode};
    use crate::shq::{Limits, Plane, VideoParams, decode};
    use std::sync::Arc;

    fn hand() -> Option<GpuHandIn> {
        let mut d = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        d.backends = wgpu::Backends::VULKAN;
        let instance = wgpu::Instance::new(d);
        let adapter = block_on(instance.request_adapter(&Default::default())).ok()?;
        let (device, queue) = block_on(adapter.request_device(&Default::default())).ok()?;
        Some(GpuHandIn { device, queue })
    }

    /// A picture with flat areas, gradients, edges and noise.
    fn picture(w: usize, h: usize, cw: usize, ch: usize) -> Vec<Plane> {
        let mut seed = 12345u32;
        let mut noise = || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 16) as u8
        };
        let mut plane = |pw: usize, ph: usize, k: usize| {
            let mut d = vec![0u8; pw * ph];
            for y in 0..ph {
                for x in 0..pw {
                    d[y * pw + x] = match (x * 4 / pw, y * 3 / ph) {
                        (0, _) => 16 + 60 * k as u8,          // flat
                        (1, _) => (16 + (x + y) % 200) as u8, // gradient
                        (2, 0) => {
                            if (x / 7 + y / 5) % 2 == 0 {
                                30
                            } else {
                                220
                            }
                        } // edges
                        _ => 16 + noise() % 220,              // noise
                    };
                }
            }
            Plane {
                width: pw,
                height: ph,
                data: d,
            }
        };
        vec![plane(w, h, 0), plane(cw, ch, 1), plane(cw, ch, 2)]
    }

    fn psnr(a: &[u8], b: &[u8]) -> f64 {
        let mse = a
            .iter()
            .zip(b)
            .map(|(&x, &y)| (x as f64 - y as f64).powi(2))
            .sum::<f64>()
            / a.len() as f64;
        if mse == 0.0 {
            99.0
        } else {
            10.0 * (255.0f64 * 255.0 / mse).log10()
        }
    }

    /// `planes` in a GPU buffer, laid out as the IDN programs lay out theirs.
    fn upload(hand: &GpuHandIn, planes: &[Plane]) -> GpuPicture {
        let sizes: Vec<(u32, u32)> = planes
            .iter()
            .map(|p| (p.width as u32, p.height as u32))
            .collect();
        let (at, bytes) = slot_layout(&sizes);
        let mut data = vec![0u8; bytes as usize];
        for (p, a) in planes.iter().zip(&at) {
            for y in 0..p.height {
                let o = a.offset as usize + y * a.row_bytes as usize;
                data[o..o + p.width].copy_from_slice(&p.data[y * p.width..(y + 1) * p.width]);
            }
        }
        let buffer = hand.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        hand.queue.write_buffer(&buffer, 0, &data);
        GpuPicture {
            buffer: Arc::new(buffer),
            planes: at,
        }
    }

    /// The GPU's bitstream decodes as the CPU's does: 4:2:2 and 4:2:0, short DC on and
    /// off, three qualities, odd sizes (partial blocks and MBs, blocks outside the
    /// picture), rows of more MBs than the rows pass has invocations, and flat pictures
    /// (skip runs). Needs a GPU (`--ignored`).
    #[test]
    #[ignore]
    fn the_gpu_encodes_as_the_cpu_does() {
        let Some(hand) = hand() else {
            eprintln!("no GPU");
            return;
        };
        let mut gpu = GpuShqEncoder::new(&hand).unwrap();
        // (Also rows wider than the rows pass's 256 invocations, tiny pictures, and a flat
        // one with a few boxes: long skip runs, REPEAT and DCONLY.)
        for (fourcc, (w, h), flat) in [
            (Fourcc::Shq2, (1920usize, 1080usize), false),
            (Fourcc::Shq0, (1918, 1078), false),
            (Fourcc::Shq2, (1278, 718), false),
            (Fourcc::Shq2, (7678, 40), false),
            (Fourcc::Shq0, (40, 24), false),
            (Fourcc::Shq2, (9, 7), false),
            (Fourcc::Shq2, (1920, 1080), true),
            (Fourcc::Shq0, (7680, 64), true),
        ] {
            let (cw, ch) = match fourcc {
                Fourcc::Shq0 => (w.div_ceil(2), h.div_ceil(2)),
                _ => (w.div_ceil(2), h),
            };
            let mut planes = picture(w, h, cw, ch);
            if flat {
                for (k, p) in planes.iter_mut().enumerate() {
                    for y in 0..p.height {
                        for x in 0..p.width {
                            p.data[y * p.width + x] = if (x / 24) % 37 == 3 && y < 20 {
                                200
                            } else if (x / 8) % 53 == 7 {
                                90 + (y % 4) as u8
                            } else {
                                100 + k as u8
                            };
                        }
                    }
                }
            }
            let pic = upload(&hand, &planes);
            for (q, short) in [(92u8, false), (80, true), (60, false)] {
                let opt = EncodeOptions {
                    quality: q,
                    short_dc: short,
                    ..Default::default()
                };
                let g = gpu
                    .encode(&pic, fourcc, w, h, &opt)
                    .expect("encoded on the GPU");
                let c = encode(
                    &EncodeInput {
                        fourcc,
                        width: w,
                        height: h,
                        planes: &planes,
                        precise: None,
                    },
                    &opt,
                );
                let vp = VideoParams {
                    fourcc: crate::sender::fourcc_bytes(fourcc),
                    width: w as u32,
                    height: h as u32,
                    field_id: 0,
                };
                let dg = decode(&vp, &g, &Limits::default()).expect("the GPU's decodes");
                let dc = decode(&vp, &c, &Limits::default()).expect("the CPU's decodes");
                let same = g.iter().zip(&c).filter(|(a, b)| a == b).count();
                #[allow(clippy::needless_range_loop)]
                for k in 0..3 {
                    let pg = psnr(&dg.planes[k].data, &planes[k].data);
                    let pc = psnr(&dc.planes[k].data, &planes[k].data);
                    eprintln!(
                        "{fourcc:?} {w}x{h} flat {flat} q{q} short {short} plane {k}: GPU {pg:.2} dB, CPU {pc:.2} dB ({} / {} bytes, {:.1}% the same)",
                        g.len(),
                        c.len(),
                        100.0 * same as f64 / c.len().max(1) as f64
                    );
                    assert!(
                        pg > pc - 0.3,
                        "{fourcc:?} q{q} plane {k}: {pg} against {pc}"
                    );
                }
            }
        }
    }
}

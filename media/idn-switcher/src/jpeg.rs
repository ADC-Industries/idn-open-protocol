//! A small baseline JPEG encoder (JFIF, 4:2:2, the standard's example tables), for the
//! API's multiview picture: no image library and no FFmpeg needed.

use crate::frame::Frame;

const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

const Q_LUMA: [u8; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113,
    92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];

const Q_CHROMA: [u8; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99,
    47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];

const DC_LUMA_BITS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const DC_CHROMA_BITS: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
const DC_VALS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const AC_LUMA_BITS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
const AC_LUMA_VALS: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
    0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5,
    0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
    0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];
const AC_CHROMA_BITS: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
const AC_CHROMA_VALS: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71,
    0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0,
    0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26,
    0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
    0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5,
    0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3,
    0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda,
    0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];

/// (code, length) by symbol, from a table's code-length counts and symbols.
fn codes(bits: &[u8; 16], vals: &[u8]) -> [(u16, u8); 256] {
    let mut out = [(0u16, 0u8); 256];
    let (mut code, mut k) = (0u16, 0usize);
    for (len, &n) in bits.iter().enumerate() {
        for _ in 0..n {
            out[vals[k] as usize] = (code, len as u8 + 1);
            code += 1;
            k += 1;
        }
        code <<= 1;
    }
    out
}

struct Bits {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl Bits {
    fn put(&mut self, code: u32, len: u32) {
        self.acc = (self.acc << len) | (code & ((1 << len) - 1));
        self.n += len;
        while self.n >= 8 {
            let b = (self.acc >> (self.n - 8)) as u8;
            self.out.push(b);
            if b == 0xff {
                self.out.push(0); // byte stuffing
            }
            self.n -= 8;
        }
        self.acc &= (1 << self.n) - 1;
    }

    fn flush(&mut self) {
        if self.n > 0 {
            self.put(0x7f, 8 - self.n); // pad with ones
        }
    }
}

/// The value's size category and its bits (JPEG's magnitude coding).
fn category(v: i32) -> (u32, u32) {
    let a = v.unsigned_abs();
    let size = 32 - a.leading_zeros();
    let bits = if v < 0 { (v - 1) as u32 } else { v as u32 };
    (size, bits & ((1u32 << size).wrapping_sub(1)))
}

/// A 2-D DCT of an 8×8 block of level-shifted samples (separable, float).
fn fdct(b: &[f32; 64]) -> [f32; 64] {
    // The cosines worked out once (64 per block before).
    static COS: std::sync::LazyLock<[[f32; 8]; 8]> = std::sync::LazyLock::new(|| {
        let mut c = [[0f32; 8]; 8];
        for (u, row) in c.iter_mut().enumerate() {
            for (x, v) in row.iter_mut().enumerate() {
                let a = if u == 0 { (0.125f32).sqrt() } else { 0.5 };
                *v = a * (((2 * x + 1) as f32 * u as f32 * std::f32::consts::PI) / 16.0).cos();
            }
        }
        c
    });
    let c = &*COS;
    let mut tmp = [0f32; 64];
    for y in 0..8 {
        for u in 0..8 {
            tmp[y * 8 + u] = (0..8).map(|x| c[u][x] * b[y * 8 + x]).sum();
        }
    }
    let mut out = [0f32; 64];
    for v in 0..8 {
        for u in 0..8 {
            out[v * 8 + u] = (0..8).map(|y| c[v][y] * tmp[y * 8 + u]).sum();
        }
    }
    out
}

/// Encodes `f` (limited range Y'CbCr 4:2:2) as a JPEG of the same size at `quality`
/// (1–100).
pub fn encode(f: &Frame, quality: u8) -> Vec<u8> {
    let q = quality.clamp(1, 100) as u32;
    let scale = if q < 50 { 5000 / q } else { 200 - 2 * q };
    let table = |base: &[u8; 64]| -> [u8; 64] {
        let mut t = [0u8; 64];
        for (i, v) in t.iter_mut().enumerate() {
            *v = ((base[i] as u32 * scale + 50) / 100).clamp(1, 255) as u8;
        }
        t
    };
    let (ql, qc) = (table(&Q_LUMA), table(&Q_CHROMA));
    let mut out = Vec::with_capacity(f.width * f.height / 4);
    out.extend_from_slice(&[0xff, 0xd8]); // SOI
    out.extend_from_slice(&[
        0xff, 0xe0, 0, 16, b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1, 0, 1, 0, 0,
    ]);
    for (id, t) in [(0u8, &ql), (1, &qc)] {
        out.extend_from_slice(&[0xff, 0xdb, 0, 67, id]);
        out.extend(ZIGZAG.iter().map(|&z| t[z]));
    }
    // SOF0: 8-bit, three components; Y 2×1, Cb and Cr 1×1 (4:2:2).
    let (w, h) = (f.width as u16, f.height as u16);
    out.extend_from_slice(&[0xff, 0xc0, 0, 17, 8]);
    out.extend_from_slice(&h.to_be_bytes());
    out.extend_from_slice(&w.to_be_bytes());
    out.extend_from_slice(&[3, 1, 0x21, 0, 2, 0x11, 1, 3, 0x11, 1]);
    for (class_id, bits, vals) in [
        (0x00u8, &DC_LUMA_BITS, &DC_VALS[..]),
        (0x10, &AC_LUMA_BITS, &AC_LUMA_VALS[..]),
        (0x01, &DC_CHROMA_BITS, &DC_VALS[..]),
        (0x11, &AC_CHROMA_BITS, &AC_CHROMA_VALS[..]),
    ] {
        let len = 2 + 1 + 16 + vals.len();
        out.extend_from_slice(&[0xff, 0xc4, (len >> 8) as u8, len as u8, class_id]);
        out.extend_from_slice(bits);
        out.extend_from_slice(vals);
    }
    out.extend_from_slice(&[0xff, 0xda, 0, 12, 3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0]);
    let (dcl, acl) = (
        codes(&DC_LUMA_BITS, &DC_VALS),
        codes(&AC_LUMA_BITS, &AC_LUMA_VALS),
    );
    let (dcc, acc) = (
        codes(&DC_CHROMA_BITS, &DC_VALS),
        codes(&AC_CHROMA_BITS, &AC_CHROMA_VALS),
    );
    let mut bits = Bits { out, acc: 0, n: 0 };
    let mut pred = [0i32; 3];
    let cw = Frame::chroma_width(f.width);
    // Limited range to JPEG's full range, level-shifted, held to 8 bits: super-white and
    // sub-black would otherwise make coefficients baseline JPEG has no codes for (DC
    // differences past size 11, AC past 10) at high qualities.
    let luma = |v: u8| ((v as f32 - 16.0) * (255.0 / 219.0) - 128.0).clamp(-128.0, 127.0);
    let chroma = |v: u8| ((v as f32 - 128.0) * (255.0 / 224.0)).clamp(-128.0, 127.0);
    let block =
        |plane: &[u8], pw: usize, bx: usize, by: usize, conv: &dyn Fn(u8) -> f32| -> [f32; 64] {
            let mut b = [0f32; 64];
            for y in 0..8 {
                let row = (by + y).min(f.height - 1);
                for x in 0..8 {
                    b[y * 8 + x] = conv(plane[row * pw + (bx + x).min(pw - 1)]);
                }
            }
            b
        };
    let emit = |bits: &mut Bits,
                samples: [f32; 64],
                qt: &[u8; 64],
                pred: &mut i32,
                dc: &[(u16, u8); 256],
                ac: &[(u16, u8); 256]| {
        let d = fdct(&samples);
        let mut z = [0i32; 64];
        for (i, &n) in ZIGZAG.iter().enumerate() {
            z[i] = (d[n] / qt[n] as f32).round() as i32;
        }
        let diff = z[0] - *pred;
        *pred = z[0];
        let (s, b) = category(diff);
        let (c, l) = dc[s as usize];
        debug_assert!(l > 0, "DC size {s} has no code");
        bits.put(c as u32, l as u32);
        if s > 0 {
            bits.put(b, s);
        }
        let mut run = 0;
        for &v in &z[1..] {
            if v == 0 {
                run += 1;
                continue;
            }
            while run > 15 {
                let (c, l) = ac[0xf0];
                bits.put(c as u32, l as u32);
                run -= 16;
            }
            let (s, b) = category(v);
            let (c, l) = ac[((run << 4) | s) as usize];
            debug_assert!(l > 0, "AC run {run} size {s} has no code");
            bits.put(c as u32, l as u32);
            bits.put(b, s);
            run = 0;
        }
        if run > 0 {
            let (c, l) = ac[0x00];
            bits.put(c as u32, l as u32);
        }
    };
    for my in (0..f.height).step_by(8) {
        for mx in (0..f.width).step_by(16) {
            let [py, pcb, pcr] = &mut pred;
            emit(
                &mut bits,
                block(&f.y, f.width, mx, my, &luma),
                &ql,
                py,
                &dcl,
                &acl,
            );
            emit(
                &mut bits,
                block(&f.y, f.width, mx + 8, my, &luma),
                &ql,
                py,
                &dcl,
                &acl,
            );
            emit(
                &mut bits,
                block(&f.cb, cw, mx / 2, my, &chroma),
                &qc,
                pcb,
                &dcc,
                &acc,
            );
            emit(
                &mut bits,
                block(&f.cr, cw, mx / 2, my, &chroma),
                &qc,
                pcr,
                &dcc,
                &acc,
            );
        }
    }
    bits.flush();
    let mut out = bits.out;
    out.extend_from_slice(&[0xff, 0xd9]); // EOI
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Ycc;

    #[test]
    fn a_jpeg_is_well_formed() {
        let mut f = Frame::solid(320, 180, Ycc::from_rgb(200, 30, 30));
        for i in 0..f.y.len() / 2 {
            f.y[i] = 16 + (i % 200) as u8;
        }
        let j = encode(&f, 80);
        assert_eq!(&j[..2], &[0xff, 0xd8]);
        assert_eq!(&j[j.len() - 2..], &[0xff, 0xd9]);
        // Between SOS and EOI no marker but stuffed 0xff00.
        let sos = j.windows(2).position(|w| w == [0xff, 0xda]).unwrap();
        let data = &j[sos + 14..j.len() - 2];
        assert!(data.windows(2).all(|w| w[0] != 0xff || w[1] == 0));
        assert!(j.len() < 320 * 180, "{} bytes", j.len());
    }

    /// The worst picture at the finest quality: luma 0/255 in a checkerboard (outside
    /// limited range) and chroma at its ends. Every value must have a code (the debug
    /// assertions), and the stream must stay free of markers.
    #[test]
    fn extremes_at_quality_100_have_codes() {
        let mut f = Frame::solid(64, 32, Ycc::from_rgb(0, 0, 0));
        for (i, v) in f.y.iter_mut().enumerate() {
            *v = if (i % 64 + i / 64) % 2 == 0 { 0 } else { 255 };
        }
        for (i, v) in f.cb.iter_mut().enumerate() {
            *v = if i % 2 == 0 { 0 } else { 255 };
        }
        for (i, v) in f.cr.iter_mut().enumerate() {
            *v = if i % 2 == 0 { 255 } else { 0 };
        }
        let j = encode(&f, 100);
        let sos = j.windows(2).position(|w| w == [0xff, 0xda]).unwrap();
        let data = &j[sos + 14..j.len() - 2];
        assert!(data.windows(2).all(|w| w[0] != 0xff || w[1] == 0));
    }
}

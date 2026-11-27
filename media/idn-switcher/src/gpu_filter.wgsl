// An input's filters on the GPU (gpu_filter.rs): one step a pass, from `src` into `dst`,
// each thread one output word (four bytes of one plane), every byte computed from `src`
// alone — the CPU's arithmetic (filter.rs), to the bit.
//
// A picture's planes, each from a 4-byte boundary: Y (w × h), Cb, Cr (cw × h, cw = w/2
// rounded up), alpha (w × h; there whether or not the picture had one).

struct Params {
    op: u32,
    w: u32,
    h: u32,
    cw: u32,
    // Plane offsets, bytes.
    oy: u32,
    ocb: u32,
    ocr: u32,
    oa: u32,
    // Words in each plane (rounded up).
    ny: u32,
    nc: u32,
    na: u32,
    // The picture had alpha before this step (else 255 everywhere).
    had_alpha: u32,
    opacity: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
}

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var<storage, read> src: array<u32>;
@group(0) @binding(2) var<storage, read_write> dst: array<u32>;
@group(0) @binding(3) var<storage, read> table: array<u32>;

fn byte_at(at: u32) -> u32 {
    return (src[at >> 2u] >> ((at & 3u) * 8u)) & 255u;
}

fn y_at(x: u32, row: u32) -> u32 {
    return byte_at(p.oy + row * p.w + x);
}

fn cb_at(c: u32, row: u32) -> u32 {
    return byte_at(p.ocb + row * p.cw + c);
}

fn cr_at(c: u32, row: u32) -> u32 {
    return byte_at(p.ocr + row * p.cw + c);
}

fn a_at(i: u32) -> u32 {
    if p.had_alpha == 0u {
        return 255u;
    }
    return byte_at(p.oa + i);
}

// (a + b) / 2 rounded up, as `div_ceil(2)`.
fn mean_up(a: u32, b: u32) -> u32 {
    return (a + b + 1u) >> 1u;
}

// The LUT's entry for (Y, Cb/4, Cr/4): Y′ | Cb′ << 8 | Cr′ << 16.
fn lut(y: u32, b: u32, c: u32) -> u32 {
    return table[(y << 12u) | ((b >> 2u) << 6u) | (c >> 2u)];
}

// The chroma an odd pixel's LUT entry takes: between its sample and the next.
fn lut_odd(x0: u32, c: u32, row: u32) -> u32 {
    var b1 = cb_at(c, row);
    var c1 = cr_at(c, row);
    if c + 1u < p.cw {
        b1 = mean_up(b1, cb_at(c + 1u, row));
        c1 = mean_up(c1, cr_at(c + 1u, row));
    }
    return lut(y_at(x0 + 1u, row), b1, c1);
}

// A chroma sample's key coverage (its table's first byte).
fn cover(c: u32, row: u32) -> u32 {
    return table[(cb_at(c, row) << 8u) | cr_at(c, row)] & 255u;
}

// A key's coverage `k` with the alpha there: multiplied in, or as it is.
fn keyed(i: u32, k: u32) -> u32 {
    if p.had_alpha == 0u {
        return k;
    }
    return (byte_at(p.oa + i) * k + 127u) / 255u;
}

// Unsharp masking (op 5): pushed away from the 3×3 mean by the table's one word (the
// amount × 256), the edges as they are.
fn sharpened(x: u32, row: u32, y: u32) -> u32 {
    if x == 0u || row == 0u || x + 1u >= p.w || row + 1u >= p.h {
        return y;
    }
    var sum = 0;
    for (var r = row - 1u; r <= row + 1u; r++) {
        for (var c = x - 1u; c <= x + 1u; c++) {
            sum += i32(y_at(c, r));
        }
    }
    let v = i32(y);
    let d = v - (sum + 4) / 9;
    return u32(clamp(v + ((d * bitcast<i32>(table[0])) >> 8u), 1, 254));
}

// Luma sample `i` (row-major) after the step.
fn out_y(i: u32) -> u32 {
    let row = i / p.w;
    let x = i % p.w;
    let y = y_at(x, row);
    switch p.op {
        case 5u: {
            return sharpened(x, row, y);
        }
        case 0u: {
            return table[y];
        }
        case 1u: {
            let c = x / 2u;
            if (x & 1u) == 0u {
                return lut(y, cb_at(c, row), cr_at(c, row)) & 255u;
            }
            return lut_odd(x - 1u, c, row) & 255u;
        }
        default: {
            return y;
        }
    }
}

// Chroma sample `j` (row-major) after the step: Cb (`second` false) or Cr.
fn out_c(j: u32, second: bool) -> u32 {
    let row = j / p.cw;
    let c = j % p.cw;
    let b = cb_at(c, row);
    let r = cr_at(c, row);
    let shift = select(8u, 16u, second);
    switch p.op {
        case 0u: {
            let e = table[256u + ((b << 8u) | r)];
            return select(e & 255u, (e >> 8u) & 255u, second);
        }
        case 1u: {
            let x0 = 2u * c;
            let q0 = (lut(y_at(x0, row), b, r) >> shift) & 255u;
            if x0 + 1u < p.w {
                return mean_up(q0, (lut_odd(x0, c, row) >> shift) & 255u);
            }
            return q0;
        }
        case 2u: {
            return (table[(b << 8u) | r] >> shift) & 255u;
        }
        default: {
            return select(b, r, second);
        }
    }
}

// Alpha sample `i` after the step.
fn out_a(i: u32) -> u32 {
    let row = i / p.w;
    let x = i % p.w;
    switch p.op {
        case 0u: {
            if p.opacity < 255u {
                return (a_at(i) * p.opacity + 127u) / 255u;
            }
            return a_at(i);
        }
        case 2u: {
            let c = x / 2u;
            var k = cover(c, row);
            if (x & 1u) == 1u && c + 1u < p.cw {
                k = mean_up(k, cover(c + 1u, row));
            }
            return keyed(i, k);
        }
        case 3u: {
            let c = x / 2u;
            let k = table[((y_at(x, row) >> 2u) << 12u) | ((cb_at(c, row) >> 2u) << 6u)
                | (cr_at(c, row) >> 2u)] & 255u;
            return keyed(i, k);
        }
        case 4u: {
            return keyed(i, table[y_at(x, row)] & 255u);
        }
        default: {
            return a_at(i);
        }
    }
}

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    // Words: Y, then Cb, then Cr, then alpha.
    var k = id.x + id.y * 65535u * 64u;
    var plane = 0u;
    if k >= p.ny {
        k -= p.ny;
        plane = 1u;
        if k >= p.nc {
            k -= p.nc;
            plane = 2u;
            if k >= p.nc {
                k -= p.nc;
                plane = 3u;
                if k >= p.na {
                    return;
                }
            }
        }
    }
    let n = array<u32, 4>(p.w * p.h, p.cw * p.h, p.cw * p.h, p.w * p.h);
    let base = array<u32, 4>(p.oy, p.ocb, p.ocr, p.oa);
    var word = 0u;
    for (var b = 0u; b < 4u; b++) {
        let i = k * 4u + b;
        if i >= n[plane] {
            break;
        }
        var v = 0u;
        switch plane {
            case 0u: {
                v = out_y(i);
            }
            case 1u: {
                v = out_c(i, false);
            }
            case 2u: {
                v = out_c(i, true);
            }
            default: {
                v = out_a(i);
            }
        }
        word |= v << (b * 8u);
    }
    dst[(base[plane] >> 2u) + k] = word;
}

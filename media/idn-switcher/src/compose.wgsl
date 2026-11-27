// The compositor on the GPU (spec/extensions/idn-switcher.md §S.9): the same operations
// as compose.rs, with the same integer arithmetic, so a picture comes out as the CPU makes
// it (scaled pictures within one step, from float coordinates).
//
// Each invocation makes 8 pixels of a row: 8 luma samples and 4 of each chroma. Pictures
// are read as bytes from one storage buffer (each picture's Y, Cb, Cr, then A, at the
// offsets in its layer), the result written as bytes, 4 to a word.

struct Globals {
    // Program width, height, chroma width, (unused).
    size: vec4<u32>,
    // Words a row: luma, chroma; first word of Cb, of Cr.
    strides: vec4<u32>,
    // Transition: kind (0 none, 1 mix, 2 wipe, 3 fade, 4 luma wipe, 5 zoom: the preview
    // layer placed where it has grown to), wipe direction (0 left,
    // 1 right, 2 down, 3 up, 4 circle; a luma wipe: 1 inverted), soft edge, position.
    trans: vec4<f32>,
    // Fade-to-black level; whether the preview layer is there; whether a stinger's clip
    // (layer 2) goes over the bus.
    misc: vec4<f32>,
};

struct Layer {
    // Where the picture goes: x, y, width, height (program pixels; x even).
    rect: vec4<f32>,
    // Its part shown: x0, y0, x1, y1 (fractions).
    crop: vec4<f32>,
    // Its size: width, height, chroma width, has alpha.
    size: vec4<u32>,
    // Byte offsets of Y, Cb, Cr, A in its buffer.
    offs: vec4<u32>,
    // A border's rectangle x0, y0, x1, y1 (none when x1 <= x0).
    brect: vec4<f32>,
    // The border's colour Y, Cb, Cr.
    bcol: vec4<f32>,
    // Opacity (0–256 as the CPU's weight), mode (0 a bus, 1 a keyed layer),
    // 1 when the picture is shown at its own size, whole (read as it is, not scaled).
    par: vec4<f32>,
};

// The HDR program's pass (R.24, idn-switcher.md §S.34): built from this file with these
// two set (gpu_hdr.rs). Its pictures are 16-bit BT.2100 samples (two to a word; an 8-bit
// value v is v × 256), mixed without the 8-bit rounding, and written as P216 (Y, then Cb
// and Cr interleaved, two samples to a word).
const HDR: bool = false;
const S: f32 = 1.0;

@group(0) @binding(0) var<uniform> g: Globals;
@group(0) @binding(1) var<uniform> layers: array<Layer, 3>;
@group(0) @binding(2) var<storage, read_write> outb: array<u32>;
@group(0) @binding(3) var<storage, read> pics: array<u32>;

fn rd(i: u32) -> f32 {
    if HDR {
        return f32((pics[i >> 1u] >> ((i & 1u) * 16u)) & 0xFFFFu);
    }
    return f32((pics[i >> 2u] >> ((i & 3u) * 8u)) & 0xFFu);
}

// `a` + (`b` − `a`) × `w` / 256, as the CPU rounds it (HDR: not rounded).
fn lerp8(a: f32, b: f32, w: f32) -> f32 {
    if HDR {
        return (a * (256.0 - w) + b * w) / 256.0;
    }
    return floor((a * (256.0 - w) + b * w + 128.0) / 256.0);
}

const BLACK_Y: f32 = 16.0;
const BLACK_C: f32 = 128.0;

fn black(plane: u32) -> f32 {
    return select(BLACK_C, BLACK_Y, plane == 0u) * S;
}

// Is position `i` (a sample of plane `plane`, from the program's left) on row `row`
// inside layer `l`'s rectangle; and its index within it.
fn inside(l: Layer, plane: u32, i: u32, row: u32) -> vec3<i32> {
    let chroma = plane == 1u || plane == 2u;
    let x0 = select(u32(l.rect.x), u32(l.rect.x) / 2u, chroma);
    let w = select(u32(l.rect.z), (u32(l.rect.z) + 1u) / 2u, chroma);
    let y0 = u32(l.rect.y);
    let h = u32(l.rect.w);
    let ok = i >= x0 && i < x0 + w && row >= y0 && row < y0 + h;
    return vec3<i32>(select(0, 1, ok), i32(i) - i32(x0), i32(row) - i32(y0));
}

// A layer's plane (3: alpha) at index (i, j) of its rectangle, bilinear with the CPU's
// 8-bit weights.
fn sample(l: Layer, plane: u32, i: i32, j: i32) -> f32 {
    let chroma = plane == 1u || plane == 2u;
    let pw = select(l.size.x, l.size.z, chroma);
    let ph = l.size.y;
    var off: u32;
    switch plane {
        case 0u: { off = l.offs.x; }
        case 1u: { off = l.offs.y; }
        case 2u: { off = l.offs.z; }
        default: { off = l.offs.w; }
    }
    if l.par.z > 0.5 {
        return rd(off + u32(j) * pw + u32(i));
    }
    let ow = select(l.rect.z, f32((u32(l.rect.z) + 1u) / 2u), chroma);
    let oh = l.rect.w;
    let fpw = f32(pw);
    let fph = f32(ph);
    let fx = clamp(l.crop.x * fpw + (f32(i) + 0.5) * (l.crop.z - l.crop.x) * fpw / ow - 0.5, 0.0, fpw - 1.0);
    let fy = clamp(l.crop.y * fph + (f32(j) + 0.5) * (l.crop.w - l.crop.y) * fph / oh - 0.5, 0.0, fph - 1.0);
    let xa = u32(floor(fx));
    let ya = u32(floor(fy));
    let wx = round((fx - floor(fx)) * 256.0);
    let wy = round((fy - floor(fy)) * 256.0);
    let xb = min(xa + 1u, pw - 1u);
    let yb = min(ya + 1u, ph - 1u);
    let ra = off + ya * pw;
    let rb = off + yb * pw;
    let top = rd(ra + xa) * (256.0 - wx) + rd(ra + xb) * wx;
    let bot = rd(rb + xa) * (256.0 - wx) + rd(rb + xb) * wx;
    if HDR {
        return (top * (256.0 - wy) + bot * wy) / 65536.0;
    }
    return floor((top * (256.0 - wy) + bot * wy + 32768.0) / 65536.0);
}

// A bus layer (program, preview) at a sample: the picture inside its rectangle, black
// outside (letterboxed).
fn bus(slot: u32, plane: u32, i: u32, row: u32) -> f32 {
    let l = layers[slot];
    let p = inside(l, plane, i, row);
    if p.x == 0 {
        return black(plane);
    }
    return sample(l, plane, p.y, p.z);
}

// The coverage (0–255) a layer gives a sample: its alpha (for chroma the mean of the
// two pixels', rounded up), or opaque.
fn coverage(l: Layer, plane: u32, i: i32, j: i32) -> f32 {
    if l.size.w == 0u {
        return 255.0;
    }
    // (HDR: alpha is 16-bit too, as coverage 0–255 again.)
    if plane == 0u {
        return round(sample(l, 3u, i, j) / S);
    }
    let rw = i32(u32(l.rect.z));
    let a = round(sample(l, 3u, 2 * i, j) / S);
    let b = select(a, round(sample(l, 3u, 2 * i + 1, j) / S), 2 * i + 1 < rw);
    return ceil((a + b) / 2.0);
}

fn key_weight(a: f32, o: f32) -> f32 {
    return floor((a * o + 127.0) / 255.0);
}

fn wipe_weight(s: f32, t: f32, soft: f32) -> f32 {
    var m: f32;
    if soft > 0.0 {
        m = clamp((t * (1.0 + soft) - s) / soft, 0.0, 1.0);
    } else {
        m = select(0.0, 1.0, s < t);
    }
    return round(m * 256.0);
}

// The program at one sample of `plane`: index `i` across, row `row`.
fn composite(plane: u32, i: u32, row: u32) -> f32 {
    let fw = f32(g.size.x);
    let fh = f32(g.size.y);
    var v = bus(0u, plane, i, row);
    let kind = u32(g.trans.x);
    let t = g.trans.w;
    if kind != 0u && g.misc.y > 0.5 && t > 0.0 {
        let n = bus(1u, plane, i, row);
        if t >= 1.0 {
            v = n;
        } else if kind == 1u {
            v = lerp8(v, n, round(t * 256.0));
        } else if kind == 4u {
            // A luma wipe: the pattern (layer 2) at this sample — for chroma the mean of
            // its two pixels' — as 0–1 of limited range, against the position.
            let pl = layers[2];
            var lv: f32;
            if plane == 0u {
                let p = inside(pl, 0u, i, row);
                lv = select(16.0, sample(pl, 0u, p.y, p.z) / S, p.x != 0);
            } else {
                let pa = inside(pl, 0u, 2u * i, row);
                let a = select(16.0, sample(pl, 0u, pa.y, pa.z) / S, pa.x != 0);
                let pb = inside(pl, 0u, 2u * i + 1u, row);
                let b = select(a, sample(pl, 0u, pb.y, pb.z) / S, pb.x != 0 && 2u * i + 1u < g.size.x);
                lv = select(ceil((a + b) / 2.0), (a + b) / 2.0, HDR);
            }
            var sv = clamp((lv - 16.0) / 219.0, 0.0, 1.0);
            if g.trans.y > 0.5 {
                sv = 1.0 - sv;
            }
            let k = wipe_weight(sv, t, g.trans.z);
            if k != 0.0 {
                v = lerp8(v, n, k);
            }
        } else if kind == 5u {
            // A zoom: the preview where its (grown) layer covers, the program around it.
            if inside(layers[1], plane, i, row).x != 0 {
                v = n;
            }
        } else if kind == 3u {
            if t < 0.5 {
                v = lerp8(v, black(plane), min(round(t * 512.0), 256.0));
            } else {
                v = lerp8(n, black(plane), min(round((1.0 - t) * 512.0), 256.0));
            }
        } else {
            let dir = u32(g.trans.y);
            let soft = g.trans.z;
            let xscale = select(2.0, 1.0, plane == 0u);
            let x = f32(i) * xscale + xscale / 2.0;
            let y = f32(row) + 0.5;
            if dir == 4u {
                let half_diag = sqrt(fw * fw + fh * fh) / 2.0;
                let edge = t * (1.0 + soft);
                let r_in = max(edge - soft, 0.0) * half_diag;
                let r_out = edge * half_diag;
                let dx = x - fw / 2.0;
                let dy = y - fh / 2.0;
                let d2 = dx * dx + dy * dy;
                if d2 <= r_in * r_in {
                    v = n;
                } else if d2 < r_out * r_out {
                    v = lerp8(v, n, wipe_weight(sqrt(d2) / half_diag, t, soft));
                }
            } else {
                var s: f32;
                switch dir {
                    case 0u: { s = x / fw; }
                    case 1u: { s = 1.0 - x / fw; }
                    case 2u: { s = y / fh; }
                    default: { s = 1.0 - y / fh; }
                }
                let k = wipe_weight(s, t, soft);
                if k != 0.0 {
                    v = lerp8(v, n, k);
                }
            }
        }
    }
    // A stinger's clip over the bus, keyed by its coverage.
    if g.misc.z > 0.5 {
        let l = layers[2];
        let p = inside(l, plane, i, row);
        if p.x != 0 {
            let s = sample(l, plane, p.y, p.z);
            if l.size.w == 0u {
                v = s;
            } else {
                v = lerp8(v, s, key_weight(coverage(l, plane, p.y, p.z), 256.0));
            }
        }
    }
    // Fade to black.
    let ftb = round(clamp(g.misc.x, 0.0, 1.0) * 256.0);
    if ftb > 0.0 {
        v = lerp8(v, black(plane), ftb);
    }
    return v;
}

// A 16-bit sample, rounded.
fn q16(v: f32) -> u32 {
    return u32(clamp(round(v), 0.0, 65535.0));
}

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let gx = id.x;
    let row = id.y;
    if row >= g.size.y || gx >= g.strides.y {
        return;
    }
    if HDR {
        // P216: 8 luma samples as 4 words, then 4 Cb–Cr pairs a word each (the width is
        // even: a pair never straddles two rows).
        let w = g.size.x;
        for (var k = 0u; k < 4u; k++) {
            let x = gx * 8u + k * 2u;
            if x < w {
                let a = q16(composite(0u, x, row));
                let b = select(0u, q16(composite(0u, x + 1u, row)), x + 1u < w);
                outb[(row * w + x) >> 1u] = a | (b << 16u);
            }
        }
        for (var b = 0u; b < 4u; b++) {
            let c = gx * 4u + b;
            if c < g.size.z {
                let cb = q16(composite(1u, c, row));
                let cr = q16(composite(2u, c, row));
                outb[((w * g.size.y) >> 1u) + row * g.size.z + c] = cb | (cr << 16u);
            }
        }
        return;
    }
    // 8 luma samples, 2 words.
    for (var w = 0u; w < 2u; w++) {
        var word = 0u;
        for (var b = 0u; b < 4u; b++) {
            let x = gx * 8u + w * 4u + b;
            let v = select(0.0, composite(0u, x, row), x < g.size.x);
            word |= (u32(v) & 0xFFu) << (b * 8u);
        }
        outb[row * g.strides.x + gx * 2u + w] = word;
    }
    // 4 of each chroma, a word each.
    for (var plane = 1u; plane < 3u; plane++) {
        var word = 0u;
        for (var b = 0u; b < 4u; b++) {
            let c = gx * 4u + b;
            let v = select(0.0, composite(plane, c, row), c < g.size.z);
            word |= (u32(v) & 0xFFu) << (b * 8u);
        }
        let base = select(g.strides.w, g.strides.z, plane == 1u);
        outb[base + row * g.strides.y + gx] = word;
    }
}

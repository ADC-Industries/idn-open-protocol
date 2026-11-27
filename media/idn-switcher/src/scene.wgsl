// Scenes drawn on the GPU (spec/extensions/idn-switcher.md §S.12, stage 1e): the same
// operations as scene.rs, with the same integer arithmetic, from a transparent canvas.
//
// The scene comes as a list of operations, bottom to top (an item's border, then the item).
// An upright one is scaled as the CPU scales it (its taps from `tx`, `ty`) into its whole-
// pixel rectangle; a turned one is sampled through its inverse placement, as the CPU's
// slow path. Each invocation makes 8 pixels of a row: 8 luma and alpha samples, 4 of each
// chroma. Pictures are read as bytes from one storage buffer, the result written as bytes,
// 4 to a word: Y, Cb, Cr, then A.

struct Globals {
    // Canvas width, height, chroma width, operations.
    size: vec4<u32>,
    // Words a row: luma, chroma; first word of Cb, of Cr.
    strides: vec4<u32>,
    // First word of A.
    more: vec4<u32>,
};

struct Op {
    // Turned: x, y, width, height on the canvas (pixels, fractional).
    place: vec4<f32>,
    // The part shown: x0, y0, x1, y1 (fractions).
    crop: vec4<f32>,
    // Turned: cos, sin of the angle, the centre x, y.
    rot: vec4<f32>,
    // Upright: the taps across, luma (start, step) and chroma (start, step).
    tx: vec4<f32>,
    // Upright: the taps down (start, step).
    ty: vec4<f32>,
    // Upright: the rectangle x (even), y, width, height. Turned: its bounds x0, y0, x1, y1.
    irect: vec4<u32>,
    // The source: width, height, chroma width, has alpha.
    size: vec4<u32>,
    // Byte offsets of Y, Cb, Cr, A.
    offs: vec4<u32>,
    // Opacity (0–256), blend (0 normal, 1 add, 2 multiply, 3 screen), upright (1), flags
    // (1 read as it is: whole, at its own size; 2 a colour).
    par: vec4<u32>,
    // The colour: Y, Cb, Cr.
    col: vec4<u32>,
    // The corner radius (canvas pixels, 0 square; turned operations only).
    shape: vec4<f32>,
};

@group(0) @binding(0) var<uniform> g: Globals;
@group(0) @binding(1) var<storage, read> ops: array<Op>;
@group(0) @binding(2) var<storage, read_write> outb: array<u32>;
@group(0) @binding(3) var<storage, read> pics: array<u32>;

fn rd(i: u32) -> u32 {
    return (pics[i >> 2u] >> ((i & 3u) * 8u)) & 0xFFu;
}

// A plane (`pw` × `ph` from byte `off`) at (`fx`, `fy`) in sample coordinates, bilinear
// with 8-bit weights (scene.rs `bilinear`, frame.rs `resample`).
fn bil(off: u32, pw: u32, ph: u32, fx0: f32, fy0: f32) -> u32 {
    let fx = clamp(fx0, 0.0, f32(pw - 1u));
    let fy = clamp(fy0, 0.0, f32(ph - 1u));
    let xa = u32(floor(fx));
    let ya = u32(floor(fy));
    let wx = u32(round((fx - floor(fx)) * 256.0));
    let wy = u32(round((fy - floor(fy)) * 256.0));
    let xb = min(xa + 1u, pw - 1u);
    let yb = min(ya + 1u, ph - 1u);
    let ra = off + ya * pw;
    let rb = off + yb * pw;
    let top = rd(ra + xa) * (256u - wx) + rd(ra + xb) * wx;
    let bot = rd(rb + xa) * (256u - wx) + rd(rb + xb) * wx;
    return (top * (256u - wy) + bot * wy + 32768u) >> 16u;
}

fn weight(a: u32, o: u32) -> u32 {
    return (a * o + 127u) / 255u;
}

fn lerp8(a: u32, b: u32, w: u32) -> u32 {
    return (a * (256u - w) + b * w + 128u) >> 8u;
}

fn blend_y(d0: u32, s0: u32, blend: u32) -> u32 {
    let d = i32(d0);
    let s = i32(s0);
    switch blend {
        case 1u: { return u32(clamp(d + (s - 16), 16, 235)); }
        case 2u: { return u32(clamp(16 + max(d - 16, 0) * max(s - 16, 0) / 219, 16, 235)); }
        case 3u: { return u32(clamp(235 - max(235 - d, 0) * max(235 - s, 0) / 219, 16, 235)); }
        default: { return s0; }
    }
}

// An upright operation's plane `off` (`pw` wide, taps `start`/`step` across) at index
// (i, j) of its rectangle.
fn up(op: Op, off: u32, pw: u32, start: f32, step: f32, i: u32, j: u32) -> u32 {
    if (op.par.w & 1u) != 0u {
        return rd(off + j * pw + i);
    }
    let fx = start + (f32(i) + 0.5) * step - 0.5;
    let fy = op.ty.x + (f32(j) + 0.5) * op.ty.y - 0.5;
    return bil(off, pw, op.size.y, fx, fy);
}

// An upright operation's coverage at pixel `i` of its rectangle's row `j`.
fn up_alpha(op: Op, i: u32, j: u32) -> u32 {
    if op.size.w == 0u || (op.par.w & 2u) != 0u {
        return 255u;
    }
    return up(op, op.offs.w, op.size.x, op.tx.x, op.tx.y, i, j);
}

// A turned operation: a canvas point to (x, y) fractions of the source; z 0 when it
// doesn't land on the item.
fn map(op: Op, px: f32, py: f32) -> vec3<f32> {
    let dx = px - op.rot.z;
    let dy = py - op.rot.w;
    let u = (dx * op.rot.x + dy * op.rot.y) / op.place.z + 0.5;
    let v = (-dx * op.rot.y + dy * op.rot.x) / op.place.w + 0.5;
    if u < 0.0 || u >= 1.0 || v < 0.0 || v >= 1.0 {
        return vec3<f32>(0.0, 0.0, 0.0);
    }
    return vec3<f32>(op.crop.x + u * (op.crop.z - op.crop.x), op.crop.y + v * (op.crop.w - op.crop.y), 1.0);
}

// A turned operation's rounded corners: the coverage (0–255) they leave at canvas point
// (px, py), 255 but in the corners (scene.rs `corner`).
fn corner(op: Op, px: f32, py: f32) -> u32 {
    let r = op.shape.x;
    if r <= 0.0 {
        return 255u;
    }
    let dx = px - op.rot.z;
    let dy = py - op.rot.w;
    let lx = dx * op.rot.x + dy * op.rot.y + op.place.z / 2.0;
    let ly = -dx * op.rot.y + dy * op.rot.x + op.place.w / 2.0;
    let ox = max(max(r - lx, lx - (op.place.z - r)), 0.0);
    let oy = max(max(r - ly, ly - (op.place.w - r)), 0.0);
    if ox == 0.0 || oy == 0.0 {
        return 255u;
    }
    return u32(round(clamp(r - sqrt(ox * ox + oy * oy) + 0.5, 0.0, 1.0) * 255.0));
}

fn turned_alpha(op: Op, fx: f32, fy: f32) -> u32 {
    if op.size.w == 0u || (op.par.w & 2u) != 0u {
        return 255u;
    }
    return bil(op.offs.w, op.size.x, op.size.y, fx, fy);
}

// Luma and alpha at pixel (px, row).
fn pixel(px: u32, row: u32) -> vec2<u32> {
    var y = 16u;
    var a = 0u;
    for (var k = 0u; k < g.size.w; k++) {
        let op = ops[k];
        let r = op.irect;
        var sv: u32;
        var al: u32;
        if op.par.z == 1u {
            if px < r.x || px >= r.x + r.z || row < r.y || row >= r.y + r.w {
                continue;
            }
            let i = px - r.x;
            let j = row - r.y;
            al = up_alpha(op, i, j);
            if (op.par.w & 2u) != 0u {
                sv = op.col.x;
            } else {
                sv = up(op, op.offs.x, op.size.x, op.tx.x, op.tx.y, i, j);
            }
        } else {
            if px < r.x || px >= r.z || row < r.y || row >= r.w {
                continue;
            }
            let m = map(op, f32(px) + 0.5, f32(row) + 0.5);
            if m.z == 0.0 {
                continue;
            }
            let fx = m.x * f32(op.size.x) - 0.5;
            let fy = m.y * f32(op.size.y) - 0.5;
            al = (turned_alpha(op, fx, fy) * corner(op, f32(px) + 0.5, f32(row) + 0.5) + 127u) / 255u;
            if weight(al, op.par.x) == 0u {
                continue;
            }
            if (op.par.w & 2u) != 0u {
                sv = op.col.x;
            } else {
                sv = bil(op.offs.x, op.size.x, op.size.y, fx, fy);
            }
        }
        let kw = weight(al, op.par.x);
        if kw == 0u {
            continue;
        }
        y = lerp8(y, blend_y(y, sv, op.par.y), kw);
        a = a + (((255u - a) * kw + 128u) >> 8u);
    }
    return vec2<u32>(y, a);
}

// Chroma plane `plane` (1 Cb, 2 Cr) at sample (ci, row).
fn chroma(plane: u32, ci: u32, row: u32) -> u32 {
    var c = 128u;
    for (var k = 0u; k < g.size.w; k++) {
        let op = ops[k];
        let r = op.irect;
        let off = select(op.offs.z, op.offs.y, plane == 1u);
        let colour = select(op.col.z, op.col.y, plane == 1u);
        var sv: u32;
        var al: u32;
        if op.par.z == 1u {
            let x0 = r.x / 2u;
            if ci < x0 || ci >= x0 + (r.z + 1u) / 2u || row < r.y || row >= r.y + r.w {
                continue;
            }
            let i = ci - x0;
            let j = row - r.y;
            // A chroma sample covers two pixels: their mean coverage.
            let la = up_alpha(op, 2u * i, j);
            let ra = select(la, up_alpha(op, 2u * i + 1u, j), 2u * i + 1u < r.z);
            al = (la + ra + 1u) / 2u;
            if (op.par.w & 2u) != 0u {
                sv = colour;
            } else {
                sv = up(op, off, op.size.z, op.tx.z, op.tx.w, i, j);
            }
        } else {
            if ci < r.x / 2u || ci >= (r.z + 1u) / 2u || row < r.y || row >= r.w {
                continue;
            }
            let m = map(op, 2.0 * f32(ci) + 1.0, f32(row) + 0.5);
            if m.z == 0.0 {
                continue;
            }
            let fy = m.y * f32(op.size.y) - 0.5;
            al = (turned_alpha(op, m.x * f32(op.size.x) - 0.5, fy) * corner(op, 2.0 * f32(ci) + 1.0, f32(row) + 0.5) + 127u) / 255u;
            if weight(al, op.par.x) == 0u {
                continue;
            }
            if (op.par.w & 2u) != 0u {
                sv = colour;
            } else {
                sv = bil(off, op.size.z, op.size.y, m.x * f32(op.size.z) - 0.5, fy);
            }
        }
        let kw = weight(al, op.par.x);
        if kw == 0u {
            continue;
        }
        c = lerp8(c, sv, kw);
    }
    return c;
}

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let gx = id.x;
    let row = id.y;
    if row >= g.size.y || gx >= g.strides.y {
        return;
    }
    // 8 luma and alpha samples, 2 words of each.
    for (var w = 0u; w < 2u; w++) {
        var yw = 0u;
        var aw = 0u;
        for (var b = 0u; b < 4u; b++) {
            let x = gx * 8u + w * 4u + b;
            if x < g.size.x {
                let p = pixel(x, row);
                yw |= (p.x & 0xFFu) << (b * 8u);
                aw |= (p.y & 0xFFu) << (b * 8u);
            }
        }
        outb[row * g.strides.x + gx * 2u + w] = yw;
        outb[g.more.x + row * g.strides.x + gx * 2u + w] = aw;
    }
    // 4 of each chroma, a word each.
    for (var plane = 1u; plane < 3u; plane++) {
        var word = 0u;
        for (var b = 0u; b < 4u; b++) {
            let c = gx * 4u + b;
            if c < g.size.z {
                word |= (chroma(plane, c, row) & 0xFFu) << (b * 8u);
            }
        }
        let base = select(g.strides.w, g.strides.z, plane == 1u);
        outb[base + row * g.strides.y + gx] = word;
    }
}

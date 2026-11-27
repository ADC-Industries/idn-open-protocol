// A picture lifted into the HDR program's space (R.24 stage 2, idn-switcher.md §S.34):
// its 8-bit BT.709 Y'CbCr 4:2:2 samples (and alpha) as 16-bit BT.2100 PQ or HLG, as
// BT.2408 maps SDR (white at `white` nits), at the same sample indices in the 16-bit
// buffer (two samples to a word, written with atomic OR into a region cleared first:
// neighbours share words). The arithmetic is idn_open_protocol::color::to_hdr's exact one.
//
// One invocation: one chroma sample and its two pixels, of one row.

struct Job {
    // Byte (and sample) offsets of Y, Cb, Cr, A.
    offs: vec4<u32>,
    // Width, height, chroma width, has alpha.
    size: vec4<u32>,
    // Transfer (0 PQ, 1 HLG), SDR white (nits), raw (a luma wipe's pattern: levels
    // copied, not lifted), unused.
    tf: vec4<f32>,
};

@group(0) @binding(0) var<uniform> job: Job;
@group(0) @binding(1) var<storage, read> src: array<u32>;
@group(0) @binding(2) var<storage, read_write> dst: array<atomic<u32>>;

fn rd(i: u32) -> f32 {
    return f32((src[i >> 2u] >> ((i & 3u) * 8u)) & 0xFFu);
}

fn wr(i: u32, v: u32) {
    atomicOr(&dst[i >> 1u], (v & 0xFFFFu) << ((i & 1u) * 16u));
}

// (PQ_*, HLG_*, KR, KB and M709_2020_R* come first, from
// idn_open_protocol::color::transfer::wgsl(): the CPU's constants.)

fn pq(nits: f32) -> f32 {
    let y = pow(clamp(nits / 10000.0, 0.0, 1.0), PQ_M1);
    return pow((PQ_C1 + PQ_C2 * y) / (1.0 + PQ_C3 * y), PQ_M2);
}

fn hlg(e: f32) -> f32 {
    if e <= 1.0 / 12.0 {
        return sqrt(3.0 * max(e, 0.0));
    }
    return HLG_A * log(12.0 * e - HLG_B) + HLG_C;
}

// One pixel: 8-bit Y'CbCr (BT.709, limited) → 16-bit Y'CbCr (BT.2020, limited, the
// transfer), chroma unaveraged.
fn lift(y: f32, cb: f32, cr: f32) -> vec3<f32> {
    let yn = (y - 16.0) / 219.0;
    let b = (cb - 128.0) / 224.0;
    let r = (cr - 128.0) / 224.0;
    let rgb = pow(clamp(vec3<f32>(yn + 1.5748 * r, yn - 0.187324 * b - 0.468124 * r, yn + 1.8556 * b), vec3(0.0), vec3(1.0)), vec3(2.4));
    // BT.709 → BT.2020 primaries (BT.2087), linear.
    let lin = vec3<f32>(dot(M709_2020_R0, rgb), dot(M709_2020_R1, rgb), dot(M709_2020_R2, rgb));
    let white = job.tf.y;
    var e: vec3<f32>;
    if job.tf.x > 0.5 {
        // HLG on a 1000 nit display (γ 1.2): its inverse OOTF, then the OETF.
        let f = lin * (white / 1000.0);
        let yd = KR * f.x + (1.0 - KR - KB) * f.y + KB * f.z;
        let k = select(0.0, pow(yd, (1.0 - 1.2) / 1.2), yd > 0.0);
        e = vec3<f32>(hlg(f.x * k), hlg(f.y * k), hlg(f.z * k));
    } else {
        e = vec3<f32>(pq(lin.x * white), pq(lin.y * white), pq(lin.z * white));
    }
    let yy = KR * e.x + (1.0 - KR - KB) * e.y + KB * e.z;
    let ccb = (e.z - yy) / (2.0 * (1.0 - KB));
    let ccr = (e.x - yy) / (2.0 * (1.0 - KR));
    return vec3<f32>(
        clamp(round((16.0 + 219.0 * yy) * 256.0), 0.0, 65535.0),
        clamp(round((128.0 + 224.0 * ccb) * 256.0), 0.0, 65535.0),
        clamp(round((128.0 + 224.0 * ccr) * 256.0), 0.0, 65535.0),
    );
}

@compute @workgroup_size(64, 1, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let c = id.x;
    let row = id.y;
    let w = job.size.x;
    let cw = job.size.z;
    if c >= cw || row >= job.size.y {
        return;
    }
    let cb = rd(job.offs.y + row * cw + c);
    let cr = rd(job.offs.z + row * cw + c);
    let x0 = 2u * c;
    if job.tf.z > 0.5 {
        wr(job.offs.x + row * w + x0, u32(rd(job.offs.x + row * w + x0) * 256.0));
        if x0 + 1u < w {
            wr(job.offs.x + row * w + x0 + 1u, u32(rd(job.offs.x + row * w + x0 + 1u) * 256.0));
        }
        wr(job.offs.y + row * cw + c, u32(cb * 256.0));
        wr(job.offs.z + row * cw + c, u32(cr * 256.0));
        if job.size.w != 0u {
            wr(job.offs.w + row * w + x0, u32(rd(job.offs.w + row * w + x0) * 256.0));
            if x0 + 1u < w {
                wr(job.offs.w + row * w + x0 + 1u, u32(rd(job.offs.w + row * w + x0 + 1u) * 256.0));
            }
        }
        return;
    }
    let a = lift(rd(job.offs.x + row * w + x0), cb, cr);
    wr(job.offs.x + row * w + x0, u32(a.x));
    var scb = a.y;
    var scr = a.z;
    let x1 = x0 + 1u;
    if x1 < w {
        let b = lift(rd(job.offs.x + row * w + x1), cb, cr);
        wr(job.offs.x + row * w + x1, u32(b.x));
        scb = ceil((a.y + b.y) / 2.0);
        scr = ceil((a.z + b.z) / 2.0);
    }
    wr(job.offs.y + row * cw + c, u32(scb));
    wr(job.offs.z + row * cw + c, u32(scr));
    if job.size.w != 0u {
        wr(job.offs.w + row * w + x0, u32(rd(job.offs.w + row * w + x0) * 256.0));
        if x1 < w {
            wr(job.offs.w + row * w + x1, u32(rd(job.offs.w + row * w + x1) * 256.0));
        }
    }
}

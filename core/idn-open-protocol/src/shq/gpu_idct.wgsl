// SpeedHQ reconstruction on the GPU (perf queue 2b): pictures arrive as dequantised
// coefficients (shq::CoefPicture, read on the CPU) and their samples are made here, into
// the compositor's picture buffer, instead of on the CPU and uploaded.
//
// One workgroup makes one 8 × 8 block with the CPU's inverse DCT (shq/idct.rs): the same
// basis (computed on the CPU and passed in), the same sums in the same order, rounding
// half up after clamping to 0–255. Invocation (y, x) first puts coefficient 8·y + x in
// place, then makes row y's part of column x, then sample (y, x). `T` is f64, the CPU's
// precision, or f32 where the GPU has no f64 (a sample one step off now and then).

alias T = SCALAR;

struct Job {
    // The plane's width and height in samples, its blocks across and down.
    dims: vec4<u32>,
    // In `coefs`: the first word of the block table and of the values; in `pics`: the
    // plane's first byte; the dispatch's width in workgroups.
    offs: vec4<u32>,
    // Each sample row written x times (2: 4:2:0 chroma into 4:2:2 planes), down to y
    // rows; z bytes from one row to the next (0: the plane's width).
    rep: vec4<u32>,
};

@group(0) @binding(0) var<uniform> job: Job;
// basis[8·y + v] = B[y][v]; basis[64 + 8·u + x] = B[x][u] (the CPU's BASIS and BASIS_T).
@group(0) @binding(1) var<storage, read> basis: array<T, 128>;
@group(0) @binding(2) var<storage, read> coefs: array<u32>;
@group(0) @binding(3) var<storage, read_write> pics: array<atomic<u32>>;

var<workgroup> f: array<T, 64>;
var<workgroup> tmp: array<T, 64>;
var<workgroup> samples: array<u32, 64>;

// Value `i` of the plane's (16-bit, two to a word).
fn value(i: u32) -> T {
    let w = coefs[job.offs.y + (i >> 1u)];
    let half = select(w << 16u, w & 0xFFFF0000u, (i & 1u) == 1u);
    return T(bitcast<i32>(half) >> 16u);
}

@compute @workgroup_size(64)
fn main(
    @builtin(workgroup_id) wg: vec3<u32>,
    @builtin(local_invocation_index) t: u32,
) {
    let b = wg.x + wg.y * job.offs.w;
    let blocks = job.dims.z * job.dims.w;
    let at = job.offs.x + 3u * min(b, blocks - 1u);
    let start = coefs[at];
    let lo = coefs[at + 1u];
    let hi = coefs[at + 2u];
    let y = t / 8u;
    let x = t % 8u;
    // Coefficient t: its value, if the mask has it (the values before it are the bits
    // below it).
    var c = T(0);
    if t < 32u {
        if ((lo >> t) & 1u) == 1u {
            c = value(start + countOneBits(lo & ((1u << t) - 1u)));
        }
    } else if ((hi >> (t - 32u)) & 1u) == 1u {
        c = value(start + countOneBits(lo) + countOneBits(hi & ((1u << (t - 32u)) - 1u)));
    }
    f[t] = c;
    workgroupBarrier();
    // Row y's pass, tmp[y][x] = Σ_u B[x][u] · F[y][u], in increasing u, zeros left out.
    var r = T(0);
    for (var u = 0u; u < 8u; u++) {
        let cu = f[8u * y + u];
        if cu != T(0) {
            r = r + basis[64u + 8u * u + x] * cu;
        }
    }
    tmp[t] = r;
    workgroupBarrier();
    // Sample (y, x): the rows that have coefficients, in increasing v.
    var s = T(0);
    for (var v = 0u; v < 8u; v++) {
        let row = select((hi >> ((v - 4u) * 8u)) & 0xFFu, (lo >> (v * 8u)) & 0xFFu, v < 4u);
        if row != 0u {
            s = s + basis[8u * y + v] * tmp[8u * v + x];
        }
    }
    let cl = clamp(s, T(0), T(255));
    let i = u32(cl);
    // A flat block (no mask): its value.
    samples[t] = select(i + select(0u, 1u, cl - T(i) >= T(0.5)), start & 0xFFu, (lo | hi) == 0u);
    workgroupBarrier();
    // Four samples to a word: stored whole where the word is this block's alone, else
    // byte by byte into the cleared buffer.
    if b >= blocks || (x & 3u) != 0u {
        return;
    }
    let px = (b % job.dims.z) * 8u + x;
    let py = (b / job.dims.z) * 8u + y;
    if py >= job.dims.y || px >= job.dims.x {
        return;
    }
    let w = samples[t] | (samples[t + 1u] << 8u) | (samples[t + 2u] << 16u)
        | (samples[t + 3u] << 24u);
    for (var k = 0u; k < job.rep.x; k++) {
        let oy = py * job.rep.x + k;
        if oy >= job.rep.y {
            return;
        }
        let addr = job.offs.z + oy * select(job.dims.x, job.rep.z, job.rep.z != 0u) + px;
        if (addr & 3u) == 0u && px + 4u <= job.dims.x {
            atomicStore(&pics[addr >> 2u], w);
            continue;
        }
        for (var j = 0u; j < 4u; j++) {
            if px + j < job.dims.x {
                let a = addr + j;
                atomicOr(&pics[a >> 2u], samples[t + j] << ((a & 3u) * 8u));
            }
        }
    }
}

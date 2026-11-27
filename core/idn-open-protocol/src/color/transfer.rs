//! BT.2100's transfer functions, both ways: PQ (SMPTE ST 2084) and HLG, the BT.709 →
//! BT.2020 primaries (BT.2087) and BT.2020's luma weights. The one copy: the tone mapping
//! for SDR screens (`color`), the conversion to HDR (`to_hdr`) and the GPU's shaders
//! ([`wgsl`]: the same constants written as WGSL) all take them from here.

/// PQ's constants (ST 2084): m1.
pub const PQ_M1: f64 = 2610.0 / 16384.0;
/// PQ m2.
pub const PQ_M2: f64 = 2523.0 / 4096.0 * 128.0;
/// PQ c1.
pub const PQ_C1: f64 = 3424.0 / 4096.0;
/// PQ c2.
pub const PQ_C2: f64 = 2413.0 / 4096.0 * 32.0;
/// PQ c3.
pub const PQ_C3: f64 = 2392.0 / 4096.0 * 32.0;
/// HLG's constants (BT.2100): a.
pub const HLG_A: f64 = 0.17883277;
/// HLG b.
pub const HLG_B: f64 = 0.28466892;
/// HLG c.
pub const HLG_C: f64 = 0.55991073;
/// BT.2020's luma weights: Kr.
pub const KR_2020: f64 = 0.2627;
/// BT.2020 Kb.
pub const KB_2020: f64 = 0.0593;
/// BT.709 → BT.2020 primaries, linear light (BT.2087).
pub const M709_2020: [[f64; 3]; 3] = [
    [0.627_404, 0.329_283, 0.043_313],
    [0.069_097, 0.919_540, 0.011_362],
    [0.016_391, 0.088_013, 0.895_595],
];

/// ST 2084 EOTF: PQ signal 0–1 → nits.
pub fn pq_eotf(v: f64) -> f64 {
    let p = v.max(0.0).powf(1.0 / PQ_M2);
    ((p - PQ_C1).max(0.0) / (PQ_C2 - PQ_C3 * p)).powf(1.0 / PQ_M1) * 10000.0
}

/// ST 2084 inverse EOTF: nits → PQ signal 0–1.
pub fn pq_inverse_eotf(nits: f64) -> f64 {
    let y = (nits / 10000.0).clamp(0.0, 1.0).powf(PQ_M1);
    ((PQ_C1 + PQ_C2 * y) / (1.0 + PQ_C3 * y)).powf(PQ_M2)
}

/// HLG OETF: scene light 0–1 → signal 0–1.
pub fn hlg_oetf(e: f64) -> f64 {
    if e <= 1.0 / 12.0 {
        (3.0 * e.max(0.0)).sqrt()
    } else {
        HLG_A * (12.0 * e - HLG_B).ln() + HLG_C
    }
}

/// HLG inverse OETF: signal 0–1 → scene light 0–1.
pub fn hlg_inverse_oetf(v: f64) -> f64 {
    if v <= 0.5 {
        v * v / 3.0
    } else {
        (((v - HLG_C) / HLG_A).exp() + HLG_B) / 12.0
    }
}

/// The constants above as WGSL, for a shader to start with (`PQ_M1` …, `KR`, `KB`,
/// `M709_2020` as a mat3x3 of rows).
pub fn wgsl() -> String {
    let m = M709_2020;
    format!(
        "const PQ_M1: f32 = {PQ_M1:?};\nconst PQ_M2: f32 = {PQ_M2:?};\nconst PQ_C1: f32 = {PQ_C1:?};\n\
         const PQ_C2: f32 = {PQ_C2:?};\nconst PQ_C3: f32 = {PQ_C3:?};\nconst HLG_A: f32 = {HLG_A:?};\n\
         const HLG_B: f32 = {HLG_B:?};\nconst HLG_C: f32 = {HLG_C:?};\nconst KR: f32 = {KR_2020:?};\n\
         const KB: f32 = {KB_2020:?};\n\
         const M709_2020_R0: vec3<f32> = vec3<f32>({:?}, {:?}, {:?});\n\
         const M709_2020_R1: vec3<f32> = vec3<f32>({:?}, {:?}, {:?});\n\
         const M709_2020_R2: vec3<f32> = vec3<f32>({:?}, {:?}, {:?});\n",
        m[0][0], m[0][1], m[0][2], m[1][0], m[1][1], m[1][2], m[2][0], m[2][1], m[2][2]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_ways_meet() {
        for i in 0..=100 {
            let v = i as f64 / 100.0;
            // (PQ of 0 nits is 7e-7, not 0: the curve's own offset.)
            assert!((pq_inverse_eotf(pq_eotf(v)) - v).abs() < 1e-6, "PQ {v}");
            assert!((hlg_oetf(hlg_inverse_oetf(v)) - v).abs() < 1e-9, "HLG {v}");
        }
        // BT.2408's anchors: 203 nits at 58 % PQ; HLG's reference white at 75 %.
        assert!((pq_inverse_eotf(203.0) - 0.58).abs() < 0.005);
        assert!(wgsl().contains("const PQ_M1: f32 = 0.1593017578125;"));
    }
}

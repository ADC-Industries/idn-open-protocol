//! PCM samples as bytes and back: integers of 16, 24 (packed) or 32 bits, or 32-bit
//! float, little- or big-endian. One convention everywhere (a sound card's buffers, AES67,
//! WAV files, the site link): an integer of n bits reads as v / 2^(n−1), and a sample is
//! written as round(x · 2^(n−1)), clamped to the integer's range.

/// A sample layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pcm {
    /// 16-bit integers.
    I16,
    /// 24-bit integers, three bytes each.
    I24,
    /// 32-bit integers (also 24 bits left-justified in 32).
    I32,
    /// 32-bit float.
    F32,
}

impl Pcm {
    /// A layout from its bits and whether they are float (a WAVEFORMAT's), if one of these.
    pub fn of(bits: usize, float: bool) -> Option<Pcm> {
        match (float, bits) {
            (true, 32) => Some(Pcm::F32),
            (false, 16) => Some(Pcm::I16),
            (false, 24) => Some(Pcm::I24),
            (false, 32) => Some(Pcm::I32),
            _ => None,
        }
    }

    /// Bytes per sample.
    pub fn bytes(self) -> usize {
        match self {
            Pcm::I16 => 2,
            Pcm::I24 => 3,
            Pcm::I32 | Pcm::F32 => 4,
        }
    }
}

fn to_int(x: f32, full: f64) -> i64 {
    ((x as f64) * full).round().clamp(-full, full - 1.0) as i64
}

/// `bytes` read as samples (`big`: big-endian), appended to `out`. A partial sample at
/// the end is left out.
pub fn read(fmt: Pcm, big: bool, bytes: &[u8], out: &mut Vec<f32>) {
    out.reserve(bytes.len() / fmt.bytes());
    match fmt {
        Pcm::I16 => out.extend(bytes.as_chunks::<2>().0.iter().map(|b| {
            (if big { i16::from_be_bytes(*b) } else { i16::from_le_bytes(*b) }) as f32 / 32_768.0
        })),
        Pcm::I24 => out.extend(bytes.as_chunks::<3>().0.iter().map(|b| {
            let v = if big {
                i32::from_be_bytes([b[0], b[1], b[2], 0])
            } else {
                i32::from_le_bytes([0, b[0], b[1], b[2]])
            };
            (v >> 8) as f32 / 8_388_608.0
        })),
        Pcm::I32 => out.extend(bytes.as_chunks::<4>().0.iter().map(|b| {
            (if big { i32::from_be_bytes(*b) } else { i32::from_le_bytes(*b) }) as f64 as f32 / 2_147_483_648.0
        })),
        Pcm::F32 => out.extend(bytes.as_chunks::<4>().0.iter().map(|b| {
            if big { f32::from_be_bytes(*b) } else { f32::from_le_bytes(*b) }
        })),
    }
}

/// `samples` written as bytes (`big`: big-endian), appended to `out`.
pub fn write(fmt: Pcm, big: bool, samples: &[f32], out: &mut Vec<u8>) {
    out.reserve(samples.len() * fmt.bytes());
    for &x in samples {
        let x = if x.is_finite() { x } else { 0.0 };
        match fmt {
            Pcm::I16 => {
                let v = to_int(x, 32_768.0) as i16;
                out.extend_from_slice(&if big { v.to_be_bytes() } else { v.to_le_bytes() });
            }
            Pcm::I24 => {
                let v = to_int(x, 8_388_608.0) as i32;
                if big {
                    out.extend_from_slice(&v.to_be_bytes()[1..]);
                } else {
                    out.extend_from_slice(&v.to_le_bytes()[..3]);
                }
            }
            Pcm::I32 => {
                let v = to_int(x, 2_147_483_648.0) as i32;
                out.extend_from_slice(&if big { v.to_be_bytes() } else { v.to_le_bytes() });
            }
            Pcm::F32 => out.extend_from_slice(&if big { x.to_be_bytes() } else { x.to_le_bytes() }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_layout_round_trips() {
        let xs = [0.0f32, 0.5, -0.5, 0.999, -1.0, 1.0, 1.5, -2.0, f32::NAN];
        for fmt in [Pcm::I16, Pcm::I24, Pcm::I32, Pcm::F32] {
            for big in [false, true] {
                let mut b = Vec::new();
                write(fmt, big, &xs, &mut b);
                assert_eq!(b.len(), xs.len() * fmt.bytes());
                let mut back = Vec::new();
                read(fmt, big, &b, &mut back);
                let step = match fmt {
                    Pcm::I16 => 1.0 / 32_768.0,
                    Pcm::I24 => 1.0 / 8_388_608.0,
                    _ => 1e-6,
                };
                for (x, y) in xs.iter().zip(&back) {
                    let want = if x.is_nan() { 0.0 } else if fmt == Pcm::F32 { *x } else { x.clamp(-1.0, 1.0) };
                    assert!((want - y).abs() <= step, "{fmt:?} big={big}: {x} → {y}");
                }
            }
        }
        // Full scale: −1.0 is the integer's minimum, +1.0 one step under.
        let mut b = Vec::new();
        write(Pcm::I24, true, &[-1.0, 1.0], &mut b);
        assert_eq!(b, [0x80, 0, 0, 0x7f, 0xff, 0xff]);
    }
}

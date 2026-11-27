//! An ASIO driver's samples to and from f32 (spec/extensions/asio-interface.md §A.1):
//! the little-endian types; one channel's buffer at a time, non-interleaved as ASIO keeps
//! them.

// Used by the ASIO host only (Windows and Linux, with `asio`); tested everywhere.
#![cfg_attr(
    not(all(any(windows, target_os = "linux"), feature = "asio")),
    allow(dead_code)
)]
// The interface's own names (ST_Int16LSB, kAsio…), as cwASIO spells them.
#![allow(non_upper_case_globals)]

use idn_asio_abi::*;

/// Bytes a sample of `t` takes; `None`: a type not converted (big-endian, DSD).
pub fn size_of(t: SampleType) -> Option<usize> {
    match t {
        ST_Int16LSB => Some(2),
        ST_Int24LSB => Some(3),
        ST_Int32LSB | ST_Float32LSB | ST_Int32LSB16 | ST_Int32LSB18 | ST_Int32LSB20
        | ST_Int32LSB24 => Some(4),
        ST_Float64LSB => Some(8),
        _ => None,
    }
}

/// The full scale of an integer type (its significant bits).
fn scale(t: SampleType) -> f32 {
    match t {
        ST_Int16LSB | ST_Int32LSB16 => 32_768.0,
        ST_Int32LSB18 => 131_072.0,
        ST_Int32LSB20 => 524_288.0,
        ST_Int24LSB | ST_Int32LSB24 => 8_388_608.0,
        _ => 2_147_483_648.0,
    }
}

/// The significant bits of a 32-bit container type.
fn bits(t: SampleType) -> u32 {
    match t {
        ST_Int32LSB16 => 16,
        ST_Int32LSB18 => 18,
        ST_Int32LSB20 => 20,
        ST_Int32LSB24 => 24,
        _ => 32,
    }
}

fn finite(v: f32) -> f32 {
    if v.is_finite() { v } else { 0.0 }
}

/// Sample `k` of buffer `b` (of type `t`) as f32.
pub fn read(t: SampleType, b: &[u8], k: usize) -> f32 {
    match t {
        ST_Int16LSB => i16::from_le_bytes([b[2 * k], b[2 * k + 1]]) as f32 / scale(t),
        ST_Int24LSB => {
            let v = i32::from_le_bytes([0, b[3 * k], b[3 * k + 1], b[3 * k + 2]]) >> 8;
            v as f32 / scale(t)
        }
        // A driver's NaN or infinity: silence.
        ST_Float32LSB => finite(f32::from_le_bytes([
            b[4 * k],
            b[4 * k + 1],
            b[4 * k + 2],
            b[4 * k + 3],
        ])),
        ST_Float64LSB => {
            let mut a = [0u8; 8];
            a.copy_from_slice(&b[8 * k..8 * k + 8]);
            finite(f64::from_le_bytes(a) as f32)
        }
        _ => {
            let v = i32::from_le_bytes([b[4 * k], b[4 * k + 1], b[4 * k + 2], b[4 * k + 3]]);
            // A 32-bit container's N significant bits are at the bottom: the sign taken
            // from bit N-1 (a driver leaving the top bits zero isn't read as loud
            // positive values; bug run 2026-11-25).
            let n = bits(t);
            let v = if n < 32 {
                (v << (32 - n)) >> (32 - n)
            } else {
                v
            };
            v as f32 / scale(t)
        }
    }
}

/// `v` written as sample `k` of buffer `b` (of type `t`), clipped to full scale.
pub fn write(t: SampleType, b: &mut [u8], k: usize, v: f32) {
    let v = if v.is_finite() {
        v.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    match t {
        ST_Int16LSB => {
            let s = (v * scale(t)).round().clamp(-32_768.0, 32_767.0) as i16;
            b[2 * k..2 * k + 2].copy_from_slice(&s.to_le_bytes());
        }
        ST_Int24LSB => {
            let s = (v * scale(t)).round().clamp(-8_388_608.0, 8_388_607.0) as i32;
            b[3 * k..3 * k + 3].copy_from_slice(&s.to_le_bytes()[..3]);
        }
        ST_Float32LSB => b[4 * k..4 * k + 4].copy_from_slice(&v.to_le_bytes()),
        ST_Float64LSB => b[8 * k..8 * k + 8].copy_from_slice(&(v as f64).to_le_bytes()),
        _ => {
            let full = scale(t) as f64;
            let s = (v as f64 * full).round().clamp(-full, full - 1.0) as i32;
            b[4 * k..4 * k + 4].copy_from_slice(&s.to_le_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_type_round_trips() {
        for t in [
            ST_Int16LSB,
            ST_Int24LSB,
            ST_Int32LSB,
            ST_Float32LSB,
            ST_Float64LSB,
            ST_Int32LSB16,
            ST_Int32LSB18,
            ST_Int32LSB20,
            ST_Int32LSB24,
        ] {
            let n = size_of(t).unwrap();
            let mut b = vec![0u8; n * 5];
            let vals = [0.0f32, 0.5, -0.5, 1.0, -1.0];
            for (k, v) in vals.iter().enumerate() {
                write(t, &mut b, k, *v);
            }
            for (k, v) in vals.iter().enumerate() {
                let got = read(t, &b, k);
                assert!((got - v).abs() < 1e-4, "type {t}: {v} came back {got}");
            }
            // Over full scale: clipped.
            write(t, &mut b, 0, 3.0);
            assert!(read(t, &b, 0) > 0.999, "type {t}");
        }
        // A 24-bit sample's bytes: low first, sign from the top byte.
        let mut b = [0u8; 3];
        write(ST_Int24LSB, &mut b, 0, -0.5);
        assert_eq!(b, [0x00, 0x00, 0xC0]);
        assert!(size_of(ST_Int16MSB).is_none());
        // A 16-bit container whose top bits a driver left zero: still negative.
        let b = 0x0000_C000u32.to_le_bytes();
        assert!((read(ST_Int32LSB16, &b, 0) + 0.5).abs() < 1e-4);
        // A NaN from a float driver: silence.
        assert_eq!(read(ST_Float32LSB, &f32::NAN.to_le_bytes(), 0), 0.0);
    }
}

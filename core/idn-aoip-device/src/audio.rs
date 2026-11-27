//! The media packet (§5): `0x02`, the timestamp as seconds and the sample within the
//! second, then the frames interleaved, each sample big-endian signed, the top bits of a
//! left-justified 32-bit sample.

/// The header's length (§5.1).
pub const HEADER: usize = 9;

/// The timestamp's lead: a packet's first frame stamped this much earlier (§5.3:
/// 0.5 ms, 24 samples at 48 kHz).
pub fn lead(rate: u32) -> u64 {
    u64::from(rate) / 2000
}

/// A timestamp (media-clock samples) as its two fields (§5.3).
pub fn split(t: u64, rate: u32) -> (u32, u32) {
    let r = u64::from(rate.max(1));
    ((t / r) as u32, (t % r) as u32)
}

/// The fields back to a timestamp, near `now` (the seconds field holds 32 bits: the
/// whole count is taken nearest `now`'s).
pub fn join(secs: u32, sample: u32, rate: u32, now: u64) -> u64 {
    let r = u64::from(rate.max(1));
    let low = u64::from(secs) * r + u64::from(sample);
    let span = (1u64 << 32) * r;
    let base = now - now % span;
    [base.wrapping_sub(span), base, base + span]
        .into_iter()
        .map(|b| b.wrapping_add(low))
        .min_by_key(|&t| t.abs_diff(now))
        .unwrap_or(low)
}

/// Encodes one packet: `frames` frames of `channels` channels from `samples` (interleaved,
/// f32 in −1..1; a missing slot is silence), `bytes` per sample (2, 3 or 4), stamped `t`.
pub fn encode(
    t: u64,
    rate: u32,
    bytes: usize,
    channels: usize,
    samples: &[f32],
    out: &mut Vec<u8>,
) {
    let (s, i) = split(t, rate);
    out.clear();
    out.reserve(HEADER + samples.len() * bytes);
    out.push(0x02);
    out.extend_from_slice(&s.to_be_bytes());
    out.extend_from_slice(&i.to_be_bytes());
    let frames = samples.len() / channels.max(1);
    for v in &samples[..frames * channels] {
        let w = to_i32(*v).to_be_bytes();
        out.extend_from_slice(&w[..bytes]);
    }
}

/// A sample as a left-justified 32-bit value.
pub fn to_i32(v: f32) -> i32 {
    let v = if v.is_finite() {
        v.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    (f64::from(v) * 2_147_483_648.0).clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

/// A packet's timestamp fields and audio, `None` for one shorter than its header (§5.1).
pub fn header(d: &[u8]) -> Option<(u32, u32, &[u8])> {
    if d.len() < HEADER {
        return None;
    }
    let s = u32::from_be_bytes(d[1..5].try_into().ok()?);
    let i = u32::from_be_bytes(d[5..9].try_into().ok()?);
    Some((s, i, &d[HEADER..]))
}

/// Frames in a packet's audio (§5.1: from its size; a partial frame left out).
pub fn frames(audio: &[u8], channels: usize, bytes: usize) -> usize {
    audio.len() / (channels * bytes).max(1)
}

/// Sample `ch` of frame `f` as f32.
pub fn sample(audio: &[u8], channels: usize, bytes: usize, f: usize, ch: usize) -> f32 {
    let at = (f * channels + ch) * bytes;
    let mut w = [0u8; 4];
    w[..bytes].copy_from_slice(&audio[at..at + bytes]);
    (f64::from(i32::from_be_bytes(w)) / 2_147_483_648.0) as f32
}

/// One slot's samples from a packet's audio, as left-justified 32-bit values (the rings'
/// form: no float in between).
pub fn slot_i32(audio: &[u8], channels: usize, bytes: usize, slot: usize, out: &mut Vec<i32>) {
    out.clear();
    let stride = channels * bytes;
    if stride == 0 || slot >= channels {
        return;
    }
    let frames = audio.len() / stride;
    out.reserve(frames);
    let mut at = slot * bytes;
    match bytes {
        3 => {
            for _ in 0..frames {
                let b = &audio[at..at + 3];
                out.push(i32::from_be_bytes([b[0], b[1], b[2], 0]));
                at += stride;
            }
        }
        2 => {
            for _ in 0..frames {
                out.push(i32::from_be_bytes([audio[at], audio[at + 1], 0, 0]));
                at += stride;
            }
        }
        4 => {
            for _ in 0..frames {
                out.push(i32::from_be_bytes([
                    audio[at],
                    audio[at + 1],
                    audio[at + 2],
                    audio[at + 3],
                ]));
                at += stride;
            }
        }
        // No other width is set up (receiver::bits); nothing rather than a read past
        // the packet.
        _ => {}
    }
}

/// Encodes one packet from left-justified 32-bit samples, interleaved (`channels` a frame).
pub fn encode_i32(t: u64, rate: u32, bytes: usize, samples: &[i32], out: &mut Vec<u8>) {
    let (s, i) = split(t, rate);
    out.clear();
    out.reserve(HEADER + samples.len() * bytes);
    out.push(0x02);
    out.extend_from_slice(&s.to_be_bytes());
    out.extend_from_slice(&i.to_be_bytes());
    // The whole packet sized once, each sample's top bytes copied into its place. A
    // width known at compile time: a run-time one made a memmove call per sample (profile
    // round 14: a quarter of the send thread's own time).
    out.resize(HEADER + samples.len() * bytes, 0);
    let body = &mut out[HEADER..];
    match bytes {
        3 => put::<3>(body, samples),
        4 => put::<4>(body, samples),
        2 => put::<2>(body, samples),
        _ => {
            for (dst, v) in body.chunks_exact_mut(bytes.max(1)).zip(samples) {
                let n = dst.len().min(4);
                dst[..n].copy_from_slice(&v.to_be_bytes()[..n]);
            }
        }
    }
}

/// Each sample's top `N` bytes, big-endian, into `body`.
fn put<const N: usize>(body: &mut [u8], samples: &[i32]) {
    let (chunks, _) = body.as_chunks_mut::<N>();
    for (dst, v) in chunks.iter_mut().zip(samples) {
        dst.copy_from_slice(&v.to_be_bytes()[..N]);
    }
}

/// The next timestamp a flow sends from `t`, aligned up to a multiple of `fpp` (§5.3).
pub fn align(t: u64, fpp: u64) -> u64 {
    t.div_ceil(fpp.max(1)) * fpp.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_round_trip() {
        let rate = 48_000;
        let t = 1_700_000_000u64 * 48_000 + 1234;
        let samples = [0.5f32, -0.25, 0.0, 1.0];
        let mut p = Vec::new();
        encode(t, rate, 3, 2, &samples, &mut p);
        assert_eq!(p[0], 0x02);
        assert_eq!(p.len(), 9 + 4 * 3);
        // 0.5 in 24 bits: 0x40 00 00.
        assert_eq!(&p[9..12], &[0x40, 0, 0]);
        // −0.25: 0xE0 00 00.
        assert_eq!(&p[12..15], &[0xE0, 0, 0]);
        let (s, i, audio) = header(&p).unwrap();
        assert_eq!(join(s, i, rate, t + 100), t);
        assert_eq!(frames(audio, 2, 3), 2);
        assert_eq!(sample(audio, 2, 3, 0, 0), 0.5);
        assert_eq!(sample(audio, 2, 3, 0, 1), -0.25);
        assert!(sample(audio, 2, 3, 1, 1) > 0.9999);
        assert_eq!(header(&p[..8]), None);
    }

    #[test]
    fn integer_paths_match_the_float_ones() {
        let samples = [0.5f32, -0.25, 0.75, -1.0, 0.125, 0.0];
        let mut a = Vec::new();
        encode(9, 48_000, 3, 2, &samples, &mut a);
        let ints: Vec<i32> = samples.iter().map(|&x| to_i32(x)).collect();
        let mut b = Vec::new();
        encode_i32(9, 48_000, 3, &ints, &mut b);
        assert_eq!(a, b);
        let mut slot = Vec::new();
        slot_i32(&a[HEADER..], 2, 3, 1, &mut slot);
        assert_eq!(slot.len(), 3);
        for (f, v) in slot.iter().enumerate() {
            assert_eq!(
                (*v as f64 / 2_147_483_648.0) as f32,
                sample(&a[HEADER..], 2, 3, f, 1)
            );
        }
    }

    #[test]
    fn timestamps() {
        assert_eq!(lead(48_000), 24);
        assert_eq!(split(48_000 * 3 + 7, 48_000), (3, 7));
        assert_eq!(align(100, 32), 128);
        assert_eq!(align(128, 32), 128);
        // The seconds field wraps at 2^32 s: rebuilt nearest now.
        let rate = 48_000;
        let t = (1u64 << 32) * 48_000 + 5;
        let (s, i) = split(t, rate);
        assert_eq!(s, 0);
        assert_eq!(join(s, i, rate, t - 10), t);
        // 16 and 32 bits.
        let mut p = Vec::new();
        encode(0, rate, 2, 1, &[-1.0], &mut p);
        assert_eq!(&p[9..], &[0x80, 0x00]);
        encode(0, rate, 4, 1, &[0.5], &mut p);
        assert_eq!(&p[9..], &[0x40, 0, 0, 0]);
    }
}

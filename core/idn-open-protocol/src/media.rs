//! Video and audio message extension headers and the `fowt` audio payload (spec §2.4, §2.5).

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaError {
    /// Extension header shorter than its fixed part.
    ShortHeader,
    /// Field id above 3 (§2.4).
    BadFieldId,
    /// `data_len` does not match the channel count, scales and sample count (§2.5).
    BadAudioLength,
    /// Audio FourCC other than `fowt`.
    UnsupportedAudio,
}

impl fmt::Display for MediaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for MediaError {}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn i64_at(b: &[u8], o: usize) -> i64 {
    i64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Per-frame metadata after the fixed header: XML up to the first NUL. A missing NUL is
/// tolerated (the text runs to the end of the header); empty means none.
fn metadata_at(b: &[u8], o: usize) -> Option<String> {
    let rest = b.get(o..)?;
    let end = rest.iter().position(|&c| c == 0).unwrap_or(rest.len());
    (end > 0).then(|| String::from_utf8_lossy(&rest[..end]).into_owned())
}

fn push_metadata(out: &mut Vec<u8>, md: &Option<String>) {
    if let Some(s) = md {
        out.extend(s.bytes().filter(|&c| c != 0));
    }
    out.push(0);
}

/// Information that comes with each video frame.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoHeader {
    /// The codec code, e.g. `*b"SHQ2"`, `*b"H264"` or `*b"HEVC"`.
    pub fourcc: [u8; 4],
    /// Width in pixels.
    pub width: u32,
    /// Height of this picture: the frame height, or the field height for a field.
    pub height: u32,
    /// Frame rate numerator, e.g. 30000.
    pub frame_rate_n: u32,
    /// Frame rate denominator, e.g. 1001.
    pub frame_rate_d: u32,
    /// Display aspect ratio, e.g. 1.778 for 16:9.
    pub aspect_ratio: f32,
    /// 0: progressive frame; 1: first field (even lines); 2: second field (odd lines).
    pub field_id: u32,
    /// 1: the full-quality stream; 0: the low-bandwidth stream.
    pub stream: u32,
    /// The source's timecode, in 100 ns units.
    pub timecode: i64,
    /// When the source made the frame, in 100 ns units since the Unix epoch.
    pub timestamp: i64,
    /// Per-frame XML metadata from the source, e.g. its colour description.
    pub metadata: Option<String>,
}

impl VideoHeader {
    pub const FIXED_LEN: usize = 56;

    pub fn parse(ext: &[u8]) -> Result<Self, MediaError> {
        if ext.len() < Self::FIXED_LEN {
            return Err(MediaError::ShortHeader);
        }
        let field_id = u32_at(ext, 24);
        if field_id > 3 {
            return Err(MediaError::BadFieldId);
        }
        Ok(VideoHeader {
            fourcc: ext[0..4].try_into().unwrap(),
            width: u32_at(ext, 4),
            height: u32_at(ext, 8),
            frame_rate_n: u32_at(ext, 12),
            frame_rate_d: u32_at(ext, 16),
            aspect_ratio: f32::from_le_bytes(ext[20..24].try_into().unwrap()),
            field_id,
            stream: u32_at(ext, 28),
            // Bytes 32..40 are unknown: ignored on read.
            timecode: i64_at(ext, 40),
            timestamp: i64_at(ext, 48),
            metadata: metadata_at(ext, Self::FIXED_LEN),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::FIXED_LEN + 1);
        out.extend_from_slice(&self.fourcc);
        for v in [
            self.width,
            self.height,
            self.frame_rate_n,
            self.frame_rate_d,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&self.aspect_ratio.to_le_bytes());
        out.extend_from_slice(&self.field_id.to_le_bytes());
        out.extend_from_slice(&self.stream.to_le_bytes());
        out.extend_from_slice(&[0; 8]);
        out.extend_from_slice(&self.timecode.to_le_bytes());
        out.extend_from_slice(&self.timestamp.to_le_bytes());
        push_metadata(&mut out, &self.metadata);
        out
    }

    /// Parameters for the SpeedHQ decoder (chapter 3).
    pub fn shq_params(&self) -> crate::shq::VideoParams {
        crate::shq::VideoParams {
            fourcc: self.fourcc,
            width: self.width,
            height: self.height,
            field_id: self.field_id,
        }
    }
}

/// Information that comes with each block of audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioHeader {
    /// The codec code: `*b"fowt"` (standard), `*b"IDF3"` (lossless) or `[0xff, 0, 0, 0]`
    /// (AAC, from NDI|HX sources).
    pub fourcc: [u8; 4],
    /// Samples per channel in this block.
    pub samples: u32,
    /// Channel count.
    pub channels: u32,
    /// Samples per second, e.g. 48000.
    pub sample_rate: u32,
    /// The source's timecode, in 100 ns units.
    pub timecode: i64,
    /// When the source made the audio, in 100 ns units since the Unix epoch.
    pub timestamp: i64,
    /// Per-block XML metadata from the source, if any.
    pub metadata: Option<String>,
}

impl AudioHeader {
    pub const FIXED_LEN: usize = 32;
    pub const FOWT: [u8; 4] = *b"fowt";

    pub fn parse(ext: &[u8]) -> Result<Self, MediaError> {
        if ext.len() < Self::FIXED_LEN {
            return Err(MediaError::ShortHeader);
        }
        Ok(AudioHeader {
            fourcc: ext[0..4].try_into().unwrap(),
            samples: u32_at(ext, 4),
            channels: u32_at(ext, 8),
            sample_rate: u32_at(ext, 12),
            timecode: i64_at(ext, 16),
            timestamp: i64_at(ext, 24),
            metadata: metadata_at(ext, Self::FIXED_LEN),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::FIXED_LEN + 1);
        out.extend_from_slice(&self.fourcc);
        for v in [self.samples, self.channels, self.sample_rate] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&self.timecode.to_le_bytes());
        out.extend_from_slice(&self.timestamp.to_le_bytes());
        push_metadata(&mut out, &self.metadata);
        out
    }
}

/// The sample rates audio is taken at. A source's header says its rate, and everything
/// after converts from it: 1 Hz made a 48 kHz output's resampler ask for ~400 MB.
pub const AUDIO_RATES: std::ops::RangeInclusive<u32> = 8_000..=384_000;

/// Most samples per channel in one audio message: over a second at 192 kHz (senders
/// send about one video frame's worth).
pub const MAX_AUDIO_SAMPLES: usize = 1 << 18;
/// Most samples in one audio message, all channels together (16 MB of f32: a second of
/// 64 channels at 48 kHz, of 16 at 192 kHz). Silent channels cost a message nothing, so
/// this is what a message of a few hundred bytes can make a receiver allocate and clear
/// (it was 64 MB, deep bug run 2026-11-09).
pub const MAX_AUDIO_TOTAL: usize = 1 << 22;

/// Rejects channel and sample counts beyond the limits before anything is allocated.
/// Silent channels take no payload bytes, so the length checks alone don't bound the
/// output: a header claiming 2^31 samples of silence came in an 8-byte payload.
fn check_audio_size(c: usize, n: usize, max_channels: usize) -> Result<(), MediaError> {
    if c > max_channels || n > MAX_AUDIO_SAMPLES || c * n > MAX_AUDIO_TOTAL {
        return Err(MediaError::BadAudioLength);
    }
    Ok(())
}

/// Decodes a `fowt` payload to planar float32 audio, one `Vec` per channel (§2.5).
pub fn fowt_decode(h: &AudioHeader, payload: &[u8]) -> Result<Vec<Vec<f32>>, MediaError> {
    if h.fourcc != AudioHeader::FOWT {
        return Err(MediaError::UnsupportedAudio);
    }
    let (c, n) = (h.channels as usize, h.samples as usize);
    check_audio_size(c, n, 1024)?;
    let scales_len = c.checked_mul(4).ok_or(MediaError::BadAudioLength)?;
    let scales: Vec<f32> = payload
        .get(..scales_len)
        .ok_or(MediaError::BadAudioLength)?
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect();
    let active: Vec<usize> = (0..c).filter(|&i| scales[i] != 0.0).collect();
    let expect = (active.len() as u64) * 2 * n as u64 + scales_len as u64;
    if payload.len() as u64 != expect {
        return Err(MediaError::BadAudioLength);
    }
    let mut out = vec![vec![0f32; n]; c];
    let samples = &payload[scales_len..];
    for (a, &ch) in active.iter().enumerate() {
        // A NaN, infinite or negative scale is active (its samples are in the interleave)
        // but is output as silence (§2.5).
        if !(scales[ch].is_finite() && scales[ch] > 0.0) {
            continue;
        }
        let k = scales[ch] / 32767.0;
        for (t, o) in out[ch].iter_mut().enumerate() {
            let at = 2 * (t * active.len() + a);
            *o = i16::from_le_bytes([samples[at], samples[at + 1]]) as f32 * k;
        }
    }
    Ok(out)
}

/// Encodes planar float32 audio (all channels the same length) as a `fowt` payload.
pub fn fowt_encode<C: AsRef<[f32]>>(channels: &[C]) -> Vec<u8> {
    let n = channels.first().map_or(0, |c| c.as_ref().len());
    // The peak absolute value; NaN samples are ignored, so an all-NaN channel is silent.
    let scales: Vec<f32> = channels
        .iter()
        .map(|c| c.as_ref().iter().fold(0f32, |m, &x| m.max(x.abs())))
        .collect();
    let active: Vec<usize> = (0..channels.len()).filter(|&i| scales[i] != 0.0).collect();
    let mut out = Vec::with_capacity(4 * channels.len() + 2 * active.len() * n);
    for s in &scales {
        out.extend_from_slice(&s.to_le_bytes());
    }
    let mults: Vec<f32> = active.iter().map(|&ch| 32767.0 / scales[ch]).collect();
    // `t` is the sample index across all active channels, not a channel index.
    #[allow(clippy::needless_range_loop)]
    for t in 0..n {
        for (a, &ch) in active.iter().enumerate() {
            let q = (channels[ch].as_ref()[t] * mults[a] + 0.5).trunc() as i16;
            out.extend_from_slice(&q.to_le_bytes());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_header_roundtrip() {
        let h = VideoHeader {
            fourcc: *b"SHQ2",
            width: 1920,
            height: 1080,
            frame_rate_n: 30000,
            frame_rate_d: 1001,
            aspect_ratio: 16.0 / 9.0,
            field_id: 0,
            stream: 1,
            timecode: 123,
            timestamp: -5,
            metadata: Some("<m/>".into()),
        };
        let b = h.encode();
        assert_eq!(b.len(), 56 + 5);
        assert_eq!(VideoHeader::parse(&b).unwrap(), h);
        let mut none = h.clone();
        none.metadata = None;
        assert_eq!(none.encode().len(), 57);
    }

    #[test]
    fn silent_audio_cannot_claim_huge_sizes() {
        // 8 bytes of zero scales: two silent channels, so the length check passes for
        // any sample count. Must be refused, not allocated (2 × 8 GiB here).
        let h = |fourcc, samples, channels| AudioHeader {
            fourcc,
            samples,
            channels,
            sample_rate: 48000,
            timecode: 0,
            timestamp: 0,
            metadata: None,
        };
        assert_eq!(
            fowt_decode(&h(AudioHeader::FOWT, 0x7fff_ffff, 2), &[0; 8]),
            Err(MediaError::BadAudioLength)
        );
        assert_eq!(
            fowt_decode(&h(AudioHeader::FOWT, 1600, 100_000), &[0; 400_000]),
            Err(MediaError::BadAudioLength)
        );
        // IDF3 with an all-zero bitmap: 256 silent channels of 2^32 − 1 samples.
        assert_eq!(
            idf3_decode(&h(FOURCC_IDF3, u32::MAX, 256), &[0; 32]),
            Err(MediaError::BadAudioLength)
        );
        // Ordinary sizes still decode.
        assert_eq!(
            fowt_decode(&h(AudioHeader::FOWT, 1602, 2), &[0; 8]).unwrap(),
            vec![vec![0.0; 1602]; 2]
        );
    }

    #[test]
    fn audio_header_roundtrip() {
        let h = AudioHeader {
            fourcc: AudioHeader::FOWT,
            samples: 1602,
            channels: 2,
            sample_rate: 48000,
            timecode: 1,
            timestamp: 2,
            metadata: None,
        };
        assert_eq!(AudioHeader::parse(&h.encode()).unwrap(), h);
    }

    #[test]
    fn fowt_extremes_and_silence() {
        // §2.5: +s encodes as 32767, -s as -32766; a zero-scale channel is left out.
        let chans = vec![vec![0.5, -0.5, 0.25], vec![0.0; 3], vec![f32::NAN; 3]];
        let p = fowt_encode(&chans);
        assert_eq!(p.len(), 4 * 3 + 2 * 3);
        let q: Vec<i16> = p[12..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(&q[..2], &[32767, -32766]);
        let h = AudioHeader {
            fourcc: AudioHeader::FOWT,
            samples: 3,
            channels: 3,
            sample_rate: 48000,
            timecode: 0,
            timestamp: 0,
            metadata: None,
        };
        let d = fowt_decode(&h, &p).unwrap();
        assert_eq!(d[0][0], 0.5);
        assert!(d[1].iter().chain(&d[2]).all(|&x| x == 0.0));
        assert!((d[0][2] - 0.25).abs() < 1e-4);
        assert_eq!(
            fowt_decode(&h, &p[..p.len() - 1]),
            Err(MediaError::BadAudioLength)
        );
    }

    /// §2.5: −0 is silent (no samples); NaN, ±inf and negative scales are active but
    /// output silence.
    #[test]
    fn fowt_odd_scales() {
        let scales = [-0.0f32, f32::NAN, f32::INFINITY, -1.0, 1.0];
        let mut p: Vec<u8> = scales.iter().flat_map(|s| s.to_le_bytes()).collect();
        // One sample per active channel (4 active: NaN, inf, -1, 1).
        for q in [100i16, 200, 300, 32767] {
            p.extend_from_slice(&q.to_le_bytes());
        }
        let h = AudioHeader {
            fourcc: AudioHeader::FOWT,
            samples: 1,
            channels: 5,
            sample_rate: 48000,
            timecode: 0,
            timestamp: 0,
            metadata: None,
        };
        let d = fowt_decode(&h, &p).unwrap();
        assert_eq!(
            d.iter().map(|c| c[0]).collect::<Vec<_>>(),
            vec![0.0, 0.0, 0.0, 0.0, 1.0]
        );
    }

    #[test]
    fn fowt_encode_nan_sample_in_active_channel_is_zero() {
        let p = fowt_encode(&[vec![1.0, f32::NAN]]);
        assert_eq!(&p[4..], &[0xff, 0x7f, 0, 0]);
    }
}

/// FourCC of lossless float audio (IDN extension audio-f32/1, §E.6).
pub const FOURCC_IDF3: [u8; 4] = *b"IDF3";

/// Encodes planar float audio as an `IDF3` payload: an active-channel bitmap (u32 LE
/// words, bit c = channel c present), then each active channel's samples as f32 LE. A
/// channel whose samples are all exactly +0.0 is inactive; −0.0 and NaN are sent, so the
/// round trip is bit-exact (§E.6).
pub fn idf3_encode<C: AsRef<[f32]>>(channels: &[C]) -> Vec<u8> {
    let n = channels.first().map_or(0, |c| c.as_ref().len());
    let words = channels.len().div_ceil(32);
    let active: Vec<bool> = channels
        .iter()
        .map(|c| c.as_ref().iter().any(|x| x.to_bits() != 0))
        .collect();
    let mut out = Vec::with_capacity(4 * words + 4 * n * active.iter().filter(|&&a| a).count());
    for w in 0..words {
        let mut m = 0u32;
        for b in 0..32 {
            if active.get(32 * w + b).copied().unwrap_or(false) {
                m |= 1 << b;
            }
        }
        out.extend_from_slice(&m.to_le_bytes());
    }
    for (c, on) in channels.iter().zip(&active) {
        if *on {
            for x in c.as_ref() {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
    }
    out
}

/// Decodes an `IDF3` payload to planar float audio (§E.6). The length must match the
/// bitmap exactly.
pub fn idf3_decode(h: &AudioHeader, payload: &[u8]) -> Result<Vec<Vec<f32>>, MediaError> {
    let (c, n) = (h.channels as usize, h.samples as usize);
    check_audio_size(c, n, 256)?;
    let words = c.div_ceil(32);
    let bitmap = payload.get(..4 * words).ok_or(MediaError::BadAudioLength)?;
    let on = |ch: usize| {
        u32::from_le_bytes(bitmap[4 * (ch / 32)..4 * (ch / 32) + 4].try_into().unwrap())
            >> (ch % 32)
            & 1
            == 1
    };
    let active = (0..c).filter(|&ch| on(ch)).count();
    if payload.len() as u64 != 4 * words as u64 + 4 * (n as u64) * active as u64 {
        return Err(MediaError::BadAudioLength);
    }
    let mut pos = 4 * words;
    Ok((0..c)
        .map(|ch| {
            if !on(ch) {
                return vec![0.0; n];
            }
            let v = payload[pos..pos + 4 * n]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect();
            pos += 4 * n;
            v
        })
        .collect())
}

#[cfg(test)]
mod idf3_tests {
    use super::*;

    #[test]
    fn idf3_is_bit_exact_and_skips_silence() {
        let chans = vec![
            vec![0.1, -0.0, f32::NAN, 1e-30],
            vec![0.0; 4],
            vec![-1.5, 2.0, 0.0, 3.25],
        ];
        let p = idf3_encode(&chans);
        assert_eq!(p.len(), 4 + 4 * 4 * 2);
        assert_eq!(u32::from_le_bytes(p[..4].try_into().unwrap()), 0b101);
        let h = AudioHeader {
            fourcc: FOURCC_IDF3,
            samples: 4,
            channels: 3,
            sample_rate: 48000,
            timecode: 0,
            timestamp: 0,
            metadata: None,
        };
        let d = idf3_decode(&h, &p).unwrap();
        for (a, b) in chans.iter().flatten().zip(d.iter().flatten()) {
            assert_eq!(a.to_bits(), b.to_bits());
        }
        assert!(idf3_decode(&h, &p[..p.len() - 1]).is_err());
        // 40 channels need two bitmap words.
        let many: Vec<Vec<f32>> = (0..40).map(|c| vec![c as f32; 2]).collect();
        let h40 = AudioHeader {
            channels: 40,
            samples: 2,
            ..h
        };
        assert_eq!(idf3_decode(&h40, &idf3_encode(&many)).unwrap(), {
            let mut m = many.clone();
            m[0] = vec![0.0; 2]; // channel 0 is all +0.0, so inactive and decoded as zeros
            m
        });
    }
}

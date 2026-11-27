//! The flow's coding (spec/extensions/idn-iem.md §I.3): each channel its own Opus stream,
//! mono, 48 kHz, in frames of [`crate::FRAME_SAMPLES`] (5 ms), restricted low delay (the
//! CELT layer alone: 2.5 ms look-ahead), [`crate::BITRATE`] constrained VBR. Talkback the
//! other way is the same coding, frames up to 20 ms.

use opusic_c::{Application, Bitrate, Channels, SampleRate};

/// Encodes one channel.
pub struct Encoder {
    enc: opusic_c::Encoder,
    frame: usize,
    out: Vec<u8>,
}

impl Encoder {
    /// For frames of `frame` samples at 48 kHz (120, 240, 480 or 960) at `bitrate` bit/s.
    pub fn new(frame: usize, bitrate: u32) -> Result<Encoder, String> {
        let mut enc =
            opusic_c::Encoder::new(Channels::Mono, SampleRate::Hz48000, Application::LowDelay)
                .map_err(|e| format!("opus encoder: {e:?}"))?;
        let set = |r: Result<(), opusic_c::ErrorCode>| r.map_err(|e| format!("opus: {e:?}"));
        set(enc.set_bitrate(Bitrate::Value(bitrate)))?;
        set(enc.set_vbr(true))?;
        set(enc.set_vbr_constraint(true))?;
        // Middle complexity: 24 channels within a fraction of a core.
        set(enc.set_complexity(5))?;
        Ok(Encoder {
            enc,
            frame,
            out: vec![0; crate::wire::MAX_FRAME],
        })
    }

    /// One frame (exactly the frame's samples, −1…+1) as an Opus packet.
    pub fn encode(&mut self, pcm: &[f32]) -> Result<&[u8], String> {
        if pcm.len() != self.frame {
            return Err(format!(
                "a frame is {} samples, not {}",
                self.frame,
                pcm.len()
            ));
        }
        let n = self
            .enc
            .encode_float_to_slice(pcm, &mut self.out)
            .map_err(|e| format!("opus encode: {e:?}"))?;
        Ok(&self.out[..n])
    }

    /// Its look-ahead, samples at 48 kHz (part of the flow's latency).
    pub fn look_ahead(&mut self) -> usize {
        self.enc.get_look_ahead().map_or(120, |v| v as usize)
    }
}

/// Decodes one channel, concealing lost frames.
pub struct Decoder {
    dec: opusic_c::Decoder,
    pcm: Vec<f32>,
}

impl Decoder {
    pub fn new() -> Result<Decoder, String> {
        Ok(Decoder {
            dec: opusic_c::Decoder::new(Channels::Mono, SampleRate::Hz48000)
                .map_err(|e| format!("opus decoder: {e:?}"))?,
            pcm: vec![0.0; crate::MAX_TALK_SAMPLES * 6],
        })
    }

    /// A frame's samples at 48 kHz; `None` for a frame that was lost (`samples` long: the
    /// decoder conceals it from what came before).
    pub fn decode(&mut self, frame: Option<&[u8]>, samples: usize) -> Result<&[f32], String> {
        let (input, room) = match frame {
            Some(f) => (f, self.pcm.len()),
            None => (&[][..], samples.min(self.pcm.len())),
        };
        let n = self
            .dec
            .decode_float_to_slice(input, &mut self.pcm[..room], false)
            .map_err(|e| format!("opus decode: {e:?}"))?;
        Ok(&self.pcm[..n])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FRAME_SAMPLES;

    fn sine(f: f32, n: usize, start: usize) -> Vec<f32> {
        (start..start + n)
            .map(|i| 0.5 * (2.0 * std::f32::consts::PI * f * i as f32 / 48_000.0).sin())
            .collect()
    }

    /// A 1 kHz tone through the coding comes back as the tone (after the codec's delay),
    /// within a small error; a lost frame is concealed (no silence, no error).
    #[test]
    fn opus_round_trip() {
        let mut enc = Encoder::new(FRAME_SAMPLES, crate::BITRATE).unwrap();
        let mut dec = Decoder::new().unwrap();
        let delay = enc.look_ahead();
        let total = FRAME_SAMPLES * 200;
        let src = sine(1000.0, total, 0);
        let mut out = Vec::new();
        for (k, f) in src.chunks(FRAME_SAMPLES).enumerate() {
            let pkt = enc.encode(f).unwrap().to_vec();
            assert!(pkt.len() < 100, "{} bytes at 64 kbit/s, 5 ms", pkt.len());
            let lost = k == 150;
            let pcm = dec
                .decode((!lost).then_some(&pkt[..]), FRAME_SAMPLES)
                .unwrap();
            assert_eq!(pcm.len(), FRAME_SAMPLES);
            if lost {
                assert!(pcm.iter().any(|v| v.abs() > 0.01), "concealed, not silent");
            }
            out.extend_from_slice(pcm);
        }
        // Compared over a stretch well clear of the start and the lost frame.
        let (a, b) = (FRAME_SAMPLES * 20, FRAME_SAMPLES * 140);
        let mut err = 0.0f64;
        let mut sig = 0.0f64;
        for i in a..b {
            let d = f64::from(out[i] - src[i - delay]);
            err += d * d;
            sig += f64::from(src[i - delay]).powi(2);
        }
        let snr = 10.0 * (sig / err.max(1e-12)).log10();
        assert!(snr > 20.0, "SNR {snr:.1} dB");
        assert!(enc.encode(&[0.0; 10]).is_err());
    }
}

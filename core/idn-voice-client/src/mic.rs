//! The microphone's side: PCM from the app in pieces of any length, cut into 20 ms frames
//! at its own rate and coded; nothing kept while not talking.

use crate::codec::Encoder;
use std::io;

pub(crate) struct Mic {
    enc: Option<Encoder>,
    buf: Vec<f32>,
    /// `Config::fec_loss_pct`.
    fec_loss: u8,
}

impl Mic {
    pub fn new(fec_loss: u8) -> Mic {
        Mic {
            enc: None,
            buf: Vec::new(),
            fec_loss,
        }
    }

    /// Takes `pcm` at `rate`; each whole frame's packet to `send`.
    pub fn push(&mut self, pcm: &[f32], rate: u32, mut send: impl FnMut(&[u8])) -> io::Result<()> {
        if self.enc.as_ref().is_none_or(|e| e.rate() != rate) {
            self.enc = Some(Encoder::new(rate, self.fec_loss)?);
            self.buf.clear();
        }
        let Some(enc) = self.enc.as_mut() else {
            return Ok(());
        };
        let frame = enc.frame();
        self.buf.extend_from_slice(pcm);
        let mut at = 0;
        let mut r = Ok(());
        while self.buf.len() - at >= frame {
            // A frame that fails to code is skipped, not tried again with the next push
            // (which would send the frames before it twice).
            match enc.encode(&self.buf[at..at + frame]) {
                Ok(p) => send(p),
                Err(e) => r = Err(e),
            }
            at += frame;
        }
        self.buf.drain(..at);
        r
    }

    /// The talk ends: what is left of a frame, padded with silence, to `send` (the last
    /// syllable isn't cut).
    pub fn finish(&mut self, mut send: impl FnMut(&[u8])) {
        if let Some(enc) = self.enc.as_mut()
            && !self.buf.is_empty()
        {
            self.buf.resize(enc.frame(), 0.0);
            if let Ok(p) = enc.encode(&self.buf) {
                send(p);
            }
        }
        self.buf.clear();
    }

    /// Not talking: anything held is dropped.
    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

/// i16 samples as −1…+1.
pub(crate) fn to_f32(pcm: &[i16], out: &mut Vec<f32>) {
    out.clear();
    out.extend(pcm.iter().map(|&s| f32::from(s) / 32_768.0));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pieces_of_any_length_make_whole_20_ms_frames() {
        let mut m = Mic::new(10);
        let mut packets = 0;
        // 7 ms pieces at 48 kHz: 336 samples; 30 of them are 210 ms, 10 frames and a bit.
        for _ in 0..30 {
            m.push(&[0.1; 336], 48_000, |_| packets += 1).unwrap();
        }
        assert_eq!(packets, 10);
        m.finish(|_| packets += 1);
        assert_eq!(packets, 11);
        m.finish(|_| packets += 1);
        assert_eq!(packets, 11, "nothing left");
        // A new rate: frames of its own length (16 kHz: 320 samples).
        m.push(&[0.1; 640], 16_000, |_| packets += 1).unwrap();
        assert_eq!(packets, 13);
        assert!(m.push(&[0.0; 10], 44_100, |_| {}).is_err());
        let mut f = Vec::new();
        to_f32(&[i16::MIN, 0, 16_384], &mut f);
        assert_eq!(f, [-1.0, 0.0, 0.5]);
    }
}

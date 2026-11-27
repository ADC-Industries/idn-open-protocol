//! Opus as Voice codes it (§V.8): 20 ms frames, voice mono at 24 kbit/s with in-band
//! forward error correction; IEM channels stereo (decoded here, mixed down to mono).

use opusic_c::{Application, Bitrate, Channels, InbandFec, SampleRate, Signal};
use std::io;

/// Voice's bitrate.
pub(crate) const BITRATE: u32 = 24_000;

fn bad(what: &str, e: opusic_c::ErrorCode) -> io::Error {
    io::Error::other(format!("opus {what}: {e:?}"))
}

/// Codes the microphone at its own rate (one of [`crate::MIC_RATES`]).
pub(crate) struct Encoder {
    enc: opusic_c::Encoder,
    rate: u32,
    out: Vec<u8>,
}

impl Encoder {
    /// `fec_loss`: the loss its in-band FEC plans for, % (0: none). Over TCP and QUIC only
    /// the server's drops for a slow listener lose a frame; Opus codes FEC only from
    /// about 10 %.
    pub fn new(rate: u32, fec_loss: u8) -> io::Result<Encoder> {
        let sr = match rate {
            8_000 => SampleRate::Hz8000,
            12_000 => SampleRate::Hz12000,
            16_000 => SampleRate::Hz16000,
            24_000 => SampleRate::Hz24000,
            48_000 => SampleRate::Hz48000,
            r => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("microphone at {r} Hz: give 48000 (or 8000, 12000, 16000, 24000)"),
                ));
            }
        };
        let mut enc = opusic_c::Encoder::new(Channels::Mono, sr, Application::Voip)
            .map_err(|e| bad("encoder", e))?;
        let set = |r: Result<(), opusic_c::ErrorCode>| r.map_err(|e| bad("setting", e));
        set(enc.set_bitrate(Bitrate::Value(BITRATE)))?;
        set(enc.set_signal(Signal::Voice))?;
        set(enc.set_inband_fec(if fec_loss > 0 {
            InbandFec::Mode1
        } else {
            InbandFec::Off
        }))?;
        set(enc.set_packet_loss(fec_loss))?;
        // A handset's small core: middle complexity, as idn-iem's (voice at 24 kbit/s
        // gains little above it).
        set(enc.set_complexity(5))?;
        Ok(Encoder {
            enc,
            rate,
            out: vec![0; crate::MAX_PACKET],
        })
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Samples in one 20 ms frame at its rate.
    pub fn frame(&self) -> usize {
        self.rate as usize / 50
    }

    /// One 20 ms frame (−1…+1) as an Opus packet.
    pub fn encode(&mut self, pcm: &[f32]) -> io::Result<&[u8]> {
        let n = self
            .enc
            .encode_float_to_slice(pcm, &mut self.out)
            .map_err(|e| bad("encode", e))?;
        Ok(&self.out[..n])
    }
}

/// Decodes one talker's stream at 48 kHz.
pub(crate) struct Decoder {
    dec: opusic_c::Decoder,
    stereo: bool,
    pcm: Vec<f32>,
}

impl Decoder {
    pub fn new(stereo: bool) -> io::Result<Decoder> {
        let ch = if stereo {
            Channels::Stereo
        } else {
            Channels::Mono
        };
        Ok(Decoder {
            dec: opusic_c::Decoder::new(ch, SampleRate::Hz48000).map_err(|e| bad("decoder", e))?,
            stereo,
            // 120 ms, a packet's longest, in two channels.
            pcm: vec![0.0; 5_760 * 2],
        })
    }

    /// A frame of `out.len()` mono samples: `packet` decoded; with `fec`, the frame
    /// before `packet` rebuilt from its FEC; without a packet, concealed. Whatever the
    /// packet held beyond `out` is dropped (Voice's packets are 20 ms).
    pub fn decode(&mut self, packet: Option<&[u8]>, fec: bool, out: &mut [f32]) -> io::Result<()> {
        let ch = if self.stereo { 2 } else { 1 };
        let room = match packet {
            // FEC and concealment give exactly the frame asked for.
            Some(_) if !fec => self.pcm.len(),
            _ => out.len() * ch,
        };
        let n = self
            .dec
            .decode_float_to_slice(packet.unwrap_or(&[]), &mut self.pcm[..room], fec)
            .map_err(|e| bad("decode", e))?;
        for (i, o) in out.iter_mut().enumerate() {
            *o = if i >= n {
                0.0
            } else if self.stereo {
                (self.pcm[2 * i] + self.pcm[2 * i + 1]) * 0.5
            } else {
                self.pcm[i]
            };
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Speech-like sound (a gliding voiced pitch, its level rising and falling) through
    /// the coding; a lost frame rebuilt from the next packet's FEC comes back clearly
    /// closer to what was sent than one concealed; with FEC off the two are alike.
    #[test]
    fn voice_coding_round_trip() {
        let speech: Vec<f32> = (0..crate::FRAME_SAMPLES * 60)
            .map(|i| {
                let t = i as f32 / 48_000.0;
                let tau = 2.0 * std::f32::consts::PI;
                let pitch = 140.0 + 30.0 * (tau * 3.0 * t).sin();
                let v: f32 = (1..=12)
                    .map(|h| (tau * pitch * h as f32 * t).sin() / h as f32)
                    .sum();
                0.25 * v * (0.6 + 0.4 * (tau * 4.0 * t).sin())
            })
            .collect();
        let rms = |o: &[f32]| (o.iter().map(|v| v * v).sum::<f32>() / o.len() as f32).sqrt();
        let err =
            |a: &[f32], b: &[f32]| rms(&a.iter().zip(b).map(|(x, y)| x - y).collect::<Vec<_>>());
        // Each 10th frame from the 20th lost: its error rebuilt (FEC) and concealed, and
        // the packets' average size.
        let run = |loss: u8| {
            let mut enc = Encoder::new(48_000, loss).unwrap();
            let f = enc.frame();
            assert_eq!(f, crate::FRAME_SAMPLES);
            let packets: Vec<Vec<u8>> = speech
                .chunks(f)
                .map(|c| enc.encode(c).unwrap().to_vec())
                .collect();
            let avg = packets.iter().map(Vec::len).sum::<usize>() / packets.len();
            let mut clean = Decoder::new(false).unwrap();
            let mut fec = Decoder::new(false).unwrap();
            let mut plc = Decoder::new(false).unwrap();
            let (mut r, mut a, mut b) = (vec![0.0; f], vec![0.0; f], vec![0.0; f]);
            let (mut fec_err, mut plc_err) = (0.0, 0.0);
            let (mut out_level, mut in_level) = (0.0, 0.0);
            for (k, p) in packets.iter().enumerate() {
                clean.decode(Some(p), false, &mut r).unwrap();
                if k >= 20 && k % 10 == 0 {
                    fec.decode(Some(&packets[k + 1]), true, &mut a).unwrap();
                    plc.decode(None, false, &mut b).unwrap();
                    fec_err += err(&a, &r);
                    plc_err += err(&b, &r);
                } else {
                    fec.decode(Some(p), false, &mut a).unwrap();
                    plc.decode(Some(p), false, &mut b).unwrap();
                }
                if k >= 10 {
                    out_level += rms(&r);
                    in_level += rms(&speech[k * f..(k + 1) * f]);
                }
            }
            // Coded speech comes back as loud as it went in.
            let ratio = out_level / in_level;
            assert!((0.8..1.25).contains(&ratio), "level out/in {ratio}");
            (fec_err, plc_err, avg)
        };
        // 24 kbit/s: about 60 bytes a frame, FEC out of the same budget.
        let (fec_err, plc_err, avg) = run(10);
        assert!((20..=150).contains(&avg), "{avg} bytes a frame");
        assert!(
            fec_err < 0.7 * plc_err,
            "FEC {fec_err} vs concealed {plc_err}"
        );
        let (fec_err, plc_err, avg) = run(0);
        assert!((20..=150).contains(&avg), "{avg} bytes a frame");
        assert!(
            (fec_err - plc_err).abs() < 0.01,
            "FEC off: {fec_err} vs {plc_err}"
        );
        let mut out = vec![0.0; crate::FRAME_SAMPLES];
        let mut enc = Encoder::new(48_000, 10).unwrap();
        let packet = enc
            .encode(&speech[..crate::FRAME_SAMPLES])
            .unwrap()
            .to_vec();
        // Another rate codes too; one Opus can't take is refused.
        assert_eq!(Encoder::new(16_000, 10).unwrap().frame(), 320);
        assert!(Encoder::new(44_100, 10).is_err());
        // A stereo stream comes out mono.
        let mut st = Decoder::new(true).unwrap();
        st.decode(Some(packet.as_slice()), false, &mut out).unwrap();
    }
}

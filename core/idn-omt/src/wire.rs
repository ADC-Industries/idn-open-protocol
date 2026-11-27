//! OMT's frames on a TCP connection (as libomtnet, MIT, codes them; spec/extensions/
//! idn-omt.md §O.2): a 16-byte header — version 1, the type (1 metadata, 2 video, 4
//! audio), a timestamp (100 ns), the length of the frame's own metadata at its end, the
//! length of what follows — then for video 32 and for audio 24 more bytes, then the data.
//! Little-endian throughout.

use std::io::{self, Read, Write};

pub const VERSION: u8 = 1;
pub const METADATA: u8 = 1;
pub const VIDEO: u8 = 2;
pub const AUDIO: u8 = 4;

/// The video codec (the only one on the wire) and the audio's.
pub const VMX1: i32 = 0x3158_4D56;
pub const FPA1: i32 = 0x3141_5046;

/// Video flags.
pub const FLAG_INTERLACED: i32 = 1;
pub const FLAG_ALPHA: i32 = 2;
pub const FLAG_PREMULTIPLIED: i32 = 4;
pub const FLAG_PREVIEW: i32 = 8;
pub const FLAG_HIGH_BIT_DEPTH: i32 = 16;

/// The largest frame taken (OMT's own limit).
pub const MAX_FRAME: usize = 10 * 1024 * 1024;
/// The default port range of senders, and the discovery server's port.
pub const PORT_FIRST: u16 = 6400;
pub const PORT_LAST: u16 = 6600;
pub const SERVICE: &str = "_omt._tcp.local";

/// The control messages, compared exactly (as libomtnet does; the doubled `==` in tally
/// is OMT's own and kept for compatibility).
pub mod msg {
    pub const SUBSCRIBE_VIDEO: &str = r#"<OMTSubscribe Video="true" />"#;
    pub const SUBSCRIBE_AUDIO: &str = r#"<OMTSubscribe Audio="true" />"#;
    pub const SUBSCRIBE_METADATA: &str = r#"<OMTSubscribe Metadata="true" />"#;
    pub const PREVIEW_ON: &str = r#"<OMTSettings Preview="true" />"#;
    pub const PREVIEW_OFF: &str = r#"<OMTSettings Preview="false" />"#;
    pub const TALLY_NONE: &str = r#"<OMTTally Preview="false" Program=="false" />"#;
    pub const TALLY_PREVIEW: &str = r#"<OMTTally Preview="true" Program=="false" />"#;
    pub const TALLY_PROGRAM: &str = r#"<OMTTally Preview="false" Program=="true" />"#;
    pub const TALLY_BOTH: &str = r#"<OMTTally Preview="true" Program=="true" />"#;
    pub const QUALITY_PREFIX: &str = "<OMTSettings Quality=";

    /// The tally message for (program, preview).
    pub fn tally(program: bool, preview: bool) -> &'static str {
        match (program, preview) {
            (false, false) => TALLY_NONE,
            (false, true) => TALLY_PREVIEW,
            (true, false) => TALLY_PROGRAM,
            (true, true) => TALLY_BOTH,
        }
    }

    /// (program, preview) of a tally message.
    pub fn tally_of(xml: &str) -> Option<(bool, bool)> {
        Some(match xml {
            TALLY_NONE => (false, false),
            TALLY_PREVIEW => (false, true),
            TALLY_PROGRAM => (true, false),
            TALLY_BOTH => (true, true),
            _ => return None,
        })
    }
}

/// A video frame's own header.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Video {
    pub codec: i32,
    pub width: i32,
    pub height: i32,
    pub rate_n: i32,
    pub rate_d: i32,
    pub aspect: f32,
    pub flags: i32,
    /// 601, 709, or 0 (by the height).
    pub color_space: i32,
}

/// An audio frame's own header.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Audio {
    pub codec: i32,
    pub sample_rate: i32,
    pub samples: i32,
    pub channels: i32,
    /// Channel `k` is in the data when bit `k` is set (silent ones aren't sent).
    pub active: u32,
}

/// One frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub kind: u8,
    /// 100 ns units.
    pub timestamp: i64,
    pub video: Option<Video>,
    pub audio: Option<Audio>,
    /// The data (for video and audio, the frame's own metadata after it: `meta_len`).
    pub data: Vec<u8>,
    pub meta_len: usize,
}

impl Frame {
    /// A metadata frame (UTF-8 XML).
    pub fn metadata(xml: &str, timestamp: i64) -> Frame {
        Frame {
            kind: METADATA,
            timestamp,
            video: None,
            audio: None,
            data: xml.as_bytes().to_vec(),
            meta_len: 0,
        }
    }

    /// A metadata frame's text.
    pub fn text(&self) -> Option<String> {
        (self.kind == METADATA).then(|| {
            String::from_utf8_lossy(&self.data)
                .trim_end_matches('\0')
                .to_owned()
        })
    }

    /// The media data without the frame's own metadata.
    pub fn payload(&self) -> &[u8] {
        &self.data[..self.data.len().saturating_sub(self.meta_len)]
    }

    /// As bytes on the wire; with `preview`, a video frame cut to its first `preview`
    /// bytes (the receiver asked for previews).
    pub fn encode(&self, preview: Option<usize>) -> Vec<u8> {
        let ext = match self.kind {
            VIDEO => 32,
            AUDIO => 24,
            _ => 0,
        };
        let body = match (self.kind, preview) {
            (VIDEO, Some(p)) => p.min(self.data.len()),
            _ => self.data.len(),
        };
        let mut b = Vec::with_capacity(16 + ext + body);
        b.push(VERSION);
        b.push(self.kind);
        b.extend_from_slice(&self.timestamp.to_le_bytes());
        b.extend_from_slice(&(self.meta_len as u16).to_le_bytes());
        b.extend_from_slice(&((ext + body) as i32).to_le_bytes());
        if let Some(v) = &self.video {
            let flags = v.flags | if preview.is_some() { FLAG_PREVIEW } else { 0 };
            for x in [v.codec, v.width, v.height, v.rate_n, v.rate_d] {
                b.extend_from_slice(&x.to_le_bytes());
            }
            b.extend_from_slice(&v.aspect.to_le_bytes());
            b.extend_from_slice(&flags.to_le_bytes());
            b.extend_from_slice(&v.color_space.to_le_bytes());
        } else if let Some(a) = &self.audio {
            for x in [a.codec, a.sample_rate, a.samples, a.channels] {
                b.extend_from_slice(&x.to_le_bytes());
            }
            b.extend_from_slice(&a.active.to_le_bytes());
            b.extend_from_slice(&0i32.to_le_bytes());
        }
        b.extend_from_slice(&self.data[..body]);
        b
    }

    /// Reads one frame (`None` at the end of the connection).
    pub fn read(r: &mut impl Read) -> io::Result<Option<Frame>> {
        let mut h = [0u8; 16];
        match r.read_exact(&mut h) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let bad = |m: &str| idn_common::err::invalid_data(format!("OMT: {m}"));
        if h[0] != VERSION {
            return Err(bad("an unknown version"));
        }
        let kind = h[1];
        let timestamp = i64::from_le_bytes(h[2..10].try_into().unwrap());
        let meta_len = u16::from_le_bytes([h[10], h[11]]) as usize;
        let len = i32::from_le_bytes(h[12..16].try_into().unwrap());
        if len < 0 || len as usize > MAX_FRAME + 64 {
            return Err(bad("a frame too large"));
        }
        let mut rest = vec![0u8; len as usize];
        r.read_exact(&mut rest)?;
        let i = |k: usize| i32::from_le_bytes(rest[k..k + 4].try_into().unwrap());
        let (video, audio, ext) = match kind {
            VIDEO if rest.len() >= 32 => (
                Some(Video {
                    codec: i(0),
                    width: i(4),
                    height: i(8),
                    rate_n: i(12),
                    rate_d: i(16),
                    aspect: f32::from_le_bytes(rest[20..24].try_into().unwrap()),
                    flags: i(24),
                    color_space: i(28),
                }),
                None,
                32,
            ),
            AUDIO if rest.len() >= 24 => (
                None,
                Some(Audio {
                    codec: i(0),
                    sample_rate: i(4),
                    samples: i(8),
                    channels: i(12),
                    active: u32::from_le_bytes(rest[16..20].try_into().unwrap()),
                }),
                24,
            ),
            VIDEO | AUDIO => return Err(bad("a short header")),
            _ => (None, None, 0),
        };
        let data = rest.split_off(ext);
        Ok(Some(Frame {
            kind,
            timestamp,
            video,
            audio,
            meta_len: meta_len.min(data.len()),
            data,
        }))
    }

    /// Writes it.
    pub fn write(&self, w: &mut impl Write, preview: Option<usize>) -> io::Result<()> {
        w.write_all(&self.encode(preview))
    }
}

/// Planar float channels as FPA1: only the channels that aren't silent, and which they
/// are.
pub fn fpa1_encode(channels: &[Vec<f32>]) -> (Vec<u8>, u32) {
    let mut out = Vec::new();
    let mut active = 0u32;
    for (k, ch) in channels.iter().enumerate().take(32) {
        if ch.iter().any(|s| *s != 0.0) {
            active |= 1 << k;
            for s in ch {
                out.extend_from_slice(&s.to_le_bytes());
            }
        }
    }
    (out, active)
}

/// FPA1 data back to `channels` planar channels of `samples` each (silent ones zero).
pub fn fpa1_decode(
    data: &[u8],
    channels: usize,
    samples: usize,
    active: u32,
) -> Option<Vec<Vec<f32>>> {
    // The sample count comes from the sender: silent channels are made from it alone, so
    // it is bounded first (a second at 192 kHz; one frame of 0x7fffffff was 32 × 8 GiB of
    // zeros), and the active ones must be in the data.
    const MAX_SAMPLES: usize = 192_000;
    if samples > MAX_SAMPLES {
        return None;
    }
    let active_n = (0..channels.min(32))
        .filter(|k| active & (1 << k) != 0)
        .count();
    if active_n * samples * 4 > data.len() {
        return None;
    }
    let mut out = Vec::with_capacity(channels.min(32));
    let mut at = 0;
    for k in 0..channels.min(32) {
        if active & (1 << k) != 0 {
            let bytes = data.get(at..at + samples * 4)?;
            out.push(
                bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| f32::from_le_bytes(*b))
                    .collect(),
            );
            at += samples * 4;
        } else {
            out.push(vec![0.0; samples]);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let v = Frame {
            kind: VIDEO,
            timestamp: 123_456_789,
            video: Some(Video {
                codec: VMX1,
                width: 1920,
                height: 1080,
                rate_n: 30000,
                rate_d: 1001,
                aspect: 16.0 / 9.0,
                flags: FLAG_ALPHA,
                color_space: 709,
            }),
            audio: None,
            data: b"PICTUREDATA<meta/>".to_vec(),
            meta_len: 7,
        };
        let b = v.encode(None);
        assert_eq!(b.len(), 16 + 32 + 18);
        assert_eq!(&b[..2], &[1, 2]);
        let back = Frame::read(&mut &b[..]).unwrap().unwrap();
        assert_eq!(back, v);
        assert_eq!(back.payload(), b"PICTUREDATA");
        // A preview: cut, flagged.
        let p = Frame::read(&mut &v.encode(Some(4))[..]).unwrap().unwrap();
        assert_eq!(p.data, b"PICT");
        assert_eq!(p.video.unwrap().flags, FLAG_ALPHA | FLAG_PREVIEW);
        let m = Frame::metadata(msg::SUBSCRIBE_VIDEO, 0);
        let back = Frame::read(&mut &m.encode(None)[..]).unwrap().unwrap();
        assert_eq!(back.text().unwrap(), msg::SUBSCRIBE_VIDEO);
        assert_eq!(msg::tally_of(msg::tally(true, false)), Some((true, false)));
        assert!(Frame::read(&mut &b""[..]).unwrap().is_none());
        let mut bad = b.clone();
        bad[0] = 2;
        assert!(Frame::read(&mut &bad[..]).is_err());
    }

    #[test]
    fn silent_channels_are_left_out() {
        let ch = vec![vec![0.5f32, -0.5], vec![0.0, 0.0], vec![0.25, 0.0]];
        let (data, active) = fpa1_encode(&ch);
        assert_eq!(active, 0b101);
        assert_eq!(data.len(), 2 * 2 * 4);
        assert_eq!(fpa1_decode(&data, 3, 2, active).unwrap(), ch);
        assert!(fpa1_decode(&data[..4], 3, 2, active).is_none());
    }
}

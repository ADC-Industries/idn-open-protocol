//! Control-channel frame format (spec §1.2) and robust frame reading (§1.9).

use crate::scramble::{descramble_body, scramble_body};
use std::fmt;

/// Size of the fixed frame header.
pub const HEADER_LEN: usize = 12;

/// Bit 15 of `type`: the body is scrambled.
pub const SCRAMBLED_BIT: u16 = 0x8000;

/// Message kinds (`type & 0x7FFF`, §1.2).
pub mod kind {
    pub const CONTROL: u16 = 1;
    pub const METADATA: u16 = 3;
    pub const AUDIO: u16 = 5;
    pub const VIDEO: u16 = 7;
}

/// Extension-header timecode meaning "synthesize timecode" (§1.2).
pub const TIMECODE_SYNTHESIZE: i64 = i64::MAX;

/// A decoded frame: header fields plus the plaintext extension header and payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Message kind, the low 15 bits of `type`.
    pub kind: u16,
    /// Whether the frame is (or will be) scrambled on the wire.
    pub scrambled: bool,
    pub version: u16,
    pub ext: Vec<u8>,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// `hdr_len` or `data_len` exceeds the configured limit. The stream cannot be
    /// resynchronised; the connection must be closed (§1.9).
    TooLarge { hdr_len: u32, data_len: u32 },
    /// A kind-1/3 payload lacks its terminating NUL or is not UTF-8.
    BadText,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::TooLarge { hdr_len, data_len } => {
                write!(
                    f,
                    "frame too large (hdr_len {hdr_len}, data_len {data_len})"
                )
            }
            FrameError::BadText => write!(f, "text payload missing NUL or not UTF-8"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Size limits applied before allocating a frame body (§1.9).
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_hdr_len: u32,
    pub max_data_len: u32,
}

impl Default for Limits {
    /// Generous enough for one compressed UHD SpeedHQ picture on the control connection.
    fn default() -> Self {
        Limits {
            max_hdr_len: 4096,
            max_data_len: 64 << 20,
        }
    }
}

/// The fixed 12-byte frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub type_: u16,
    pub version: u16,
    pub hdr_len: u32,
    pub data_len: u32,
}

impl Header {
    pub fn parse(b: &[u8; HEADER_LEN]) -> Self {
        Header {
            type_: u16::from_le_bytes([b[0], b[1]]),
            version: u16::from_le_bytes([b[2], b[3]]),
            hdr_len: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            data_len: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
        }
    }

    pub fn kind(&self) -> u16 {
        self.type_ & !SCRAMBLED_BIT
    }

    pub fn scrambled(&self) -> bool {
        self.type_ & SCRAMBLED_BIT != 0
    }

    /// Checks the lengths against `limits`; returns the total body length.
    pub fn check(&self, limits: &Limits) -> Result<usize, FrameError> {
        if self.hdr_len > limits.max_hdr_len || self.data_len > limits.max_data_len {
            return Err(FrameError::TooLarge {
                hdr_len: self.hdr_len,
                data_len: self.data_len,
            });
        }
        Ok(self.hdr_len as usize + self.data_len as usize)
    }
}

impl Frame {
    /// A kind-1 or kind-3 XML message with an 8-byte timecode extension header.
    /// `xml` must not contain NUL; the terminating NUL is appended.
    pub fn text(kind: u16, timecode: i64, xml: &str) -> Self {
        let mut payload = Vec::with_capacity(xml.len() + 1);
        payload.extend_from_slice(xml.as_bytes());
        payload.push(0);
        Frame {
            kind,
            scrambled: true,
            version: 2,
            ext: timecode.to_le_bytes().to_vec(),
            payload,
        }
    }

    /// Serialises the frame, scrambling the body if `scrambled` is set.
    pub fn encode(&self) -> Vec<u8> {
        Self::encode_parts(
            self.kind,
            self.version,
            self.scrambled,
            &self.ext,
            &self.payload,
        )
    }

    /// Serialises a frame from borrowed parts: one buffer, scrambled in place (a large
    /// payload is copied once, not three times).
    pub fn encode_parts(
        kind: u16,
        version: u16,
        scrambled: bool,
        ext: &[u8],
        payload: &[u8],
    ) -> Vec<u8> {
        let type_ = (kind & !SCRAMBLED_BIT) | if scrambled { SCRAMBLED_BIT } else { 0 };
        let mut out = Vec::with_capacity(HEADER_LEN + ext.len() + payload.len());
        out.extend_from_slice(&type_.to_le_bytes());
        out.extend_from_slice(&version.to_le_bytes());
        out.extend_from_slice(&(ext.len() as u32).to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(ext);
        out.extend_from_slice(payload);
        if scrambled {
            let (e, p) = out[HEADER_LEN..].split_at_mut(ext.len());
            scramble_body(kind, version, e, p);
        }
        out
    }

    /// Builds a frame from a parsed header and its raw (still scrambled) body. The body's
    /// buffer becomes the payload (only the small extension header is copied out).
    pub fn from_wire(h: Header, body: Vec<u8>) -> Self {
        Self::from_buffer(h, body, 0)
    }

    /// As [`from_wire`](Self::from_wire), for a buffer holding the whole message from
    /// `at` (e.g. the 12-byte header first): no copy of the payload.
    pub fn from_buffer(h: Header, mut buf: Vec<u8>, at: usize) -> Self {
        let n = h.hdr_len as usize;
        let mut ext = buf[at..at + n].to_vec();
        buf.drain(..at + n);
        let mut payload = buf;
        if h.scrambled() {
            descramble_body(h.kind(), h.version, &mut ext, &mut payload);
        }
        Frame {
            kind: h.kind(),
            scrambled: h.scrambled(),
            version: h.version,
            ext,
            payload,
        }
    }

    /// Decodes exactly one frame from the front of `buf`. Returns the frame and the number
    /// of bytes consumed, or `Ok(None)` if `buf` does not yet hold a complete frame.
    pub fn decode(buf: &[u8], limits: &Limits) -> Result<Option<(Frame, usize)>, FrameError> {
        let Some(hb) = buf.get(..HEADER_LEN) else {
            return Ok(None);
        };
        let h = Header::parse(hb.try_into().unwrap());
        let body_len = h.check(limits)?;
        let Some(body) = buf.get(HEADER_LEN..HEADER_LEN + body_len) else {
            return Ok(None);
        };
        Ok(Some((
            Frame::from_wire(h, body.to_vec()),
            HEADER_LEN + body_len,
        )))
    }

    /// The timecode of a kind-1/3 extension header, if it is 8 bytes long.
    pub fn timecode(&self) -> Option<i64> {
        Some(i64::from_le_bytes(self.ext.get(..8)?.try_into().ok()?))
    }

    /// The XML text of a kind-1/3 payload, without its NUL. Never reads past the payload
    /// (§1.9); anything after the first NUL is ignored.
    pub fn text_payload(&self) -> Result<&str, FrameError> {
        let nul = self
            .payload
            .iter()
            .position(|&b| b == 0)
            .ok_or(FrameError::BadText)?;
        std::str::from_utf8(&self.payload[..nul]).map_err(|_| FrameError::BadText)
    }
}

/// Incremental frame reader for a byte stream (e.g. the TCP control connection).
#[derive(Debug, Default)]
pub struct FrameReader {
    buf: Vec<u8>,
    limits: Limits,
}

impl FrameReader {
    pub fn new(limits: Limits) -> Self {
        FrameReader {
            buf: Vec::new(),
            limits,
        }
    }

    /// Appends received bytes.
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Returns the next complete frame, if any. An error is fatal for the connection.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        match Frame::decode(&self.buf, &self.limits)? {
            Some((f, used)) => {
                self.buf.drain(..used);
                Ok(Some(f))
            }
            None => Ok(None),
        }
    }
}

/// Reads one frame from a blocking reader. Returns `Ok(None)` on clean EOF at a frame
/// boundary.
pub fn read_frame<R: std::io::Read>(r: &mut R, limits: &Limits) -> std::io::Result<Option<Frame>> {
    use std::io::{Error, ErrorKind};
    let mut hb = [0u8; HEADER_LEN];
    match r.read_exact(&mut hb) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let h = Header::parse(&hb);
    let body_len = h
        .check(limits)
        .map_err(|e| Error::new(ErrorKind::InvalidData, e))?;
    // Grown as the data comes, not allocated at the size the header claims (up to the
    // limit, 64 MB), which a peer could send and then not follow.
    let mut body = Vec::with_capacity(body_len.min(1 << 20));
    let got = std::io::Read::read_to_end(
        &mut std::io::Read::take(&mut *r, body_len as u64),
        &mut body,
    )?;
    if got < body_len {
        return Err(Error::new(ErrorKind::UnexpectedEof, "frame cut short"));
    }
    Ok(Some(Frame::from_wire(h, body)))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Spec §1.3.3 test vectors.
    const V1: &str = "018002000800000021000000ed065d6b17747967b2693e7dcbe61489f6c57f392a050cef\
                      3bd026c10fee40916aea819fd891bdf4db";
    const V2: &str = "03800200080000000d000000434c30423b98b22ef5f968f3ff1814725c48181e4e";
    const V3: &str = "0380020008000000220000002a9b22e8368f04df4aac5ea6c8834abfbff39c2194aeb62b\
                      43005604da24431e68709aee3639c0705a20";

    fn check(vec: &str, kind: u16, tc: i64, xml: &str) {
        let wire = idn_common::hex::unhex(vec).unwrap();
        let built = Frame::text(kind, tc, xml).encode();
        assert_eq!(idn_common::hex::hex(&built), vec, "encode {xml}");
        let (f, used) = Frame::decode(&wire, &Limits::default()).unwrap().unwrap();
        assert_eq!(used, wire.len());
        assert_eq!(f.kind, kind);
        assert_eq!(f.version, 2);
        assert_eq!(f.timecode(), Some(tc));
        assert_eq!(f.text_payload().unwrap(), xml);
    }

    #[test]
    fn vector_1_kind1_scheme_a() {
        check(V1, kind::CONTROL, 0, r#"<ndi_identify name="SND (LAB)"/>"#);
    }

    #[test]
    fn vector_2_kind3_scheme_b() {
        check(V2, kind::METADATA, TIMECODE_SYNTHESIZE, "<lab_reply/>");
    }

    #[test]
    fn vector_3_kind3_scheme_b() {
        check(
            V3,
            kind::METADATA,
            0,
            r#"<lab_meta n="90">hello</lab_meta>"#,
        );
    }

    #[test]
    fn stream_reader_splits_frames() {
        let mut all = idn_common::hex::unhex(V1).unwrap();
        all.extend(idn_common::hex::unhex(V2).unwrap());
        all.extend(idn_common::hex::unhex(V3).unwrap());
        let mut r = FrameReader::default();
        let mut got = Vec::new();
        for b in all.chunks(5) {
            r.push(b);
            while let Some(f) = r.next_frame().unwrap() {
                got.push(f.text_payload().unwrap().to_owned());
            }
        }
        assert_eq!(got.len(), 3);
        assert_eq!(got[1], "<lab_reply/>");
    }

    #[test]
    fn oversize_rejected_before_allocation() {
        let mut hdr = idn_common::hex::unhex(V2).unwrap()[..HEADER_LEN].to_vec();
        hdr[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            Frame::decode(&hdr, &Limits::default()),
            Err(FrameError::TooLarge { .. })
        ));
    }

    #[test]
    fn missing_nul_is_message_error() {
        let f = Frame {
            kind: 1,
            scrambled: true,
            version: 2,
            ext: vec![0; 8],
            payload: b"<x/>".to_vec(),
        };
        assert_eq!(f.text_payload(), Err(FrameError::BadText));
    }

    #[test]
    fn plain_frame_roundtrip() {
        let mut f = Frame::text(kind::METADATA, 0, "<a/>");
        f.scrambled = false;
        let wire = f.encode();
        assert_eq!(&wire[..2], &[3, 0]);
        assert_eq!(&wire[HEADER_LEN + 8..], b"<a/>\0");
        assert_eq!(
            Frame::decode(&wire, &Limits::default()).unwrap().unwrap().0,
            f
        );
    }
}

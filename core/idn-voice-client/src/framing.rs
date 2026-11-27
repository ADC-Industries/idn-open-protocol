//! The bytes on the wire.
//!
//! - **The link's stream** (QUIC, `idn-voice/1`): one bidirectional stream, each way a
//!   sequence of frames: a u32 little-endian length (of what follows), a kind byte (0 JSON,
//!   1 sound), the payload. To the server the length is at most [`MAX_FRAME`]; from it at
//!   most [`MAX_FROM_SERVER`] (a `history` or `presence` answer can be long). More, or a
//!   frame cut short, ends the connection. Kinds other than 0 and 1 are skipped.
//! - **Sound** (the WebSocket's binary messages, and the link's kind-1 payloads): from the
//!   client a `1` and the Opus packet; to it a 13-byte header (kind, talker, channel, seq;
//!   little-endian) and the packet (VOICE-CLIENT.md §7).

use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// A link frame's length (its kind byte and payload) at most: 64 KiB.
pub const MAX_FRAME: usize = 64 * 1024;
/// A frame or WebSocket message from the server at most (its JSON answers).
pub const MAX_FROM_SERVER: usize = 4 * 1024 * 1024;
/// An Opus packet the server takes at most.
pub const MAX_PACKET: usize = 4_000;

pub(crate) const KIND_JSON: u8 = 0;
pub(crate) const KIND_SOUND: u8 = 1;

/// A link frame, ready to write.
pub(crate) fn link_frame(kind: u8, payload: &[u8]) -> io::Result<Vec<u8>> {
    let len = payload.len() + 1;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("a link frame of {len} bytes (at most {MAX_FRAME})"),
        ));
    }
    let mut b = Vec::with_capacity(4 + len);
    b.extend_from_slice(&(len as u32).to_le_bytes());
    b.push(kind);
    b.extend_from_slice(payload);
    Ok(b)
}

/// Writes one link frame.
pub(crate) async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    kind: u8,
    payload: &[u8],
) -> io::Result<()> {
    w.write_all(&link_frame(kind, payload)?).await
}

/// Reads one link frame: (kind, payload); `None` where the stream ended between frames.
pub(crate) async fn read_frame<R: AsyncRead + Unpin>(
    r: &mut R,
    max: usize,
) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut len = [0u8; 4];
    // The first byte alone: an end there is a clean end, anywhere else a cut frame.
    match r.read(&mut len[..1]).await? {
        0 => return Ok(None),
        _ => r.read_exact(&mut len[1..]).await?,
    };
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a link frame of {len} bytes (1 to {max})"),
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    let kind = body[0];
    body.remove(0);
    Ok(Some((kind, body)))
}

/// The microphone's packet as the server takes it: a `1`, then the packet.
pub(crate) fn mic_message(packet: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(1 + packet.len());
    b.push(1);
    b.extend_from_slice(packet);
    b
}

/// One sound frame from the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFrame<'a> {
    /// 1 talk, 2 replay, 3 IEM (stereo).
    pub kind: u8,
    /// The talking session (0 for a replay).
    pub talker: u32,
    /// The channel (0 for a call or instant talk).
    pub channel: u32,
    /// The talker's packet number.
    pub seq: u32,
    pub packet: &'a [u8],
}

impl<'a> AudioFrame<'a> {
    /// A frame read; `None` for one too short or of an unknown kind.
    pub fn parse(b: &'a [u8]) -> Option<AudioFrame<'a>> {
        if b.len() < 13 || !(1..=3).contains(&b[0]) {
            return None;
        }
        let u = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        Some(AudioFrame {
            kind: b[0],
            talker: u(1),
            channel: u(5),
            seq: u(9),
            packet: &b[13..],
        })
    }

    /// As the server writes it (the tests' fake server).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(13 + self.packet.len());
        b.push(self.kind);
        b.extend_from_slice(&self.talker.to_le_bytes());
        b.extend_from_slice(&self.channel.to_le_bytes());
        b.extend_from_slice(&self.seq.to_le_bytes());
        b.extend_from_slice(self.packet);
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn link_frames_go_through_and_wrong_lengths_are_refused() {
        let mut wire = Vec::new();
        write_frame(&mut wire, KIND_JSON, br#"{"t":"ping"}"#)
            .await
            .unwrap();
        write_frame(&mut wire, KIND_SOUND, &[1, 2, 3])
            .await
            .unwrap();
        // Length 13 (kind + 12 bytes), little-endian, then the kind.
        assert_eq!(&wire[..5], &[13, 0, 0, 0, 0]);
        let mut r = &wire[..];
        assert_eq!(
            read_frame(&mut r, MAX_FRAME).await.unwrap(),
            Some((KIND_JSON, br#"{"t":"ping"}"#.to_vec()))
        );
        assert_eq!(
            read_frame(&mut r, MAX_FRAME).await.unwrap(),
            Some((KIND_SOUND, vec![1, 2, 3]))
        );
        assert_eq!(read_frame(&mut r, MAX_FRAME).await.unwrap(), None);
        // The largest frame passes; one byte more doesn't, either way.
        assert!(link_frame(KIND_SOUND, &vec![0; MAX_FRAME - 1]).is_ok());
        assert!(link_frame(KIND_SOUND, &vec![0; MAX_FRAME]).is_err());
        let big = ((MAX_FRAME + 1) as u32).to_le_bytes();
        assert!(read_frame(&mut &big[..], MAX_FRAME).await.is_err());
        assert!(
            read_frame(&mut &[0u8, 0, 0, 0][..], MAX_FRAME)
                .await
                .is_err()
        );
        // Cut inside a frame: an error, not an end.
        assert!(read_frame(&mut &wire[..7], MAX_FRAME).await.is_err());
        assert!(read_frame(&mut &wire[..2], MAX_FRAME).await.is_err());
    }

    #[test]
    fn sound_frames_read_as_the_server_writes_them() {
        let f = AudioFrame {
            kind: 1,
            talker: 17,
            channel: 3,
            seq: 0x0102_0304,
            packet: &[9, 8, 7],
        };
        let b = f.to_bytes();
        assert_eq!(&b[..13], &[1, 17, 0, 0, 0, 3, 0, 0, 0, 4, 3, 2, 1]);
        assert_eq!(AudioFrame::parse(&b), Some(f));
        assert_eq!(AudioFrame::parse(&b[..12]), None);
        let mut other = b.clone();
        other[0] = 4;
        assert_eq!(AudioFrame::parse(&other), None);
        assert_eq!(mic_message(&[5, 6]), [1, 5, 6]);
    }
}

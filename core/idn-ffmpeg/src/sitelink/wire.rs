//! The link's media messages: a picture (`V`) and a block of sound (`A`), made and read.

use super::*;

/// `V`, codec, keyframe, width, height, frame rate, pts, the parameter sets, the access unit.
pub(super) fn video_message(
    codec: Codec,
    key: bool,
    size: (usize, usize),
    fps: (u32, u32),
    pts: i64,
    extra: &[u8],
    data: &[u8],
) -> Vec<u8> {
    let mut m = Vec::with_capacity(34 + extra.len() + data.len());
    m.push(b'V');
    m.extend_from_slice(&codec.fourcc());
    m.push(key as u8);
    for x in [size.0 as u32, size.1 as u32, fps.0, fps.1] {
        m.extend_from_slice(&x.to_le_bytes());
    }
    m.extend_from_slice(&pts.to_le_bytes());
    m.extend_from_slice(&(extra.len() as u32).to_le_bytes());
    m.extend_from_slice(extra);
    m.extend_from_slice(data);
    m
}

pub(super) fn parse_video(m: &[u8]) -> Option<VideoMsg<'_>> {
    let u32_at = |i: usize| Some(u32::from_le_bytes(m.get(i..i + 4)?.try_into().ok()?));
    let codec = Codec::from_fourcc(m.get(1..5)?.try_into().ok()?)?;
    let keyframe = *m.get(5)? != 0;
    let fps = (u32_at(14)?, u32_at(18)?);
    let pts = i64::from_le_bytes(m.get(22..30)?.try_into().ok()?);
    let n = u32_at(30)? as usize;
    let extra = m.get(34..34 + n)?;
    Some(VideoMsg {
        codec,
        keyframe,
        fps,
        pts,
        extra,
        data: &m[34 + n..],
    })
}

/// `A`, sample rate, channels, samples a channel, then the samples: 24-bit, planar.
pub(super) fn audio_message(rate: u32, channels: &[Vec<f32>]) -> Vec<u8> {
    let n = channels.first().map_or(0, |c| c.len());
    let mut m = Vec::with_capacity(11 + channels.len() * n * 3);
    m.push(b'A');
    m.extend_from_slice(&rate.to_le_bytes());
    m.extend_from_slice(&(channels.len() as u16).to_le_bytes());
    m.extend_from_slice(&(n as u32).to_le_bytes());
    for c in channels {
        let c = &c[..n.min(c.len())];
        idn_common::pcm::write(idn_common::pcm::Pcm::I24, false, c, &mut m);
        // (A shorter channel padded, so the planes stay where the count says.)
        m.resize(m.len() + (n - c.len()) * 3, 0);
    }
    m
}

pub(super) fn parse_audio(m: &[u8]) -> Option<AudioBlock> {
    let rate = u32::from_le_bytes(m.get(1..5)?.try_into().ok()?);
    let ch = u16::from_le_bytes(m.get(5..7)?.try_into().ok()?) as usize;
    let n = u32::from_le_bytes(m.get(7..11)?.try_into().ok()?) as usize;
    let body = m.get(11..11 + ch * n * 3)?;
    let channels = (0..ch)
        .map(|c| {
            let mut v = Vec::with_capacity(n);
            idn_common::pcm::read(
                idn_common::pcm::Pcm::I24,
                false,
                &body[c * n * 3..(c + 1) * n * 3],
                &mut v,
            );
            v
        })
        .collect();
    Some(AudioBlock {
        sample_rate: rate,
        channels,
    })
}

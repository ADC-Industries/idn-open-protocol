//! NDI|HX payloads (spec chapter 7): the compressed-packet header around H.264, HEVC and
//! AAC data, and helpers to turn them into standard elementary streams.

use std::fmt;

pub const FOURCC_H264: [u8; 4] = *b"H264";
pub const FOURCC_HEVC: [u8; 4] = *b"HEVC";
/// AAC audio FourCC: the value 0x000000FF (§7.2).
pub const FOURCC_AAC: [u8; 4] = [0xff, 0, 0, 0];
pub const PACKET_HEADER: usize = 44;
pub const FLAG_KEYFRAME: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HxError {
    /// Shorter than the 44-byte header, or `version` is not 44.
    BadHeader,
    /// `44 + data_size + extra_size` differs from the payload length (§7.1.1).
    BadLength,
}

impl fmt::Display for HxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for HxError {}

/// A compressed packet (§7.1.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressedPacket<'a> {
    pub fourcc: [u8; 4],
    pub pts: i64,
    pub dts: i64,
    pub counter: u64,
    pub flags: u32,
    pub data: &'a [u8],
    pub extra: &'a [u8],
}

impl<'a> CompressedPacket<'a> {
    pub fn parse(p: &'a [u8]) -> Result<Self, HxError> {
        if p.len() < PACKET_HEADER
            || u32::from_le_bytes(p[0..4].try_into().unwrap()) != PACKET_HEADER as u32
        {
            return Err(HxError::BadHeader);
        }
        let u32_at = |o: usize| u32::from_le_bytes(p[o..o + 4].try_into().unwrap());
        let i64_at = |o: usize| i64::from_le_bytes(p[o..o + 8].try_into().unwrap());
        let (ds, es) = (u32_at(36) as u64, u32_at(40) as u64);
        if PACKET_HEADER as u64 + ds + es != p.len() as u64 {
            return Err(HxError::BadLength);
        }
        let d_end = PACKET_HEADER + ds as usize;
        Ok(CompressedPacket {
            fourcc: p[4..8].try_into().unwrap(),
            pts: i64_at(8),
            dts: i64_at(16),
            counter: i64_at(24) as u64,
            flags: u32_at(32),
            data: &p[PACKET_HEADER..d_end],
            extra: &p[d_end..],
        })
    }

    pub fn keyframe(&self) -> bool {
        self.flags & FLAG_KEYFRAME != 0
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(PACKET_HEADER + self.data.len() + self.extra.len());
        out.extend_from_slice(&(PACKET_HEADER as u32).to_le_bytes());
        out.extend_from_slice(&self.fourcc);
        out.extend_from_slice(&self.pts.to_le_bytes());
        out.extend_from_slice(&self.dts.to_le_bytes());
        out.extend_from_slice(&self.counter.to_le_bytes());
        out.extend_from_slice(&self.flags.to_le_bytes());
        out.extend_from_slice(&(self.data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.extra.len() as u32).to_le_bytes());
        out.extend_from_slice(self.data);
        out.extend_from_slice(self.extra);
        out
    }

    /// Bytes to feed a decoder: the access unit, with the extra data's parameter sets
    /// inserted only if a keyframe lacks them in-band (older senders; §7.1.1).
    /// `hevc` comes from the video header's FourCC, which names the stream's codec; the
    /// packet's own FourCC field isn't trusted for it (R1-80).
    /// (Borrowed when nothing is added: most access units.)
    pub fn decoder_input(&self, hevc: bool) -> std::borrow::Cow<'a, [u8]> {
        if self.keyframe() {
            std::borrow::Cow::Owned(with_parameter_sets(self.data, self.extra, hevc))
        } else {
            std::borrow::Cow::Borrowed(self.data)
        }
    }
}

/// Builds a 7-byte ADTS header for one raw AAC frame of `frame_len` bytes, from a
/// 2-byte (or longer) AudioSpecificConfig. Returns `None` for configurations ADTS cannot
/// express (object type above 4, explicit frequency, channel config 0).
pub fn adts_header(asc: &[u8], frame_len: usize) -> Option<[u8; 7]> {
    if asc.len() < 2 {
        return None;
    }
    let object = asc[0] >> 3;
    let freq = ((asc[0] & 7) << 1) | (asc[1] >> 7);
    let chan = (asc[1] >> 3) & 0xf;
    if !(1..=4).contains(&object) || freq >= 13 || chan == 0 || chan > 7 {
        return None;
    }
    let len = frame_len + 7;
    if len >= 1 << 13 {
        return None;
    }
    let profile = object - 1;
    Some([
        0xff,
        0xf1, // MPEG-4, layer 0, no CRC
        (profile << 6) | (freq << 2) | (chan >> 2),
        ((chan & 3) << 6) | ((len >> 11) as u8 & 3),
        (len >> 3) as u8,
        ((len as u8 & 7) << 5) | 0x1f,
        0xfc,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_roundtrip_and_length_check() {
        let p = CompressedPacket {
            fourcc: FOURCC_H264,
            pts: 10,
            dts: 9,
            counter: 3,
            flags: FLAG_KEYFRAME,
            data: &[0, 0, 0, 1, 0x09, 0xf0],
            extra: &[0, 0, 0, 1, 0x67, 0x64],
        };
        let b = p.encode();
        assert_eq!(&b[..4], &[44, 0, 0, 0]);
        let q = CompressedPacket::parse(&b).unwrap();
        assert_eq!(q, p);
        assert!(q.keyframe());
        // The AUD-only keyframe lacks parameter sets: the extra's SPS goes in after the AUD.
        assert_eq!(q.decoder_input(false), [p.data, p.extra].concat());
        assert_eq!(
            CompressedPacket::parse(&b[..b.len() - 1]),
            Err(HxError::BadLength)
        );
        let mut bad = b.clone();
        bad[36] = 0xff; // data_size overflows the payload
        assert_eq!(CompressedPacket::parse(&bad), Err(HxError::BadLength));
        assert_eq!(CompressedPacket::parse(&b[..40]), Err(HxError::BadHeader));
    }

    /// §7.1.1: keyframes keep SPS/PPS in-band (after the AUD) as well as in `params`.
    #[test]
    fn access_units_keep_parameter_sets_in_band() {
        let nal = |t: u8, body: &[u8]| [&[0u8, 0, 0, 1, t][..], body].concat();
        let (aud, sps, pps) = (
            nal(0x09, &[0x10]),
            nal(0x67, &[0x64, 0, 0x1f]),
            nal(0x68, &[0xeb]),
        );
        let (idr, p, idr2) = (
            nal(0x65, &[0x88, 1]),
            nal(0x41, &[0x9a, 2]),
            nal(0x65, &[0x88, 3]),
        );
        let stream = [&aud[..], &sps, &pps, &idr, &aud, &p, &aud, &idr2].concat();
        let aus = access_units(&stream, false);
        assert_eq!(aus.len(), 3);
        assert!(aus[0].keyframe && !aus[1].keyframe && aus[2].keyframe);
        assert_eq!(
            aus[0].data,
            [&aud[..], &sps, &pps, &idr].concat(),
            "in-band, in order"
        );
        assert_eq!(aus[0].params, [&sps[..], &pps].concat());
        assert!(aus[2].params.is_empty());
        // A later keyframe without its own: the latest ones are inserted after the AUD.
        let fixed = with_parameter_sets(&aus[2].data, &aus[0].params, false);
        assert_eq!(fixed, [&aud[..], &sps, &pps, &idr2].concat());
        // Already in-band: unchanged (no duplicates).
        assert_eq!(
            with_parameter_sets(&aus[0].data, &aus[0].params, false),
            aus[0].data
        );
        // Without an AUD, at the start.
        assert_eq!(
            with_parameter_sets(&idr2, &aus[0].params, false),
            [&sps[..], &pps, &idr2].concat()
        );
        // A stream whose parameter sets stand alone before the first AUD.
        let s2 = [&sps[..], &pps, &aud, &idr].concat();
        let a2 = access_units(&s2, false);
        assert_eq!(a2.len(), 1);
        assert!(a2[0].keyframe && a2[0].params == [&sps[..], &pps].concat());
        assert_eq!(
            a2[0].data,
            [&aud[..], &sps, &pps, &idr].concat(),
            "AUD first"
        );
    }

    #[test]
    fn without_delimiters_an_sei_starts_the_next_picture() {
        let nal = |t: u8, body: &[u8]| [&[0u8, 0, 0, 1, t][..], body].concat();
        let (sps, pps, sei) = (
            nal(0x67, &[0x64]),
            nal(0x68, &[0xeb]),
            nal(0x06, &[5, 1, 0]),
        );
        // first_mb_in_slice = 0 (a first slice): exp-Golomb `1` in the top bit.
        let (idr, p) = (nal(0x65, &[0x88]), nal(0x41, &[0x9a]));
        let stream = [&sps[..], &pps, &sei, &idr, &sei, &p].concat();
        let aus = access_units(&stream, false);
        assert_eq!(aus.len(), 2);
        assert_eq!(
            aus[1].data,
            [&sei[..], &p].concat(),
            "the SEI belongs to its picture (R1-79)"
        );
    }

    #[test]
    fn adts_for_aac_lc_48k_stereo() {
        // AudioSpecificConfig 0x11 0x90: AAC-LC (2), 48 kHz (index 3), 2 channels.
        let h = adts_header(&[0x11, 0x90], 100).unwrap();
        assert_eq!(h[0..2], [0xff, 0xf1]);
        assert_eq!(h[2], (1 << 6) | (3 << 2));
        assert_eq!(h[3] >> 6, 2);
        let len = ((h[3] as usize & 3) << 11) | ((h[4] as usize) << 3) | (h[5] as usize >> 5);
        assert_eq!(len, 107);
        assert!(adts_header(&[0x11], 1).is_none());
    }
}

/// NAL units of an Annex B byte stream, each including its start code.
pub fn nal_units(b: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= b.len() {
        if b[i] == 0 && b[i + 1] == 0 && b[i + 2] == 1 {
            starts.push(if i > 0 && b[i - 1] == 0 { i - 1 } else { i });
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(k, &s)| &b[s..starts.get(k + 1).copied().unwrap_or(b.len())])
        .collect()
}

/// The NAL header byte(s) following the start code.
fn nal_header(n: &[u8]) -> &[u8] {
    let p = n.iter().position(|&x| x == 1).map_or(n.len(), |p| p + 1);
    &n[p..]
}

/// One access unit: whether it is a keyframe, its parameter sets (for the extra data),
/// and all its NAL units in order, parameter sets included (§7.1.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    pub keyframe: bool,
    pub params: Vec<u8>,
    pub data: Vec<u8>,
}

fn nal_kind(n: &[u8], hevc: bool) -> Option<(bool, bool)> {
    let h0 = *nal_header(n).first()?;
    Some(if hevc {
        let t = (h0 >> 1) & 0x3f;
        (t == 35, (32..=34).contains(&t))
    } else {
        let t = h0 & 0x1f;
        (t == 9, t == 7 || t == 8)
    })
}

/// A keyframe's access unit with its parameter sets in-band: `data` unchanged if it has
/// any, else `params` inserted after the access unit delimiter (or at the start). NDI
/// Studio Monitor shows black video when keyframes carry them only as extra data
/// (§7.1.1).
pub fn with_parameter_sets(data: &[u8], params: &[u8], hevc: bool) -> Vec<u8> {
    let nals = nal_units(data);
    if params.is_empty()
        || nals
            .iter()
            .any(|n| nal_kind(n, hevc).is_some_and(|(_, p)| p))
    {
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(data.len() + params.len());
    let mut rest = data;
    if let Some(first) = nals
        .first()
        .filter(|n| nal_kind(n, hevc).is_some_and(|(aud, _)| aud))
    {
        // End of the delimiter NAL within `data` (it is a sub-slice of it).
        let at = (first.as_ptr() as usize - data.as_ptr() as usize) + first.len();
        out.extend_from_slice(&data[..at]);
        rest = &data[at..];
    }
    out.extend_from_slice(params);
    out.extend_from_slice(rest);
    out
}

/// Splits an H.264 (`hevc == false`) or HEVC Annex B stream into access units at access
/// unit delimiters (or, without them, at each first slice). Parameter sets are separated
/// so they can travel as extra data (§7.1.1).
pub fn access_units(b: &[u8], hevc: bool) -> Vec<AccessUnit> {
    let mut out: Vec<AccessUnit> = Vec::new();
    let mut has_slice = false;
    for n in nal_units(b) {
        let h = nal_header(n);
        let Some(&h0) = h.first() else { continue };
        let (aud, param, idr, slice, sei) = if hevc {
            let t = (h0 >> 1) & 0x3f;
            (
                t == 35,
                (32..=34).contains(&t),
                (16..=23).contains(&t),
                t < 32,
                t == 39, // prefix SEI (a suffix SEI, 40, belongs to the picture before)
            )
        } else {
            let t = h0 & 0x1f;
            (
                t == 9,
                t == 7 || t == 8,
                t == 5,
                (1..=5).contains(&t),
                t == 6,
            )
        };
        // A new unit starts at a delimiter, or, when the stream has no delimiters, at a
        // parameter set, an SEI or a first slice after a slice (H.264 §7.4.1.2.3; first_mb
        // / first-slice detection simplified).
        if out.is_empty()
            || aud
            || (has_slice
                && (param
                    || sei
                    || (slice && !out.last().unwrap().data.is_empty() && first_slice(h, hevc))))
        {
            out.push(AccessUnit {
                keyframe: false,
                params: Vec::new(),
                data: Vec::new(),
            });
            has_slice = false;
        }
        let au = out.last_mut().unwrap();
        // Parameter sets stay in the access unit (in-band) and are also collected for the
        // extra data.
        if param {
            au.params.extend_from_slice(n);
        }
        au.data.extend_from_slice(n);
        if idr {
            au.keyframe = true;
        }
        has_slice |= slice;
    }
    // A unit holding only parameter sets (no slice) belongs to the next one.
    let mut merged: Vec<AccessUnit> = Vec::new();
    let mut carry: Option<AccessUnit> = None;
    for mut au in out {
        let has_slice = nal_units(&au.data)
            .iter()
            .any(|n| nal_kind(n, hevc).is_some_and(|(aud, p)| !aud && !p));
        if let Some(c) = carry.take() {
            // Keep "AUD first" (§7.1.1): the carried NAL units go after this unit's AUD.
            let at = nal_units(&au.data)
                .first()
                .filter(|n| nal_kind(n, hevc).is_some_and(|(aud, _)| aud))
                .map_or(0, |n| {
                    (n.as_ptr() as usize - au.data.as_ptr() as usize) + n.len()
                });
            au.params = [c.params, au.params].concat();
            au.data = [&au.data[..at], &c.data[..], &au.data[at..]].concat();
        }
        if has_slice {
            merged.push(au);
        } else {
            carry = Some(au);
        }
    }
    merged
}

/// Whether a slice NAL starts a new picture: first_mb_in_slice (H.264) or
/// first_slice_segment_in_pic_flag (HEVC) is 0 / set, i.e. the first bit after the
/// header is 1.
fn first_slice(h: &[u8], hevc: bool) -> bool {
    let skip = if hevc { 2 } else { 1 };
    h.get(skip).is_some_and(|b| b & 0x80 != 0)
}

/// Splits an ADTS stream into (AudioSpecificConfig, raw AAC frames).
pub fn adts_frames(b: &[u8]) -> Option<([u8; 2], Vec<&[u8]>)> {
    let mut frames = Vec::new();
    let mut asc = None;
    let mut i = 0;
    while i + 7 <= b.len() {
        if b[i] != 0xff || b[i + 1] & 0xf0 != 0xf0 {
            return None;
        }
        let len =
            ((b[i + 3] as usize & 3) << 11) | ((b[i + 4] as usize) << 3) | (b[i + 5] as usize >> 5);
        let hdr = if b[i + 1] & 1 == 0 { 9 } else { 7 };
        if len < hdr || i + len > b.len() {
            return None;
        }
        let profile = (b[i + 2] >> 6) + 1;
        let freq = (b[i + 2] >> 2) & 0xf;
        let chan = ((b[i + 2] & 1) << 2) | (b[i + 3] >> 6);
        asc.get_or_insert([
            (profile << 3) | (freq >> 1),
            ((freq & 1) << 7) | (chan << 3),
        ]);
        frames.push(&b[i + hdr..i + len]);
        i += len;
    }
    Some((asc?, frames))
}

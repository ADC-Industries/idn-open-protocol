//! A file's own compressed video passed through to receivers that decode it (R.12).

use super::*;

/// Whether the file's video can go out as it is (§E.18, R.12): H.264/HEVC, no
/// B-frames, keyframes at most 2.5 s apart (scanned over the first seconds).
pub(super) fn pass_through(demux: &mut Demuxer, v: usize) -> Result<PassThrough, String> {
    let s = &demux.streams[v];
    let codec = match s.codec_id {
        ff::AVCodecID::AV_CODEC_ID_H264 => Codec::H264,
        ff::AVCodecID::AV_CODEC_ID_HEVC => Codec::Hevc,
        other => {
            return Err(format!(
                "{} is decoded and sent as SpeedHQ",
                codec_name(other)
            ));
        }
    };
    if s.video_delay > 0 {
        return Err("the file has B-frames".into());
    }
    // Receivers would show the compressed pictures as stored: on their side.
    if s.rotation != 0 {
        return Err(format!(
            "the picture is turned {} degrees (a phone's): decoded and turned",
            s.rotation
        ));
    }
    let (length_size, params) = parse_config(codec, &s.extradata)?;
    let (index, tb) = (s.index, s.time_base);
    let secs = |pts: i64| pts as f64 * tb.0 as f64 / tb.1.max(1) as f64;
    let (mut keys, mut first, mut last) = (Vec::new(), None, 0.0);
    // At most so many packets of any stream: a file whose video ends early (a short clip
    // on a long recording) was otherwise read to its end before it played.
    let mut budget = 4000;
    while let Ok(Some(p)) = demux.read() {
        budget -= 1;
        if budget == 0 {
            break;
        }
        if p.stream != index {
            continue;
        }
        let (Some(pts), dts) = (p.pts, p.dts) else {
            continue;
        };
        if dts.is_some_and(|d| d != pts) {
            return Err("the file has B-frames".into());
        }
        let t = secs(pts);
        first.get_or_insert(t);
        last = t;
        if p.keyframe {
            keys.push(t);
        }
        if t - first.unwrap() > 6.0 {
            break;
        }
    }
    let gap = keys
        .windows(2)
        .map(|w| w[1] - w[0])
        .fold(keys.first().map_or(f64::MAX, |_| 0.0), f64::max)
        .max(if keys.len() < 2 {
            last - keys.first().copied().unwrap_or(0.0)
        } else {
            0.0
        });
    if keys.is_empty() || gap > 2.5 {
        return Err(format!(
            "keyframes {gap:.1} s apart (at most 2.5 for pass-through)"
        ));
    }
    Ok(PassThrough {
        codec,
        length_size,
        params,
    })
}

/// Length-prefixed NAL units to Annex B.
pub(crate) fn to_annex_b(data: &[u8], length_size: Option<usize>) -> Option<Vec<u8>> {
    let Some(ls) = length_size else {
        return Some(data.to_vec());
    };
    let mut out = Vec::with_capacity(data.len() + 16);
    let mut at = 0;
    while at < data.len() {
        let len = data
            .get(at..at + ls)?
            .iter()
            .fold(0usize, |a, &b| (a << 8) | b as usize);
        at += ls;
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(data.get(at..at + len)?);
        at += len;
    }
    Some(out)
}

pub(super) fn codec_name(id: ff::AVCodecID) -> String {
    // SAFETY: avcodec_get_name always returns a static NUL-terminated name.
    unsafe { std::ffi::CStr::from_ptr(ff::avcodec_get_name(id)) }
        .to_string_lossy()
        .into_owned()
}

/// A video stream's colour description into an output's: the matrix (unstated: BT.601
/// below 720 lines, else BT.709, as FFmpeg assumes), PQ/HLG and BT.2020 primaries.
/// Pictures go out with the file's own Y'CbCr (only resampled, and full range made
/// limited), so this is what they are. RGB files (converted here) keep the defaults. An
/// HLG phone video was labelled BT.709 SDR, and an SD file got the HD matrix.
pub(super) fn stream_colour(
    s: &crate::ffi::StreamInfo,
    c: &mut idn_open_protocol::color::ColorConfig,
) {
    use ff::AVColorPrimaries as P;
    use ff::AVColorSpace as S;
    use ff::AVColorTransferCharacteristic as T;
    use idn_open_protocol::color::{Matrix, Primaries, Transfer};
    let fmt = crate::ffi::pix_fmt(s.pix_fmt);
    // SAFETY: a descriptor lookup (null for an unknown format, checked below).
    let desc = unsafe { ff::av_pix_fmt_desc_get(fmt) };
    if s.pix_fmt < 0
        || desc.is_null()
        // SAFETY: `desc` checked non-null just before.
        || unsafe { (*desc).flags } & ff::AV_PIX_FMT_FLAG_RGB as u64 != 0
    {
        return;
    }
    c.matrix = match s.color_space {
        S::AVCOL_SPC_BT470BG | S::AVCOL_SPC_SMPTE170M => Matrix::Bt601,
        S::AVCOL_SPC_BT2020_NCL | S::AVCOL_SPC_BT2020_CL => Matrix::Bt2020,
        S::AVCOL_SPC_BT709 => Matrix::Bt709,
        _ if s.height > 0 && s.height < 720 => Matrix::Bt601,
        _ => Matrix::Bt709,
    };
    c.transfer = match s.color_trc {
        T::AVCOL_TRC_SMPTE2084 => Transfer::Pq,
        T::AVCOL_TRC_ARIB_STD_B67 => Transfer::Hlg,
        _ => Transfer::Sdr,
    };
    c.primaries = match s.color_primaries {
        P::AVCOL_PRI_BT2020 => Primaries::Bt2020,
        _ => Primaries::Bt709,
    };
    c.limited = true;
}

/// avcC / hvcC (MP4, MKV): the NAL length size and the parameter sets in Annex B.
pub(crate) fn parse_config(codec: Codec, x: &[u8]) -> Result<(Option<usize>, Vec<u8>), String> {
    if x.is_empty() || x.starts_with(&[0, 0, 1]) || x.starts_with(&[0, 0, 0, 1]) {
        return Ok((None, x.to_vec())); // already Annex B
    }
    let bad = || "unreadable codec configuration".to_owned();
    let mut out = Vec::new();
    let mut nal = |b: &[u8]| {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(b);
    };
    let take = |at: &mut usize, n: usize| -> Result<&[u8], String> {
        let s = x.get(*at..*at + n).ok_or_else(bad)?;
        *at += n;
        Ok(s)
    };
    let u16_at = |at: &mut usize| -> Result<usize, String> {
        let b = take(at, 2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]) as usize)
    };
    match codec {
        Codec::H264 => {
            let len = (*x.get(4).ok_or_else(bad)? & 3) as usize + 1;
            let mut at = 5;
            let sps = (take(&mut at, 1)?[0] & 0x1f) as usize;
            for _ in 0..sps {
                let n = u16_at(&mut at)?;
                nal(take(&mut at, n)?);
            }
            let pps = take(&mut at, 1)?[0] as usize;
            for _ in 0..pps {
                let n = u16_at(&mut at)?;
                nal(take(&mut at, n)?);
            }
            Ok((Some(len), out))
        }
        Codec::Hevc => {
            let len = (*x.get(21).ok_or_else(bad)? & 3) as usize + 1;
            let mut at = 22;
            let arrays = take(&mut at, 1)?[0] as usize;
            for _ in 0..arrays {
                let _kind = take(&mut at, 1)?;
                let n = u16_at(&mut at)?;
                for _ in 0..n {
                    let l = u16_at(&mut at)?;
                    nal(take(&mut at, l)?);
                }
            }
            Ok((Some(len), out))
        }
        Codec::Av1 => Err(bad()),
    }
}

impl EncoderFactory for WithFileCodec {
    fn codecs(&self) -> Vec<Codec> {
        let mut c = EncoderFactory::codecs(&*self.gpu);
        if let Some(f) = self.file.filter(|f| !c.contains(f)) {
            c.push(f);
        }
        c
    }

    fn open(&self, cfg: &EncoderConfig) -> io::Result<Box<dyn VideoEncoder>> {
        EncoderFactory::open(&*self.gpu, cfg)
    }
}

//! Decoded pictures made planar for sending: colour range, 4:2:0 or 4:2:2, stills,
//! rotation.

use super::*;

/// [`to_planar`] from a mapped GPU surface (`uncached`, [`Frame::mapped`]) or memory.
pub(super) fn to_planar_from(
    f: FrameRef,
    scaler: &mut Option<Scaler>,
    scaled: &mut Option<Frame>,
    coded: Option<CodedPicture>,
    uncached: bool,
) -> io::Result<PlanarFrame> {
    let (w, h) = f.size();
    // A picture with alpha (a stinger, a lower third) keeps it: SHQ7, 4:2:2 and alpha
    // (a ProRes 4444 stinger covered the whole picture).
    let alpha = f.has_alpha();
    // A screen's BGR0 (or RGB): the library's converter, on all cores with AVX2 (swscale
    // took 22 ms a 2160p picture on one).
    if let Some((data, stride, bgr)) = f.packed_rgb() {
        use idn_open_protocol::color::{AlphaMode, ColorConfig, bgra_to_planes, rgba_to_planes};
        let convert = if bgr { bgra_to_planes } else { rgba_to_planes };
        let (layout, mode) = if alpha {
            (Fourcc::Shq7, AlphaMode::Straight)
        } else {
            (Fourcc::Shq2, AlphaMode::Opaque)
        };
        let planes = convert(data, w, h, stride, layout, mode, &ColorConfig::default());
        return Ok(PlanarFrame {
            layout,
            width: w,
            height: h,
            planes: Arc::new(planes),
            timestamp: None,
            timecode: None,
            audio: None,
            coded,
        });
    }
    let layout = if alpha {
        Fourcc::Shq7
    } else if sends_420(&f) {
        Fourcc::Shq0
    } else {
        Fourcc::Shq2
    };
    let direct = if layout == Fourcc::Shq0 {
        f.planes_420(uncached)
    } else {
        None
    };
    let mut planes = match direct {
        Some(p) => Vec::from(p),
        None => {
            let (fmt, ch) = match layout {
                Fourcc::Shq0 => (ff::AVPixelFormat::AV_PIX_FMT_YUV420P, h.div_ceil(2)),
                Fourcc::Shq7 => (ff::AVPixelFormat::AV_PIX_FMT_YUVA422P, h),
                _ => (ff::AVPixelFormat::AV_PIX_FMT_YUV422P, h),
            };
            if scaled
                .as_ref()
                .is_none_or(|d| d.size() != (w, h) || d.format() != fmt)
            {
                *scaled = Some(Frame::alloc(w, h, fmt)?);
            }
            let dst = scaled.as_ref().unwrap();
            Scaler::run(scaler, f, dst)?;
            let cw = w.div_ceil(2);
            let mut p = vec![
                dst.to_plane(0, w, h),
                dst.to_plane(1, cw, ch),
                dst.to_plane(2, cw, ch),
            ];
            if layout == Fourcc::Shq7 {
                p.push(dst.to_plane(3, w, h));
            }
            p
        }
    };
    // Full range (JPEG, many phones) made limited: receivers, NDI's among them, take
    // pictures as limited range, and full range shown so clipped its whites and blacks.
    if f.full_range() {
        to_limited(&mut planes);
    }
    Ok(PlanarFrame {
        layout,
        width: w,
        height: h,
        planes: Arc::new(planes),
        timestamp: None,
        timecode: None,
        audio: None,
        coded,
    })
}

/// A decoded or captured picture as planes to send, BT.709 limited range, with its
/// compressed form when passing through. Above 1080p a 4:2:0 picture (nearly every
/// file's and decoder's) goes out 4:2:0 (SHQ0), as NDI 6.3's senders send such sizes
/// (spec §3.1): no chroma rows made up (8.4 ms a 2160p picture in swscale), and a quarter
/// fewer blocks to code. Everything else as 4:2:2, as NDI's.
pub(crate) fn to_planar(
    f: FrameRef,
    scaler: &mut Option<Scaler>,
    scaled: &mut Option<Frame>,
    coded: Option<CodedPicture>,
) -> io::Result<PlanarFrame> {
    to_planar_from(f, scaler, scaled, coded, false)
}

/// Whether [`to_planar`] sends this picture as 4:2:0: above 1080p, from 4:2:0.
pub(super) fn sends_420(f: &FrameRef) -> bool {
    let (w, h) = f.size();
    w * h > 1920 * 1080 && f.is_420()
}

/// Full-range Y'CbCr planes (Y, Cb, Cr) to limited range, in place.
pub(super) fn to_limited(planes: &mut [idn_open_protocol::shq::Plane]) {
    let lut = |f: &dyn Fn(f32) -> f32| -> [u8; 256] {
        std::array::from_fn(|v| f(v as f32).round().clamp(0.0, 255.0) as u8)
    };
    let y = lut(&|v| 16.0 + v * 219.0 / 255.0);
    let c = lut(&|v| 128.0 + (v - 128.0) * 224.0 / 255.0);
    // Alpha (a fourth plane) stays as it is.
    for (k, p) in planes.iter_mut().take(3).enumerate() {
        let t = if k == 0 { &y } else { &c };
        for v in p.data.iter_mut() {
            *v = t[*v as usize];
        }
    }
}

/// A decoded still as 4:2:2 planes (and alpha), BT.709: swscale's default matrix is
/// BT.601 whatever the size (red came out as BT.601's). A YCbCr picture
/// (JPEG: BT.601, full range) goes through RGB with its own matrix.
pub(super) fn still_planar(f: FrameRef) -> io::Result<PlanarFrame> {
    use ff::AVPixelFormat::*;
    let (w, h) = f.size();
    let alpha = f.has_alpha();
    let rgb;
    let src = if f.is_rgb() {
        f
    } else {
        rgb = Frame::alloc(
            w,
            h,
            if alpha {
                AV_PIX_FMT_RGBA
            } else {
                AV_PIX_FMT_RGB24
            },
        )?;
        crate::ffi::convert_with(f, &rgb, f.ycbcr(), (true, true))?;
        rgb.as_ref()
    };
    let dst = Frame::alloc(
        w,
        h,
        if alpha {
            AV_PIX_FMT_YUVA422P
        } else {
            AV_PIX_FMT_YUV422P
        },
    )?;
    crate::ffi::convert_with(src, &dst, (false, true), (true, false))?;
    let cw = w.div_ceil(2);
    let mut planes = vec![
        dst.to_plane(0, w, h),
        dst.to_plane(1, cw, h),
        dst.to_plane(2, cw, h),
    ];
    if alpha {
        planes.push(dst.to_plane(3, w, h));
    }
    Ok(PlanarFrame {
        layout: if alpha { Fourcc::Shq7 } else { Fourcc::Shq2 },
        width: w,
        height: h,
        planes: Arc::new(planes),
        timestamp: None,
        timecode: None,
        audio: None,
        coded: None,
    })
}

/// A picture file (PNG, JPEG, …: anything FFmpeg reads, its first frame) as 4:2:2 planes,
/// decoded once: for showing a still as it is, rather than playing it as a looped
/// one-frame file (the switcher's stills went through a 4K H.264 encode and decode at
/// 4–10 pictures a second). BT.709, limited range; with its alpha (SHQ7)
/// when the picture has one.
pub fn still(path: impl AsRef<Path>) -> io::Result<PlanarFrame> {
    let path = path.as_ref().to_string_lossy().into_owned();
    let mut demux = Demuxer::open(&path)?;
    let v = demux
        .streams
        .iter()
        .position(|s| s.video)
        .ok_or_else(|| io::Error::other(format!("{path}: no picture")))?;
    let mut dec = FileDecoder::open(&demux.streams[v], None)?;
    let first = |dec: &mut FileDecoder| -> io::Result<Option<PlanarFrame>> {
        match dec.receive()? {
            Some((f, _)) => still_planar(f).map(Some),
            None => Ok(None),
        }
    };
    while let Some(p) = demux.read()? {
        if p.stream != v {
            continue;
        }
        dec.send(&p)?;
        if let Some(f) = first(&mut dec)? {
            return Ok(f);
        }
    }
    dec.drain();
    first(&mut dec)?.ok_or_else(|| io::Error::other(format!("{path}: no picture decoded")))
}

/// A picture turned counter-clockwise by `ccw` degrees (90, 180, 270): what the file's
/// display matrix asks for (a phone's portrait video went out lying on its side). 4:2:0
/// turns as it is. 4:2:2's chroma after a quarter turn is full width and half height:
/// averaged across and repeated down, back to 4:2:2.
pub(super) fn rotated(pf: PlanarFrame, ccw: u16) -> PlanarFrame {
    let q = (ccw / 90) % 4;
    if q == 0 || pf.planes.len() < 3 {
        return pf;
    }
    let (w, h) = if q == 2 {
        (pf.width, pf.height)
    } else {
        (pf.height, pf.width)
    };
    let chroma = |c: &idn_open_protocol::shq::Plane| {
        let t = turn(c, q);
        if q == 2 || pf.layout == Fourcc::Shq0 {
            return t;
        }
        let (cw, rw, rh) = (w.div_ceil(2), t.width, t.height);
        let mut data = Vec::with_capacity(cw * h);
        for row in 0..h {
            let src = &t.data[(row / 2).min(rh - 1) * rw..][..rw];
            data.extend((0..cw).map(|x| {
                let (a, b) = (src[(2 * x).min(rw - 1)], src[(2 * x + 1).min(rw - 1)]);
                (a as u16 + b as u16).div_ceil(2) as u8
            }));
        }
        idn_open_protocol::shq::Plane {
            width: cw,
            height: h,
            data,
        }
    };
    // The planes on threads of their own (luma is two thirds of the work).
    let planes = std::thread::scope(|sc| {
        let u = sc.spawn(|| chroma(&pf.planes[1]));
        let v = sc.spawn(|| chroma(&pf.planes[2]));
        let y = turn(&pf.planes[0], q);
        vec![y, u.join().expect("turning"), v.join().expect("turning")]
    });
    PlanarFrame {
        width: w,
        height: h,
        planes: Arc::new(planes),
        ..pf
    }
}

/// A plane turned counter-clockwise by `quarters` quarter turns (1–3), in 64 × 64 tiles
/// read a source row at a time (a plain transpose read the source a column at a time, a
/// cache miss a sample).
pub(super) fn turn(
    p: &idn_open_protocol::shq::Plane,
    quarters: u16,
) -> idn_open_protocol::shq::Plane {
    let (w, h, d) = (p.width, p.height, &p.data);
    if quarters == 2 {
        return idn_open_protocol::shq::Plane {
            width: w,
            height: h,
            data: d.iter().rev().copied().collect(),
        };
    }
    // Quarter turns: the result is h wide and w high. Source (x, y) goes to
    // counter-clockwise (y, w-1-x), clockwise (h-1-y, x).
    let mut out = vec![0u8; w * h];
    const T: usize = 64;
    for ty in (0..h).step_by(T) {
        for tx in (0..w).step_by(T) {
            let xs = tx..(tx + T).min(w);
            for y in ty..(ty + T).min(h) {
                let src = &d[y * w..(y + 1) * w][xs.clone()];
                if quarters == 1 {
                    for (x, &v) in xs.clone().zip(src) {
                        out[(w - 1 - x) * h + y] = v;
                    }
                } else {
                    let col = h - 1 - y;
                    for (x, &v) in xs.clone().zip(src) {
                        out[x * h + col] = v;
                    }
                }
            }
        }
    }
    idn_open_protocol::shq::Plane {
        width: h,
        height: w,
        data: out,
    }
}

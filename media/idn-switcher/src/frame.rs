//! Pictures as the compositor works on them: 8-bit Y'CbCr 4:2:2 planes, limited range
//! (Y 16–235, Cb/Cr 16–240), with an optional alpha plane (0–255), at the program size
//! (spec/extensions/idn-switcher.md §S.2).

use idn_common::sync::LockOk as _;
use idn_open_protocol::shq::{CoefPicture, Fourcc, Picture, Plane};
use std::sync::Arc;

/// An HDR source's picture at full precision (R.24 stage 2, idn-switcher.md §S.34): its
/// 16-bit Y, Cb, Cr (4:2:2, P216's scale), in its transfer, beside the 8-bit [`Frame`] the
/// rest of the switcher uses. An HDR program's compositor takes these instead of lifting
/// the 8-bit picture, so the source keeps its highlights. Empty planes: none at 16 bits,
/// but the 8-bit samples are already in that transfer (copied up, not lifted).
pub struct Hdr16 {
    pub transfer: idn_open_protocol::color::Transfer,
    pub y: Vec<u16>,
    pub cb: Vec<u16>,
    pub cr: Vec<u16>,
}

/// Frames that have their [`Hdr16`] (kept beside them, not in them: the picture's own
/// `Arc` carries it to the compositor; a picture made from it, scaled or filtered, has
/// none and is lifted).
static HDR16: std::sync::Mutex<Vec<(std::sync::Weak<Frame>, Arc<Hdr16>)>> =
    std::sync::Mutex::new(Vec::new());

/// Gives `f` its full-precision planes.
pub fn attach_hdr16(f: &Arc<Frame>, h: Arc<Hdr16>) {
    let mut v = HDR16.lock_ok();
    v.retain(|(w, _)| w.strong_count() > 0);
    v.push((Arc::downgrade(f), h));
}

/// `f`'s full-precision planes, if it has them.
pub fn hdr16_of(f: &Arc<Frame>) -> Option<Arc<Hdr16>> {
    let v = HDR16.lock_ok();
    v.iter()
        .find(|(w, _)| std::ptr::eq(w.as_ptr(), Arc::as_ptr(f)))
        .map(|(_, h)| h.clone())
}

/// A 4:2:2 picture: `cb` and `cr` are `(width + 1) / 2` samples wide, full height.
#[derive(Debug, PartialEq)]
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub cb: Vec<u8>,
    pub cr: Vec<u8>,
    /// Coverage, 0 (transparent) to 255 (opaque); `None`: opaque.
    pub a: Option<Vec<u8>>,
    /// A picture that came as coefficients, for the GPU to reconstruct (perf queue 2b),
    /// its planes empty (`a` is `Some(empty)` when it has alpha). On the CPU it is
    /// reconstructed where samples are needed ([`Frame::pixels`], [`Frame::scaled`]).
    pub coefs: Option<Arc<CoefPicture>>,
    /// A picture kept on the GPU (§E.27: a source on this machine's, or one decoded there),
    /// its planes empty: copied into the compositor's buffers on the GPU, read back where
    /// samples are needed ([`Frame::pixels`]).
    pub gpu: Option<Arc<OnGpu>>,
}

/// A picture on the compositor's GPU: its planes (4:2:0 or 4:2:2) in a buffer there.
pub struct OnGpu {
    #[cfg(feature = "gpu")]
    pub picture: idn_open_protocol::gpu::GpuPicture,
    #[cfg(feature = "gpu")]
    pub hand: idn_open_protocol::gpu::GpuHandIn,
}

impl std::fmt::Debug for OnGpu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OnGpu")
    }
}

impl PartialEq for OnGpu {
    /// The same picture only.
    fn eq(&self, other: &OnGpu) -> bool {
        std::ptr::eq(self, other)
    }
}

impl OnGpu {
    /// Its samples, read back (4:2:2, or 4:2:0 as decoded).
    #[cfg(feature = "gpu")]
    fn read(&self) -> Result<Picture, String> {
        self.read_of(&self.picture)
    }

    /// Shrunk to `w` × `h` on the GPU, then read back: a tile's worth, not the whole
    /// picture (the multiview's tiles of 2160p pictures kept there read each whole, 50-140
    /// ms of a frame, lab 381).
    #[cfg(feature = "gpu")]
    fn read_shrunk(&self, w: usize, h: usize) -> Result<Picture, String> {
        static SHRINK: std::sync::Mutex<
            Option<(Option<u64>, Arc<idn_open_protocol::gpu::PlaneShrink>)>,
        > = std::sync::Mutex::new(None);
        let at = &self.picture.planes;
        let (pw, ph) = (at[0].width as usize, at[0].height as usize);
        let sizes: Vec<(u32, u32)> = at
            .iter()
            .take(3)
            .map(|p| {
                (
                    (p.width as usize * w).div_ceil(pw.max(1)) as u32,
                    (p.height as usize * h).div_ceil(ph.max(1)) as u32,
                )
            })
            .collect();
        let shrink = {
            let mut s = SHRINK.lock_ok();
            let key = idn_open_protocol::gpu::device_key(&self.hand.device);
            if s.as_ref().is_none_or(|(k, _)| key.is_none() || *k != key) {
                *s = Some((
                    key,
                    Arc::new(idn_open_protocol::gpu::PlaneShrink::new(&self.hand.device)),
                ));
            }
            s.as_ref().expect("made above").1.clone()
        };
        let small = shrink
            .run(&self.hand, &self.picture, &sizes)
            .ok_or("not shrunk on the GPU")?;
        self.read_of(&small)
    }

    #[cfg(feature = "gpu")]
    fn read_of(&self, picture: &idn_open_protocol::gpu::GpuPicture) -> Result<Picture, String> {
        let planes = picture.read_planes(&self.hand).map_err(|e| e.to_string())?;
        let at = &picture.planes;
        let (w, h) = (at[0].width as usize, at[0].height as usize);
        let full = at.get(1).is_some_and(|c| c.height as usize == h);
        Ok(Picture {
            fourcc: if full { Fourcc::Shq2 } else { Fourcc::Shq0 },
            width: w,
            height: h,
            format: idn_open_protocol::shq::Format::New,
            quality: 100,
            short_dc: false,
            high_precision: false,
            planes: planes
                .into_iter()
                .zip(at)
                .map(|(data, a)| Plane {
                    width: a.width as usize,
                    height: a.height as usize,
                    data,
                })
                .collect(),
            precise: None,
            coefficients: None,
        })
    }
}

/// A colour as Y'CbCr, limited range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ycc(pub u8, pub u8, pub u8);

impl Ycc {
    pub const BLACK: Ycc = Ycc(16, 128, 128);
    pub const WHITE: Ycc = Ycc(235, 128, 128);

    /// From full-range RGB (BT.709).
    pub fn from_rgb(r: u8, g: u8, b: u8) -> Ycc {
        let (r, g, b) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let cb = (b - y) / 1.8556;
        let cr = (r - y) / 1.5748;
        let q = |v: f32| v.round().clamp(0.0, 255.0) as u8;
        Ycc(
            q(16.0 + 219.0 * y),
            q(128.0 + 224.0 * cb),
            q(128.0 + 224.0 * cr),
        )
    }
}

impl Ycc {
    /// To full-range RGB (BT.709), as `#rrggbb`.
    pub fn to_hex(self) -> String {
        let y = (self.0 as f32 - 16.0) / 219.0;
        let cb = (self.1 as f32 - 128.0) / 224.0;
        let cr = (self.2 as f32 - 128.0) / 224.0;
        let r = y + 1.5748 * cr;
        let g = y - 0.1873 * cb - 0.4681 * cr;
        let b = y + 1.8556 * cb;
        let q = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
        format!("#{:02x}{:02x}{:02x}", q(r), q(g), q(b))
    }
}

impl Clone for Frame {
    /// A copy whose planes come from the memory dropped frames hand back (else each
    /// compose step starts from fresh pages, a 2160p frame's 16 MB faulted in each time).
    fn clone(&self) -> Frame {
        let copy = |v: &Vec<u8>| {
            if v.is_empty() {
                return Vec::new();
            }
            let mut d = Plane::buffer(v.len());
            d.copy_from_slice(v);
            d
        };
        Frame {
            width: self.width,
            height: self.height,
            y: copy(&self.y),
            cb: copy(&self.cb),
            cr: copy(&self.cr),
            a: self.a.as_ref().map(copy),
            coefs: self.coefs.clone(),
            gpu: self.gpu.clone(),
        }
    }
}

impl Drop for Frame {
    /// Its planes go back to the decoder for its next picture (an input's planes are the
    /// decoder's, [`Frame::from_owned`]); elsewhere they are simply freed.
    fn drop(&mut self) {
        let t = std::mem::take;
        for data in [t(&mut self.y), t(&mut self.cb), t(&mut self.cr)]
            .into_iter()
            .chain(self.a.take())
        {
            if data.capacity() > 0 {
                Plane {
                    width: 0,
                    height: 0,
                    data,
                }
                .recycle();
            }
        }
    }
}

impl Frame {
    /// The chroma planes' width for a picture `width` wide.
    pub fn chroma_width(width: usize) -> usize {
        width.div_ceil(2)
    }

    /// A picture of one colour, opaque.
    pub fn solid(width: usize, height: usize, c: Ycc) -> Frame {
        let cw = Frame::chroma_width(width);
        Frame {
            width,
            height,
            y: vec![c.0; width * height],
            cb: vec![c.1; cw * height],
            cr: vec![c.2; cw * height],
            a: None,
            coefs: None,
            gpu: None,
        }
    }

    /// Black.
    pub fn black(width: usize, height: usize) -> Frame {
        Frame::solid(width, height, Ycc::BLACK)
    }

    /// A decoded picture in any layout, as 4:2:2 (with its alpha when it has one).
    /// As [`Frame::from_picture`], taking the picture: 4:2:2 planes of exactly the
    /// picture's size (SpeedHQ's usual layout, every width since the decoder writes the
    /// visible samples only) become the frame's as they are, with no copy (each input's
    /// pictures were copied, 7% of a switcher with four 1080p30 inputs); others are
    /// converted and the picture's memory goes back to the decoder.
    pub fn from_owned(p: Picture) -> Frame {
        let (w, h) = (p.width, p.height);
        if let Some(c) = p.coefficients {
            return Frame {
                width: w,
                height: h,
                y: Vec::new(),
                cb: Vec::new(),
                cr: Vec::new(),
                a: c.fourcc.has_alpha().then(Vec::new),
                coefs: Some(c),
                gpu: None,
            };
        }
        let cw = Frame::chroma_width(w);
        let fits =
            |pl: &Plane, pw: usize| pl.width == pw && pl.height == h && pl.data.len() == pw * h;
        let as_is = matches!(p.fourcc, Fourcc::Shq2 | Fourcc::Shq7)
            && p.planes.len() >= 3
            && fits(&p.planes[0], w)
            && fits(&p.planes[1], cw)
            && fits(&p.planes[2], cw)
            && (!p.fourcc.has_alpha() || p.planes.get(3).is_some_and(|a| fits(a, w)));
        if !as_is {
            let f = Frame::from_picture(&p);
            p.recycle();
            return f;
        }
        let mut planes = p.planes.into_iter().map(|pl| pl.data);
        let (y, cb, cr) = (
            planes.next().unwrap_or_default(),
            planes.next().unwrap_or_default(),
            planes.next().unwrap_or_default(),
        );
        let a = planes.next();
        Frame {
            width: w,
            height: h,
            y,
            cb,
            cr,
            a,
            coefs: None,
            gpu: None,
        }
    }

    pub fn from_picture(p: &Picture) -> Frame {
        let (w, h) = (p.width, p.height);
        let cw = Frame::chroma_width(w);
        let y = crop(&p.planes[0], w, h);
        let chroma = |pl: &Plane| -> Vec<u8> {
            match p.fourcc {
                Fourcc::Shq2 | Fourcc::Shq7 => crop(pl, cw, h),
                // 4:2:0: each chroma row serves two picture rows.
                Fourcc::Shq0 => {
                    let mut out = Vec::with_capacity(cw * h);
                    for row in 0..h {
                        let r = (row / 2).min(pl.height.saturating_sub(1));
                        out.extend((0..cw).map(|x| sample(pl, x, r)));
                    }
                    out
                }
                // 4:4:4: two chroma samples become one.
                Fourcc::Shq4 | Fourcc::Shq9 => {
                    let mut out = Vec::with_capacity(cw * h);
                    for row in 0..h {
                        out.extend((0..cw).map(|x| {
                            let a = sample(pl, 2 * x, row) as u16;
                            let b = sample(pl, (2 * x + 1).min(w - 1), row) as u16;
                            (a + b).div_ceil(2) as u8
                        }));
                    }
                    out
                }
            }
        };
        Frame {
            width: w,
            height: h,
            y,
            cb: chroma(&p.planes[1]),
            cr: chroma(&p.planes[2]),
            a: p.fourcc.has_alpha().then(|| crop(&p.planes[3], w, h)),
            coefs: None,
            gpu: None,
        }
    }

    /// The picture with its samples: itself, or reconstructed on the CPU from its
    /// coefficients.
    pub fn pixels(&self) -> Frame {
        #[cfg(feature = "gpu")]
        if let Some(g) = &self.gpu {
            return match g.read() {
                Ok(p) => Frame::from_owned(p),
                Err(_) => Frame::black(self.width, self.height),
            };
        }
        match &self.coefs {
            Some(c) => Frame::from_owned(idn_open_protocol::shq::reconstruct(c)),
            None => self.clone(),
        }
    }

    /// Whether its samples aren't here (coefficients, or kept on the GPU): those who need
    /// them take [`pixels`](Self::pixels).
    pub fn deferred(&self) -> bool {
        self.coefs.is_some() || self.gpu.is_some()
    }

    /// A picture kept on the GPU (§E.27), as decoded or received there: its sizes, no
    /// samples here.
    #[cfg(feature = "gpu")]
    pub fn on_gpu(
        picture: idn_open_protocol::gpu::GpuPicture,
        hand: idn_open_protocol::gpu::GpuHandIn,
    ) -> Frame {
        let (w, h) = (
            picture.planes[0].width as usize,
            picture.planes[0].height as usize,
        );
        Frame {
            width: w,
            height: h,
            y: Vec::new(),
            cb: Vec::new(),
            cr: Vec::new(),
            a: None,
            coefs: None,
            gpu: Some(Arc::new(OnGpu { picture, hand })),
        }
    }

    /// The planes as an output sends them (Y, Cb, Cr; the alpha is dropped: the program
    /// is opaque).
    pub fn into_planes(mut self) -> Vec<Plane> {
        if self.deferred() {
            return self.pixels().into_planes();
        }
        let cw = Frame::chroma_width(self.width);
        let t = std::mem::take;
        vec![
            Plane {
                width: self.width,
                height: self.height,
                data: t(&mut self.y),
            },
            Plane {
                width: cw,
                height: self.height,
                data: t(&mut self.cb),
            },
            Plane {
                width: cw,
                height: self.height,
                data: t(&mut self.cr),
            },
        ]
    }

    /// The alpha at a pixel (255 without an alpha plane).
    pub fn alpha(&self, x: usize, y: usize) -> u8 {
        self.a
            .as_ref()
            .and_then(|a| a.get(y * self.width + x).copied())
            .unwrap_or(255)
    }

    /// This picture fitted into `width` × `height`: scaled keeping its aspect, centred,
    /// the rest black (transparent when it has alpha). The same picture when it fits
    /// exactly.
    pub fn fitted(&self, width: usize, height: usize) -> Frame {
        if (self.width, self.height) == (width, height) {
            return self.pixels();
        }
        let s = (width as f64 / self.width.max(1) as f64)
            .min(height as f64 / self.height.max(1) as f64);
        let (sw, sh) = (
            ((self.width as f64 * s).round() as usize).clamp(1, width),
            ((self.height as f64 * s).round() as usize).clamp(1, height),
        );
        let scaled = self.scaled((0.0, 0.0, 1.0, 1.0), sw, sh);
        let mut out = Frame::black(width, height);
        if self.a.is_some() {
            out.a = Some(vec![0; width * height]);
        }
        // Even x offset, so chroma columns line up.
        let (ox, oy) = (((width - sw) / 2) & !1, (height - sh) / 2);
        out.paste(&scaled, ox, oy);
        out
    }

    /// The part `crop` (x0, y0, x1, y1 as fractions of the picture) scaled to exactly
    /// `width` × `height`, bilinear (8-bit weights, rows in parallel).
    pub fn scaled(&self, crop: (f64, f64, f64, f64), width: usize, height: usize) -> Frame {
        #[cfg(feature = "gpu")]
        if let Some(g) = &self.gpu
            && crop == (0.0, 0.0, 1.0, 1.0)
            && width <= self.width
            && height <= self.height
            && let Ok(p) = g.read_shrunk(width, height)
        {
            // Shrunk there: the size asked for exactly.
            return Frame::from_owned(p);
        }
        if self.gpu.is_some() {
            return self.pixels().scaled(crop, width, height);
        }
        // From coefficients: reconstructed first, at half size when that is still at
        // least what is shown (the multiview's tiles), a quarter of the work.
        if let Some(c) = &self.coefs {
            let shown = |a: f64, b: f64, n: usize| ((b - a) * n as f64 / 2.0) as usize;
            let half = width <= shown(crop.0, crop.2, self.width)
                && height <= shown(crop.1, crop.3, self.height);
            let src = if half {
                Frame::from_owned(idn_open_protocol::shq::reconstruct_half(c))
            } else {
                self.pixels()
            };
            return src.scaled(crop, width, height);
        }
        let cw = Frame::chroma_width(width);
        let scw = Frame::chroma_width(self.width);
        // Each plane on the library's worker pool, in bands (threads started for each
        // plane and band cost more than the scaling).
        let (w, h) = (self.width, self.height);
        let mut planes = vec![
            (&self.y[..], w, h, width, height),
            (&self.cb[..], scw, h, cw, height),
            (&self.cr[..], scw, h, cw, height),
        ];
        if let Some(a) = &self.a {
            planes.push((&a[..], w, h, width, height));
        }
        let mut out = idn_open_protocol::scale::bilinear_planes(&planes, crop).into_iter();
        let (y, cb, cr) = (
            out.next().unwrap_or_default(),
            out.next().unwrap_or_default(),
            out.next().unwrap_or_default(),
        );
        let a = out.next();
        Frame {
            width,
            height,
            y,
            cb,
            cr,
            a,
            coefs: None,
            gpu: None,
        }
    }

    /// Copies `src` into this picture with its top left at (`x`, `y`) (`x` even),
    /// clipped; alpha too, where both have it.
    pub fn paste(&mut self, src: &Frame, x: usize, y: usize) {
        if src.deferred() {
            return self.paste(&src.pixels(), x, y);
        }
        let (cw, scw) = (
            Frame::chroma_width(self.width),
            Frame::chroma_width(src.width),
        );
        for row in 0..src.height.min(self.height.saturating_sub(y)) {
            let n = src.width.min(self.width.saturating_sub(x));
            let (d, s) = ((y + row) * self.width + x, row * src.width);
            self.y[d..d + n].copy_from_slice(&src.y[s..s + n]);
            if let (Some(da), Some(sa)) = (self.a.as_mut(), src.a.as_ref()) {
                da[d..d + n].copy_from_slice(&sa[s..s + n]);
            }
            let cn = scw.min(cw.saturating_sub(x / 2));
            let (dc, sc) = ((y + row) * cw + x / 2, row * scw);
            self.cb[dc..dc + cn].copy_from_slice(&src.cb[sc..sc + cn]);
            self.cr[dc..dc + cn].copy_from_slice(&src.cr[sc..sc + cn]);
        }
    }
}

fn sample(p: &Plane, x: usize, y: usize) -> u8 {
    let x = x.min(p.width.saturating_sub(1));
    let y = y.min(p.height.saturating_sub(1));
    p.data.get(y * p.width + x).copied().unwrap_or(128)
}

/// A plane's top-left `w` × `h` (padded by repeating the edge if the plane is smaller).
fn crop(p: &Plane, w: usize, h: usize) -> Vec<u8> {
    if p.width == w && p.height == h {
        return p.data.clone();
    }
    let mut out = Vec::with_capacity(w * h);
    for y in 0..h {
        out.extend((0..w).map(|x| sample(p, x, y)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_go_both_ways() {
        for hex in ["#203060", "#ff0000", "#000000", "#ffffff", "#808080"] {
            let r = u8::from_str_radix(&hex[1..3], 16).unwrap();
            let g = u8::from_str_radix(&hex[3..5], 16).unwrap();
            let b = u8::from_str_radix(&hex[5..7], 16).unwrap();
            let back = Ycc::from_rgb(r, g, b).to_hex();
            // 8-bit limited range loses a little: within 2 per channel.
            let d = |i: usize| {
                (u8::from_str_radix(&back[i..i + 2], 16).unwrap() as i32
                    - u8::from_str_radix(&hex[i..i + 2], 16).unwrap() as i32)
                    .abs()
            };
            assert!(
                d(1) <= 2 && d(3) <= 2 && d(5) <= 2,
                "{hex} came back {back}"
            );
        }
    }

    #[test]
    fn fitting_letterboxes_and_keeps_colour() {
        let red = Ycc::from_rgb(255, 0, 0);
        let f = Frame::solid(640, 480, red).fitted(1920, 1080);
        assert_eq!((f.width, f.height), (1920, 1080));
        // 4:3 in 16:9: 1440 wide, centred; black bars left and right.
        assert_eq!(f.y[540 * 1920 + 960], red.0);
        assert_eq!(f.y[540 * 1920 + 100], 16);
        assert_eq!(f.cr[540 * 960 + 480], red.2);
        assert_eq!(f.cr[540 * 960 + 50], 128);
    }

    #[test]
    fn a_crop_scales_the_part() {
        // Left half white, right half black; the right half scaled up is all black.
        let mut f = Frame::black(64, 32);
        for row in 0..32 {
            for x in 0..32 {
                f.y[row * 64 + x] = 235;
            }
        }
        let right = f.scaled((0.5, 0.0, 1.0, 1.0), 64, 32);
        assert!(right.y[8 * 64 + 4..8 * 64 + 60].iter().all(|&v| v == 16));
        let left = f.scaled((0.0, 0.0, 0.5, 1.0), 16, 8);
        assert!(left.y.iter().all(|&v| v == 235));
    }
}

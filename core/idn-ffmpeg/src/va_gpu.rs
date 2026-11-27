//! Pictures the GPU decoded (VA-API) kept on the GPU (spec/extensions/idn-extensions.md
//! §E.27, R.28): each picture's surface mapped as DMA-BUF (a layer per plane: Y as R8,
//! Cb and Cr as GR88), its planes imported into the application's Vulkan device as
//! textures (once per surface of the decoder's pool), and converted there into planar
//! Y'CbCr, 4:2:0 or 4:2:2 as decoded, in a buffer laid out as `gpu/1`'s slots are. Nothing goes through the
//! CPU's memory; on an integrated GPU the download it replaces read 12 MB and wrote 16 MB
//! a 2160p picture.
//!
//! 8-bit pictures: 4:2:0 (NV12, or three planes) and 4:2:2 (NV16, three planes, or packed
//! YUYV, as Intel's decoders give HEVC 4:2:2); 10-bit ones, or a driver that can't import
//! the planes, are downloaded as before.

use crate::ffi;
use crate::dmabuf::{PlaneAt, Shape, import};
use crate::gpu_convert::{Converter, err};
use ffmpeg_sys_next as ff;
use idn_open_protocol::gpu::{GpuHandIn, GpuPicture};
use std::collections::HashMap;
use std::io;

pub struct VaToGpu {
    hand: GpuHandIn,
    /// Each surface's planes as textures (up to three; the first again where there are
    /// fewer) and their shape, by surface and size.
    cache: HashMap<(usize, u32, u32), ([wgpu::Texture; 3], Shape)>,
    /// The decoder's pool of surfaces the cache is for, held: surface ids belong to it,
    /// and a new pool (a decoder switched, a new SPS) reuses them, so the cache showed the
    /// old decoder's last pictures, frozen.
    frames: *mut ff::AVBufferRef,
    conv: Converter,
}

// SAFETY: wgpu's types are Send and Sync (checked in idn_open_protocol::gpu); the cache's
// keys are plain numbers; the frames reference is FFmpeg's refcounted buffer, used only
// by the importer's owner.
unsafe impl Send for VaToGpu {}

impl Drop for VaToGpu {
    fn drop(&mut self) {
        // The textures first (they import the pool's surfaces), then the pool.
        self.cache.clear();
        if !self.frames.is_null() {
            // SAFETY: our own reference, taken with av_buffer_ref.
            unsafe { ff::av_buffer_unref(&mut self.frames) };
        }
    }
}

impl VaToGpu {
    /// On `hand`'s device, which must import DMA-BUF (`VULKAN_EXTERNAL_MEMORY_DMA_BUF`).
    pub fn new(hand: &GpuHandIn) -> io::Result<VaToGpu> {
        if !hand
            .device
            .features()
            .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF)
        {
            return Err(err("this device doesn't import DMA-BUF"));
        }
        let conv = Converter::new(hand);
        Ok(VaToGpu {
            hand: hand.clone(),
            cache: HashMap::new(),
            frames: std::ptr::null_mut(),
            conv,
        })
    }

    /// Plane `p` of a surface as a `format` texture of `w` × `h`.
    fn import(
        &self,
        p: &PlaneAt,
        format: wgpu::TextureFormat,
        size: (u32, u32),
    ) -> io::Result<wgpu::Texture> {
        import(&self.hand, p, format, size, false)
    }
}

impl VaToGpu {
    /// A buffer for a picture: one no picture holds any more, or a new one.
    /// The decoded picture `fr` (a VA-API surface) as planar 4:2:0 on the GPU, the GPU
    /// done with it.
    pub fn picture(&mut self, fr: &ffi::Frame) -> io::Result<GpuPicture> {
        // SAFETY: FFmpeg calls on a live frame; the mapped frame is unmapped when dropped,
        // after the GPU's work on it is waited for.
        unsafe {
            let f = fr.0;
            if (*f).format != ff::AVPixelFormat::AV_PIX_FMT_VAAPI as i32 {
                return Err(err("not a VA-API picture"));
            }
            let (w, h) = ((*f).width as u32, (*f).height as u32);
            // 4:2:2 (chroma every row) or 4:2:0, by the decoder's pictures' own format.
            let frames = (*f).hw_frames_ctx;
            if frames.is_null() {
                return Err(err("a GPU picture without its frames"));
            }
            if self.frames.is_null() || (*self.frames).data != (*frames).data {
                self.cache.clear();
                if !self.frames.is_null() {
                    ff::av_buffer_unref(&mut self.frames);
                }
                self.frames = ff::av_buffer_ref(frames);
            }
            let sw = (*((*frames).data as *const ff::AVHWFramesContext)).sw_format;
            use ff::AVPixelFormat::*;
            let full = match sw {
                AV_PIX_FMT_NV12 | AV_PIX_FMT_YUV420P => false,
                AV_PIX_FMT_NV16 | AV_PIX_FMT_YUYV422 | AV_PIX_FMT_YUV422P => true,
                other => return Err(err(format!("decoded as {other:?} (8-bit only)"))),
            };
            let (cw, ch) = (w.div_ceil(2), if full { h } else { h.div_ceil(2) });
            let surface = (*f).data[3] as usize;
            // Mapped each time: the mapping waits for the decode to finish.
            let m = ffi::Frame::new()?;
            (*m.0).format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            if ff::av_hwframe_map(m.0, f, ff::AV_HWFRAME_MAP_READ as i32) < 0 {
                return Err(err("mapping a surface as DMA-BUF"));
            }
            let key = (surface, w, h);
            if !self.cache.contains_key(&key) {
                let d = &*((*m.0).data[0] as *const ff::AVDRMFrameDescriptor);
                let (planes, shape) = crate::dmabuf::planes(d)?;
                use wgpu::TextureFormat::{R8Unorm, Rg8Unorm, Rgba8Unorm};
                let t = match shape {
                    Shape::SemiPlanar => {
                        let y = self.import(&planes[0], R8Unorm, (w, h))?;
                        let c = self.import(&planes[1], Rg8Unorm, (cw, ch))?;
                        [y.clone(), c, y]
                    }
                    Shape::Planar => [
                        self.import(&planes[0], R8Unorm, (w, h))?,
                        self.import(&planes[1], R8Unorm, (cw, ch))?,
                        self.import(&planes[2], R8Unorm, (cw, ch))?,
                    ],
                    Shape::Packed => {
                        let t = self.import(&planes[0], Rgba8Unorm, (cw, h))?;
                        [t.clone(), t.clone(), t]
                    }
                };
                if self.cache.len() >= 64 {
                    self.cache.clear();
                }
                self.cache.insert(key, (t, shape));
            }
            let (t, shape) = &self.cache[&key];
            let mode = match shape {
                Shape::SemiPlanar => 0,
                Shape::Planar => 1,
                Shape::Packed => 2,
            };
            let views: Vec<wgpu::TextureView> = t
                .iter()
                .map(|t| t.create_view(&Default::default()))
                .collect();
            let pic =
                self.conv
                    .convert([&views[0], &views[1], &views[2]], mode, (w, h), (cw, ch))?;
            drop(m);
            Ok(pic)
        }
    }
}

//! A Wayland screen share's pictures kept on the GPU: the
//! desktop hands each picture as a DMA-BUF (its GPU memory, PipeWire's `DmaBuf` buffers,
//! a layout this device imports: DRM format modifiers from Vulkan), imported into the
//! capture's Vulkan device once per buffer, and made planar 4:2:2 Y'CbCr there by the
//! converter decoded surfaces use. Nothing goes through this process's memory; through
//! shared memory the desktop read each picture back from its GPU, and the CPU reordered,
//! converted and encoded it.

use crate::dmabuf::{PlaneAt, import};
use crate::gpu_convert::Converter;
use idn_open_protocol::gpu::{GpuHandIn, GpuPicture};
use std::io;

/// A picture as PipeWire gave it: a DMA-BUF of `fmt` (SPA's video format), `w` × `h`,
/// its plane at `offset`, rows `stride` apart, laid out as `modifier` says.
pub(crate) struct DmaPicture {
    pub fd: i32,
    pub offset: u32,
    pub stride: u32,
    pub modifier: u64,
    pub fmt: u32,
    pub w: u32,
    pub h: u32,
}

/// The texture format of SPA's video format (packed RGB, 8 bits): BGRx and BGRA as
/// B8G8R8A8, RGBx and RGBA as R8G8B8A8.
pub(crate) fn texture_format(spa: u32) -> Option<wgpu::TextureFormat> {
    use crate::wayland::spa_format as f;
    match spa {
        f::BGRX | f::BGRA => Some(wgpu::TextureFormat::Bgra8Unorm),
        f::RGBX | f::RGBA => Some(wgpu::TextureFormat::Rgba8Unorm),
        _ => None,
    }
}

/// The formats offered as DMA-BUF, each with the modifiers this device imports it with:
/// none where it imports none.
pub(crate) fn offers(hand: &GpuHandIn) -> Vec<(u32, Vec<u64>)> {
    use crate::wayland::spa_format as f;
    let bgr = idn_open_protocol::gpu::dmabuf_modifiers(hand, wgpu::TextureFormat::Bgra8Unorm);
    let rgb = idn_open_protocol::gpu::dmabuf_modifiers(hand, wgpu::TextureFormat::Rgba8Unorm);
    [
        (f::BGRX, &bgr),
        (f::BGRA, &bgr),
        (f::RGBX, &rgb),
        (f::RGBA, &rgb),
    ]
    .into_iter()
    .filter(|(_, m)| !m.is_empty())
    .map(|(f, m)| (f, m.clone()))
    .collect()
}

/// What an imported picture is known by: its buffer, layout and size.
type ImportKey = (u64, u64, u32, u32, u64, u32, u32, u32);

/// The capture's device, its imported buffers and the converter.
pub(crate) struct ShareGpu {
    hand: GpuHandIn,
    conv: Converter,
    /// Imported buffers: (the DMA-BUF's identity (device, inode), offset, stride,
    /// modifier, format, width, height) and the texture; the newest last, at most 8.
    cache: Vec<(ImportKey, wgpu::Texture, wgpu::TextureView)>,
}

impl ShareGpu {
    pub(crate) fn new(hand: &GpuHandIn) -> ShareGpu {
        ShareGpu {
            hand: hand.clone(),
            conv: Converter::new(hand),
            cache: Vec::new(),
        }
    }

    /// `d` imported (once per buffer) and made planar on the GPU, the GPU done with the
    /// buffer on return (so it may go back to the desktop).
    pub(crate) fn picture(&mut self, d: &DmaPicture) -> io::Result<GpuPicture> {
        let format = texture_format(d.fmt)
            .ok_or_else(|| io::Error::other("screen share: a format the GPU path can't read"))?;
        // SAFETY: fstat on a descriptor PipeWire holds open for this buffer.
        let id = unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            if libc::fstat(d.fd, &mut st) != 0 {
                return Err(io::Error::last_os_error());
            }
            (st.st_dev as u64, st.st_ino as u64)
        };
        let key = (id.0, id.1, d.offset, d.stride, d.modifier, d.fmt, d.w, d.h);
        let at = match self.cache.iter().position(|c| c.0 == key) {
            Some(i) => i,
            None => {
                let tex = import(
                    &self.hand,
                    &PlaneAt {
                        fd: d.fd,
                        modifier: d.modifier,
                        offset: u64::from(d.offset),
                        pitch: u64::from(d.stride),
                    },
                    format,
                    (d.w, d.h),
                    false,
                )?;
                let view = tex.create_view(&Default::default());
                if self.cache.len() >= 8 {
                    self.cache.remove(0);
                }
                self.cache.push((key, tex, view));
                self.cache.len() - 1
            }
        };
        let v = &self.cache[at].2;
        self.conv
            .convert([v, v, v], 3, (d.w, d.h), (d.w.div_ceil(2), d.h))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A screen's picture (RGB, as a texture) made planar on the GPU gives the samples the
    /// CPU's conversion gives (within rounding); this GPU's import modifiers listed. Skips
    /// without a GPU.
    #[test]
    fn rgb_made_planar_on_the_gpu_as_on_the_cpu() {
        let Some(hand) = idn_open_protocol::gpu::open_device("wayland_gpu test", false) else {
            eprintln!("no GPU");
            return;
        };
        let mods = idn_open_protocol::gpu::dmabuf_modifiers(&hand, wgpu::TextureFormat::Bgra8Unorm);
        eprintln!("B8G8R8A8 DMA-BUF modifiers here: {mods:x?}");
        let (w, h) = (1280u32, 720u32);
        // A pattern of colours: RGBA for the CPU, BGRA for the texture.
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                rgba[i] = (x * 255 / w) as u8;
                rgba[i + 1] = (y * 255 / h) as u8;
                rgba[i + 2] = ((x + y) % 256) as u8;
                rgba[i + 3] = 255;
            }
        }
        let bgra: Vec<u8> = rgba
            .chunks(4)
            .flat_map(|p| [p[2], p[1], p[0], p[3]])
            .collect();
        let size = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };
        let tex = hand.device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        hand.queue.write_texture(
            tex.as_image_copy(),
            &bgra,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            size,
        );
        let view = tex.create_view(&Default::default());
        let mut conv = Converter::new(&hand);
        let pic = conv
            .convert([&view, &view, &view], 3, (w, h), (w / 2, h))
            .unwrap();
        let gpu = pic.read_planes(&hand).unwrap();
        let cpu = idn_open_protocol::color::rgba_to_planes(
            &rgba,
            w as usize,
            h as usize,
            (w * 4) as usize,
            idn_open_protocol::shq::Fourcc::Shq2,
            idn_open_protocol::color::AlphaMode::Opaque,
            &idn_open_protocol::color::ColorConfig::default(),
        );
        for (k, (g, c)) in gpu.iter().zip(&cpu).enumerate() {
            assert_eq!(g.len(), c.data.len(), "plane {k}");
            let worst = g
                .iter()
                .zip(&c.data)
                .map(|(a, b)| (*a as i32 - *b as i32).abs())
                .max()
                .unwrap();
            eprintln!("plane {k}: worst difference {worst}");
            assert!(worst <= 2, "plane {k}: {worst}");
        }
    }
}

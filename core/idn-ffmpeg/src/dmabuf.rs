//! Other programs' and devices' pictures as DMA-BUF (Linux), imported into the
//! application's Vulkan device: a mapped surface's planes (VA-API's decoders and
//! encoders, Vulkan Video's frames map the same way) and a Wayland desktop's screen.
//! The DRM descriptor read into planes, and a plane made a texture to read or draw into.

use crate::gpu_convert::err;
use ffmpeg_sys_next as ff;
use idn_open_protocol::gpu::GpuHandIn;
use std::io;
use wgpu::hal::api::Vulkan;

/// DRM's FourCC codes for the planes (`drm_fourcc.h`).
const DRM_R8: u32 = u32::from_le_bytes(*b"R8  ");
const DRM_GR88: u32 = u32::from_le_bytes(*b"GR88");
const DRM_NV12: u32 = u32::from_le_bytes(*b"NV12");
const DRM_NV16: u32 = u32::from_le_bytes(*b"NV16");
const DRM_YUYV: u32 = u32::from_le_bytes(*b"YUYV");

/// How a surface's samples lie, for the shader (`P::mode`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Shape {
    /// Y, then Cb and Cr interleaved (NV12, NV16).
    SemiPlanar,
    /// Y, Cb, Cr each a plane.
    Planar,
    /// Y0 Cb Y1 Cr, four bytes for two pixels (YUYV).
    Packed,
}

/// A plane of a mapped surface: its object (descriptor), modifier, start and row pitch.
pub(crate) struct PlaneAt {
    pub(crate) fd: i32,
    pub(crate) modifier: u64,
    pub(crate) offset: u64,
    pub(crate) pitch: u64,
}

/// The planes of a mapped surface (Y, then its chroma as the surface has it) and their
/// shape.
///
/// # Safety
/// `d` is a live DRM frame descriptor.
pub(crate) unsafe fn planes(d: &ff::AVDRMFrameDescriptor) -> io::Result<(Vec<PlaneAt>, Shape)> {
    let mut out = Vec::new();
    let mut formats = Vec::new();
    for l in &d.layers[..(d.nb_layers.clamp(0, 4) as usize)] {
        let ok = match l.format {
            DRM_R8 | DRM_GR88 | DRM_YUYV => l.nb_planes == 1,
            DRM_NV12 | DRM_NV16 => l.nb_planes == 2,
            f => return Err(err(format!("a surface's layer {:?}", f.to_le_bytes()))),
        };
        if !ok {
            return Err(err("a surface's planes"));
        }
        formats.push(l.format);
        for p in &l.planes[..l.nb_planes as usize] {
            let o = p.object_index;
            if !(0..d.nb_objects.min(4)).contains(&o) || p.offset < 0 || p.pitch <= 0 {
                return Err(err("a surface's plane"));
            }
            let obj = &d.objects[o as usize];
            out.push(PlaneAt {
                fd: obj.fd,
                modifier: obj.format_modifier,
                offset: p.offset as u64,
                pitch: p.pitch as u64,
            });
        }
    }
    let shape = match (formats.as_slice(), out.len()) {
        ([DRM_YUYV], 1) => Shape::Packed,
        ([DRM_R8, DRM_GR88], 2) | ([DRM_NV12 | DRM_NV16], 2) => Shape::SemiPlanar,
        ([DRM_R8, DRM_R8, DRM_R8], 3) => Shape::Planar,
        _ => return Err(err("a surface of planes this can't read")),
    };
    Ok((out, shape))
}

/// A surface's plane `p` as a texture on `hand`'s Vulkan device: to read (`target`
/// false: holding a picture, read as it is) or to draw into (`target`: an encoder's
/// input, written whole).
pub(crate) fn import(
    hand: &GpuHandIn,
    p: &PlaneAt,
    format: wgpu::TextureFormat,
    (w, h): (u32, u32),
    target: bool,
) -> io::Result<wgpu::Texture> {
    {
        use std::os::fd::FromRawFd;
        let size = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };
        // SAFETY: a duplicate of the surface's descriptor, ours to give; the import takes
        // it (or closes it on failure). The texture shares the surface's memory, which
        // lives while FFmpeg's pool keeps the surface (the decoder's lifetime, this
        // cache's too).
        unsafe {
            let fd = libc::dup(p.fd);
            if fd < 0 {
                return Err(err(io::Error::last_os_error()));
            }
            let fd = std::os::fd::OwnedFd::from_raw_fd(fd);
            let hal = hand
                .device
                .as_hal::<Vulkan>()
                .ok_or_else(|| err("not a Vulkan device"))?;
            let tex = hal
                .texture_from_dmabuf_fd(
                    fd,
                    &wgpu::hal::TextureDescriptor {
                        label: Some("va plane"),
                        size,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format,
                        usage: if target {
                            wgpu::TextureUses::COLOR_TARGET
                        } else {
                            wgpu::TextureUses::RESOURCE
                        },
                        memory_flags: wgpu::hal::MemoryFlags::empty(),
                        view_formats: Vec::new(),
                    },
                    p.modifier,
                    p.pitch,
                    p.offset,
                )
                .map_err(|e| err(format!("importing a plane: {e}")))?;
            drop(hal);
            // Holding a picture: read as it is (no transition that could discard it).
            // To draw into: written whole, its contents before don't matter.
            Ok(hand.device.create_texture_from_hal::<Vulkan>(
                tex,
                &wgpu::TextureDescriptor {
                    label: Some("va plane"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: if target {
                        wgpu::TextureUsages::RENDER_ATTACHMENT
                    } else {
                        wgpu::TextureUsages::TEXTURE_BINDING
                    },
                    view_formats: &[],
                },
                if target {
                    wgpu::TextureUses::UNINITIALIZED
                } else {
                    wgpu::TextureUses::RESOURCE
                },
            ))
        }
    }
}

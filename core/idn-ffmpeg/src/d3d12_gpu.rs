//! Pictures the GPU decoded on Windows kept on the GPU (spec/extensions/idn-extensions.md
//! §E.27): FFmpeg decodes with Direct3D 12 (D3D12VA) on the application's own device, so
//! each decoded surface is already where the application draws. Its NV12 texture is
//! wrapped once per surface, its planes viewed as R8 and RG8, and converted into planar
//! Y'CbCr (`gpu_convert`). Nothing goes through the CPU's memory, and no copy between
//! graphics APIs is made.
//!
//! 8-bit 4:2:0 (NV12) only; 10-bit pictures, an application on Vulkan, or a device
//! without NV12 textures are downloaded as before.

use crate::ffi;
use crate::gpu_convert::{Converter, err};
use ffmpeg_sys_next as ff;
use idn_open_protocol::gpu::{GpuHandIn, GpuPicture};
use std::collections::HashMap;
use std::io;
use wgpu::hal::api::Dx12;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D12::{ID3D12Device, ID3D12Fence, ID3D12Resource};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
use windows::core::Interface;

/// The application's Direct3D 12 device, when its wgpu device is on Direct3D 12.
pub(crate) fn device_of(hand: &GpuHandIn) -> Option<ID3D12Device> {
    // SAFETY: the interface is cloned (reference counted) and used while the device lives.
    unsafe { hand.device.as_hal::<Dx12>() }.map(|h| h.raw_device().clone())
}

/// Where an `AVD3D12VAFrame`'s fields lie, by the FFmpeg loaded: 8.x added
/// `subresource_index` after the texture (and `flags` at the end).
pub(crate) struct Layout {
    pub(crate) subresource: Option<usize>,
    pub(crate) sync: usize,
}

pub(crate) fn frame_layout() -> Layout {
    // SAFETY: a plain query of the library loaded.
    let major = unsafe { ff::avutil_version() } >> 16;
    if major >= 60 {
        Layout {
            subresource: Some(8),
            sync: 16,
        }
    } else {
        Layout {
            subresource: None,
            sync: 8,
        }
    }
}

pub struct D3d12ToGpu {
    conv: Converter,
    hand: GpuHandIn,
    /// Each surface wrapped as an NV12 texture, by its resource and size.
    cache: HashMap<(usize, u32, u32), wgpu::Texture>,
    /// The decoder's pool the cache is for, held (as `va_gpu`'s).
    frames: *mut ff::AVBufferRef,
    layout: Layout,
}

// SAFETY: wgpu's types are Send and Sync; the cache's keys are plain numbers; the frames
// reference is FFmpeg's refcounted buffer, used only by the importer's owner.
unsafe impl Send for D3d12ToGpu {}

impl Drop for D3d12ToGpu {
    fn drop(&mut self) {
        self.cache.clear();
        if !self.frames.is_null() {
            // SAFETY: our own reference, taken with av_buffer_ref.
            unsafe { ff::av_buffer_unref(&mut self.frames) };
        }
    }
}

impl D3d12ToGpu {
    /// On `hand`'s device, which must be on Direct3D 12 and take NV12 textures
    /// (`TEXTURE_FORMAT_NV12`).
    pub fn new(hand: &GpuHandIn) -> io::Result<D3d12ToGpu> {
        if device_of(hand).is_none() {
            return Err(err("the application's GPU isn't on Direct3D 12"));
        }
        if !hand
            .device
            .features()
            .contains(wgpu::Features::TEXTURE_FORMAT_NV12)
        {
            return Err(err("this device has no NV12 textures"));
        }
        Ok(D3d12ToGpu {
            conv: Converter::new(hand),
            hand: hand.clone(),
            cache: HashMap::new(),
            frames: std::ptr::null_mut(),
            layout: frame_layout(),
        })
    }

    /// The decoded picture `fr` (a Direct3D 12 surface) as planar 4:2:0 on the GPU, the
    /// GPU done with it.
    pub fn picture(&mut self, fr: &ffi::Frame) -> io::Result<GpuPicture> {
        // SAFETY: FFmpeg's frame and its D3D12 surface are live while `fr` is; the GPU's
        // work on the surface is waited for before this returns.
        unsafe {
            let f = fr.0;
            if (*f).format != ff::AVPixelFormat::AV_PIX_FMT_D3D12 as i32 {
                return Err(err("not a Direct3D 12 picture"));
            }
            let (w, h) = ((*f).width as u32, (*f).height as u32);
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
            if sw != ff::AVPixelFormat::AV_PIX_FMT_NV12 {
                return Err(err(format!("decoded as {sw:?} (8-bit 4:2:0 only)")));
            }
            let at = (*f).data[0] as *const u8;
            if at.is_null() {
                return Err(err("a Direct3D 12 picture without its surface"));
            }
            let texture = *(at as *const *mut std::ffi::c_void);
            let index = match self.layout.subresource {
                Some(o) => *(at.add(o) as *const i32),
                None => 0,
            };
            let fence = *(at.add(self.layout.sync) as *const *mut std::ffi::c_void);
            let value = *(at.add(self.layout.sync + 16) as *const u64);
            // The decode done: waited for here (the decoding thread waits for its
            // picture anyway, as VA-API's mapping does).
            if let Some(fence) = ID3D12Fence::from_raw_borrowed(&fence)
                && fence.GetCompletedValue() < value
            {
                fence
                    .SetEventOnCompletion(value, HANDLE::default())
                    .map_err(err)?;
            }
            let resource = ID3D12Resource::from_raw_borrowed(&texture)
                .ok_or_else(|| err("a Direct3D 12 picture without its texture"))?
                .clone();
            let desc = resource.GetDesc();
            if desc.Format != DXGI_FORMAT_NV12 {
                return Err(err(format!("a surface of format {:?}", desc.Format)));
            }
            let layers = u32::from(desc.DepthOrArraySize).max(1);
            if index < 0 || index as u32 >= layers {
                return Err(err("a surface's index"));
            }
            let key = (texture as usize, w, h);
            if !self.cache.contains_key(&key) {
                let size = wgpu::Extent3d {
                    width: desc.Width as u32,
                    height: desc.Height,
                    depth_or_array_layers: layers,
                };
                let hal = wgpu::hal::dx12::Device::texture_from_raw(
                    resource,
                    wgpu::TextureFormat::NV12,
                    wgpu::TextureDimension::D2,
                    size,
                    1,
                    1,
                );
                // Read only, and in the common state the decoder leaves it in, from which
                // Direct3D 12 promotes it to a shader's read implicitly.
                let t = self.hand.device.create_texture_from_hal::<Dx12>(
                    hal,
                    &wgpu::TextureDescriptor {
                        label: Some("decoded (D3D12VA)"),
                        size,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::NV12,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING,
                        view_formats: &[],
                    },
                    wgpu::TextureUses::RESOURCE,
                );
                if self.cache.len() >= 64 {
                    self.cache.clear();
                }
                self.cache.insert(key, t);
            }
            let t = &self.cache[&key];
            let plane = |aspect, format| {
                t.create_view(&wgpu::TextureViewDescriptor {
                    label: None,
                    format: Some(format),
                    dimension: Some(wgpu::TextureViewDimension::D2),
                    usage: None,
                    aspect,
                    base_mip_level: 0,
                    mip_level_count: Some(1),
                    base_array_layer: index as u32,
                    array_layer_count: Some(1),
                })
            };
            let y = plane(wgpu::TextureAspect::Plane0, wgpu::TextureFormat::R8Unorm);
            let c = plane(wgpu::TextureAspect::Plane1, wgpu::TextureFormat::Rg8Unorm);
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            self.conv.convert([&y, &c, &y], 0, (w, h), (cw, ch))
        }
    }
}

//! "IDN Source (GPU)": a source's pictures handed to OBS's renderer on the GPU (§E.27). A
//! source on this machine on the same GPU gives its pictures there (`gpu/1`); they are
//! copied, GPU to GPU, into textures OBS shares with this plugin's own device and drawn by
//! OBS with an effect that turns Y'CbCr into RGB. Pictures that come as samples are
//! uploaded into the same textures. Nothing is converted on the CPU.
//!
//! The textures (Y, Cb, Cr, 8 bits each; two sets, one shown while the other is filled):
//! Linux, Vulkan images in linear layout exported as DMA-BUF and imported into OBS's
//! OpenGL (`gs_texture_create_from_dmabuf`); Windows, Direct3D 12 textures in a shared
//! heap opened by OBS's Direct3D 11 (`gs_texture_open_nt_shared`), so this device is on
//! Direct3D 12 there. SDR, 8 bits: HDR and 16-bit sources stay with "IDN Source".
//!
//! Where OBS can't share them (OBS drawing in software, Mesa's llvmpipe: a DMA-BUF it can't
//! map crashed OBS; or the sharing refused), the planes go into OBS's own textures from
//! this memory instead (a picture on the GPU read back first): slower, never wrong.

use idn_common::sync::LockOk as _;
use crate::ffi;
use idn_open_protocol::gpu::GpuHandIn;
use idn_open_protocol::gpu::GpuPicture;
use idn_open_protocol::gpu::block_on;
use idn_open_protocol::shq::Picture;
use std::sync::Mutex;

/// One plane's texture: OBS's, and when shared ours with what keeps the sharing.
struct Shared {
    tex: Option<wgpu::Texture>,
    obs: *mut ffi::gs_texture_t,
    #[cfg(unix)]
    _fd: Option<std::os::fd::OwnedFd>,
    #[cfg(windows)]
    _handle: Option<Handle>,
}

#[cfg(windows)]
struct Handle(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: a handle this plugin made and owns.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// A set of three planes and the size it is for.
struct Set {
    planes: Vec<Shared>,
    size: (u32, u32),
    chroma: (u32, u32),
}

struct State {
    /// Sharing textures with OBS (decided at the first picture), or uploading into OBS's.
    sharing: Option<bool>,
    sets: [Option<Set>; 2],
    /// The set OBS draws; the other is filled.
    shown: Option<usize>,
    /// How the shown picture's Y'CbCr becomes RGB: OBS's colour matrix rows and range.
    matrix: [f32; 16],
    range_min: [f32; 3],
    range_max: [f32; 3],
}

/// A source's GPU view: this plugin's device and the textures OBS draws.
pub(crate) struct GpuView {
    pub hand: GpuHandIn,
    state: Mutex<State>,
}

// SAFETY: OBS's texture pointers are used only on OBS's graphics context (entered), under
// the lock; wgpu's types are Send and Sync.
unsafe impl Send for GpuView {}
unsafe impl Sync for GpuView {}

/// The effect OBS draws the planes with (its effect language: HLSL-like, compiled for
/// Direct3D and OpenGL).
const EFFECT: &str = r#"
uniform float4x4 ViewProj;
uniform texture2d tex_y;
uniform texture2d tex_u;
uniform texture2d tex_v;
uniform float4 color_vec0;
uniform float4 color_vec1;
uniform float4 color_vec2;
uniform float3 color_range_min = {0.0, 0.0, 0.0};
uniform float3 color_range_max = {1.0, 1.0, 1.0};

sampler_state def_sampler {
    Filter   = Linear;
    AddressU = Clamp;
    AddressV = Clamp;
};

struct VertInOut {
    float4 pos : POSITION;
    float2 uv  : TEXCOORD0;
};

VertInOut VSDefault(VertInOut vert_in)
{
    VertInOut vert_out;
    vert_out.pos = mul(float4(vert_in.pos.xyz, 1.0), ViewProj);
    vert_out.uv  = vert_in.uv;
    return vert_out;
}

float4 PSPlanes(VertInOut vert_in) : TARGET
{
    float3 yuv = float3(tex_y.Sample(def_sampler, vert_in.uv).r,
                        tex_u.Sample(def_sampler, vert_in.uv).r,
                        tex_v.Sample(def_sampler, vert_in.uv).r);
    yuv = clamp(yuv, color_range_min, color_range_max);
    float4 y4 = float4(yuv, 1.0);
    return float4(dot(color_vec0, y4), dot(color_vec1, y4), dot(color_vec2, y4), 1.0);
}

technique Draw
{
    pass
    {
        vertex_shader = VSDefault(vert_in);
        pixel_shader  = PSPlanes(vert_in);
    }
}
"#;

/// OBS's effect, made once on its graphics context.
struct Effect(*mut ffi::gs_effect_t);
// SAFETY: the effect is made, used and kept only on OBS's graphics thread, under
// `THE_EFFECT`'s lock.
unsafe impl Send for Effect {}
static THE_EFFECT: Mutex<Option<Effect>> = Mutex::new(None);

impl GpuView {
    /// This plugin's device: Vulkan on Linux, Direct3D 12 on Windows (OBS's Direct3D 11
    /// opens only its shared textures), the high-performance GPU.
    pub fn new() -> Result<GpuView, String> {
        let mut d = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        d.backends = if cfg!(windows) {
            wgpu::Backends::DX12
        } else {
            wgpu::Backends::VULKAN
        };
        let instance = wgpu::Instance::new(d);
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .map_err(|e| format!("no GPU: {e}"))?;
        // NV12 textures where offered: H.264/HEVC pictures decoded with Direct3D 12 on this
        // device stay on it (§E.27).
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("IDN Source (GPU)"),
            required_features: adapter.features() & wgpu::Features::TEXTURE_FORMAT_NV12,
            ..Default::default()
        }))
        .map_err(|e| e.to_string())?;
        Ok(GpuView {
            hand: GpuHandIn { device, queue },
            state: Mutex::new(State {
                sharing: None,
                sets: [None, None],
                shown: None,
                matrix: [0.0; 16],
                range_min: [0.0; 3],
                range_max: [1.0; 3],
            }),
        })
    }

    /// The shown picture's size.
    pub fn size(&self) -> (u32, u32) {
        let s = self.state.lock_ok();
        s.shown
            .and_then(|i| s.sets[i].as_ref())
            .map_or((0, 0), |set| set.size)
    }

    /// Takes picture `p` (or its copy on the GPU, `gpu`) into the set not shown (made, or remade for a new size, as needed), waits
    /// for the GPU, then shows it. Its colour as OBS turns it into RGB: `matrix`,
    /// `range_min` and `range_max` from `video_format_get_parameters`.
    pub fn put(
        &self,
        p: &Picture,
        gpu: Option<&GpuPicture>,
        matrix: [f32; 16],
        range_min: [f32; 3],
        range_max: [f32; 3],
    ) -> Result<(), String> {
        if p.planes.len() < 3 {
            return Err("not Y'CbCr planes".into());
        }
        let size = (p.width as u32, p.height as u32);
        let chroma = (p.planes[1].width as u32, p.planes[1].height as u32);
        let k = {
            let s = self.state.lock_ok();
            1 - s.shown.unwrap_or(1)
        };
        // The set for this size (OBS's textures made on its graphics context).
        let fresh = {
            let s = self.state.lock_ok();
            s.sets[k]
                .as_ref()
                .is_none_or(|set| set.size != size || set.chroma != chroma)
        };
        // OBS's graphics context is never entered with the state lock held: OBS's
        // graphics thread holds the context and then takes the lock to draw (a deadlock,
        // OBS's video stopped for good).
        let decided = self.state.lock_ok().sharing;
        let sharing = match decided {
            Some(v) => v,
            None => {
                let v = sharing_possible();
                *self.state.lock_ok().sharing.get_or_insert(v)
            }
        };
        if fresh {
            let set = match sharing {
                true => self.make_set(size, chroma),
                false => upload_set(size, chroma),
            };
            let set = match set {
                Ok(set) => set,
                // Shared textures refused: OBS's own from here on.
                Err(e) if sharing => {
                    crate::log(
                        ffi::LOG_WARNING,
                        &format!("IDN Source (GPU): {e}; pictures uploaded into OBS's textures"),
                    );
                    self.state.lock_ok().sharing = Some(false);
                    upload_set(size, chroma)?
                }
                Err(e) => return Err(e),
            };
            let old = self.state.lock_ok().sets[k].replace(set);
            if let Some(old) = old {
                destroy(old);
            }
        }
        if !self.state.lock_ok().sharing.unwrap_or(false) {
            // Into OBS's textures from this memory (read back first if on the GPU).
            let read;
            let planes: Vec<&[u8]> = match gpu {
                Some(g) => {
                    read = g.read_planes(&self.hand).map_err(|e| e.to_string())?;
                    read.iter().map(Vec::as_slice).collect()
                }
                None => p.planes.iter().map(|pl| pl.data.as_slice()).collect(),
            };
            // The textures, taken out under the lock; written with it let go. Set `k` isn't
            // shown, and only this thread changes the sets.
            let textures: Vec<(*mut ffi::gs_texture_t, u32)> = {
                let s = self.state.lock_ok();
                let set = s.sets[k].as_ref().expect("made above");
                set.planes
                    .iter()
                    .enumerate()
                    .map(|(i, plane)| (plane.obs, if i == 0 { set.size.0 } else { set.chroma.0 }))
                    .collect()
            };
            // SAFETY: OBS's textures, written on its graphics context.
            unsafe {
                ffi::obs_enter_graphics();
                for (i, (tex, width)) in textures.iter().enumerate() {
                    if let Some(data) = planes.get(i) {
                        ffi::gs_texture_set_image(*tex, data.as_ptr(), *width, false);
                    }
                }
                ffi::obs_leave_graphics();
            }
            let mut s = self.state.lock_ok();
            s.shown = Some(k);
            (s.matrix, s.range_min, s.range_max) = (matrix, range_min, range_max);
            return Ok(());
        }
        {
            let s = self.state.lock_ok();
            let set = s.sets[k].as_ref().expect("made above");
            let q = &self.hand.queue;
            match gpu {
                // On the GPU: copied there.
                Some(g) => {
                    let mut enc = self.hand.device.create_command_encoder(&Default::default());
                    for (i, plane) in set.planes.iter().enumerate() {
                        if let Some(t) = &plane.tex {
                            g.copy_plane(&mut enc, i, t);
                        }
                    }
                    q.submit([enc.finish()]);
                }
                // As samples: uploaded.
                None => {
                    for (i, plane) in set.planes.iter().enumerate() {
                        let (Some(t), Some(pl)) = (&plane.tex, p.planes.get(i)) else {
                            continue;
                        };
                        q.write_texture(
                            t.as_image_copy(),
                            &pl.data,
                            wgpu::TexelCopyBufferLayout {
                                offset: 0,
                                bytes_per_row: Some(pl.width as u32),
                                rows_per_image: Some(pl.height as u32),
                            },
                            wgpu::Extent3d {
                                width: pl.width as u32,
                                height: pl.height as u32,
                                depth_or_array_layers: 1,
                            },
                        );
                    }
                    q.submit([]);
                }
            }
        }
        // Done on the GPU before OBS draws it.
        self.hand
            .device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(std::time::Duration::from_secs(2)),
            })
            .map_err(|e| e.to_string())?;
        let mut s = self.state.lock_ok();
        s.shown = Some(k);
        (s.matrix, s.range_min, s.range_max) = (matrix, range_min, range_max);
        Ok(())
    }

    /// Draws the shown picture (OBS's `video_render`, on its graphics thread).
    pub fn render(&self) {
        let s = self.state.lock_ok();
        let Some(set) = s.shown.and_then(|i| s.sets[i].as_ref()) else {
            return;
        };
        // SAFETY: OBS's graphics calls on its graphics thread (video_render), with textures
        // and an effect made on its context.
        unsafe {
            let mut e = THE_EFFECT.lock_ok();
            if e.is_none() {
                let mut err: *mut std::ffi::c_char = std::ptr::null_mut();
                let code = std::ffi::CString::new(EFFECT).expect("no NUL");
                let fx =
                    ffi::gs_effect_create(code.as_ptr(), c"idn_planes.effect".as_ptr(), &mut err);
                if fx.is_null() {
                    if !err.is_null() {
                        crate::log(
                            ffi::LOG_WARNING,
                            &format!(
                                "IDN Source (GPU): the effect: {}",
                                std::ffi::CStr::from_ptr(err).to_string_lossy()
                            ),
                        );
                        ffi::bfree(err as *mut _);
                    }
                    return;
                }
                *e = Some(Effect(fx));
            }
            let fx = e.as_ref().expect("made above").0;
            let param = |n: &std::ffi::CStr| ffi::gs_effect_get_param_by_name(fx, n.as_ptr());
            let tex = [set.planes[0].obs, set.planes[1].obs, set.planes[2].obs];
            ffi::gs_effect_set_texture(param(c"tex_y"), tex[0]);
            ffi::gs_effect_set_texture(param(c"tex_u"), tex[1]);
            ffi::gs_effect_set_texture(param(c"tex_v"), tex[2]);
            let row = |r: usize| ffi::vec4 {
                __bindgen_anon_1: ffi::vec4__bindgen_ty_1 {
                    ptr: [
                        s.matrix[4 * r],
                        s.matrix[4 * r + 1],
                        s.matrix[4 * r + 2],
                        s.matrix[4 * r + 3],
                    ],
                },
            };
            for (r, name) in [c"color_vec0", c"color_vec1", c"color_vec2"]
                .iter()
                .enumerate()
            {
                ffi::gs_effect_set_vec4(param(name), &row(r));
            }
            let v3 = |a: [f32; 3]| ffi::vec3 {
                __bindgen_anon_1: ffi::vec3__bindgen_ty_1 {
                    ptr: [a[0], a[1], a[2], 0.0],
                },
            };
            ffi::gs_effect_set_vec3(param(c"color_range_min"), &v3(s.range_min));
            ffi::gs_effect_set_vec3(param(c"color_range_max"), &v3(s.range_max));
            static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
                crate::log(
                    ffi::LOG_INFO,
                    &format!("IDN Source (GPU): drawing {}x{}", set.size.0, set.size.1),
                );
            }
            while ffi::gs_effect_loop(fx, c"Draw".as_ptr()) {
                ffi::gs_draw_sprite(tex[0], 0, set.size.0, set.size.1);
            }
        }
    }

    /// Three planes for `size` (chroma `chroma`), shared with OBS.
    fn make_set(&self, size: (u32, u32), chroma: (u32, u32)) -> Result<Set, String> {
        let mut planes = Vec::with_capacity(3);
        for (w, h) in [size, chroma, chroma] {
            match self.make_plane(w, h) {
                Ok(p) => planes.push(p),
                Err(e) => {
                    // The planes made already: their OBS textures go too.
                    destroy(Set {
                        planes,
                        size,
                        chroma,
                    });
                    return Err(e);
                }
            }
        }
        Ok(Set {
            planes,
            size,
            chroma,
        })
    }

    #[cfg(unix)]
    fn make_plane(&self, w: u32, h: u32) -> Result<Shared, String> {
        use ash::vk;
        use std::os::fd::{AsRawFd, FromRawFd};
        use wgpu::hal::api::Vulkan;
        let d = &self.hand.device;
        // SAFETY: Vulkan calls on this device, results checked; the image and its memory
        // go to wgpu, which frees them with the texture.
        let (image, fd, pitch, offset) = unsafe {
            let hal = d.as_hal::<Vulkan>().ok_or("not a Vulkan device")?;
            let (inst, dev) = (hal.shared_instance().raw_instance(), hal.raw_device());
            let phys = hal.raw_physical_device();
            let mut ext = vk::ExternalMemoryImageCreateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(vk::Format::R8_UNORM)
                .extent(vk::Extent3D {
                    width: w,
                    height: h,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::LINEAR)
                .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .push_next(&mut ext);
            let image = dev.create_image(&info, None).map_err(|e| e.to_string())?;
            let req = dev.get_image_memory_requirements(image);
            let props = inst.get_physical_device_memory_properties(phys);
            let Some(t) = props
                .memory_types_as_slice()
                .iter()
                .enumerate()
                .position(|(i, _)| req.memory_type_bits & (1 << i) != 0)
            else {
                dev.destroy_image(image, None);
                return Err("no memory for a shared texture".into());
            };
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let mut export = vk::ExportMemoryAllocateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(t as u32)
                .push_next(&mut dedicated)
                .push_next(&mut export);
            let mem = match dev.allocate_memory(&alloc, None) {
                Ok(m) => m,
                Err(e) => {
                    dev.destroy_image(image, None);
                    return Err(e.to_string());
                }
            };
            if let Err(e) = dev.bind_image_memory(image, mem, 0) {
                dev.free_memory(mem, None);
                dev.destroy_image(image, None);
                return Err(e.to_string());
            }
            let layout = dev.get_image_subresource_layout(
                image,
                vk::ImageSubresource {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    array_layer: 0,
                },
            );
            let x = ash::khr::external_memory_fd::Device::new(inst, dev);
            let fd = match x.get_memory_fd(
                &vk::MemoryGetFdInfoKHR::default()
                    .memory(mem)
                    .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
            ) {
                Ok(fd) => std::os::fd::OwnedFd::from_raw_fd(fd),
                Err(e) => {
                    dev.free_memory(mem, None);
                    dev.destroy_image(image, None);
                    return Err(e.to_string());
                }
            };
            let tex = hal.texture_from_raw(
                image,
                &wgpu::hal::TextureDescriptor {
                    label: Some("IDN plane"),
                    size: wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::R8Unorm,
                    usage: wgpu::TextureUses::COPY_DST | wgpu::TextureUses::RESOURCE,
                    memory_flags: wgpu::hal::MemoryFlags::empty(),
                    view_formats: Vec::new(),
                },
                None,
                wgpu::hal::vulkan::TextureMemory::Dedicated(mem),
            );
            (tex, fd, layout.row_pitch, layout.offset)
        };
        // SAFETY: the hal texture made above on this device.
        let tex = unsafe {
            d.create_texture_from_hal::<Vulkan>(
                image,
                &wgpu::TextureDescriptor {
                    label: Some("IDN plane"),
                    size: wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::R8Unorm,
                    usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::TextureUses::UNINITIALIZED,
            )
        };
        // DRM_FORMAT_R8; a linear layout (modifier 0).
        const DRM_R8: u32 = u32::from_le_bytes(*b"R8  ");
        let (fds, strides, offsets, modifiers) =
            ([fd.as_raw_fd()], [pitch as u32], [offset as u32], [0u64]);
        // SAFETY: OBS's graphics context entered for the call.
        let obs = unsafe {
            ffi::obs_enter_graphics();
            let t = ffi::gs_texture_create_from_dmabuf(
                w,
                h,
                DRM_R8,
                ffi::gs_color_format_GS_R8,
                1,
                fds.as_ptr(),
                strides.as_ptr(),
                offsets.as_ptr(),
                modifiers.as_ptr(),
            );
            ffi::obs_leave_graphics();
            t
        };
        if obs.is_null() {
            return Err("OBS couldn't import a shared texture (DMA-BUF)".into());
        }
        Ok(Shared {
            tex: Some(tex),
            obs,
            _fd: Some(fd),
        })
    }

    #[cfg(windows)]
    fn make_plane(&self, w: u32, h: u32) -> Result<Shared, String> {
        use wgpu::hal::api::Dx12;
        use windows::Win32::Foundation::GENERIC_ALL;
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::Win32::Graphics::Dxgi::Common::*;
        let d = &self.hand.device;
        // SAFETY: Direct3D 12 calls on this device, results checked; the resource goes to
        // wgpu, the handle to OBS (opened in this process) and kept here.
        let (res, handle) = unsafe {
            let hal = d.as_hal::<Dx12>().ok_or("not a Direct3D 12 device")?;
            let dev = hal.raw_device().clone();
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Alignment: 0,
                Width: w as u64,
                Height: h,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_R8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
                // Back to the common state after each submission: Direct3D 11 reads it then.
                Flags: D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS,
            };
            let mut res: Option<ID3D12Resource> = None;
            dev.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_SHARED,
                &desc,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut res,
            )
            .map_err(|e| e.to_string())?;
            let res = res.ok_or("no texture")?;
            let h = dev
                .CreateSharedHandle(&res, None, GENERIC_ALL.0, windows::core::PCWSTR::null())
                .map_err(|e| e.to_string())?;
            (res, Handle(h))
        };
        let size = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };
        // SAFETY: a texture of this device; wgpu owns it from here.
        let tex = unsafe {
            let hal = wgpu::hal::dx12::Device::texture_from_raw(
                res,
                wgpu::TextureFormat::R8Unorm,
                wgpu::TextureDimension::D2,
                size,
                1,
                1,
            );
            d.create_texture_from_hal::<Dx12>(
                hal,
                &wgpu::TextureDescriptor {
                    label: Some("IDN plane"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::R8Unorm,
                    usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::TextureUses::UNINITIALIZED,
            )
        };
        // SAFETY: OBS's graphics context entered for the call; the handle is this
        // process's (OBS opens it with Direct3D 11.1).
        let obs = unsafe {
            ffi::obs_enter_graphics();
            let t = ffi::gs_texture_open_nt_shared(handle.0.0 as usize as u32);
            ffi::obs_leave_graphics();
            t
        };
        if obs.is_null() {
            return Err("OBS couldn't open a shared texture".into());
        }
        Ok(Shared {
            tex: Some(tex),
            obs,
            _handle: Some(handle),
        })
    }
}

/// Whether OBS can draw textures this plugin shares: not when it draws in software
/// (llvmpipe and the like can't map a GPU's DMA-BUF; OBS crashed). Asked on OBS's context.
fn sharing_possible() -> bool {
    #[cfg(unix)]
    {
        // GL_RENDERER, through the GL OBS has loaded (OBS draws with OpenGL on Linux).
        // SAFETY: a C function of OBS's GL called on its graphics context; the string it
        // returns lives with the context.
        unsafe {
            ffi::obs_enter_graphics();
            let f = libc::dlsym(libc::RTLD_DEFAULT, c"glGetString".as_ptr());
            let name = if f.is_null() {
                None
            } else {
                let get: extern "C" fn(u32) -> *const std::ffi::c_char = std::mem::transmute(f);
                let s = get(0x1F01); // GL_RENDERER
                (!s.is_null()).then(|| std::ffi::CStr::from_ptr(s).to_string_lossy().into_owned())
            };
            ffi::obs_leave_graphics();
            let software = name.as_deref().is_none_or(|n| {
                let n = n.to_ascii_lowercase();
                n.contains("llvmpipe") || n.contains("softpipe") || n.contains("software")
            });
            crate::log(
                ffi::LOG_INFO,
                &format!(
                    "IDN Source (GPU): OBS draws with {}: {}",
                    name.as_deref().unwrap_or("an unknown renderer"),
                    if software {
                        "pictures uploaded into its textures"
                    } else {
                        "textures shared with it"
                    }
                ),
            );
            !software
        }
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// `GS_DYNAMIC` (graphics.h: a macro, not in the bindings): a texture written often.
const GS_DYNAMIC: u32 = 1 << 1;

/// A set of OBS's own textures (dynamic: written from this memory).
fn upload_set(size: (u32, u32), chroma: (u32, u32)) -> Result<Set, String> {
    let mut planes: Vec<Shared> = Vec::with_capacity(3);
    // SAFETY: OBS's graphics calls on its context.
    unsafe {
        ffi::obs_enter_graphics();
        for (w, h) in [size, chroma, chroma] {
            let t = ffi::gs_texture_create(
                w,
                h,
                ffi::gs_color_format_GS_R8,
                1,
                std::ptr::null_mut(),
                GS_DYNAMIC,
            );
            if t.is_null() {
                // The ones made already go too.
                for p in &planes {
                    ffi::gs_texture_destroy(p.obs);
                }
                ffi::obs_leave_graphics();
                return Err("OBS couldn't make a texture".into());
            }
            planes.push(Shared {
                tex: None,
                obs: t,
                #[cfg(unix)]
                _fd: None,
                #[cfg(windows)]
                _handle: None,
            });
        }
        ffi::obs_leave_graphics();
    }
    Ok(Set {
        planes,
        size,
        chroma,
    })
}

/// A set's OBS textures destroyed (on OBS's graphics context), ours dropped.
fn destroy(set: Set) {
    // SAFETY: OBS's textures, made on its context.
    unsafe {
        ffi::obs_enter_graphics();
        for p in &set.planes {
            ffi::gs_texture_destroy(p.obs);
        }
        ffi::obs_leave_graphics();
    }
}

impl Drop for GpuView {
    fn drop(&mut self) {
        // (Owned here: no lock taken around OBS's graphics.)
        let s = self.state.get_mut().unwrap();
        for set in s.sets.iter_mut().filter_map(Option::take) {
            destroy(set);
        }
    }
}

//! Vulkan's external memory for other APIs and devices (Linux): the DRM format
//! modifiers this device imports DMA-BUF with, and buffers whose memory CUDA or another
//! Vulkan device imports.

use super::*;

/// The DRM format modifiers `hand`'s Vulkan device imports single-plane DMA-BUF images of
/// `format` with, to read them in a shader (Linux; `Bgra8Unorm` or `Rgba8Unorm`): none
/// where it imports none (another backend, no `VULKAN_EXTERNAL_MEMORY_DMA_BUF`).
#[cfg(target_os = "linux")]
pub fn dmabuf_modifiers(hand: &GpuHandIn, format: wgpu::TextureFormat) -> Vec<u64> {
    use ash::vk;
    use wgpu::hal::api::Vulkan;
    if !hand
        .device
        .features()
        .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF)
    {
        return Vec::new();
    }
    let vk_format = match format {
        wgpu::TextureFormat::Bgra8Unorm => vk::Format::B8G8R8A8_UNORM,
        wgpu::TextureFormat::Rgba8Unorm => vk::Format::R8G8B8A8_UNORM,
        _ => return Vec::new(),
    };
    // SAFETY: queries of the physical device behind this wgpu device, through its own
    // instance; nothing is created or changed.
    unsafe {
        let Some(hal) = hand.device.as_hal::<Vulkan>() else {
            return Vec::new();
        };
        let instance = hal.shared_instance().raw_instance();
        let pd = hal.raw_physical_device();
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut props = vk::FormatProperties2::default().push_next(&mut list);
        instance.get_physical_device_format_properties2(pd, vk_format, &mut props);
        let n = list.drm_format_modifier_count as usize;
        if n == 0 {
            return Vec::new();
        }
        let mut each = vec![vk::DrmFormatModifierPropertiesEXT::default(); n];
        let mut list =
            vk::DrmFormatModifierPropertiesListEXT::default().drm_format_modifier_properties(&mut each);
        let mut props = vk::FormatProperties2::default().push_next(&mut list);
        instance.get_physical_device_format_properties2(pd, vk_format, &mut props);
        let n = list.drm_format_modifier_count as usize;
        each.truncate(n);
        each.iter()
            .filter(|m| {
                m.drm_format_modifier_plane_count == 1
                    && m.drm_format_modifier_tiling_features
                        .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
            })
            .map(|m| m.drm_format_modifier)
            .collect()
    }
}

/// A buffer on `hand`'s Vulkan device whose memory another API may import (Linux:
/// `VK_KHR_external_memory_fd`, an opaque descriptor): the buffer (`STORAGE`,
/// `COPY_SRC`, `COPY_DST`), the descriptor (the importer takes it), and the allocation's
/// size (what the importer is told). The memory is freed when the buffer is dropped.
#[cfg(target_os = "linux")]
pub fn exportable_buffer(
    hand: &GpuHandIn,
    size: u64,
) -> std::io::Result<(wgpu::Buffer, std::os::fd::OwnedFd, u64)> {
    use ash::vk;
    use std::os::fd::FromRawFd;
    use wgpu::hal::api::Vulkan;
    let e = |m: &str| std::io::Error::other(format!("an exportable buffer: {m}"));
    // SAFETY: a buffer and its memory made, bound and exported on this device; the wgpu
    // buffer made from it frees both when it goes (the callback), and nothing else holds
    // them.
    unsafe {
        let hal = hand.device.as_hal::<Vulkan>().ok_or_else(|| e("not a Vulkan device"))?;
        let dev = hal.raw_device().clone();
        let instance = hal.shared_instance().raw_instance();
        let pd = hal.raw_physical_device();
        let mut ext = vk::ExternalMemoryBufferCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
        let info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(
                vk::BufferUsageFlags::STORAGE_BUFFER
                    | vk::BufferUsageFlags::TRANSFER_SRC
                    | vk::BufferUsageFlags::TRANSFER_DST,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .push_next(&mut ext);
        let buf = dev.create_buffer(&info, None).map_err(|r| e(&r.to_string()))?;
        let req = dev.get_buffer_memory_requirements(buf);
        let props = instance.get_physical_device_memory_properties(pd);
        let Some(kind) = (0..props.memory_type_count).find(|&i| {
            req.memory_type_bits & (1 << i) != 0
                && props.memory_types[i as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
        }) else {
            dev.destroy_buffer(buf, None);
            return Err(e("no device memory for it"));
        };
        let mut export = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(kind)
            .push_next(&mut export);
        let mem = match dev.allocate_memory(&alloc, None) {
            Ok(m) => m,
            Err(r) => {
                dev.destroy_buffer(buf, None);
                return Err(e(&r.to_string()));
            }
        };
        let free = {
            let dev = dev.clone();
            move || {
                dev.destroy_buffer(buf, None);
                dev.free_memory(mem, None);
            }
        };
        if let Err(r) = dev.bind_buffer_memory(buf, mem, 0) {
            free();
            return Err(e(&r.to_string()));
        }
        let fdf = ash::khr::external_memory_fd::Device::new(instance, &dev);
        let fd = match fdf.get_memory_fd(
            &vk::MemoryGetFdInfoKHR::default()
                .memory(mem)
                .handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD),
        ) {
            Ok(fd) => std::os::fd::OwnedFd::from_raw_fd(fd),
            Err(r) => {
                free();
                return Err(e(&format!("exporting it ({r})")));
            }
        };
        drop(hal);
        let raw = wgpu::hal::vulkan::Buffer::from_raw_externally_owned(buf, Box::new(free));
        let b = hand.device.create_buffer_from_hal::<Vulkan>(
            raw,
            &wgpu::BufferDescriptor {
                label: Some("exported"),
                size,
                usage: wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::COPY_SRC
                    | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            },
        );
        Ok((b, fd, req.size))
    }
}

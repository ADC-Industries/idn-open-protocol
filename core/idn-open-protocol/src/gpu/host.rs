//! Shared memory read by the GPU itself: a source's picture
//! ring (§E.26) imported as a GPU buffer (Vulkan's `VK_EXT_external_memory_host`), so a
//! picture goes from the ring to the GPU without the CPU copying it. For a device opened
//! with [`request_device`]; elsewhere (Direct3D 12 for now) the CPU copies as before.

use std::io;

/// A device on `adapter`, as `adapter.request_device(desc)` makes it, with Vulkan's
/// `VK_EXT_external_memory_host` turned on where the GPU has it (for [`import_host`]).
pub fn request_device(
    adapter: &wgpu::Adapter,
    desc: &wgpu::DeviceDescriptor<'_>,
) -> Result<(wgpu::Device, wgpu::Queue), wgpu::RequestDeviceError> {
    #[cfg(any(target_os = "linux", windows))]
    {
        use ash::ext::external_memory_host as host;
        use wgpu::hal::api::Vulkan;
        // SAFETY: the extension is added only where the physical device has it, and
        // nothing else wgpu asked for is changed; the device is handed to wgpu at once.
        let opened = unsafe {
            adapter.as_hal::<Vulkan>().and_then(|a| {
                if !a
                    .physical_device_capabilities()
                    .supports_extension(host::NAME)
                {
                    return None;
                }
                let add: Box<wgpu::hal::vulkan::CreateDeviceCallback<'_>> = Box::new(|args| {
                    if !args.extensions.contains(&host::NAME) {
                        args.extensions.push(host::NAME);
                    }
                });
                a.open_with_callback(
                    desc.required_features,
                    &desc.required_limits,
                    &desc.memory_hints,
                    Some(add),
                )
                .ok()
            })
        };
        if let Some(open) = opened {
            // SAFETY: a device opened on this adapter, with what `desc` asks.
            return unsafe { adapter.create_device_from_hal::<Vulkan>(open, desc) };
        }
    }
    super::block_on(adapter.request_device(desc))
}

/// `len` bytes of this process's memory at `ptr` (a mapping of shared memory, page
/// aligned) as a buffer on `device` the GPU reads (`COPY_SRC`, `STORAGE`), as far as the
/// GPU's alignment for imports allows (the buffer's size says). `None` where it can't: a
/// device without the extension, another backend, a driver that refuses.
///
/// # Safety
/// The memory must stay mapped until the buffer, and every use of it on the GPU, is gone.
pub unsafe fn import_host(
    device: &wgpu::Device,
    ptr: *mut u8,
    len: usize,
) -> io::Result<Option<wgpu::Buffer>> {
    #[cfg(any(target_os = "linux", windows))]
    {
        use ash::ext::external_memory_host as host;
        use ash::vk;
        use wgpu::hal::api::Vulkan;
        const HANDLE: vk::ExternalMemoryHandleTypeFlags =
            vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT;
        // SAFETY: only the list of enabled extensions is read.
        let enabled = unsafe { device.as_hal::<Vulkan>() }
            .is_some_and(|h| h.enabled_device_extensions().contains(&host::NAME));
        if !enabled {
            return Ok(None);
        }
        let err =
            |what: &'static str| move |e: vk::Result| io::Error::other(format!("{what}: {e}"));
        let made = super::vk_share::with_vk(device, |instance, phys, dev| -> io::Result<_> {
            // SAFETY: Vulkan calls on this device's handles; what is made is destroyed on
            // failure or handed to wgpu, which owns it from then on.
            unsafe {
                let mut hp = vk::PhysicalDeviceExternalMemoryHostPropertiesEXT::default();
                let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut hp);
                instance.get_physical_device_properties2(phys, &mut p2);
                let align = hp.min_imported_host_pointer_alignment.max(1) as usize;
                let size = (len / align * align) as u64;
                if !(ptr as usize).is_multiple_of(align) || size == 0 {
                    return Ok(None);
                }
                let ext = host::Device::new(instance, dev);
                let mut props = vk::MemoryHostPointerPropertiesEXT::default();
                (ext.fp().get_memory_host_pointer_properties_ext)(
                    dev.handle(),
                    HANDLE,
                    ptr as *const _,
                    &mut props,
                )
                .result()
                .map_err(err("host memory"))?;
                let mut external =
                    vk::ExternalMemoryBufferCreateInfo::default().handle_types(HANDLE);
                let info = vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(
                        vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::STORAGE_BUFFER,
                    )
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .push_next(&mut external);
                let buf = dev.create_buffer(&info, None).map_err(err("a buffer"))?;
                let req = dev.get_buffer_memory_requirements(buf);
                let bits = req.memory_type_bits & props.memory_type_bits;
                if bits == 0 || req.size > size {
                    dev.destroy_buffer(buf, None);
                    return Ok(None);
                }
                let mut import = vk::ImportMemoryHostPointerInfoEXT::default()
                    .handle_type(HANDLE)
                    .host_pointer(ptr as *mut _);
                let alloc = vk::MemoryAllocateInfo::default()
                    .allocation_size(size)
                    .memory_type_index(bits.trailing_zeros())
                    .push_next(&mut import);
                let mem = match dev.allocate_memory(&alloc, None) {
                    Ok(m) => m,
                    Err(e) => {
                        dev.destroy_buffer(buf, None);
                        return Err(err("importing memory")(e));
                    }
                };
                if let Err(e) = dev.bind_buffer_memory(buf, mem, 0) {
                    dev.free_memory(mem, None);
                    dev.destroy_buffer(buf, None);
                    return Err(err("binding memory")(e));
                }
                Ok(Some((buf, mem, size)))
            }
        });
        let Some((buf, mem, size)) = made.transpose()?.flatten() else {
            return Ok(None);
        };
        // SAFETY: the buffer and its memory are this device's, bound, and from here owned
        // by wgpu, which destroys and frees them when the buffer goes.
        let buffer = unsafe {
            let hal = wgpu::hal::vulkan::Buffer::from_raw_managed(buf, mem, 0, size);
            device.create_buffer_from_hal::<Vulkan>(
                hal,
                &wgpu::BufferDescriptor {
                    label: Some("shared memory"),
                    size,
                    usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::STORAGE,
                    mapped_at_creation: false,
                },
            )
        };
        return Ok(Some(buffer));
    }
    #[allow(unreachable_code)]
    {
        let _ = (device, ptr, len);
        Ok(None)
    }
}

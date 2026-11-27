//! Exporting and importing memory between programs: Vulkan (Linux, Windows), Direct3D 12 (Windows).

use super::*;

/// What keeps an exported slot's memory reachable by its name: Linux, its descriptor,
/// handed out by this process's share server; Windows, its named handle. Never read:
/// held for what dropping it does (closing the descriptor or handle), hence the allows.
pub(super) enum Keep {
    #[cfg(target_os = "linux")]
    Fd(#[allow(dead_code)] idn_common::shm::SharedFd),
    #[cfg(windows)]
    Handle(#[allow(dead_code)] win::Named),
}

/// A slot's memory as exported: its name, what keeps it, and what an importer needs to
/// allocate alike (Vulkan's memory type and size; 0 for Direct3D 12).
pub(super) struct Exported {
    pub(super) name: String,
    pub(super) keep: Keep,
    pub(super) memory_type: u32,
    pub(super) alloc_size: u64,
}

#[cfg(any(target_os = "linux", windows))]
pub(super) mod vk_share {
    use super::*;
    use ash::vk;
    use wgpu::hal::api::Vulkan;

    #[cfg(target_os = "linux")]
    const HANDLE: vk::ExternalMemoryHandleTypeFlags = vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD;
    #[cfg(windows)]
    const HANDLE: vk::ExternalMemoryHandleTypeFlags =
        vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32;
    #[cfg(target_os = "linux")]
    const EXTENSION: &std::ffi::CStr = ash::khr::external_memory_fd::NAME;
    #[cfg(windows)]
    const EXTENSION: &std::ffi::CStr = ash::khr::external_memory_win32::NAME;

    const USAGE: vk::BufferUsageFlags = vk::BufferUsageFlags::from_raw(
        vk::BufferUsageFlags::TRANSFER_SRC.as_raw()
            | vk::BufferUsageFlags::TRANSFER_DST.as_raw()
            | vk::BufferUsageFlags::STORAGE_BUFFER.as_raw(),
    );

    pub(super) fn vk_err(what: &str) -> impl Fn(vk::Result) -> io::Error + '_ {
        move |e| io::Error::other(format!("gpu/1: {what}: {e}"))
    }

    /// Runs `f` with the device's Vulkan instance, physical and logical devices; `None` when
    /// the device isn't on Vulkan.
    pub(in crate::gpu) fn with_vk<T>(
        device: &wgpu::Device,
        f: impl FnOnce(&ash::Instance, vk::PhysicalDevice, &ash::Device) -> T,
    ) -> Option<T> {
        // SAFETY: the raw handles are used only while the hal device is held, and nothing
        // made through them outlives the wgpu objects wrapping it.
        let hal = unsafe { device.as_hal::<Vulkan>() }?;
        Some(f(
            hal.shared_instance().raw_instance(),
            hal.raw_physical_device(),
            hal.raw_device(),
        ))
    }

    pub(super) fn device_id(device: &wgpu::Device) -> Option<String> {
        with_vk(device, |instance, phys, _| {
            let mut id = vk::PhysicalDeviceIDProperties::default();
            let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
            // SAFETY: a physical device of this instance; Vulkan 1.1 (wgpu's minimum).
            unsafe { instance.get_physical_device_properties2(phys, &mut p2) };
            idn_common::hex::hex(&[id.device_uuid, id.driver_uuid].concat())
        })
    }

    pub(super) fn extension_enabled(device: &wgpu::Device) -> bool {
        // SAFETY: only the list of enabled extensions is read.
        unsafe { device.as_hal::<Vulkan>() }
            .is_some_and(|h| h.enabled_device_extensions().contains(&EXTENSION))
    }

    /// Whether the device can export and import buffers as this system's handles.
    pub(in crate::gpu) fn can_share(device: &wgpu::Device) -> bool {
        extension_enabled(device)
            && with_vk(device, |instance, phys, _| {
                let info = vk::PhysicalDeviceExternalBufferInfo::default()
                    .usage(USAGE)
                    .handle_type(HANDLE);
                let mut props = vk::ExternalBufferProperties::default();
                // SAFETY: a physical device of this instance.
                unsafe {
                    instance.get_physical_device_external_buffer_properties(phys, &info, &mut props)
                };
                let m = props.external_memory_properties;
                m.external_memory_features.contains(
                    vk::ExternalMemoryFeatureFlags::EXPORTABLE
                        | vk::ExternalMemoryFeatureFlags::IMPORTABLE,
                ) && m.compatible_handle_types.contains(HANDLE)
            })
            .unwrap_or(false)
    }

    /// Memory to import: its name, the exporter's memory type and allocation size.
    type Import<'a> = (&'a str, u32, u64);

    /// A buffer of `bytes` whose memory is exportable (`import: None`) or imported, with
    /// the memory type and size the exporter used.
    pub(super) fn make(
        device: &wgpu::Device,
        bytes: u64,
        import: Option<Import>,
    ) -> io::Result<(wgpu::Buffer, Option<Exported>)> {
        if !extension_enabled(device) {
            return Err(unsupported(
                "this GPU can't share memory with other programs",
            ));
        }
        // The memory's own reference to hand over (Linux, a descriptor: fetched before
        // the allocation takes it; Windows, a name).
        #[cfg(target_os = "linux")]
        let given = match import {
            Some((name, ..)) => Some(idn_common::shm::fetch_fd(name)?),
            None => None,
        };
        #[cfg(windows)]
        let export_name = super::win::fresh_name("idn-gpu");
        let made = with_vk(device, |instance, phys, dev| -> io::Result<_> {
            let mut ext = vk::ExternalMemoryBufferCreateInfo::default().handle_types(HANDLE);
            let info = vk::BufferCreateInfo::default()
                .size(bytes)
                .usage(USAGE)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .push_next(&mut ext);
            // SAFETY: Vulkan calls on this device, each result checked; on any failure what
            // was made is destroyed before returning.
            unsafe {
                let buf = dev.create_buffer(&info, None).map_err(vk_err("a buffer"))?;
                let req = dev.get_buffer_memory_requirements(buf);
                let (type_index, size) =
                    match &import {
                        Some((_, t, s)) => (*t, *s),
                        None => {
                            let props = instance.get_physical_device_memory_properties(phys);
                            let Some(t) =
                                props.memory_types_as_slice().iter().enumerate().position(
                                    |(i, m)| {
                                        req.memory_type_bits & (1 << i) != 0
                                            && m.property_flags
                                                .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
                                    },
                                )
                            else {
                                dev.destroy_buffer(buf, None);
                                return Err(unsupported("no device memory to share"));
                            };
                            (t as u32, req.size)
                        }
                    };
                if type_index >= 32
                    || req.memory_type_bits & (1 << type_index) == 0
                    || size < req.size
                {
                    dev.destroy_buffer(buf, None);
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "gpu/1: the offered memory doesn't fit this GPU's buffer",
                    ));
                }
                let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().buffer(buf);
                let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(HANDLE);
                let alloc = vk::MemoryAllocateInfo::default()
                    .allocation_size(size)
                    .memory_type_index(type_index)
                    .push_next(&mut dedicated);
                #[cfg(target_os = "linux")]
                let (mem, exported) = {
                    use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
                    // An imported descriptor is Vulkan's once the allocation succeeds.
                    let fd = given.map_or(-1, |f| f.into_raw_fd());
                    let mut import_info = vk::ImportMemoryFdInfoKHR::default()
                        .handle_type(HANDLE)
                        .fd(fd);
                    let alloc = if fd >= 0 {
                        alloc.push_next(&mut import_info)
                    } else {
                        alloc.push_next(&mut export)
                    };
                    let mem = match dev.allocate_memory(&alloc, None) {
                        Ok(m) => m,
                        Err(e) => {
                            // A descriptor not taken stays ours to close.
                            if fd >= 0 {
                                drop(OwnedFd::from_raw_fd(fd));
                            }
                            dev.destroy_buffer(buf, None);
                            return Err(vk_err("memory")(e));
                        }
                    };
                    let exported = if fd < 0 {
                        let x = ash::khr::external_memory_fd::Device::new(instance, dev);
                        match x.get_memory_fd(
                            &vk::MemoryGetFdInfoKHR::default()
                                .memory(mem)
                                .handle_type(HANDLE),
                        ) {
                            Ok(fd) => {
                                let shared = idn_common::shm::SharedFd::new(
                                    "idn-gpu",
                                    OwnedFd::from_raw_fd(fd),
                                );
                                match shared {
                                    Ok(sh) => Some(Exported {
                                        name: sh.name().to_owned(),
                                        keep: Keep::Fd(sh),
                                        memory_type: type_index,
                                        alloc_size: size,
                                    }),
                                    Err(e) => {
                                        dev.free_memory(mem, None);
                                        dev.destroy_buffer(buf, None);
                                        return Err(e);
                                    }
                                }
                            }
                            Err(e) => {
                                dev.free_memory(mem, None);
                                dev.destroy_buffer(buf, None);
                                return Err(vk_err("exporting memory")(e));
                            }
                        }
                    } else {
                        None
                    };
                    (mem, exported)
                };
                #[cfg(windows)]
                let (mem, exported) = {
                    // Exported under a name of this session's (`Local\…`); imported by it.
                    let wide = super::win::wide(match &import {
                        Some((name, ..)) => name,
                        None => &export_name,
                    });
                    let mut export_name_info = vk::ExportMemoryWin32HandleInfoKHR::default()
                        .dw_access(super::win::GENERIC_ALL)
                        .name(wide.as_ptr());
                    let mut import_info = vk::ImportMemoryWin32HandleInfoKHR::default()
                        .handle_type(HANDLE)
                        .name(wide.as_ptr());
                    let alloc = if import.is_some() {
                        alloc.push_next(&mut import_info)
                    } else {
                        alloc
                            .push_next(&mut export)
                            .push_next(&mut export_name_info)
                    };
                    let mem = match dev.allocate_memory(&alloc, None) {
                        Ok(m) => m,
                        Err(e) => {
                            dev.destroy_buffer(buf, None);
                            return Err(vk_err("memory")(e));
                        }
                    };
                    let exported = if import.is_none() {
                        let x = ash::khr::external_memory_win32::Device::new(instance, dev);
                        match x.get_memory_win32_handle(
                            &vk::MemoryGetWin32HandleInfoKHR::default()
                                .memory(mem)
                                .handle_type(HANDLE),
                        ) {
                            // The handle keeps the name: open while the slot is offered.
                            Ok(h) => Some(Exported {
                                name: export_name.clone(),
                                keep: Keep::Handle(super::win::Named(h)),
                                memory_type: type_index,
                                alloc_size: size,
                            }),
                            Err(e) => {
                                dev.free_memory(mem, None);
                                dev.destroy_buffer(buf, None);
                                return Err(vk_err("exporting memory")(e));
                            }
                        }
                    } else {
                        None
                    };
                    (mem, exported)
                };
                if let Err(e) = dev.bind_buffer_memory(buf, mem, 0) {
                    dev.free_memory(mem, None);
                    dev.destroy_buffer(buf, None);
                    return Err(vk_err("binding memory")(e));
                }
                Ok((buf, mem, size, exported))
            }
        })
        .ok_or_else(|| unsupported("not a Vulkan device"))??;
        let (buf, mem, size, exported) = made;
        // SAFETY: the buffer and its memory are this device's, bound, and from here owned by
        // wgpu, which destroys and frees them when the buffer goes.
        let buffer = unsafe {
            let hal = wgpu::hal::vulkan::Buffer::from_raw_managed(buf, mem, 0, size);
            device.create_buffer_from_hal::<Vulkan>(hal, &slot_descriptor(bytes))
        };
        Ok((buffer, exported))
    }

    pub(super) fn export(
        device: &wgpu::Device,
        bytes: u64,
    ) -> io::Result<(wgpu::Buffer, Exported)> {
        let (b, e) = make(device, bytes, None)?;
        Ok((
            b,
            e.ok_or_else(|| io::Error::other("gpu/1: nothing exported"))?,
        ))
    }

    pub(super) fn import(
        device: &wgpu::Device,
        name: &str,
        bytes: u64,
        memory_type: u32,
        alloc_size: u64,
    ) -> io::Result<wgpu::Buffer> {
        make(device, bytes, Some((name, memory_type, alloc_size))).map(|(b, _)| b)
    }
}

/// A slot buffer's description, either side.
pub(super) fn slot_descriptor(bytes: u64) -> wgpu::BufferDescriptor<'static> {
    wgpu::BufferDescriptor {
        label: Some("gpu/1 slot"),
        size: bytes,
        usage: wgpu::BufferUsages::COPY_SRC
            | wgpu::BufferUsages::COPY_DST
            | wgpu::BufferUsages::STORAGE,
        mapped_at_creation: false,
    }
}

/// Windows: names in the session's namespace, and Direct3D 12's shared buffers.
#[cfg(windows)]
pub(super) mod win {
    use super::*;
    use wgpu::hal::api::Dx12;
    use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL as GA, HANDLE};
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC};
    use windows::core::PCWSTR;

    pub(super) const GENERIC_ALL: u32 = GA.0;

    /// A named handle, closed when dropped (the name lives while a handle to it is open).
    pub(in crate::gpu) struct Named(pub(super) isize);

    impl Drop for Named {
        fn drop(&mut self) {
            // SAFETY: a handle this program opened and owns.
            unsafe {
                let _ = CloseHandle(HANDLE(self.0 as *mut std::ffi::c_void));
            }
        }
    }

    /// `Local\PREFIX-` and 16 random hex digits: this session's, and not guessable.
    pub(super) fn fresh_name(prefix: &str) -> String {
        use std::hash::{BuildHasher, Hasher};
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(idn_common::epoch::now().as_nanos());
        format!("Local\\{prefix}-{:016x}", h.finish())
    }

    /// A name as this module makes them.
    pub(super) fn valid_name(n: &str) -> bool {
        n.strip_prefix("Local\\idn-gpu-")
            .is_some_and(|r| r.len() == 16 && r.chars().all(|c| c.is_ascii_hexdigit()))
    }

    pub(super) fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub(super) fn d3d<T>(r: windows::core::Result<T>, what: &str) -> io::Result<T> {
        r.map_err(|e| io::Error::other(format!("gpu/1: {what}: {e}")))
    }

    /// The device's Direct3D 12 device, when it is on Direct3D 12.
    pub(super) fn raw(device: &wgpu::Device) -> Option<ID3D12Device> {
        // SAFETY: the interface is cloned (a reference counted), used while the device lives.
        unsafe { device.as_hal::<Dx12>() }.map(|h| h.raw_device().clone())
    }

    pub(super) fn device_id(device: &wgpu::Device) -> Option<String> {
        let d = raw(device)?;
        // SAFETY: a plain query of a live device.
        let luid = unsafe { d.GetAdapterLuid() };
        Some(format!(
            "d3d12-{:08x}{:08x}",
            luid.HighPart as u32, luid.LowPart
        ))
    }

    pub(super) fn wrap(device: &wgpu::Device, res: ID3D12Resource, bytes: u64) -> wgpu::Buffer {
        // SAFETY: a buffer resource of this device, of `bytes` bytes; wgpu owns it from here
        // (buffers go back to the common state at the end of each submission).
        unsafe {
            let hal = wgpu::hal::dx12::Device::buffer_from_raw(res, bytes);
            device.create_buffer_from_hal::<Dx12>(hal, &slot_descriptor(bytes))
        }
    }

    pub(super) fn export(
        device: &wgpu::Device,
        bytes: u64,
    ) -> io::Result<(wgpu::Buffer, Exported)> {
        let d = raw(device).ok_or_else(|| unsupported("not a Direct3D 12 device"))?;
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Alignment: 0,
            Width: bytes,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_UNKNOWN,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        };
        let mut res: Option<ID3D12Resource> = None;
        // SAFETY: Direct3D 12 calls on a live device, results checked.
        unsafe {
            d3d(
                d.CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_SHARED,
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut res,
                ),
                "a shared buffer",
            )?;
        }
        let res = res.ok_or_else(|| io::Error::other("gpu/1: no buffer"))?;
        let name = fresh_name("idn-gpu");
        let w = wide(&name);
        // SAFETY: as above; the handle is ours to close.
        let h = unsafe {
            d3d(
                d.CreateSharedHandle(&res, None, GENERIC_ALL, PCWSTR(w.as_ptr())),
                "sharing a buffer",
            )?
        };
        Ok((
            wrap(device, res, bytes),
            Exported {
                name,
                keep: Keep::Handle(Named(h.0 as isize)),
                memory_type: 0,
                alloc_size: 0,
            },
        ))
    }

    pub(super) fn import(
        device: &wgpu::Device,
        name: &str,
        bytes: u64,
    ) -> io::Result<wgpu::Buffer> {
        let d = raw(device).ok_or_else(|| unsupported("not a Direct3D 12 device"))?;
        let w = wide(name);
        // SAFETY: as above; the handle is closed once the resource is open.
        let res = unsafe {
            let h = d3d(
                d.OpenSharedHandleByName(PCWSTR(w.as_ptr()), GENERIC_ALL),
                "opening a shared buffer",
            )?;
            let _closer = Named(h.0 as isize);
            let mut res: Option<ID3D12Resource> = None;
            d3d(d.OpenSharedHandle(h, &mut res), "opening a shared buffer")?;
            res.ok_or_else(|| io::Error::other("gpu/1: no buffer"))?
        };
        // SAFETY: a plain query of the resource.
        if unsafe { res.GetDesc() }.Width < bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "gpu/1: the shared buffer is smaller than offered",
            ));
        }
        Ok(wrap(device, res, bytes))
    }
}

/// The device's identity for "the same GPU": on Vulkan its device and driver UUIDs (64 hex
/// digits), on Direct3D 12 `d3d12-` and its adapter's LUID. Two programs share pictures on
/// the GPU only when theirs are equal (so one on Vulkan and one on Direct3D 12 don't).
pub fn device_id(device: &wgpu::Device) -> Option<String> {
    #[cfg(any(target_os = "linux", windows))]
    if let Some(id) = vk_share::device_id(device) {
        return Some(id);
    }
    #[cfg(windows)]
    if let Some(id) = win::device_id(device) {
        return Some(id);
    }
    let _ = device;
    None
}

/// One exported slot of `bytes` on `device`.
pub(super) fn export(device: &wgpu::Device, bytes: u64) -> io::Result<(wgpu::Buffer, Exported)> {
    #[cfg(windows)]
    if device_id(device).is_some_and(|i| i.starts_with("d3d12-")) {
        return win::export(device, bytes);
    }
    #[cfg(any(target_os = "linux", windows))]
    {
        if !vk_share::can_share(device) {
            return Err(unsupported("this GPU can't share memory between programs"));
        }
        vk_share::export(device, bytes)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (device, bytes);
        Err(unsupported("gpu/1: not on this system yet"))
    }
}

/// Slot `name` as offered, imported on `device`.
pub(super) fn import(
    device: &wgpu::Device,
    name: &str,
    bytes: u64,
    memory_type: u32,
    alloc_size: u64,
) -> io::Result<wgpu::Buffer> {
    #[cfg(windows)]
    {
        if !win::valid_name(name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "gpu/1: a slot's name",
            ));
        }
        if device_id(device).is_some_and(|i| i.starts_with("d3d12-")) {
            return win::import(device, name, bytes);
        }
    }
    #[cfg(any(target_os = "linux", windows))]
    {
        vk_share::import(device, name, bytes, memory_type, alloc_size)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (device, name, bytes, memory_type, alloc_size);
        Err(unsupported("gpu/1: not on this system yet"))
    }
}

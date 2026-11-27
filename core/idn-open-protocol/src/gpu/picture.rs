//! A picture on the GPU (a buffer and its planes): copying a plane into a texture, reading it back; the devices' keys and the buffers pictures are made in.

use idn_common::sync::LockOk as _;
use super::*;

/// A picture handed over on the GPU: its planes in `buffer` (a copy of the sender's slot,
/// the receiver's own), laid out as [`slot_layout`] lays them.
#[derive(Clone, Debug)]
pub struct GpuPicture {
    /// The planes' buffer (`COPY_SRC`, `STORAGE`), the receiver's own.
    pub buffer: std::sync::Arc<wgpu::Buffer>,
    /// Where each plane is in it: Y, Cb, Cr (and alpha).
    pub planes: Vec<PlaneAt>,
}

impl GpuPicture {
    /// Copies plane `k` into `texture` (an R8 texture of its size, `COPY_DST`).
    pub fn copy_plane(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        k: usize,
        texture: &wgpu::Texture,
    ) {
        let p = self.planes[k];
        encoder.copy_buffer_to_texture(
            wgpu::TexelCopyBufferInfo {
                buffer: &self.buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: p.offset,
                    bytes_per_row: Some(p.row_bytes),
                    rows_per_image: Some(p.height),
                },
            },
            texture.as_image_copy(),
            wgpu::Extent3d {
                width: p.width,
                height: p.height,
                depth_or_array_layers: 1,
            },
        );
    }

    /// The planes read back into CPU memory (packed rows), for what needs them there (a
    /// recording): a copy to a mapped buffer, waited for.
    pub fn read_planes(&self, hand: &GpuHandIn) -> io::Result<Vec<Vec<u8>>> {
        let size = self.buffer.size();
        // A buffer of this size on this device from the last ones used, else a new one:
        // each picture made and mapped its own (16 MB at 4K, every frame of a recording
        // or an HDR display).
        let kept = {
            let key = device_key(&hand.device);
            let mut pool = READ_BACK.lock_ok();
            pool.iter()
                .position(|(d, b)| key.is_some() && *d == key && b.size() == size)
                .map(|i| pool.swap_remove(i).1)
        };
        let down = kept.unwrap_or_else(|| {
            hand.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("gpu/1 read back"),
                size,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        });
        let mut enc = hand.device.create_command_encoder(&Default::default());
        enc.copy_buffer_to_buffer(&self.buffer, 0, &down, 0, size);
        let sub = hand.queue.submit([enc.finish()]);
        map_read(&hand.device, down.slice(..), Some(sub)).map_err(|e| {
            io::Error::other(format!("gpu/1: the picture could not be read back ({e})"))
        })?;
        let data = down
            .slice(..)
            .get_mapped_range()
            .map_err(|e| io::Error::other(format!("gpu/1 read back: {e}")))?;
        let planes = self
            .planes
            .iter()
            .map(|p| {
                let (w, r) = (p.width as usize, p.row_bytes as usize);
                let mut out = Vec::with_capacity(w * p.height as usize);
                for y in 0..p.height as usize {
                    let at = p.offset as usize + y * r;
                    out.extend_from_slice(&data[at..at + w]);
                }
                out
            })
            .collect();
        drop(data);
        down.unmap();
        if let Some(key) = device_key(&hand.device) {
            let mut pool = READ_BACK.lock_ok();
            if pool.len() >= READ_BACK_KEPT {
                pool.remove(0);
            }
            pool.push((Some(key), down));
        }
        Ok(planes)
    }
}

/// Read-back buffers kept for [`GpuPicture::read_planes`], with their devices' keys.
pub(super) static READ_BACK: std::sync::Mutex<Vec<(Option<u64>, wgpu::Buffer)>> =
    std::sync::Mutex::new(Vec::new());
pub(super) const READ_BACK_KEPT: usize = 4;

/// The device's native handle (Vulkan's `VkDevice`, Direct3D 12's `ID3D12Device`), which
/// tells devices apart across the process. `wgpu::Device`'s own equality compares ids
/// that start again in each wgpu instance: a process with two (OBS's plugin, the tests)
/// was handed another device's buffers. `None` elsewhere: nothing pooled.
pub fn device_key(device: &wgpu::Device) -> Option<u64> {
    #[cfg(any(target_os = "linux", windows))]
    {
        use ash::vk::Handle;
        // SAFETY: only the handle's value is read.
        if let Some(h) = unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() } {
            return Some(h.raw_device().handle().as_raw());
        }
    }
    #[cfg(windows)]
    {
        use windows::core::Interface;
        // SAFETY: only the interface's address is read.
        if let Some(h) = unsafe { device.as_hal::<wgpu::hal::api::Dx12>() } {
            return Some(h.raw_device().as_raw() as u64);
        }
    }
    let _ = device;
    None
}

// Send and Sync stated for these types instead of worked out from their fields: proving
// them through wgpu's types (its devices reach naga's) took every program that holds an
// input or output past the compiler's recursion limit. Sound because the wgpu types they
// hold are Send and Sync, which the compiler checks here, once.
const _: () = {
    const fn shared<T: Send + Sync>() {}
    shared::<wgpu::Device>();
    shared::<wgpu::Queue>();
    shared::<wgpu::Buffer>();
    shared::<wgpu::BindGroupLayout>();
    shared::<wgpu::ComputePipeline>();
};
/// Buffers kept for pictures copied on the GPU (a sender's, for its taps there), and the
/// pass that copies them, with its device.
pub struct BufferPool(
    pub std::sync::Mutex<Buffers>,
    pub std::sync::Mutex<Option<(Option<u64>, PlaneCopy)>>,
);

impl Default for BufferPool {
    fn default() -> Self {
        BufferPool(
            std::sync::Mutex::new(Buffers::new(
                "tapped on the GPU",
                wgpu::BufferUsages::COPY_DST
                    | wgpu::BufferUsages::COPY_SRC
                    | wgpu::BufferUsages::STORAGE,
                8,
            )),
            std::sync::Mutex::new(None),
        )
    }
}

// SAFETY: fields of the types checked above, and of std's Send + Sync types.
unsafe impl Send for BufferPool {}
// SAFETY: as above.
unsafe impl Sync for BufferPool {}
// SAFETY: as above.
unsafe impl Send for GpuHandIn {}
// SAFETY: as above.
unsafe impl Sync for GpuHandIn {}
// SAFETY: as above.
unsafe impl Send for GpuPicture {}
// SAFETY: as above.
unsafe impl Sync for GpuPicture {}
// SAFETY: as above (and the share server's handle, plain data).
unsafe impl Send for GpuRing {}
// SAFETY: as above.
unsafe impl Sync for GpuRing {}
// SAFETY: as above.
unsafe impl Send for GpuSlots {}
// SAFETY: as above.
unsafe impl Sync for GpuSlots {}

pub(super) const WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Waits for the GPU's work up to `sub` (or all of it).
pub(super) fn wait(device: &wgpu::Device, sub: Option<wgpu::SubmissionIndex>) -> io::Result<()> {
    device
        .poll(wgpu::PollType::Wait {
            submission_index: sub,
            timeout: Some(WAIT),
        })
        .map(|_| ())
        .map_err(|e| io::Error::other(format!("GPU: {e}")))
}

/// Maps `slice` of a buffer for reading once the work up to `sub` is done, and waits for
/// its callback, not only for the GPU: another thread polling the same device can be the
/// one that runs the callback, a moment after this thread's poll returned (seen once,
/// the buffer was mapped after the caller had given up and the next submit using it
/// failed the whole device). On a timeout the map is cancelled, so the buffer is
/// never left mapped.
pub(crate) fn map_read(
    device: &wgpu::Device,
    slice: wgpu::BufferSlice<'_>,
    sub: Option<wgpu::SubmissionIndex>,
) -> io::Result<()> {
    use std::sync::atomic::{AtomicU8, Ordering};
    // 0 waiting, 1 mapped, 2 failed.
    let state = std::sync::Arc::new(AtomicU8::new(0));
    {
        let state = state.clone();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            state.store(if r.is_ok() { 1 } else { 2 }, Ordering::Release)
        });
    }
    let end = std::time::Instant::now() + WAIT;
    loop {
        match state.load(Ordering::Acquire) {
            1 => return Ok(()),
            2 => return Err(io::Error::other("GPU: a buffer could not be mapped")),
            _ => {}
        }
        if std::time::Instant::now() >= end {
            slice.buffer().unmap();
            return Err(io::Error::other("GPU: a buffer wasn't mapped in time"));
        }
        let polled = device.poll(wgpu::PollType::Wait {
            submission_index: sub.clone(),
            timeout: Some(WAIT),
        });
        if polled.is_err() {
            // The callback may still be on its way from another thread's poll.
            std::thread::yield_now();
        }
    }
}

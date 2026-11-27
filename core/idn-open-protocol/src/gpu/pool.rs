//! What every program that works on the GPU needs alike, one copy for all of them:
//! waiting for wgpu's requests, opening a device, buffers handed out as pictures
//! and taken back, and staging buffers kept mapped for writing.

use super::GpuHandIn;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How long the GPU may take before it counts as stuck.
const WAIT: Duration = Duration::from_secs(2);

/// `f` polled to its end on this thread: wgpu's requests (an adapter, a device), which
/// finish by themselves on its native backends.
pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    let mut f = std::pin::pin!(f);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    loop {
        if let std::task::Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::yield_now();
    }
}

/// wgpu's instance as IDN's programs open it: on Windows, Direct3D 12 (unless
/// `WGPU_BACKEND` says) and its shaders compiled by DXC, `dxcompiler.dll` beside the
/// program (unless `WGPU_DX12_COMPILER` says). Without it wgpu takes FXC, which couldn't
/// compile the SpeedHQ encoder ("unable to unroll loop", 2026-11-05).
pub fn instance_descriptor() -> wgpu::InstanceDescriptor {
    #[allow(unused_mut)]
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
    #[cfg(windows)]
    {
        if std::env::var_os("WGPU_BACKEND").is_none() {
            desc.backends = wgpu::Backends::DX12;
        }
        if std::env::var_os("WGPU_DX12_COMPILER").is_none()
            && let Some(dll) = std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(|d| d.join("dxcompiler.dll")))
                .filter(|p| p.is_file())
        {
            desc.backend_options.dx12.shader_compiler = wgpu::Dx12Compiler::DynamicDxc {
                dxc_path: dll.to_string_lossy().into_owned(),
            };
        }
    }
    desc
}

/// A device on this machine's best GPU, with wgpu's default limits and features (and
/// shared memory readable by the GPU where it can: [`super::import_host`]): Direct3D
/// 12 on Windows (unless `WGPU_BACKEND` says), what shares pictures with the switcher, the
/// monitor and OBS. `None` without one, or with only a software one unless `software`.
pub fn open_device(label: &str, software: bool) -> Option<GpuHandIn> {
    let instance = wgpu::Instance::new(instance_descriptor());
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        ..Default::default()
    }))
    .ok()?;
    if adapter.get_info().device_type == wgpu::DeviceType::Cpu && !software {
        return None;
    }
    let (device, queue) = super::request_device(
        &adapter,
        &wgpu::DeviceDescriptor {
            label: Some(label),
            // Other programs' pictures imported where the GPU can (DMA-BUF on Linux: a
            // Wayland desktop's screen share, VA-API's surfaces).
            required_features: adapter.features() & wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF,
            ..Default::default()
        },
    )
    .ok()?;
    Some(GpuHandIn { device, queue })
}

/// Buffers handed out (as pictures, in `Arc`s) and taken back once nothing holds them: one
/// of the size asked for that is free, else a new one; `keep` of them kept for reuse, any
/// more (a receiver holding many) made and let go. A new size lets the others go.
pub struct Buffers {
    label: &'static str,
    usage: wgpu::BufferUsages,
    keep: usize,
    list: Vec<Arc<wgpu::Buffer>>,
}

impl Buffers {
    /// None yet; each made with `label` and `usage`.
    pub const fn new(label: &'static str, usage: wgpu::BufferUsages, keep: usize) -> Buffers {
        Buffers {
            label,
            usage,
            keep,
            list: Vec::new(),
        }
    }

    /// A buffer of `size` bytes no one else holds.
    pub fn get(&mut self, device: &wgpu::Device, size: u64) -> Arc<wgpu::Buffer> {
        self.list.retain(|b| b.size() == size);
        if let Some(b) = self.list.iter().find(|b| Arc::strong_count(b) == 1) {
            return b.clone();
        }
        let b = Arc::new(device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(self.label),
            size,
            usage: self.usage,
            mapped_at_creation: false,
        }));
        if self.list.len() < self.keep {
            self.list.push(b.clone());
        }
        b
    }
}

/// Staging buffers kept mapped for writing (`MAP_WRITE`), each mapped again once its copy
/// is done, up to three in flight: what goes up is written in place. Through the queue's
/// own staging each upload was a new allocation, its pages faulted in (2,500 a second for
/// a 1080×1920 picture).
pub struct Staging {
    label: &'static str,
    list: Vec<(wgpu::Buffer, Arc<AtomicBool>)>,
}

impl Staging {
    /// None yet; each made with `label`.
    pub const fn new(label: &'static str) -> Staging {
        Staging {
            label,
            list: Vec::new(),
        }
    }

    /// `size` bytes (a multiple of 4) written by `fill` into a staging buffer (it says whether it wrote
    /// them), then `encode` given an encoder and that buffer to record its copies (and any
    /// work after them), submitted: what is submitted later on the queue runs after it.
    /// `Ok(false)`: `fill` couldn't, nothing submitted.
    pub fn upload(
        &mut self,
        hand: &GpuHandIn,
        size: u64,
        fill: impl FnOnce(&mut wgpu::WriteOnly<'_, [u8]>) -> bool,
        encode: impl FnOnce(&mut wgpu::CommandEncoder, &wgpu::Buffer),
    ) -> io::Result<bool> {
        let d = &hand.device;
        // Buffers too small for this size are let go once free (sizes vary: a frame's
        // coefficients; a new buffer each frame was a zeroed allocation each frame).
        self.list
            .retain(|(b, ready)| b.size() >= size || !ready.load(Ordering::Acquire));
        // (The mappings' callbacks run as the device is polled.)
        let _ = d.poll(wgpu::PollType::Poll);
        let free = |list: &[(wgpu::Buffer, Arc<AtomicBool>)]| {
            list.iter()
                .position(|s| s.0.size() >= size && s.1.load(Ordering::Acquire))
        };
        let k = match free(&self.list) {
            Some(k) => k,
            None if self.list.len() < 3 => {
                self.list.push((
                    d.create_buffer(&wgpu::BufferDescriptor {
                        label: Some(self.label),
                        // With room to grow: the next somewhat larger fits too.
                        size: (size + size / 4).next_multiple_of(wgpu::COPY_BUFFER_ALIGNMENT),
                        usage: wgpu::BufferUsages::MAP_WRITE | wgpu::BufferUsages::COPY_SRC,
                        mapped_at_creation: true,
                    }),
                    Arc::new(AtomicBool::new(true)),
                ));
                self.list.len() - 1
            }
            None => {
                // All three in flight: the oldest's mapping waited for.
                d.poll(wgpu::PollType::Wait {
                    submission_index: None,
                    timeout: Some(WAIT),
                })
                .map_err(|e| io::Error::other(format!("GPU: {e}")))?;
                free(&self.list)
                    .ok_or_else(|| io::Error::other("GPU: no staging buffer came back"))?
            }
        };
        let (up, ready) = &self.list[k];
        {
            let mut view = up
                .slice(..size)
                .get_mapped_range_mut()
                .map_err(|e| io::Error::other(format!("GPU: {e}")))?;
            if !fill(&mut view.slice(..)) {
                return Ok(false);
            }
        }
        up.unmap();
        ready.store(false, Ordering::Release);
        let mut enc = d.create_command_encoder(&Default::default());
        encode(&mut enc, up);
        hand.queue.submit([enc.finish()]);
        let again = ready.clone();
        up.slice(..).map_async(wgpu::MapMode::Write, move |r| {
            if r.is_ok() {
                again.store(true, Ordering::Release);
            }
        });
        Ok(true)
    }
}


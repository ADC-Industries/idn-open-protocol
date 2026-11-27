//! Pictures on the GPU between programs on one machine (`gpu/1`,
//! spec/extensions/idn-extensions.md §E.27), with `machine/1` (§E.26) around it: an output
//! whose application hands it its GPU (a wgpu device, [`GpuHandIn`]) and its pictures as GPU
//! buffers keeps a ring of GPU memory other programs on this machine import; a receiver
//! that hands its own GPU to its input, on the same GPU, copies each picture GPU to GPU and
//! hands it on as a buffer. Nothing goes through the CPU's memory for them: at 2160p30 that
//! was about 1 GB/s each way over the memory a mini PC's GPU shares with its CPU.
//!
//! Linux, Vulkan: plain buffers (no images: no tiling or compression to agree on) of
//! exportable memory (`VK_KHR_external_memory_fd`, an opaque descriptor), handed over by
//! [`idn_common::shm::SharedFd`] as the CPU rings are. An importer must be on the same
//! device and driver (their UUIDs, [`device_id`]) and allocate with the exporter's memory
//! type and size. Elsewhere (Windows, macOS) nothing is offered for now and `machine/1`
//! carries the pictures.
//!
//! A slot's planes lie one after another, each row rounded up to 256 bytes (what a copy
//! into a texture needs), each plane starting on 256 bytes ([`slot_layout`]). The output
//! copies its picture into a slot with a small shader (its own rows may be packed
//! otherwise), waits for the GPU, then stamps the slot, as the CPU ring's stamps go.

use std::io;

mod copy;
#[cfg(target_os = "linux")]
mod external;
mod host;
mod picture;
mod pool;
mod ring;
mod scale;
mod share;
pub use copy::*;
pub use picture::*;
pub use ring::*;
pub use scale::*;
pub use share::*;
pub use host::{import_host, request_device};
pub use pool::{Buffers, Staging, block_on, instance_descriptor, open_device};
#[cfg(target_os = "linux")]
pub use external::{dmabuf_modifiers, exportable_buffer};
/// The Vulkan bindings wgpu uses (for adapters that reach Vulkan directly: FFmpeg's
/// Vulkan device, external memory).
pub use ash;

/// Slots of the GPU ring: the picture being copied out, the next, and room.
pub const GPU_SLOTS: usize = 3;
/// A slot's rows and planes are aligned to this (wgpu's `COPY_BYTES_PER_ROW_ALIGNMENT`).
pub const ROW_ALIGN: u64 = 256;
/// The largest slot made or imported (8K 4:4:4 with alpha, as the CPU ring's).
pub const MAX_SLOT_BYTES: u64 = 8192 * 8192 * 4 + (1 << 20);

fn unsupported(m: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, m.to_owned())
}

/// The GPU an application hands to an output or input: its wgpu device and queue (cheap,
/// reference-counted handles). Pictures then go GPU to GPU between programs on this
/// machine that are on the same GPU (§E.27).
#[derive(Clone, Debug)]
pub struct GpuHandIn {
    /// The device the application draws or computes with.
    pub device: wgpu::Device,
    /// Its queue.
    pub queue: wgpu::Queue,
}

/// Where a plane lies in a buffer: its first byte, the bytes from one row to the next, and
/// its size in samples (one byte each).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaneAt {
    /// Its first byte in the buffer.
    pub offset: u64,
    /// Bytes from the start of one row to the next.
    pub row_bytes: u32,
    /// Samples a row.
    pub width: u32,
    /// Rows.
    pub height: u32,
}

/// The planes of `sizes` (width, height) laid out in a slot: rows and starts on
/// [`ROW_ALIGN`]; and the bytes they take.
pub fn slot_layout(sizes: &[(u32, u32)]) -> (Vec<PlaneAt>, u64) {
    let mut at = 0u64;
    let planes = sizes
        .iter()
        .map(|&(width, height)| {
            let row_bytes = (width as u64).next_multiple_of(ROW_ALIGN);
            let p = PlaneAt {
                offset: at,
                row_bytes: row_bytes as u32,
                width,
                height,
            };
            at = (at + row_bytes * height as u64).next_multiple_of(ROW_ALIGN);
            p
        })
        .collect();
    (planes, at.max(ROW_ALIGN))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// Two devices on this machine's GPU (as two programs would have), or none: the test
    /// then passes saying so.
    fn two_devices() -> Option<(GpuHandIn, GpuHandIn)> {
        let mut d = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        d.backends = wgpu::Backends::VULKAN;
        let instance = wgpu::Instance::new(d);
        let adapter = block_on(instance.request_adapter(&Default::default())).ok()?;
        if adapter.get_info().device_type == wgpu::DeviceType::Cpu {
            return None;
        }
        let dev = || {
            let (device, queue) = block_on(adapter.request_device(&Default::default())).ok()?;
            Some(GpuHandIn { device, queue })
        };
        Some((dev()?, dev()?))
    }

    #[test]
    fn slots_lie_on_256_bytes() {
        let (p, bytes) = slot_layout(&[(1920, 1080), (960, 1080), (960, 1080)]);
        assert_eq!(p[0].row_bytes, 2048);
        assert_eq!(p[1].offset, 2048 * 1080);
        assert_eq!(p[1].row_bytes, 1024);
        assert_eq!(p[2].offset, 2048 * 1080 + 1024 * 1080);
        assert_eq!(bytes, 2048 * 1080 + 2 * 1024 * 1080);
        assert!(p.iter().all(|p| p.offset % ROW_ALIGN == 0));
    }

    /// A picture in the switcher's layout (rows padded to 8 samples) goes into a slot on one
    /// device, and comes out, copied GPU to GPU, on another that imported the slot.
    #[test]
    fn a_picture_goes_from_one_device_to_another() {
        let Some((a, b)) = two_devices() else {
            eprintln!("no Vulkan GPU here: not tested");
            return;
        };
        if !vk_share::can_share(&a.device) {
            eprintln!("this GPU can't share memory: not tested");
            return;
        }
        assert_eq!(device_id(&a.device), device_id(&b.device));
        // 100 × 6, 4:2:2: source rows of 104 and 52 bytes, planes one after another.
        let (w, h) = (100u32, 6u32);
        let sizes = [(w, h), (w / 2, h), (w / 2, h)];
        let rows = [104u32, 52, 52];
        let mut from = Vec::new();
        let mut data = Vec::new();
        for (k, (&(pw, ph), &r)) in sizes.iter().zip(&rows).enumerate() {
            from.push(PlaneAt {
                offset: data.len() as u64,
                row_bytes: r,
                width: pw,
                height: ph,
            });
            for y in 0..ph {
                for x in 0..r {
                    data.push((k as u32 * 70 + y * 7 + x) as u8);
                }
            }
        }
        let src = a.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: data.len().next_multiple_of(4) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        a.queue.write_buffer(&src, 0, &data);
        let (to, need) = slot_layout(&sizes);
        let ring = GpuRing::create(&a, need).expect("a ring");
        ring.write(1, &src, &from, &to).expect("written");
        let offer = crate::xml::parse(&format!("<x_idn_machine gen=\"1\"{}/>", ring.offer_attrs()))
            .expect("an offer");
        let slots = GpuSlots::open(&b, |k| offer.attr(k))
            .expect("opened")
            .expect("the same GPU");
        let pic = slots.copy_out(1, to.clone()).expect("copied");
        let planes = pic.read_planes(&b).expect("read back");
        for (k, p) in planes.iter().enumerate() {
            let (pw, ph) = sizes[k];
            assert_eq!(p.len(), (pw * ph) as usize);
            for y in 0..ph {
                for x in 0..pw {
                    assert_eq!(
                        p[(y * pw + x) as usize],
                        (k as u32 * 70 + y * 7 + x) as u8,
                        "plane {k} at {x},{y}"
                    );
                }
            }
        }
        // Another GPU's (or a forged) id: not taken.
        let id = device_id(&a.device).unwrap();
        let forged = format!("<x_idn_machine gen=\"1\"{}/>", ring.offer_attrs())
            .replace(&id, &"0".repeat(64));
        let other = crate::xml::parse(&forged).unwrap();
        assert!(GpuSlots::open(&b, |k| other.attr(k)).unwrap().is_none());
    }
}

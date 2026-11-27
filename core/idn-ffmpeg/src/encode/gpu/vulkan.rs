//! A picture on the application's GPU handed to a Vulkan Video encoder (FFmpeg's
//! `*_vulkan`) without coming to this memory (route 2):
//! drawn as NV12 into a buffer of the application's Vulkan device whose memory FFmpeg's
//! Vulkan device imports (an opaque descriptor; the same GPU), then copied there into
//! the encoder's input image on one of FFmpeg's own queues, with its frame's timeline
//! semaphore, layout and locks kept as FFmpeg's own uploads keep them. (FFmpeg's encoder
//! images can't be exported as DMA-BUF to be drawn into, as VA-API's surfaces are.)
//!
//! FFmpeg's Vulkan structures are declared here as FFmpeg 8 (libavutil 60) lays them
//! out; with another libavutil this writer isn't used (the picture is read back).

use super::nv12::{Nv12Draw, nv12_pitch, nv12_size};
use crate::ffi;
use crate::gpu_convert::err;
use ffmpeg_sys_next as ff;
use idn_open_protocol::gpu::ash::{self, vk};
use idn_open_protocol::gpu::{GpuHandIn, GpuPicture};
use std::ffi::{c_char, c_int, c_void};
use std::io;
use std::os::fd::IntoRawFd;

#[repr(C)]
struct QueueFamily {
    idx: c_int,
    num: c_int,
    flags: vk::QueueFlags,
    video_caps: vk::VideoCodecOperationFlagsKHR,
}

/// `AVVulkanDeviceContext` (libavutil 60: the deprecated fixed queues and queue locks
/// still in it).
#[repr(C)]
struct DeviceCtx {
    alloc: *const c_void,
    get_proc_addr: vk::PFN_vkGetInstanceProcAddr,
    inst: vk::Instance,
    phys_dev: vk::PhysicalDevice,
    act_dev: vk::Device,
    device_features: vk::PhysicalDeviceFeatures2<'static>,
    enabled_inst_extensions: *const *const c_char,
    nb_enabled_inst_extensions: c_int,
    enabled_dev_extensions: *const *const c_char,
    nb_enabled_dev_extensions: c_int,
    fixed_queues: [c_int; 10],
    lock_queue: Option<unsafe extern "C" fn(*mut ff::AVHWDeviceContext, u32, u32)>,
    unlock_queue: Option<unsafe extern "C" fn(*mut ff::AVHWDeviceContext, u32, u32)>,
    qf: [QueueFamily; 64],
    nb_qf: c_int,
}

/// `AVVkFrame`.
#[repr(C)]
struct VkFrame {
    img: [vk::Image; 8],
    tiling: vk::ImageTiling,
    mem: [vk::DeviceMemory; 8],
    size: [usize; 8],
    flags: vk::MemoryPropertyFlags,
    access: [vk::AccessFlags; 8],
    layout: [vk::ImageLayout; 8],
    sem: [vk::Semaphore; 8],
    sem_value: [u64; 8],
    internal: *mut c_void,
    offset: [isize; 8],
    queue_family: [u32; 8],
}

/// `AVVulkanFramesContext`.
#[repr(C)]
struct FramesCtx {
    tiling: vk::ImageTiling,
    usage: vk::ImageUsageFlags,
    create_pnext: *mut c_void,
    alloc_pnext: [*mut c_void; 8],
    flags: c_int,
    img_flags: vk::ImageCreateFlags,
    format: [vk::Format; 8],
    nb_layers: c_int,
    lock_frame: Option<unsafe extern "C" fn(*mut ff::AVHWFramesContext, *mut VkFrame)>,
    unlock_frame: Option<unsafe extern "C" fn(*mut ff::AVHWFramesContext, *mut VkFrame)>,
}

/// Whether FFmpeg's Vulkan structures are laid out as declared here.
pub(crate) fn layout_known() -> bool {
    // SAFETY: a plain version query.
    unsafe { ff::avutil_version() >> 16 == 60 }
}

/// The NV12 buffer, ours and as FFmpeg's device sees it, with what copies from it there.
struct Shared {
    key: (u32, u32, u32, usize),
    ours: wgpu::Buffer,
    dev: ash::Device,
    buf: vk::Buffer,
    mem: vk::DeviceMemory,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    queue: vk::Queue,
    family: u32,
}

impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: objects `shared` made on `dev`, destroyed once, after the device is idle.
        unsafe {
            let _ = self.dev.device_wait_idle();
            self.dev.destroy_fence(self.fence, None);
            self.dev.destroy_command_pool(self.pool, None);
            self.dev.destroy_buffer(self.buf, None);
            self.dev.free_memory(self.mem, None);
        }
    }
}

pub(crate) struct VkWriter {
    hand: GpuHandIn,
    draw: Nv12Draw,
    shared: Option<Shared>,
}

// SAFETY: wgpu's types are Send and Sync; the Vulkan handles are used only by the
// writer's owner (the encoder), FFmpeg's queue and frame locked while they are.
unsafe impl Send for VkWriter {}

impl VkWriter {
    pub(crate) fn new(hand: &GpuHandIn) -> io::Result<VkWriter> {
        if !layout_known() {
            return Err(err(
                "this FFmpeg's Vulkan structures aren't the ones known here",
            ));
        }
        Ok(VkWriter {
            hand: hand.clone(),
            draw: Nv12Draw::new(hand),
            shared: None,
        })
    }

    /// The buffer for `key` on FFmpeg's device `dc` (its `AVHWDeviceContext`), made if
    /// new: ours, exported, imported there, and a command buffer on a queue that copies.
    unsafe fn shared(
        &mut self,
        key: (u32, u32, u32, usize),
        dc: *mut ff::AVHWDeviceContext,
    ) -> io::Result<&Shared> {
        if self.shared.as_ref().is_some_and(|s| s.key == key) {
            return Ok(self.shared.as_ref().expect("checked"));
        }
        self.shared = None;
        let (_, dh, pitch, _) = key;
        let size = nv12_size(dh, pitch);
        let (ours, fd, alloc) = idn_open_protocol::gpu::exportable_buffer(&self.hand, size)?;
        let e = |m: String| err(format!("FFmpeg's Vulkan device: {m}"));
        // SAFETY: `dc` is FFmpeg's live Vulkan device context (the caller's contract), its `hwctx`
        // laid out as `DeviceCtx` for libavutil 60 (checked in `new`); each object made here is
        // destroyed on the failure paths after it.
        unsafe {
            let hw = &*((*dc).hwctx as *const DeviceCtx);
            let entry = ash::StaticFn {
                get_instance_proc_addr: hw.get_proc_addr,
            };
            let instance = ash::Instance::load(&entry, hw.inst);
            // FFmpeg's device must be the application's GPU (and driver): the memory is
            // that GPU's, and imported on another (two cards of one name: FFmpeg picks
            // its device by name) it is undefined. Otherwise read back.
            let theirs = {
                let mut id = vk::PhysicalDeviceIDProperties::default();
                let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
                instance.get_physical_device_properties2(hw.phys_dev, &mut p2);
                idn_common::hex::hex(&[id.device_uuid, id.driver_uuid].concat())
            };
            if idn_open_protocol::gpu::device_id(&self.hand.device).as_deref() != Some(&theirs) {
                return Err(e("not the application's GPU".into()));
            }
            let dev = ash::Device::load(instance.fp_v1_0(), hw.act_dev);
            // A queue that copies (transfer, else graphics or compute), not a video one.
            let fam = hw.qf[..hw.nb_qf.clamp(0, 64) as usize]
                .iter()
                .filter(|q| q.num > 0 && q.video_caps.is_empty())
                .find(|q| q.flags.contains(vk::QueueFlags::TRANSFER))
                .or_else(|| {
                    hw.qf[..hw.nb_qf.clamp(0, 64) as usize].iter().find(|q| {
                        q.num > 0
                            && q.flags
                                .intersects(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE)
                    })
                })
                .map(|q| q.idx as u32)
                .ok_or_else(|| e("no queue that copies".into()))?;
            let mut ext = vk::ExternalMemoryBufferCreateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
            let buf = dev
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(size)
                        .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                        .sharing_mode(vk::SharingMode::EXCLUSIVE)
                        .push_next(&mut ext),
                    None,
                )
                .map_err(|r| e(r.to_string()))?;
            let req = dev.get_buffer_memory_requirements(buf);
            let props = instance.get_physical_device_memory_properties(hw.phys_dev);
            let Some(kind) = (0..props.memory_type_count).find(|&i| {
                req.memory_type_bits & (1 << i) != 0
                    && props.memory_types[i as usize]
                        .property_flags
                        .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
            }) else {
                dev.destroy_buffer(buf, None);
                return Err(e("no memory type to import into".into()));
            };
            let raw = fd.into_raw_fd();
            let mut import = vk::ImportMemoryFdInfoKHR::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD)
                .fd(raw);
            let mem = match dev.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(alloc)
                    .memory_type_index(kind)
                    .push_next(&mut import),
                None,
            ) {
                Ok(m) => m,
                Err(r) => {
                    // (The import takes the descriptor only when it works.)
                    libc::close(raw);
                    dev.destroy_buffer(buf, None);
                    return Err(e(format!("importing our memory ({r})")));
                }
            };
            let fail = |m: String| {
                dev.destroy_buffer(buf, None);
                dev.free_memory(mem, None);
                e(m)
            };
            dev.bind_buffer_memory(buf, mem, 0)
                .map_err(|r| fail(r.to_string()))?;
            let pool = dev
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .queue_family_index(fam)
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                    None,
                )
                .map_err(|r| fail(r.to_string()))?;
            let cmd = match dev.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            ) {
                Ok(c) => c[0],
                Err(r) => {
                    dev.destroy_command_pool(pool, None);
                    return Err(fail(r.to_string()));
                }
            };
            let fence = match dev.create_fence(&vk::FenceCreateInfo::default(), None) {
                Ok(f) => f,
                Err(r) => {
                    dev.destroy_command_pool(pool, None);
                    return Err(fail(r.to_string()));
                }
            };
            let queue = dev.get_device_queue(fam, 0);
            self.shared = Some(Shared {
                key,
                ours,
                dev,
                buf,
                mem,
                pool,
                cmd,
                fence,
                queue,
                family: fam,
            });
        }
        Ok(self.shared.as_ref().expect("made"))
    }

    /// `pic` into the encoder's Vulkan frame `hw` (NV12) of `size`, both GPUs done with
    /// it on return.
    pub(crate) fn write(
        &mut self,
        hw: &ffi::Frame,
        pic: &GpuPicture,
        (dw, dh): (usize, usize),
    ) -> io::Result<()> {
        let (dw, dh) = (dw as u32, dh as u32);
        let pitch = nv12_pitch(dw);
        // SAFETY: FFmpeg's frame and its contexts, live while `hw` is; FFmpeg's Vulkan
        // objects used with its queue and frame locked, and its frame's state kept up to
        // date for its encoder (layout, access, semaphore value).
        unsafe {
            let f = hw.0;
            let frames = (*f).hw_frames_ctx;
            if frames.is_null() {
                return Err(err("a frame without its frames"));
            }
            let fc = (*frames).data as *mut ff::AVHWFramesContext;
            let dc = (*fc).device_ctx;
            if (*dc).type_ != ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN {
                return Err(err("not a Vulkan frame"));
            }
            let s = self.shared((dw, dh, pitch, dc as usize), dc)?;
            let (dev, cmd, family, queue, fence, buf) =
                (s.dev.clone(), s.cmd, s.family, s.queue, s.fence, s.buf);
            self.draw.draw(
                pic,
                &self.shared.as_ref().expect("made").ours,
                (dw, dh),
                pitch,
            )?;
            let vkf = (*f).data[0] as *mut VkFrame;
            let vfc = &*((*fc).hwctx as *const FramesCtx);
            let hwd = &*((*dc).hwctx as *const DeviceCtx);
            if let Some(lock) = vfc.lock_frame {
                lock(fc, vkf);
            }
            let images: Vec<usize> = (0..8)
                .take_while(|&i| (*vkf).img[i] != vk::Image::null())
                .collect();
            let result = (|| -> io::Result<()> {
                let e = |r: vk::Result| err(format!("copying into the encoder's image ({r})"));
                dev.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                    .map_err(e)?;
                dev.begin_command_buffer(
                    cmd,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(e)?;
                let barriers: Vec<vk::ImageMemoryBarrier> = images
                    .iter()
                    .map(|&i| {
                        vk::ImageMemoryBarrier::default()
                            .src_access_mask((*vkf).access[i])
                            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                            .old_layout((*vkf).layout[i])
                            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                            .image((*vkf).img[i])
                            .subresource_range(vk::ImageSubresourceRange {
                                aspect_mask: vk::ImageAspectFlags::COLOR,
                                base_mip_level: 0,
                                level_count: 1,
                                base_array_layer: 0,
                                layer_count: 1,
                            })
                    })
                    .collect();
                dev.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::ALL_COMMANDS,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &barriers,
                );
                // Y, then CbCr: two planes of one image, or two images.
                let region = |offset: u64, row_texels: u32, aspect, w: u32, h: u32| {
                    vk::BufferImageCopy::default()
                        .buffer_offset(offset)
                        .buffer_row_length(row_texels)
                        .buffer_image_height(0)
                        .image_subresource(vk::ImageSubresourceLayers {
                            aspect_mask: aspect,
                            mip_level: 0,
                            base_array_layer: 0,
                            layer_count: 1,
                        })
                        .image_extent(vk::Extent3D {
                            width: w,
                            height: h,
                            depth: 1,
                        })
                };
                let cy = u64::from(pitch) * u64::from(dh);
                let (cw, ch) = (dw.div_ceil(2), dh.div_ceil(2));
                if images.len() >= 2 {
                    dev.cmd_copy_buffer_to_image(
                        cmd,
                        buf,
                        (*vkf).img[0],
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[region(0, pitch, vk::ImageAspectFlags::COLOR, dw, dh)],
                    );
                    dev.cmd_copy_buffer_to_image(
                        cmd,
                        buf,
                        (*vkf).img[1],
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[region(cy, pitch / 2, vk::ImageAspectFlags::COLOR, cw, ch)],
                    );
                } else {
                    dev.cmd_copy_buffer_to_image(
                        cmd,
                        buf,
                        (*vkf).img[0],
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[
                            region(0, pitch, vk::ImageAspectFlags::PLANE_0, dw, dh),
                            region(cy, pitch / 2, vk::ImageAspectFlags::PLANE_1, cw, ch),
                        ],
                    );
                }
                dev.end_command_buffer(cmd).map_err(e)?;
                // After what FFmpeg did with the frame (its semaphore's value), and its
                // encoder after this (the value one on).
                let sems: Vec<vk::Semaphore> = images.iter().map(|&i| (*vkf).sem[i]).collect();
                let waits: Vec<u64> = images.iter().map(|&i| (*vkf).sem_value[i]).collect();
                let signals: Vec<u64> = waits.iter().map(|v| v + 1).collect();
                let stages = vec![vk::PipelineStageFlags::TRANSFER; sems.len()];
                let mut timeline = vk::TimelineSemaphoreSubmitInfo::default()
                    .wait_semaphore_values(&waits)
                    .signal_semaphore_values(&signals);
                let cmds = [cmd];
                let submit = vk::SubmitInfo::default()
                    .wait_semaphores(&sems)
                    .wait_dst_stage_mask(&stages)
                    .command_buffers(&cmds)
                    .signal_semaphores(&sems)
                    .push_next(&mut timeline);
                let dcm = dc;
                if let Some(lock) = hwd.lock_queue {
                    lock(dcm, family, 0);
                }
                let r = dev.queue_submit(queue, &[submit], fence);
                if let Some(unlock) = hwd.unlock_queue {
                    unlock(dcm, family, 0);
                }
                r.map_err(e)?;
                // Submitted: the frame's semaphores will reach the values one on, and its
                // layout is ours, whatever the wait below says. Said to FFmpeg now: after
                // a timeout (the writer then retired) its next use of the frame would
                // wait for the old value and signal one the submit also signals.
                for &i in &images {
                    (*vkf).layout[i] = vk::ImageLayout::TRANSFER_DST_OPTIMAL;
                    (*vkf).access[i] = vk::AccessFlags::TRANSFER_WRITE;
                    (*vkf).sem_value[i] += 1;
                }
                dev.wait_for_fences(&[fence], true, 2_000_000_000)
                    .map_err(e)?;
                dev.reset_fences(&[fence]).map_err(e)?;
                Ok(())
            })();
            if let Some(unlock) = vfc.unlock_frame {
                unlock(fc, vkf);
            }
            result
        }
    }
}

//! Pictures on the application's GPU put into an encoder's input without the CPU's
//! memory (§E.27): VA-API's surfaces and Vulkan Video's images (Linux), NVENC's CUDA
//! frames (Linux), Direct3D 12's textures (Windows); the writer chosen by the encoder's
//! frames.

use crate::ffi;
use std::io;

#[cfg(target_os = "linux")]
mod cuda;
#[cfg(windows)]
mod d3d12;
#[cfg(target_os = "linux")]
mod nv12;
#[cfg(target_os = "linux")]
mod vaapi;
#[cfg(target_os = "linux")]
mod vulkan;

/// Pictures on the application's GPU put into an encoder's input there: VA-API's
/// surfaces drawn into (Linux), Direct3D 12's textures copied into (Windows).
// One per encoder: its size doesn't matter.
#[allow(clippy::large_enum_variant)]
pub(crate) enum GpuWriter {
    /// VA-API surfaces and Vulkan Video's images (both mapped as DMA-BUF).
    #[cfg(target_os = "linux")]
    Va(vaapi::VaWriter),
    /// NVENC's CUDA frames (through CUDA's external memory).
    #[cfg(target_os = "linux")]
    Cuda(cuda::CudaWriter),
    /// Vulkan Video's frames (through FFmpeg's own Vulkan device).
    #[cfg(target_os = "linux")]
    Vk(vulkan::VkWriter),
    #[cfg(windows)]
    D3d12(d3d12::D3d12Writer),
}

impl GpuWriter {
    /// The writer for the encoder's frames like `hw`.
    pub(crate) fn new(
        hand: &idn_open_protocol::gpu::GpuHandIn,
        #[cfg_attr(windows, allow(unused_variables))] hw: &ffi::Frame,
    ) -> io::Result<GpuWriter> {
        #[cfg(target_os = "linux")]
        {
            use ffmpeg_sys_next::AVHWDeviceType as T;
            // SAFETY: reading the frame's frames context (FFmpeg's, held by the frame).
            let kind = unsafe {
                let frames = (*hw.0).hw_frames_ctx;
                (!frames.is_null()).then(|| {
                    (*(*((*frames).data as *const ffmpeg_sys_next::AVHWFramesContext)).device_ctx).type_
                })
            };
            if kind == Some(T::AV_HWDEVICE_TYPE_CUDA) {
                return cuda::CudaWriter::new(hand).map(GpuWriter::Cuda);
            }
            if kind == Some(T::AV_HWDEVICE_TYPE_VULKAN) {
                return vulkan::VkWriter::new(hand).map(GpuWriter::Vk);
            }
            vaapi::VaWriter::new(hand).map(GpuWriter::Va)
        }
        #[cfg(windows)]
        return d3d12::D3d12Writer::new(hand).map(GpuWriter::D3d12);
    }

    pub(crate) fn write(
        &mut self,
        hw: &ffi::Frame,
        pic: &idn_open_protocol::gpu::GpuPicture,
        size: (usize, usize),
    ) -> io::Result<()> {
        match self {
            #[cfg(target_os = "linux")]
            GpuWriter::Va(w) => w.write(hw, pic, size),
            #[cfg(target_os = "linux")]
            GpuWriter::Cuda(w) => w.write(hw, pic, size),
            #[cfg(target_os = "linux")]
            GpuWriter::Vk(w) => w.write(hw, pic, size),
            #[cfg(windows)]
            GpuWriter::D3d12(w) => w.write(hw, pic, size),
        }
    }
}


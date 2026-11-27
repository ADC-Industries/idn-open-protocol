//! Hardware devices: VA-API's, Vulkan's (Vulkan Video), CUDA's (NVENC fed CUDA frames), and the GPU that decodes.

use super::*;

/// Opens a VA-API device (a DRM render node).
pub fn vaapi_device(path: &str) -> io::Result<BufferRef> {
    let p = CString::new(path).map_err(io::Error::other)?;
    let mut dev = ptr::null_mut();
    check(
        // SAFETY: an out pointer of ours and a NUL-terminated path; the result is checked.
        unsafe {
            ff::av_hwdevice_ctx_create(
                &mut dev,
                ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                p.as_ptr(),
                ptr::null_mut(),
                0,
            )
        },
        &format!("VA-API device {path}"),
    )?;
    Ok(BufferRef(dev))
}

/// A Vulkan device for FFmpeg's Vulkan Video encoders: the GPU whose name contains
/// `name` (the application's, so that pictures are shared on it), else the first.
#[cfg(target_os = "linux")]
pub fn vulkan_device(name: Option<&str>) -> io::Result<BufferRef> {
    let n = name
        .map(|n| CString::new(n).map_err(io::Error::other))
        .transpose()?;
    let mut dev = ptr::null_mut();
    check(
        // SAFETY: an out pointer of ours and a NUL-terminated name or null; the result is checked.
        unsafe {
            ff::av_hwdevice_ctx_create(
                &mut dev,
                ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN,
                n.as_ref().map_or(ptr::null(), |n| n.as_ptr()),
                ptr::null_mut(),
                0,
            )
        },
        "Vulkan device",
    )?;
    Ok(BufferRef(dev))
}

/// A CUDA device (NVIDIA), for NVENC fed CUDA frames.
#[cfg(target_os = "linux")]
pub fn cuda_device() -> io::Result<BufferRef> {
    let mut dev = ptr::null_mut();
    check(
        // SAFETY: an out pointer of ours; the result is checked.
        unsafe {
            ff::av_hwdevice_ctx_create(
                &mut dev,
                ff::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
                ptr::null(),
                ptr::null_mut(),
                0,
            )
        },
        "CUDA device",
    )?;
    Ok(BufferRef(dev))
}

/// The type of a hardware device or frames' device.
pub(crate) fn device_type(dev: &BufferRef) -> ff::AVHWDeviceType {
    // SAFETY: `dev` is a device context (callers pass an encoder's device), whose data is
    // an AVHWDeviceContext.
    unsafe { (*((*dev.0).data as *const ff::AVHWDeviceContext)).type_ }
}

/// A GPU that decodes: its device, the pixel format of its pictures, and a name for logs.
pub struct HwDecode {
    pub dev: BufferRef,
    pub fmt: ff::AVPixelFormat,
    pub name: &'static str,
}

// SAFETY: a BufferRef (Send and Sync) and plain values.
unsafe impl Sync for HwDecode {}

impl HwDecode {
    /// VA-API (AMD and Intel on Linux) on an open device.
    pub fn vaapi(dev: &BufferRef) -> Self {
        HwDecode {
            dev: BufferRef(dev.new_ref()),
            fmt: ff::AVPixelFormat::AV_PIX_FMT_VAAPI,
            name: "VA-API",
        }
    }

    /// NVIDIA's decoder on an open CUDA device (the encoders', one context for both).
    pub fn cuda_on(dev: &BufferRef) -> Self {
        HwDecode {
            dev: BufferRef(dev.new_ref()),
            fmt: ff::AVPixelFormat::AV_PIX_FMT_CUDA,
            name: "NVDEC",
        }
    }

    /// NVIDIA's decoder (NVDEC, through CUDA), Linux and Windows.
    pub fn cuda() -> io::Result<Self> {
        Self::create(
            ff::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
            ff::AVPixelFormat::AV_PIX_FMT_CUDA,
            "NVDEC",
        )
    }

    /// Direct3D 11 video decoding: any GPU on Windows (Intel, AMD, NVIDIA).
    pub fn d3d11va() -> io::Result<Self> {
        Self::create(
            ff::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
            ff::AVPixelFormat::AV_PIX_FMT_D3D11,
            "D3D11VA",
        )
    }

    /// Direct3D 12 video decoding on `device`, the application's own (§E.27): its
    /// pictures are then on the device the application draws with.
    #[cfg(all(windows, feature = "gpu"))]
    pub fn d3d12_on(
        device: windows::Win32::Graphics::Direct3D12::ID3D12Device,
    ) -> io::Result<Self> {
        use windows::core::Interface;
        // SAFETY: a D3D12VA device context of ours; its `hwctx` starts with the ID3D12Device
        // pointer, a reference handed over (`into_raw`) that FFmpeg releases with it.
        unsafe {
            let dev = ff::av_hwdevice_ctx_alloc(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D12VA);
            if dev.is_null() {
                return Err(io::Error::other("D3D12VA device: this FFmpeg has none"));
            }
            let dev = BufferRef(dev);
            // `AVD3D12VADeviceContext::device`, its first field: a reference handed over
            // (FFmpeg releases it with the context).
            let hw = (*dev.0).data as *mut ff::AVHWDeviceContext;
            *((*hw).hwctx as *mut *mut std::ffi::c_void) = device.into_raw();
            check(ff::av_hwdevice_ctx_init(dev.0), "D3D12VA device")?;
            Ok(HwDecode {
                dev,
                fmt: ff::AVPixelFormat::AV_PIX_FMT_D3D12,
                name: "D3D12VA",
            })
        }
    }

    fn create(
        kind: ff::AVHWDeviceType,
        fmt: ff::AVPixelFormat,
        name: &'static str,
    ) -> io::Result<Self> {
        let mut dev = ptr::null_mut();
        check(
            // SAFETY: an out pointer of ours; the result is checked.
            unsafe { ff::av_hwdevice_ctx_create(&mut dev, kind, ptr::null(), ptr::null_mut(), 0) },
            &format!("{name} device"),
        )?;
        Ok(HwDecode {
            dev: BufferRef(dev),
            fmt,
            name,
        })
    }

    /// Sets a decoder up to decode on this GPU (before it is opened).
    pub(super) unsafe fn attach(&self, c: *mut ff::AVCodecContext) {
        // SAFETY: the caller's context, not open yet (this fn's contract); the device reference
        // is a new one the context owns.
        unsafe {
            (*c).hw_device_ctx = self.dev.new_ref();
            // The format the get_format callback picks, where it can find it.
            (*c).opaque = self.fmt as i32 as isize as *mut std::ffi::c_void;
            (*c).get_format = Some(pick_hw);
            // Surfaces held while helper threads download them (at most 2 × 6 on
            // their way) and by frame threads.
            (*c).extra_hw_frames = 16;
        }
    }
}

/// Whether a frame is in GPU memory (any kind).
pub(super) fn on_gpu(f: *const ff::AVFrame) -> bool {
    // SAFETY: callers pass a live frame of theirs (a Frame's or a FrameRef's).
    unsafe { !(*f).hw_frames_ctx.is_null() }
}

//! A picture on the application's GPU handed to NVENC on Linux without coming to this
//! memory (route 1): NVENC takes system memory or CUDA,
//! not Vulkan, so the picture is drawn as NV12 (scaled to the encoder's size, bilinear)
//! into a buffer of the application's Vulkan device whose memory CUDA imports (an opaque
//! descriptor), then copied on the GPU into the encoder's CUDA frame. libcuda is loaded
//! at run time (the NVIDIA driver's own library).

use super::nv12::{Nv12Draw, nv12_pitch, nv12_size};
use crate::ffi;
use crate::gpu_convert::err;
use ffmpeg_sys_next as ff;
use idn_open_protocol::gpu::{GpuHandIn, GpuPicture};
use std::ffi::{c_int, c_void};
use std::io;
use std::os::fd::IntoRawFd;

// ---- libcuda (the driver API), by hand: the few calls this needs.

type CuResult = c_int;
type CuDevicePtr = u64;
type CuContext = *mut c_void;
type CuExternalMemory = *mut c_void;

#[repr(C)]
struct ExtMemHandleDesc {
    kind: u32,
    _pad: u32,
    /// The union: an `int fd` (opaque descriptor) in its first bytes.
    handle: [u64; 2],
    size: u64,
    flags: u32,
    reserved: [u32; 16],
}

#[repr(C)]
struct ExtMemBufferDesc {
    offset: u64,
    size: u64,
    flags: u32,
    reserved: [u32; 16],
}

#[repr(C)]
struct Memcpy2d {
    src_x_bytes: usize,
    src_y: usize,
    src_memory_type: u32,
    src_host: *const c_void,
    src_device: CuDevicePtr,
    src_array: *mut c_void,
    src_pitch: usize,
    dst_x_bytes: usize,
    dst_y: usize,
    dst_memory_type: u32,
    dst_host: *mut c_void,
    dst_device: CuDevicePtr,
    dst_array: *mut c_void,
    dst_pitch: usize,
    width_bytes: usize,
    height: usize,
}

const EXT_MEM_OPAQUE_FD: u32 = 1;
const MEMORY_TYPE_DEVICE: u32 = 2;

struct Cuda {
    _lib: libloading::Library,
    import: unsafe extern "C" fn(*mut CuExternalMemory, *const ExtMemHandleDesc) -> CuResult,
    mapped: unsafe extern "C" fn(
        *mut CuDevicePtr,
        CuExternalMemory,
        *const ExtMemBufferDesc,
    ) -> CuResult,
    destroy: unsafe extern "C" fn(CuExternalMemory) -> CuResult,
    free: unsafe extern "C" fn(CuDevicePtr) -> CuResult,
    copy: unsafe extern "C" fn(*const Memcpy2d) -> CuResult,
    push: unsafe extern "C" fn(CuContext) -> CuResult,
    pop: unsafe extern "C" fn(*mut CuContext) -> CuResult,
    sync: unsafe extern "C" fn() -> CuResult,
    ctx_device: unsafe extern "C" fn(*mut i32) -> CuResult,
    uuid: unsafe extern "C" fn(*mut [u8; 16], i32) -> CuResult,
}

// SAFETY: function pointers into a library kept loaded with them.
unsafe impl Send for Cuda {}
unsafe impl Sync for Cuda {}

fn cuda() -> io::Result<&'static Cuda> {
    static CUDA: std::sync::OnceLock<Result<Cuda, String>> = std::sync::OnceLock::new();
    // SAFETY: the NVIDIA driver's libcuda; its entry points typed as cuda.h declares them,
    // the library kept loaded with the pointers.
    CUDA.get_or_init(|| unsafe {
        let lib = libloading::Library::new("libcuda.so.1").map_err(|e| format!("libcuda: {e}"))?;
        macro_rules! f {
            ($n:literal) => {
                *lib.get($n).map_err(|e| format!("libcuda: {e}"))?
            };
        }
        Ok(Cuda {
            import: f!(b"cuImportExternalMemory"),
            mapped: f!(b"cuExternalMemoryGetMappedBuffer"),
            destroy: f!(b"cuDestroyExternalMemory"),
            free: f!(b"cuMemFree_v2"),
            copy: f!(b"cuMemcpy2D_v2"),
            push: f!(b"cuCtxPushCurrent_v2"),
            pop: f!(b"cuCtxPopCurrent_v2"),
            sync: f!(b"cuCtxSynchronize"),
            ctx_device: f!(b"cuCtxGetDevice"),
            uuid: f!(b"cuDeviceGetUuid"),
            _lib: lib,
        })
    })
    .as_ref()
    .map_err(|e| io::Error::other(e.clone()))
}

fn cu(r: CuResult, what: &str) -> io::Result<()> {
    if r == 0 {
        Ok(())
    } else {
        Err(err(format!("CUDA {what}: error {r}")))
    }
}

/// A Vulkan buffer CUDA sees: the wgpu buffer, CUDA's handle on its memory and the
/// device pointer mapped from it.
struct Shared {
    buffer: wgpu::Buffer,
    mem: CuExternalMemory,
    ptr: CuDevicePtr,
    ctx: CuContext,
}

impl Drop for Shared {
    fn drop(&mut self) {
        if let Ok(c) = cuda() {
            // SAFETY: the mapped pointer and external memory this Shared imported, freed once with
            // their context pushed, then popped again.
            unsafe {
                (c.push)(self.ctx);
                (c.free)(self.ptr);
                (c.destroy)(self.mem);
                let mut old = std::ptr::null_mut();
                (c.pop)(&mut old);
            }
        }
    }
}

pub(crate) struct CudaWriter {
    hand: GpuHandIn,
    draw: Nv12Draw,
    /// The shared buffer, for this size and pitch (made again when they change).
    shared: Option<((u32, u32, u32), Shared)>,
}

// SAFETY: wgpu's types are Send and Sync; CUDA's handles are used only by the writer's
// owner (the encoder), with the context pushed.
unsafe impl Send for CudaWriter {}

impl CudaWriter {
    pub(crate) fn new(hand: &GpuHandIn) -> io::Result<CudaWriter> {
        cuda()?;
        Ok(CudaWriter {
            hand: hand.clone(),
            draw: Nv12Draw::new(hand),
            shared: None,
        })
    }

    /// The CUDA context of the encoder's frame `hw` (its frames' device's).
    fn context(hw: &ffi::Frame) -> io::Result<CuContext> {
        // SAFETY: `hw` is a live frame of the encoder's pool; its frames context is checked, and
        // the device checked to be CUDA before `hwctx` is read (`AVCUDADeviceContext`).
        unsafe {
            let frames = (*hw.0).hw_frames_ctx;
            if frames.is_null() {
                return Err(err("a frame without its frames"));
            }
            let fc = &*((*frames).data as *const ff::AVHWFramesContext);
            let dev = &*fc.device_ctx;
            if dev.type_ != ff::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA {
                return Err(err("not a CUDA frame"));
            }
            // `AVCUDADeviceContext`: its context first.
            Ok(*(dev.hwctx as *const CuContext))
        }
    }

    /// The buffer CUDA shares for `key` (width, height, pitch), made if new.
    fn shared(&mut self, key: (u32, u32, u32), ctx: CuContext) -> io::Result<&Shared> {
        if self
            .shared
            .as_ref()
            .is_none_or(|s| s.0 != key || s.1.ctx != ctx)
        {
            self.shared = None;
            let (_, dh, pitch) = key;
            let size = nv12_size(dh, pitch);
            let (buffer, fd, alloc) = idn_open_protocol::gpu::exportable_buffer(&self.hand, size)?;
            let c = cuda()?;
            // SAFETY: libcuda calls with the encoder's context pushed (popped on every path); the
            // descriptor is CUDA's once imported (closed here when not); sizes are the export's.
            unsafe {
                cu((c.push)(ctx), "context")?;
                // CUDA's device must be the application's GPU (the memory is that GPU's;
                // imported on another, it's undefined): its UUID is the Vulkan device
                // UUID, the first half of `device_id`. Otherwise read back.
                let mut dev = 0i32;
                let mut id = [0u8; 16];
                let same = (c.ctx_device)(&mut dev) == 0
                    && (c.uuid)(&mut id, dev) == 0
                    && idn_open_protocol::gpu::device_id(&self.hand.device)
                        .is_some_and(|d| d.get(..32) == Some(idn_common::hex::hex(&id).as_str()));
                if !same {
                    let mut old = std::ptr::null_mut();
                    (c.pop)(&mut old);
                    return Err(err("CUDA's device isn't the application's GPU"));
                }
                let fd = fd.into_raw_fd();
                let desc = ExtMemHandleDesc {
                    kind: EXT_MEM_OPAQUE_FD,
                    _pad: 0,
                    handle: [fd as u32 as u64, 0],
                    size: alloc,
                    flags: 0,
                    reserved: [0; 16],
                };
                let mut mem = std::ptr::null_mut();
                let r = (c.import)(&mut mem, &desc);
                if r != 0 {
                    // (CUDA takes the descriptor only when the import works.)
                    libc::close(fd);
                    let mut old = std::ptr::null_mut();
                    (c.pop)(&mut old);
                    return cu(r, "importing the Vulkan memory").map(|_| unreachable!());
                }
                let bd = ExtMemBufferDesc {
                    offset: 0,
                    size,
                    flags: 0,
                    reserved: [0; 16],
                };
                let mut ptr = 0;
                let r = (c.mapped)(&mut ptr, mem, &bd);
                let mut old = std::ptr::null_mut();
                if r != 0 {
                    (c.destroy)(mem);
                    (c.pop)(&mut old);
                    return cu(r, "mapping the Vulkan memory").map(|_| unreachable!());
                }
                (c.pop)(&mut old);
                self.shared = Some((
                    key,
                    Shared {
                        buffer,
                        mem,
                        ptr,
                        ctx,
                    },
                ));
            }
        }
        Ok(&self.shared.as_ref().expect("made").1)
    }

    /// `pic` drawn into the encoder's CUDA frame `hw` (NV12) of `size`, both GPUs done
    /// with it on return.
    pub(crate) fn write(
        &mut self,
        hw: &ffi::Frame,
        pic: &GpuPicture,
        (dw, dh): (usize, usize),
    ) -> io::Result<()> {
        let (dw, dh) = (dw as u32, dh as u32);
        let ctx = Self::context(hw)?;
        let pitch = nv12_pitch(dw);
        let ptr = self.shared((dw, dh, pitch), ctx)?.ptr;
        let out = &self.shared.as_ref().expect("made").1.buffer;
        self.draw.draw(pic, out, (dw, dh), pitch)?;
        // Into the encoder's frame, on the GPU: Y, then CbCr.
        let c = cuda()?;
        // SAFETY: `hw` is the encoder's live CUDA NV12 frame of dw × dh; the copies stay inside the
        // shared buffer (`nv12_size`) and the frame's planes, with the context pushed and popped.
        unsafe {
            let f = &*hw.0;
            cu((c.push)(ctx), "context")?;
            let plane = |src_off: u64, dst: CuDevicePtr, dst_pitch: usize, rows: u32| Memcpy2d {
                src_x_bytes: 0,
                src_y: 0,
                src_memory_type: MEMORY_TYPE_DEVICE,
                src_host: std::ptr::null(),
                src_device: ptr + src_off,
                src_array: std::ptr::null_mut(),
                src_pitch: pitch as usize,
                dst_x_bytes: 0,
                dst_y: 0,
                dst_memory_type: MEMORY_TYPE_DEVICE,
                dst_host: std::ptr::null_mut(),
                dst_device: dst,
                dst_array: std::ptr::null_mut(),
                dst_pitch,
                width_bytes: (dw.div_ceil(2) * 2) as usize,
                height: rows as usize,
            };
            let ycopy = plane(0, f.data[0] as CuDevicePtr, f.linesize[0] as usize, dh);
            let ccopy = plane(
                u64::from(pitch) * u64::from(dh),
                f.data[1] as CuDevicePtr,
                f.linesize[1] as usize,
                dh.div_ceil(2),
            );
            let r = (c.copy)(&ycopy);
            let r = if r == 0 { (c.copy)(&ccopy) } else { r };
            let r = if r == 0 { (c.sync)() } else { r };
            let mut old = std::ptr::null_mut();
            (c.pop)(&mut old);
            cu(r, "copying into the encoder's frame")?;
        }
        Ok(())
    }
}

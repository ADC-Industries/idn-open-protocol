//! libgbm, loaded at run time (its ABI from gbm.h, MIT): the capture's buffers on the
//! desktop's GPU, and its shared-memory ones.

use super::*;

pub(super) type Ptr = *mut c_void;

pub(super) struct Gbm {
    pub(super) _lib: libloading::Library,
    pub(super) create_device: unsafe extern "C" fn(c_int) -> Ptr,
    pub(super) device_destroy: unsafe extern "C" fn(Ptr),
    pub(super) bo_create_with_modifiers: unsafe extern "C" fn(Ptr, u32, u32, u32, *const u64, u32) -> Ptr,
    pub(super) bo_create_with_modifiers2:
        Option<unsafe extern "C" fn(Ptr, u32, u32, u32, *const u64, u32, u32) -> Ptr>,
    pub(super) bo_get_fd: unsafe extern "C" fn(Ptr) -> c_int,
    pub(super) bo_get_stride: unsafe extern "C" fn(Ptr) -> u32,
    pub(super) bo_get_offset: unsafe extern "C" fn(Ptr, c_int) -> u32,
    pub(super) bo_get_modifier: unsafe extern "C" fn(Ptr) -> u64,
    pub(super) bo_get_plane_count: unsafe extern "C" fn(Ptr) -> c_int,
    pub(super) bo_destroy: unsafe extern "C" fn(Ptr),
}

/// GBM_BO_USE_RENDERING: the desktop draws (or copies) into it.
pub(super) const GBM_BO_USE_RENDERING: u32 = 1 << 2;

pub(super) fn gbm() -> io::Result<&'static Gbm> {
    static LIB: OnceLock<Result<Gbm, String>> = OnceLock::new();
    // SAFETY: libgbm's entry points, typed as gbm.h declares them, kept loaded with the
    // pointers.
    LIB.get_or_init(|| unsafe {
        let l = libloading::Library::new("libgbm.so.1").map_err(|e| format!("libgbm: {e}"))?;
        macro_rules! f {
            ($n:literal) => {
                *l.get(concat!($n, "\0").as_bytes())
                    .map_err(|e| format!("libgbm: {}: {e}", $n))?
            };
        }
        Ok(Gbm {
            create_device: f!("gbm_create_device"),
            device_destroy: f!("gbm_device_destroy"),
            bo_create_with_modifiers: f!("gbm_bo_create_with_modifiers"),
            bo_create_with_modifiers2: l.get(b"gbm_bo_create_with_modifiers2\0").ok().map(|s| *s),
            bo_get_fd: f!("gbm_bo_get_fd"),
            bo_get_stride: f!("gbm_bo_get_stride"),
            bo_get_offset: f!("gbm_bo_get_offset"),
            bo_get_modifier: f!("gbm_bo_get_modifier"),
            bo_get_plane_count: f!("gbm_bo_get_plane_count"),
            bo_destroy: f!("gbm_bo_destroy"),
            _lib: l,
        })
    })
    .as_ref()
    .map_err(|e| io::Error::other(e.clone()))
}

/// A GBM device on the render node of the device `dev` (the desktop's, as it said).
pub(super) struct GbmDevice {
    pub(super) dev: Ptr,
    pub(super) _node: OwnedFd,
}

impl GbmDevice {
    pub(super) fn open(dev: u64) -> io::Result<GbmDevice> {
        let g = gbm()?;
        let (major, minor) = major_minor(dev);
        // The device's render node (it may have said its primary node).
        let dir = format!("/sys/dev/char/{major}:{minor}/device/drm");
        let render = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .find(|n| n.starts_with("renderD"))
            .ok_or_else(|| io::Error::other(format!("{dir}: no render node")))?;
        let path = CString::new(format!("/dev/dri/{render}")).map_err(io::Error::other)?;
        // SAFETY: a path we made; the descriptor is ours.
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: just opened, ours.
        let node = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: a render node's descriptor, open while the device lives (kept in it).
        let dev = unsafe { (g.create_device)(node.as_raw_fd()) };
        if dev.is_null() {
            return Err(io::Error::other("libgbm: no device on the render node"));
        }
        Ok(GbmDevice { dev, _node: node })
    }
}

impl Drop for GbmDevice {
    fn drop(&mut self) {
        if let Ok(g) = gbm() {
            // SAFETY: made by gbm_create_device; its buffers are gone first (dropped
            // before it, see Direct's field order).
            unsafe { (g.device_destroy)(self.dev) }
        }
    }
}

/// Where a buffer's picture is.
pub(super) enum Mem {
    /// On the GPU: a GBM buffer, its DMA-BUF, its one plane's place and layout.
    Dma {
        bo: Ptr,
        fd: OwnedFd,
        offset: u32,
        stride: u32,
        modifier: u64,
    },
    /// In shared memory, mapped here.
    Shm {
        map: *mut u8,
        len: usize,
        stride: u32,
    },
}

pub(super) struct Buf {
    pub(super) wl: WlBuffer,
    pub(super) mem: Mem,
    /// DRM format, size.
    pub(super) fourcc: u32,
    pub(super) w: u32,
    pub(super) h: u32,
}

impl Drop for Buf {
    fn drop(&mut self) {
        self.wl.destroy();
        match &self.mem {
            Mem::Dma { bo, .. } => {
                if let Ok(g) = gbm() {
                    // SAFETY: made by gbm_bo_create_with_modifiers*, destroyed once.
                    unsafe { (g.bo_destroy)(*bo) }
                }
            }
            Mem::Shm { map, len, .. } => {
                // SAFETY: our mapping of that length.
                unsafe { libc::munmap(map.cast(), *len) };
            }
        }
    }
}

//! A screen read straight from the compositor (plan A): the
//! staging Wayland protocol `ext-image-copy-capture-v1` with an output as its source
//! (`ext-output-image-capture-source-manager-v1`). COSMIC and wlroots desktops (Sway,
//! Hyprland) offer it to ordinary programs; GNOME and KDE don't, and the portal stays the
//! way there. No permission dialog and no restore token: a Node started over SSH or as a
//! service reads the screen as the tray would.
//!
//! The buffers are ours: on the GPU (GBM, in a layout this device imports, the driver
//! choosing among the ones both sides take: no guessing a tiling the desktop picked), or
//! in shared memory where that can't be done. A picture is asked for after the last was
//! handed on; the desktop answers when the screen has changed, and the last picture is
//! sent again each period meanwhile (a still slide is still a source).

// (Without the `gpu` feature, the GPU buffers' code has no caller.)
#![cfg_attr(not(feature = "gpu"), allow(dead_code, unused_imports))]

use idn_common::sync::LockOk as _;

use super::*;
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_output::{self, WlOutput},
    wl_registry::WlRegistry,
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self as frame_v1, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{self as manager_v1, ExtImageCopyCaptureManagerV1},
    ext_image_copy_capture_session_v1::{self as session_v1, ExtImageCopyCaptureSessionV1},
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};

// DRM's four-character codes for 8-bit packed RGB (drm_fourcc.h), little-endian words:
// XRGB8888 is B, G, R, X in memory.
const DRM_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
const DRM_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
const DRM_XBGR8888: u32 = u32::from_le_bytes(*b"XB24");
const DRM_ABGR8888: u32 = u32::from_le_bytes(*b"AB24");
/// The modifier meaning "none said" (implicit layout): never one to allocate with.
const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// SPA's name for a DRM format (what the rest of the screen code speaks).
pub(super) fn spa_of_drm(f: u32) -> Option<u32> {
    match f {
        DRM_XRGB8888 => Some(pod::BGRX),
        DRM_ARGB8888 => Some(pod::BGRA),
        DRM_XBGR8888 => Some(pod::RGBX),
        DRM_ABGR8888 => Some(pod::RGBA),
        _ => None,
    }
}

/// wl_shm's format codes are DRM's, but for its two first (0 and 1).
pub(super) fn drm_of_shm(f: u32) -> u32 {
    match f {
        0 => DRM_ARGB8888,
        1 => DRM_XRGB8888,
        other => other,
    }
}

/// A `dev_t` as the protocol sends it (the host's `dev_t`, native order).
pub(super) fn dev_of(bytes: &[u8]) -> Option<u64> {
    match bytes.len() {
        8 => Some(u64::from_ne_bytes(bytes.try_into().ok()?)),
        4 => Some(u64::from(u32::from_ne_bytes(bytes.try_into().ok()?))),
        _ => None,
    }
}

/// glibc's major and minor of a `dev_t`.
pub(super) fn major_minor(dev: u64) -> (u64, u64) {
    let major = ((dev >> 32) & 0xffff_f000) | ((dev >> 8) & 0xfff);
    let minor = ((dev >> 12) & 0xffff_ff00) | (dev & 0xff);
    (major, minor)
}

/// The modifiers a format may be allocated with: the desktop's, that this device imports
/// (the driver picks among them).
pub(super) fn usable_modifiers(desktop: &[u64], ours: &[u64]) -> Vec<u64> {
    desktop
        .iter()
        .copied()
        .filter(|m| *m != MOD_INVALID && ours.contains(m))
        .collect()
}

mod gbm;
mod protocol;
use gbm::*;
use protocol::*;

/// Whether this desktop lets ordinary programs read its screens (the two globals), and
/// it isn't switched off (`IDN_SHARE_DIRECT=off`).
pub fn direct_available() -> bool {
    if idn_common::switches::off("IDN_SHARE_DIRECT") {
        return false;
    }
    let Ok(conn) = Connection::connect_to_env() else {
        return false;
    };
    let Ok((globals, _q)) = registry_queue_init::<St>(&conn) else {
        return false;
    };
    globals.contents().with_list(|l| {
        let has = |n: &str| l.iter().any(|g| g.interface == n);
        has("ext_image_copy_capture_manager_v1")
            && has("ext_output_image_capture_source_manager_v1")
    })
}

/// What a direct capture started with.
pub struct DirectInfo {
    /// The output's connector (`DP-2`).
    pub connector: String,
    /// Its picture's size.
    pub size: (u32, u32),
    /// Where it is in the desktop (logical), for the pointer.
    pub area: Option<(i32, i32, u32, u32)>,
    /// Pictures on the GPU (else shared memory).
    pub on_gpu: bool,
}

/// A running direct capture; stops when dropped.
pub struct DirectStream {
    /// Its thread, stopped and waited for with it.
    _worker: idn_common::worker::Worker,
    ended: Arc<Mutex<Option<String>>>,
    area: SharedArea,
    pub pictures: Arc<AtomicU64>,
    pub info: DirectInfo,
}

/// Where the screen is in the desktop now (logical), shared with the capture's thread.
type SharedArea = Arc<Mutex<Option<(i32, i32, u32, u32)>>>;

/// A direct capture's connection died on a GPU buffer before its first picture (the
/// desktop couldn't import it) in this process: later ones start in shared memory.
static GPU_BROKE: AtomicBool = AtomicBool::new(false);

impl DirectStream {
    /// Reads the output `connector` (`None`: the only one, else the first) every
    /// `period`, as [`ScreenStream::start`] does: the newest picture to `sink`, or the
    /// last again. `cursor`: the pointer drawn in.
    pub fn start(
        connector: Option<String>,
        period: Duration,
        cursor: bool,
        gpu: Option<ShareDevice>,
        sink: impl FnMut(usize, usize, Shot) + Send + 'static,
    ) -> io::Result<DirectStream> {
        let stop = Arc::new(AtomicBool::new(false));
        let ended: Arc<Mutex<Option<String>>> = Arc::default();
        let pictures = Arc::new(AtomicU64::new(0));
        let (tx, rx) = std::sync::mpsc::sync_channel::<io::Result<DirectInfo>>(1);
        let (area_tx, area_rx) = std::sync::mpsc::sync_channel(1);
        let (s, e, p) = (stop.clone(), ended.clone(), pictures.clone());
        let thread = std::thread::Builder::new()
            .name("idn-screen-direct".into())
            .spawn(move || {
                let mut d = match Direct::open(connector.as_deref(), cursor, gpu) {
                    Ok(d) => d,
                    Err(err) => {
                        let _ = tx.send(Err(err));
                        return;
                    }
                };
                let _ = tx.send(Ok(d.info()));
                let _ = area_tx.send(d.area_now.clone());
                let why = d.run(period, &s, &p, sink);
                if let Err(err) = why {
                    // Ended on a GPU buffer before a single picture (a desktop that
                    // can't import ours ends the connection): memory for the next start.
                    if d.conn_died && d.info().on_gpu && p.load(Ordering::Relaxed) == 0 {
                        GPU_BROKE.store(true, Ordering::Relaxed);
                    }
                    *e.lock_ok() = Some(err.to_string());
                }
            })?;
        let info = rx
            .recv()
            .map_err(|_| io::Error::other("screen (direct): its thread ended"))??;
        let area = area_rx
            .recv()
            .unwrap_or_else(|_| Arc::new(Mutex::new(info.area)));
        Ok(DirectStream {
            area,
            _worker: idn_common::worker::Worker::from_parts(stop, thread),
            ended,
            pictures,
            info,
        })
    }

    /// Where the screen is in the desktop now (logical), for the pointer.
    pub fn area(&self) -> Option<(i32, i32, u32, u32)> {
        *self.area.lock_ok()
    }

    /// Why it ended, once it has (the output went, the desktop stopped it).
    pub fn ended(&self) -> Option<String> {
        self.ended.lock_ok().clone()
    }
}

/// The capture's Wayland side, on its thread. (Field order is drop order: buffers before
/// the GBM device they came from.)
struct Direct {
    bufs: Vec<Buf>,
    gbm: Option<GbmDevice>,
    #[cfg(feature = "gpu")]
    on_gpu: Option<super::gpu::ShareGpu>,
    #[cfg(feature = "gpu")]
    hand: Option<ShareDevice>,
    gpu_failed: bool,
    /// The connection to the desktop died (a protocol error: one an import of our buffer
    /// can cause), not the capture stopped or the output gone.
    conn_died: bool,
    /// The output's index in `st.outputs`, and where it is now (the pointer's area: it
    /// follows a mode, scale or layout change).
    pick: usize,
    area_now: SharedArea,
    frame: Option<ExtImageCopyCaptureFrameV1>,
    session: ExtImageCopyCaptureSessionV1,
    _source: ExtImageCaptureSourceV1,
    shm: WlShm,
    dmabuf: Option<ZwpLinuxDmabufV1>,
    conn: Connection,
    queue: wayland_client::EventQueue<St>,
    qh: QueueHandle<St>,
    st: St,
    connector: String,
    area: Option<(i32, i32, u32, u32)>,
}

impl Direct {
    fn open(connector: Option<&str>, cursor: bool, gpu: Option<ShareDevice>) -> io::Result<Direct> {
        let e = |what: &str, err: &dyn std::fmt::Display| {
            io::Error::other(format!("screen (direct): {what}: {err}"))
        };
        let conn = Connection::connect_to_env().map_err(|x| e("the desktop", &x))?;
        let (globals, mut queue) =
            registry_queue_init::<St>(&conn).map_err(|x| e("the desktop's globals", &x))?;
        let qh = queue.handle();
        let copy: ExtImageCopyCaptureManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .map_err(|x| e("image copy capture", &x))?;
        let sources: ExtOutputImageCaptureSourceManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .map_err(|x| e("output capture sources", &x))?;
        let shm: WlShm = globals.bind(&qh, 1..=1, ()).map_err(|x| e("wl_shm", &x))?;
        let dmabuf: Option<ZwpLinuxDmabufV1> = globals.bind(&qh, 3..=3, ()).ok();
        let xdg: Option<ZxdgOutputManagerV1> = globals.bind(&qh, 1..=3, ()).ok();
        let mut st = St {
            outputs: Vec::new(),
            pending: Constraints::default(),
            constraints: None,
            changed: false,
            stopped: false,
            frame: FrameState::Idle,
            transform: 0,
        };
        let outs: Vec<(u32, u32)> = globals.contents().with_list(|l| {
            l.iter()
                .filter(|g| g.interface == "wl_output")
                .map(|g| (g.name, g.version))
                .collect()
        });
        for (i, (name, version)) in outs.into_iter().enumerate() {
            let o: WlOutput = globals.registry().bind(name, version.min(4), &qh, i);
            if let Some(x) = &xdg {
                let _ = x.get_xdg_output(&o, &qh, i);
            }
            st.outputs.push(OutputInfo {
                wl: Some(o),
                ..Default::default()
            });
        }
        queue
            .roundtrip(&mut st)
            .and_then(|_| queue.roundtrip(&mut st))
            .map_err(|x| e("the desktop's outputs", &x))?;
        let pick = match connector {
            // A position in the list (no other list of screens here: from SSH or a
            // service), in the desktop's own order.
            Some(c) if let Some(n) = c.strip_prefix('#').and_then(|n| n.parse::<usize>().ok()) => {
                if n == 0 || n > st.outputs.len() {
                    return Err(io::Error::other(format!(
                        "screen (direct): no screen {n} (the desktop has {})",
                        st.outputs.len()
                    )));
                }
                n - 1
            }
            Some(c) => st
                .outputs
                .iter()
                .position(|o| o.name.eq_ignore_ascii_case(c))
                .ok_or_else(|| {
                    let have: Vec<&str> = st.outputs.iter().map(|o| o.name.as_str()).collect();
                    io::Error::other(format!(
                        "screen (direct): no output {c} (the desktop has {})",
                        have.join(", ")
                    ))
                })?,
            None if st.outputs.is_empty() => {
                return Err(io::Error::other(
                    "screen (direct): the desktop has no outputs",
                ));
            }
            None => 0,
        };
        let o = &st.outputs[pick];
        let connector = o.name.clone();
        let area = o.area();
        let wl = o.wl.clone().expect("bound above");
        let source = sources.create_source(&wl, &qh, ());
        let options = if cursor {
            manager_v1::Options::PaintCursors
        } else {
            manager_v1::Options::empty()
        };
        let session = copy.create_session(&source, options, &qh, ());
        // Its first constraints.
        let t0 = Instant::now();
        while st.constraints.is_none() && !st.stopped {
            if t0.elapsed() > Duration::from_secs(5) {
                return Err(io::Error::other(
                    "screen (direct): the desktop didn't describe its buffers",
                ));
            }
            queue
                .roundtrip(&mut st)
                .map_err(|x| e("the capture session", &x))?;
        }
        if st.stopped {
            return Err(io::Error::other(
                "screen (direct): the desktop refused the capture",
            ));
        }
        #[cfg(not(feature = "gpu"))]
        let _ = &gpu;
        let mut d = Direct {
            bufs: Vec::new(),
            gbm: None,
            #[cfg(feature = "gpu")]
            on_gpu: None,
            #[cfg(feature = "gpu")]
            hand: gpu.filter(|_| !idn_common::switches::off("IDN_SHARE_DMABUF")),
            gpu_failed: GPU_BROKE.load(Ordering::Relaxed),
            conn_died: false,
            pick,
            area_now: Arc::new(Mutex::new(area)),
            frame: None,
            session,
            _source: source,
            shm,
            dmabuf,
            conn,
            queue,
            qh,
            st,
            connector,
            area,
        };
        d.allocate()?;
        Ok(d)
    }

    fn info(&self) -> DirectInfo {
        // (A monitor turned a quarter: its pictures come the other way round.)
        let quarter = self
            .st
            .outputs
            .get(self.pick)
            .is_some_and(|o| o.transform & 1 == 1);
        DirectInfo {
            connector: self.connector.clone(),
            size: self
                .bufs
                .first()
                .map_or((0, 0), |b| if quarter { (b.h, b.w) } else { (b.w, b.h) }),
            area: self.area,
            on_gpu: self
                .bufs
                .first()
                .is_some_and(|b| matches!(b.mem, Mem::Dma { .. })),
        }
    }

    /// The buffer for the constraints in force: on the GPU where it can be, else in
    /// shared memory.
    fn allocate(&mut self) -> io::Result<()> {
        self.st.changed = false;
        self.bufs.clear();
        let c = self.st.constraints.clone().unwrap_or_default();
        if c.size.0 == 0 || c.size.1 == 0 {
            return Err(io::Error::other(
                "screen (direct): a buffer size of nothing",
            ));
        }
        // A turned monitor's pictures are turned back on the CPU: in memory from the start.
        let turned = self
            .st
            .outputs
            .get(self.pick)
            .is_some_and(|o| o.transform != 0);
        if !self.gpu_failed && !turned {
            match self.allocate_dma(&c) {
                Ok(Some(b)) => {
                    if let Mem::Dma { modifier, .. } = &b.mem {
                        idn_open_protocol::log::log(&format!(
                            "screen (direct): {} {}x{} on the GPU (DMA-BUF, modifier {modifier:#x}, the driver's choice)",
                            self.connector, b.w, b.h
                        ));
                    }
                    self.bufs.push(b);
                    return Ok(());
                }
                Ok(None) => {}
                Err(err) => {
                    idn_open_protocol::log::log(&format!(
                        "screen (direct): pictures through memory ({err})"
                    ));
                }
            }
        }
        let b = self.allocate_shm(&c)?;
        idn_open_protocol::log::log(&format!(
            "screen (direct): {} {}x{} through shared memory",
            self.connector, b.w, b.h
        ));
        self.bufs.push(b);
        Ok(())
    }

    /// A GBM buffer in a format and layout both the desktop and this device take; `None`
    /// where there is no GPU path (no device given, no linux-dmabuf).
    #[cfg(feature = "gpu")]
    fn allocate_dma(&mut self, c: &Constraints) -> io::Result<Option<Buf>> {
        let (Some(hand), Some(dmabuf), Some(dev)) = (&self.hand, &self.dmabuf, c.dev) else {
            return Ok(None);
        };
        let g = gbm()?;
        if self.gbm.is_none() {
            self.gbm = Some(GbmDevice::open(dev)?);
        }
        let gdev = self.gbm.as_ref().expect("just opened").dev;
        let (w, h) = c.size;
        for want in [DRM_XRGB8888, DRM_ARGB8888, DRM_XBGR8888, DRM_ABGR8888] {
            let Some((_, theirs)) = c.dma.iter().find(|(f, _)| *f == want) else {
                continue;
            };
            let Some(format) = spa_of_drm(want).and_then(super::gpu::texture_format) else {
                continue;
            };
            let ours = idn_open_protocol::gpu::dmabuf_modifiers(hand, format);
            let mods = usable_modifiers(theirs, &ours);
            if mods.is_empty() {
                continue;
            }
            // SAFETY: a live GBM device; the modifier list outlives the call.
            let bo = unsafe {
                match g.bo_create_with_modifiers2 {
                    Some(f) => f(
                        gdev,
                        w,
                        h,
                        want,
                        mods.as_ptr(),
                        mods.len() as u32,
                        GBM_BO_USE_RENDERING,
                    ),
                    None => (g.bo_create_with_modifiers)(
                        gdev,
                        w,
                        h,
                        want,
                        mods.as_ptr(),
                        mods.len() as u32,
                    ),
                }
            };
            if bo.is_null() {
                continue;
            }
            // SAFETY: a live buffer object; the descriptor gbm_bo_get_fd gives is ours.
            let (fd, offset, stride, modifier, planes) = unsafe {
                (
                    (g.bo_get_fd)(bo),
                    (g.bo_get_offset)(bo, 0),
                    (g.bo_get_stride)(bo),
                    (g.bo_get_modifier)(bo),
                    (g.bo_get_plane_count)(bo),
                )
            };
            if fd < 0 || planes != 1 {
                if fd >= 0 {
                    // SAFETY: ours, unused.
                    unsafe { libc::close(fd) };
                }
                // SAFETY: made above, not used again.
                unsafe { (g.bo_destroy)(bo) };
                continue;
            }
            // SAFETY: ours (above).
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            let params = dmabuf.create_params(&self.qh, ());
            params.add(
                fd.as_fd(),
                0,
                offset,
                stride,
                (modifier >> 32) as u32,
                modifier as u32,
            );
            let wl = params.create_immed(
                w as i32,
                h as i32,
                want,
                zwp_linux_buffer_params_v1::Flags::empty(),
                &self.qh,
                (),
            );
            params.destroy();
            return Ok(Some(Buf {
                wl,
                mem: Mem::Dma {
                    bo,
                    fd,
                    offset,
                    stride,
                    modifier,
                },
                fourcc: want,
                w,
                h,
            }));
        }
        Ok(None)
    }

    #[cfg(not(feature = "gpu"))]
    fn allocate_dma(&mut self, _: &Constraints) -> io::Result<Option<Buf>> {
        let _ = (&self.dmabuf, &self.gbm);
        Ok(None)
    }

    /// A shared-memory buffer in a format the CPU path reorders.
    fn allocate_shm(&mut self, c: &Constraints) -> io::Result<Buf> {
        let (w, h) = c.size;
        let fourcc = [DRM_XRGB8888, DRM_ARGB8888, DRM_XBGR8888, DRM_ABGR8888]
            .into_iter()
            .find(|f| c.shm.contains(f))
            .ok_or_else(|| {
                io::Error::other(format!(
                    "screen (direct): no shared-memory format here of {:x?}",
                    c.shm
                ))
            })?;
        let stride = w * 4;
        let len = stride as usize * h as usize;
        // SAFETY: a fresh anonymous file, sized and mapped here; the mapping is ours.
        let (fd, map) = unsafe {
            let fd = libc::memfd_create(c"idn-screen".as_ptr(), libc::MFD_CLOEXEC);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let fd = OwnedFd::from_raw_fd(fd);
            if libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) != 0 {
                return Err(io::Error::last_os_error());
            }
            let map = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            );
            if map == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            (fd, map.cast::<u8>())
        };
        let shm_code = match fourcc {
            DRM_ARGB8888 => 0,
            DRM_XRGB8888 => 1,
            f => f,
        };
        let format = wl_shm::Format::try_from(shm_code).map_err(|_| {
            io::Error::other("screen (direct): a shared-memory format wl_shm lacks")
        })?;
        let pool = self.shm.create_pool(fd.as_fd(), len as i32, &self.qh, ());
        let wl = pool.create_buffer(0, w as i32, h as i32, stride as i32, format, &self.qh, ());
        pool.destroy();
        Ok(Buf {
            wl,
            mem: Mem::Shm { map, len, stride },
            fourcc,
            w,
            h,
        })
    }

    /// The next picture asked for, into the buffer.
    fn ask(&mut self) {
        let Some(b) = self.bufs.first() else {
            return;
        };
        let f = self.session.create_frame(&self.qh, ());
        f.attach_buffer(&b.wl);
        f.damage_buffer(0, 0, b.w as i32, b.h as i32);
        f.capture();
        self.frame = Some(f);
        self.st.frame = FrameState::Waiting;
    }

    /// Waits for the desktop's events until `deadline` (or one came).
    fn wait(&mut self, deadline: Instant) -> io::Result<()> {
        let e = |x: &dyn std::fmt::Display| io::Error::other(format!("screen (direct): {x}"));
        self.queue
            .dispatch_pending(&mut self.st)
            .map_err(|x| e(&x))?;
        self.conn.flush().map_err(|x| e(&x))?;
        if let Some(guard) = self.queue.prepare_read() {
            let ms = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(1000) as c_int;
            let mut pfd = libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one pollfd of ours.
            let n = unsafe { libc::poll(&mut pfd, 1, ms) };
            if n > 0 {
                match guard.read() {
                    Ok(_) => {}
                    Err(wayland_client::backend::WaylandError::Io(x))
                        if x.kind() == io::ErrorKind::WouldBlock => {}
                    Err(x) => return Err(e(&x)),
                }
            }
        }
        self.queue
            .dispatch_pending(&mut self.st)
            .map_err(|x| e(&x))?;
        Ok(())
    }

    /// The picture in the buffer, for the sink.
    fn take(&mut self, spare: &mut Option<Arc<[u8]>>) -> io::Result<(usize, usize, Shot)> {
        let b = self
            .bufs
            .first()
            .ok_or_else(|| io::Error::other("no buffer"))?;
        let (w, h) = (b.w as usize, b.h as usize);
        let spa = spa_of_drm(b.fourcc).unwrap_or(pod::BGRX);
        // A rotated or flipped monitor: its picture turned back on the CPU (the GPU path
        // takes the buffer as it is).
        let turn = self.st.transform;
        if turn != 0 && matches!(b.mem, Mem::Dma { .. }) {
            return Err(io::Error::other(format!(
                "a turned monitor (transform {turn})"
            )));
        }
        match &b.mem {
            #[cfg(feature = "gpu")]
            Mem::Dma {
                fd,
                offset,
                stride,
                modifier,
                ..
            } => {
                wait_for_writes(fd.as_raw_fd());
                let hand = self.hand.as_ref().expect("a DMA-BUF only with a device");
                let g = self
                    .on_gpu
                    .get_or_insert_with(|| super::gpu::ShareGpu::new(hand));
                let pic = g.picture(&super::gpu::DmaPicture {
                    fd: fd.as_raw_fd(),
                    offset: *offset,
                    stride: *stride,
                    modifier: *modifier,
                    fmt: spa,
                    w: b.w,
                    h: b.h,
                })?;
                Ok((w, h, Shot::Gpu(pic)))
            }
            #[cfg(not(feature = "gpu"))]
            Mem::Dma { .. } => Err(io::Error::other("no GPU path in this build")),
            Mem::Shm { map, len, stride } => {
                // SAFETY: our mapping; the desktop is done writing (ready came) and
                // writes again only once we ask.
                let src = unsafe { std::slice::from_raw_parts(*map, *len) };
                let rgba = to_rgba(spa, src, w, h, *stride as usize, spare.take());
                if turn != 0 {
                    let (t, tw, th) = untransform(&rgba, w, h, turn);
                    return Ok((tw, th, Shot::Rgba(t.into())));
                }
                Ok((w, h, Shot::Rgba(rgba)))
            }
        }
    }

    fn run(
        &mut self,
        period: Duration,
        stop: &AtomicBool,
        count: &AtomicU64,
        mut sink: impl FnMut(usize, usize, Shot),
    ) -> io::Result<()> {
        let mut last: Option<(usize, usize, Shot)> = None;
        let mut spare: Option<Arc<[u8]>> = None;
        let mut next = Instant::now();
        // GPU buffers the desktop failed to fill in a row: at 3, memory from then on.
        let mut gpu_fails = 0u32;
        self.ask();
        while !stop.load(Ordering::Relaxed) {
            self.wait(next).inspect_err(|_| self.conn_died = true)?;
            if let Some(a) = self.st.outputs.get(self.pick).and_then(OutputInfo::area) {
                *self.area_now.lock_ok() = Some(a);
            }
            if self.st.stopped {
                return Err(io::Error::other(format!(
                    "screen (direct): the desktop stopped the capture of {}",
                    self.connector
                )));
            }
            match std::mem::replace(&mut self.st.frame, FrameState::Idle) {
                FrameState::Ready => {
                    gpu_fails = 0;
                    if let Some(f) = self.frame.take() {
                        f.destroy();
                    }
                    match self.take(&mut spare) {
                        Ok(p) => {
                            if let Some((_, _, Shot::Rgba(old))) = last.replace(p) {
                                spare = Some(old);
                            }
                        }
                        Err(err)
                            if matches!(
                                self.bufs.first().map(|b| &b.mem),
                                Some(Mem::Dma { .. })
                            ) =>
                        {
                            idn_open_protocol::log::log(&format!(
                                "screen (direct): pictures through memory from now ({err})"
                            ));
                            self.gpu_failed = true;
                            self.allocate()?;
                            self.ask();
                        }
                        Err(err) => return Err(err),
                    }
                }
                FrameState::Failed(reason) => {
                    if let Some(f) = self.frame.take() {
                        f.destroy();
                    }
                    let on_gpu = matches!(self.bufs.first().map(|b| &b.mem), Some(Mem::Dma { .. }));
                    let stopped = matches!(
                        frame_v1::FailureReason::try_from(reason),
                        Ok(frame_v1::FailureReason::Stopped)
                    );
                    if on_gpu && !stopped && !self.st.changed {
                        gpu_fails += 1;
                        if gpu_fails >= 3 {
                            idn_open_protocol::log::log(&format!(
                                "screen (direct): pictures through memory from now (the desktop failed {gpu_fails} GPU buffers, reason {reason})"
                            ));
                            self.gpu_failed = true;
                            gpu_fails = 0;
                            self.allocate()?;
                            self.ask();
                            continue;
                        }
                    }
                    match frame_v1::FailureReason::try_from(reason) {
                        Ok(frame_v1::FailureReason::Stopped) => {
                            return Err(io::Error::other(format!(
                                "screen (direct): the capture of {} stopped",
                                self.connector
                            )));
                        }
                        // New constraints follow (a new size): the buffer made again.
                        Ok(frame_v1::FailureReason::BufferConstraints) => {
                            let t0 = Instant::now();
                            while !self.st.changed && t0.elapsed() < Duration::from_secs(2) {
                                self.wait(Instant::now() + Duration::from_millis(50))
                                    .inspect_err(|_| self.conn_died = true)?;
                            }
                        }
                        _ => std::thread::sleep(Duration::from_millis(20)),
                    }
                    if self.st.changed {
                        self.allocate()?;
                    }
                    self.ask();
                }
                FrameState::Waiting => self.st.frame = FrameState::Waiting,
                FrameState::Idle => {}
            }
            if self.st.changed && self.frame.is_none() {
                self.allocate()?;
                self.ask();
            }
            if Instant::now() >= next {
                next += period;
                if next < Instant::now() {
                    next = Instant::now() + period;
                }
                if let Some((w, h, shot)) = &last {
                    sink(*w, *h, shot.clone());
                    count.fetch_add(1, Ordering::Relaxed);
                }
                // The next picture asked for once this one is handed on.
                if self.frame.is_none() {
                    self.ask();
                }
            }
        }
        Ok(())
    }
}

impl Drop for Direct {
    fn drop(&mut self) {
        // The desktop told we are done (the buffers go with their own drop, after).
        if let Some(f) = self.frame.take() {
            f.destroy();
        }
        self.session.destroy();
        self._source.destroy();
        let _ = self.conn.flush();
    }
}

/// Waits (up to 100 ms) for the GPU work writing into a DMA-BUF to finish: its implicit
/// fence, as a sync file (Linux 6.0 and later; before, nothing to wait on here).
#[cfg(feature = "gpu")]
fn wait_for_writes(fd: c_int) {
    #[repr(C)]
    struct ExportSyncFile {
        flags: u32,
        fd: i32,
    }
    // DMA_BUF_IOCTL_EXPORT_SYNC_FILE: _IOWR('b', 2, struct dma_buf_export_sync_file).
    const EXPORT_SYNC_FILE: libc::c_ulong = 0xC008_6202;
    const DMA_BUF_SYNC_READ: u32 = 1;
    let mut a = ExportSyncFile {
        flags: DMA_BUF_SYNC_READ,
        fd: -1,
    };
    // SAFETY: the ioctl's own structure; the sync file it gives is ours to close.
    unsafe {
        if libc::ioctl(fd, EXPORT_SYNC_FILE, &mut a) == 0 && a.fd >= 0 {
            let mut p = libc::pollfd {
                fd: a.fd,
                events: libc::POLLIN,
                revents: 0,
            };
            libc::poll(&mut p, 1, 100);
            libc::close(a.fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_devices_and_modifiers() {
        assert_eq!(DRM_XRGB8888, 0x3432_5258);
        assert_eq!(spa_of_drm(DRM_XRGB8888), Some(pod::BGRX));
        assert_eq!(spa_of_drm(DRM_ABGR8888), Some(pod::RGBA));
        assert_eq!(drm_of_shm(0), DRM_ARGB8888);
        assert_eq!(drm_of_shm(1), DRM_XRGB8888);
        assert_eq!(drm_of_shm(DRM_XBGR8888), DRM_XBGR8888);
        // 226:128 (renderD128) as glibc packs it.
        assert_eq!(major_minor((226 << 8) | 128), (226, 128));
        assert_eq!(
            dev_of(&((226u64 << 8) | 1).to_ne_bytes()),
            Some((226 << 8) | 1)
        );
        // The desktop's list, less what this device can't import and "none said".
        let theirs = [0x0300_0000_0060_6014, MOD_INVALID, 0];
        assert_eq!(
            usable_modifiers(&theirs, &[0, 0x0300_0000_0060_6014]),
            vec![0x0300_0000_0060_6014, 0]
        );
        assert!(usable_modifiers(&theirs, &[0x0300_0000_0060_6010]).is_empty());
    }
}

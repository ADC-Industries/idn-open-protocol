//! The PipeWire stream of a granted screen: formats offered and chosen (DMA-BUF where
//! the device imports it, else shared memory), each picture taken on PipeWire's thread
//! and handed to ours, which makes it RGBA or planar on the GPU and gives the sink the
//! newest at the asked rate.

use super::*;
use idn_common::sync::LockOk as _;

/// A picture as it came: (format, width, height, stride, bytes).
pub(super) type Raw = (u32, usize, usize, usize, Vec<u8>);

/// A DMA-BUF picture as it came, its buffer held until the GPU has read it.
// (Read with the `gpu` feature only.)
#[cfg_attr(not(feature = "gpu"), allow(dead_code))]
pub(super) struct DmaRaw {
    /// The buffer (a `*mut PwBuffer`), given back to the stream once read.
    buf: usize,
    /// The buffer's DMA-BUF, our own descriptor for it (PipeWire's may be closed on its
    /// thread meanwhile, a renegotiation, and its number reused for something else).
    fd: std::os::fd::OwnedFd,
    offset: u32,
    stride: u32,
    modifier: u64,
    fmt: u32,
    w: u32,
    h: u32,
}

/// What the stream's thread hands ours.
pub(super) enum Incoming {
    Raw(Raw),
    Dma(DmaRaw),
}

/// The device a screen share's pictures may stay on (DMA-BUF), with the `gpu` feature.
#[cfg(feature = "gpu")]
pub type ShareDevice = idn_open_protocol::gpu::GpuHandIn;
/// (Without the `gpu` feature there is none.)
#[cfg(not(feature = "gpu"))]
pub type ShareDevice = std::convert::Infallible;

/// A picture for the sink: RGBA in memory, or planar on the GPU.
#[derive(Clone)]
pub enum Shot {
    Rgba(Arc<[u8]>),
    #[cfg(feature = "gpu")]
    Gpu(idn_open_protocol::gpu::GpuPicture),
}

/// The stream, for giving buffers back from our thread (with the loop's lock) until it
/// goes.
#[derive(Default)]
pub(super) struct RequeueState {
    /// `None` once the stream is being destroyed.
    live: Mutex<Option<(&'static Lib, usize, usize)>>,
    /// The buffers our thread holds and PipeWire still has: one it removes (new buffers
    /// for a new format) is not given back. Locked after the loop's lock.
    held: Mutex<std::collections::HashSet<usize>>,
}
pub(super) type Requeue = Arc<RequeueState>;

/// `buf` back to the stream (from our thread), if both are still there.
pub(super) fn give_back(r: &Requeue, buf: usize) {
    let g = r.live.lock_ok();
    if let Some((lib, tloop, stream)) = *g {
        // SAFETY: the live stream (`live` is cleared before it is destroyed), with its loop
        // locked; `buf` is one of its buffers still (in `held`, which PipeWire's removal
        // takes it out of under the same lock).
        unsafe {
            (lib.thread_loop_lock)(tloop as *mut c_void);
            if r.held.lock_ok().remove(&buf) {
                (lib.stream_queue_buffer)(stream as *mut c_void, buf as *mut PwBuffer);
            }
            (lib.thread_loop_unlock)(tloop as *mut c_void);
        }
    }
}

/// PipeWire is freeing `b` (on its thread, the loop locked): not to be given back.
pub(super) unsafe extern "C" fn on_remove_buffer(data: *mut c_void, b: *mut PwBuffer) {
    // SAFETY: `data` is the State given with the listener, freed only after the stream is
    // destroyed.
    let s = unsafe { &*(data as *const State) };
    s.requeue.held.lock_ok().remove(&(b as usize));
}

/// Offers the stream shared memory only from now (its DMA-BUF pictures couldn't be read):
/// the desktop gives it new buffers.
// (Called with the `gpu` feature only.)
#[cfg_attr(not(feature = "gpu"), allow(dead_code))]
pub(super) fn memory_only(r: &Requeue) {
    let g = r.live.lock_ok();
    if let Some((lib, tloop, stream)) = *g {
        let f = pod::enum_format();
        let mut ps = [f.as_ptr()];
        // SAFETY: the live stream (`Requeue` is cleared before it is destroyed), updated with its
        // loop locked; the pod outlives the call.
        unsafe {
            (lib.thread_loop_lock)(tloop as *mut c_void);
            (lib.stream_update_params)(stream as *mut c_void, ps.as_mut_ptr(), 1);
            (lib.thread_loop_unlock)(tloop as *mut c_void);
        }
    }
}

pub(super) struct State {
    lib: &'static Lib,
    stream: *mut c_void,
    format: Option<(u32, u32, u32)>,
    /// The chosen format's DMA-BUF modifier, when its buffers are DMA-BUF.
    modifier: Option<u64>,
    /// The formats offered as DMA-BUF, with the modifiers this device imports (none
    /// without a device, or after a picture couldn't be read: shared memory only).
    offers: Vec<(u32, Vec<u64>)>,
    no_dmabuf: Arc<std::sync::atomic::AtomicBool>,
    /// Since when the desktop is to fix its DMA-BUF layout (a Format still choosing):
    /// shared memory after [`FIXATE_WAIT`] without one.
    awaiting: Arc<Mutex<Option<Instant>>>,
    /// To our own thread: PipeWire's is small and must not wait.
    tx: std::sync::mpsc::SyncSender<Incoming>,
    /// Picture buffers our thread is done with, copied into again (
    /// a new one each picture).
    back: std::sync::mpsc::Receiver<Vec<u8>>,
    pictures: Arc<AtomicU64>,
    ended: Arc<Mutex<Option<String>>>,
    /// The buffers our thread holds (see [`RequeueState`]).
    requeue: Requeue,
}

pub(super) unsafe extern "C" fn on_state(
    data: *mut c_void,
    _old: c_int,
    state: c_int,
    error: *const c_char,
) {
    // SAFETY: `data` is the State given with the listener, freed only after the stream is
    // destroyed; PipeWire calls its callbacks on its loop's thread, one at a time.
    let s = unsafe { &mut *(data as *mut State) };
    // PW_STREAM_STATE_ERROR = -1, UNCONNECTED = 0.
    if state <= 0 && s.pictures.load(Ordering::Relaxed) > 0 || state < 0 {
        let why = if error.is_null() {
            "the screen share ended".to_owned()
        } else {
            // SAFETY: PipeWire's error message, NUL-terminated (null checked).
            unsafe { CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned()
        };
        *s.ended.lock_ok() = Some(why);
    }
}

/// What to do with a Format whose buffers would be DMA-BUF.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Dmabuf {
    /// Not fixed yet (the producer is to choose and announce it): nothing until it is.
    Wait,
    /// Fixed, and a modifier this device imports: DMA-BUF laid out so.
    Use(u64),
    /// Fixed to one this device can't be told: shared memory from now.
    Memory(u64),
}

/// The decision for `c` (a Format with a modifier) given `ours`, the modifiers this device
/// imports its format with. Only a single, fixed modifier is the buffers' layout: a list
/// (marked to be fixated or not) is what the producer chooses from, and it may choose any.
pub(super) fn dmabuf_decision(c: &pod::Chosen, ours: &[u64]) -> Dmabuf {
    let Some(m) = c.modifier else {
        return Dmabuf::Wait;
    };
    // (A choice lists its default among the values: one value throughout is fixed.)
    if c.dont_fixate || c.modifiers.iter().any(|&x| x != m) {
        Dmabuf::Wait
    } else if ours.contains(&m) {
        Dmabuf::Use(m)
    } else {
        Dmabuf::Memory(m)
    }
}

/// How long a share waits for the desktop to fix its buffers' layout before asking for
/// shared memory instead.
const FIXATE_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

pub(super) unsafe extern "C" fn on_param(data: *mut c_void, id: u32, param: *const u8) {
    // SAFETY: `data` is the State given with the listener, freed only after the stream is
    // destroyed; PipeWire calls its callbacks on its loop's thread, one at a time.
    let s = unsafe { &mut *(data as *mut State) };
    if id != pod::PARAM_FORMAT || param.is_null() {
        return;
    }
    // SAFETY: a SPA pod from PipeWire: its 8-byte header (body size, then type), then
    // `size` bytes of body.
    let size =
        unsafe { u32::from_ne_bytes(std::slice::from_raw_parts(param, 4).try_into().unwrap()) }
            as usize;
    // SAFETY: as above: header and body, `size` bytes as the pod gives.
    let p = unsafe { std::slice::from_raw_parts(param, size + 8) };
    let chosen = pod::parse_format_full(p);
    s.format = chosen.as_ref().map(|c| (c.fmt, c.w, c.h));
    s.modifier = None;
    let dmabuf_ok = !s.no_dmabuf.load(Ordering::Relaxed);
    if let Some(c) = chosen.filter(|c| c.modifier.is_some() && dmabuf_ok) {
        // The modifiers this device imports `c.fmt` with (never DRM_FORMAT_MOD_INVALID,
        // "the driver's own layout", which Vulkan can't be told).
        let ours: Vec<u64> = s
            .offers
            .iter()
            .find(|o| o.0 == c.fmt)
            .map(|o| o.1.clone())
            .unwrap_or_default();
        match dmabuf_decision(&c, &ours) {
            Dmabuf::Wait => {
                // The producer fixates (PipeWire's DMA-BUF negotiation): it allocates with
                // one of these, the driver choosing, and offers that one fixed; a Format
                // with it follows. (Choosing here, the first, tiled an NVIDIA desktop: the
                // driver gave COSMIC a taller block than the first, which ours was read
                // as.)
                *s.awaiting.lock_ok() = Some(Instant::now());
                return;
            }
            Dmabuf::Use(m) => {
                *s.awaiting.lock_ok() = None;
                s.modifier = Some(m);
                // Its buffers as DMA-BUF.
                let b = pod::buffers_dmabuf();
                let mut ps = [b.as_ptr()];
                // SAFETY: our stream, on its loop's thread (a callback); the pod outlives the call.
                unsafe { (s.lib.stream_update_params)(s.stream, ps.as_mut_ptr(), 1) };
                return;
            }
            Dmabuf::Memory(first) => {
                *s.awaiting.lock_ok() = None;
                idn_open_protocol::log::log(&format!(
                    "screen share: the desktop's buffer layout (modifier {first:#x}) isn't one this GPU imports; pictures through memory"
                ));
                s.no_dmabuf.store(true, Ordering::Relaxed);
                let f = pod::enum_format();
                let mut ps = [f.as_ptr()];
                // SAFETY: our stream, on its loop's thread (a callback); the pod outlives the call.
                unsafe { (s.lib.stream_update_params)(s.stream, ps.as_mut_ptr(), 1) };
                return;
            }
        }
    }
    *s.awaiting.lock_ok() = None;
    // Mappable buffers, please.
    let b = pod::buffers();
    let mut ps = [b.as_ptr()];
    // SAFETY: our stream, on its loop's thread (a callback); the pod outlives the call.
    unsafe { (s.lib.stream_update_params)(s.stream, ps.as_mut_ptr(), 1) };
}

/// SPA_CHUNK_FLAG_CORRUPTED: the picture in the buffer is damaged.
const CHUNK_CORRUPTED: i32 = 1;

pub(super) unsafe extern "C" fn on_process(data: *mut c_void) {
    // SAFETY: `data` is the State given with the listener, freed only after the stream is
    // destroyed; PipeWire calls its callbacks on its loop's thread, one at a time.
    let s = unsafe { &mut *(data as *mut State) };
    // The newest picture; older ones queued back unused.
    let mut newest: *mut PwBuffer = std::ptr::null_mut();
    loop {
        // SAFETY: our stream, on its loop's thread.
        let b = unsafe { (s.lib.stream_dequeue_buffer)(s.stream) };
        if b.is_null() {
            break;
        }
        if !newest.is_null() {
            // SAFETY: a buffer dequeued from this stream, given back once.
            unsafe { (s.lib.stream_queue_buffer)(s.stream, newest) };
        }
        newest = b;
    }
    if newest.is_null() {
        return;
    }
    // A DMA-BUF picture: to our thread, which gives the buffer back once the GPU has read
    // it (not here: the desktop would draw the next picture into it meanwhile).
    if let (Some((fmt, w, h)), Some(modifier)) = (s.format, s.modifier) {
        // SAFETY: a buffer dequeued from this stream: its spa_buffer, data and chunk are
        // PipeWire's while it is ours; the DMA-BUF is duplicated, so ours outlives theirs.
        unsafe {
            let sb = &*(*newest).buffer;
            if sb.n_datas > 0 && !sb.datas.is_null() {
                let d = &*sb.datas;
                let chunk = &*d.chunk;
                // Not an empty or damaged picture (as the shared-memory path below).
                let usable = chunk.size > 0 && chunk.flags & CHUNK_CORRUPTED == 0;
                let fd = (usable && d.kind == pod::DATA_DMABUF && d.fd >= 0)
                    .then(|| libc::fcntl(d.fd as c_int, libc::F_DUPFD_CLOEXEC, 0))
                    .filter(|&f| f >= 0)
                    .map(|f| <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(f));
                if let Some(fd) = fd
                    && s.tx
                        .try_send(Incoming::Dma(DmaRaw {
                            buf: newest as usize,
                            fd,
                            offset: chunk.offset,
                            stride: chunk.stride.max(0) as u32,
                            modifier,
                            fmt,
                            w,
                            h,
                        }))
                        .is_ok()
                {
                    // (Our thread gives it back only after this returns: that takes the
                    // loop's lock, held here.)
                    s.requeue.held.lock_ok().insert(newest as usize);
                    s.pictures.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            }
        }
        // SAFETY: a buffer dequeued from this stream, given back once.
        unsafe { (s.lib.stream_queue_buffer)(s.stream, newest) };
        return;
    }
    if let Some((fmt, w, h)) = s.format {
        // SAFETY: the buffer's memory, read only within `maxsize` (off + need checked) through
        // PipeWire's pointer or our own read-only mapping, unmapped after the copy.
        unsafe {
            let sb = &*(*newest).buffer;
            if sb.n_datas > 0 && !sb.datas.is_null() {
                let d = &*sb.datas;
                let chunk = &*d.chunk;
                let (w, h) = (w as usize, h as usize);
                let stride = if chunk.stride > 0 {
                    chunk.stride as usize
                } else {
                    w * 4
                };
                let off = chunk.offset as usize % d.maxsize.max(1) as usize;
                let need = stride * h;
                // Rows at least a picture wide (a smaller stride, during a resize or from a
                // buggy desktop, panicked the copy and froze the capture).
                if chunk.size > 0
                    && chunk.flags & CHUNK_CORRUPTED == 0
                    && w > 0
                    && h > 0
                    && stride >= w * 4
                    && off + need <= d.maxsize as usize
                {
                    // Shared memory mapped here, read-only, for this picture (PipeWire's
                    // own mapping follows the buffer's flags, which needn't say readable).
                    let copy = if d.kind == 2 && d.fd >= 0 {
                        let len = d.mapoffset as usize + d.maxsize as usize;
                        // (Its pages mapped in one go: faulted in one by one, a 2160p
                        // picture was thousands of faults on PipeWire's thread.)
                        let m = libc::mmap(
                            std::ptr::null_mut(),
                            len,
                            libc::PROT_READ,
                            libc::MAP_SHARED | libc::MAP_POPULATE,
                            d.fd as c_int,
                            0,
                        );
                        if m == libc::MAP_FAILED {
                            None
                        } else {
                            let src = std::slice::from_raw_parts(
                                (m as *const u8).add(d.mapoffset as usize + off),
                                need,
                            );
                            let mut v = s.back.try_recv().unwrap_or_default();
                            v.clear();
                            v.extend_from_slice(src);
                            libc::munmap(m, len);
                            Some(v)
                        }
                    } else if d.kind == 1 && !d.data.is_null() {
                        let mut v = s.back.try_recv().unwrap_or_default();
                        v.clear();
                        v.extend_from_slice(std::slice::from_raw_parts(
                            (d.data as *const u8).add(off),
                            need,
                        ));
                        Some(v)
                    } else {
                        None
                    };
                    // To our thread (dropped when it is still busy with one).
                    if let Some(v) = copy
                        && s.tx.try_send(Incoming::Raw((fmt, w, h, stride, v))).is_ok()
                    {
                        s.pictures.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    }
    // SAFETY: a buffer dequeued from this stream, given back once.
    unsafe { (s.lib.stream_queue_buffer)(s.stream, newest) };
}

/// A running screen share stream; stops when dropped.
pub struct ScreenStream {
    lib: &'static Lib,
    tloop: *mut c_void,
    context: *mut c_void,
    core: *mut c_void,
    stream: *mut c_void,
    _hook: Box<SpaHook>,
    _events: Box<StreamEvents>,
    state: *mut State,
    requeue: Requeue,
    pub pictures: Arc<AtomicU64>,
    ended: Arc<Mutex<Option<String>>>,
    pub started: Instant,
}

// SAFETY: PipeWire's objects are used from other threads only with the thread loop
// locked (Drop, `memory_only`); `state` is PipeWire's thread's, freed after the stream.
unsafe impl Send for ScreenStream {}

impl ScreenStream {
    /// Reads the granted screen: every `period`, the newest picture to `sink`, or the last
    /// one again (a desktop sends pictures only when something changes; a still slide is
    /// still a source). RGBA, the same buffer when repeated; or, given a `gpu` device
    /// that imports the desktop's DMA-BUF pictures, planar on that GPU (nothing through
    /// this memory), shared memory again if a picture can't be read there.
    pub fn start(
        grant: Grant,
        period: std::time::Duration,
        gpu: Option<ShareDevice>,
        mut sink: impl FnMut(usize, usize, Shot) + Send + 'static,
    ) -> io::Result<ScreenStream> {
        let lib = lib()?;
        let pictures = Arc::new(AtomicU64::new(0));
        let ended: Arc<Mutex<Option<String>>> = Arc::default();
        let requeue: Requeue = Arc::default();
        let no_dmabuf = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let awaiting: Arc<Mutex<Option<Instant>>> = Arc::default();
        #[cfg(feature = "gpu")]
        let offers = gpu
            .as_ref()
            .filter(|_| !idn_common::switches::off("IDN_SHARE_DMABUF"))
            .map(super::gpu::offers)
            .unwrap_or_default();
        #[cfg(not(feature = "gpu"))]
        let offers: Vec<(u32, Vec<u64>)> = {
            let _ = &gpu;
            Vec::new()
        };
        // Pictures converted and handed on here; it ends when the stream is gone.
        let (tx, rx) = std::sync::mpsc::sync_channel::<Incoming>(2);
        let (back_tx, back) = std::sync::mpsc::sync_channel::<Vec<u8>>(4);
        let (rq, nd, aw) = (requeue.clone(), no_dmabuf.clone(), awaiting.clone());
        std::thread::Builder::new()
            .name("idn-screen-share".into())
            .spawn(move || {
                #[cfg(feature = "gpu")]
                let mut on_gpu = gpu.as_ref().map(super::gpu::ShareGpu::new);
                #[cfg(feature = "gpu")]
                let mut said = false;
                let mut last: Option<(usize, usize, Shot)> = None;
                let mut spare: Option<Arc<[u8]>> = None;
                let mut next = Instant::now();
                loop {
                    // Until the next tick: the newest picture that came (an older DMA-BUF
                    // one given back unread).
                    let mut newest: Option<Incoming> = None;
                    loop {
                        match rx.recv_timeout(next.saturating_duration_since(Instant::now())) {
                            Ok(pic) => {
                                if let Some(Incoming::Dma(old)) = newest.replace(pic) {
                                    give_back(&rq, old.buf);
                                }
                            }
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                        }
                    }
                    match newest {
                        Some(Incoming::Raw((fmt, w, h, stride, data))) => {
                            // The buffer from two pictures back: the output kept the last
                            // one until this one replaces it, so that one is free by now.
                            let reuse = spare.take();
                            let rgba = to_rgba(fmt, &data, w, h, stride, reuse);
                            spare = match last.replace((w, h, Shot::Rgba(rgba))) {
                                Some((_, _, Shot::Rgba(a))) => Some(a),
                                _ => None,
                            };
                            let _ = back_tx.try_send(data);
                        }
                        Some(Incoming::Dma(d)) => {
                            #[cfg(feature = "gpu")]
                            {
                                let got = on_gpu.as_mut().map(|g| {
                                    g.picture(&super::gpu::DmaPicture {
                                        fd: std::os::fd::AsRawFd::as_raw_fd(&d.fd),
                                        offset: d.offset,
                                        stride: d.stride,
                                        modifier: d.modifier,
                                        fmt: d.fmt,
                                        w: d.w,
                                        h: d.h,
                                    })
                                });
                                // Read: the buffer goes back to the desktop.
                                give_back(&rq, d.buf);
                                match got {
                                    Some(Ok(pic)) => {
                                        if !said {
                                            said = true;
                                            idn_open_protocol::log::log(&format!(
                                                "screen share: pictures taken on the GPU as they are (DMA-BUF, modifier {:#x}), none through memory",
                                                d.modifier
                                            ));
                                        }
                                        last = Some((d.w as usize, d.h as usize, Shot::Gpu(pic)));
                                    }
                                    Some(Err(e)) => {
                                        idn_open_protocol::log::log(&format!(
                                            "screen share: the GPU can't read the desktop's pictures ({e}); through shared memory from now"
                                        ));
                                        nd.store(true, Ordering::Relaxed);
                                        memory_only(&rq);
                                    }
                                    None => {
                                        nd.store(true, Ordering::Relaxed);
                                        memory_only(&rq);
                                    }
                                }
                            }
                            #[cfg(not(feature = "gpu"))]
                            {
                                let _ = &nd;
                                give_back(&rq, d.buf);
                            }
                        }
                        None => {}
                    }
                    // A desktop that never fixes the layout it chose: shared memory.
                    let stalled = aw
                        .lock_ok()
                        .is_some_and(|t| t.elapsed() >= FIXATE_WAIT);
                    if stalled {
                        *aw.lock_ok() = None;
                        idn_open_protocol::log::log(
                            "screen share: the desktop didn't settle its buffers' layout; pictures through memory",
                        );
                        nd.store(true, Ordering::Relaxed);
                        memory_only(&rq);
                    }
                    if let Some((w, h, shot)) = &last {
                        sink(*w, *h, shot.clone());
                    }
                    next = (next + period).max(Instant::now());
                }
            })?;
        // SAFETY: PipeWire's calls in their documented order, under the thread loop's lock once
        // it runs; `state`, the hook and the events live as long as the stream (kept in
        // ScreenStream, freed in Drop after it); the grant's descriptor goes to PipeWire.
        unsafe {
            let name = CString::new("idn-screen").unwrap();
            let tloop = (lib.thread_loop_new)(name.as_ptr(), std::ptr::null());
            if tloop.is_null() {
                return Err(io::Error::other("PipeWire: no thread loop"));
            }
            let context =
                (lib.context_new)((lib.thread_loop_get_loop)(tloop), std::ptr::null_mut(), 0);
            if context.is_null() {
                (lib.thread_loop_destroy)(tloop);
                return Err(io::Error::other("PipeWire: no context"));
            }
            if (lib.thread_loop_start)(tloop) < 0 {
                (lib.context_destroy)(context);
                (lib.thread_loop_destroy)(tloop);
                return Err(io::Error::other("PipeWire: the loop didn't start"));
            }
            (lib.thread_loop_lock)(tloop);
            let fail = |why: &str| {
                (lib.thread_loop_unlock)(tloop);
                (lib.thread_loop_stop)(tloop);
                (lib.context_destroy)(context);
                (lib.thread_loop_destroy)(tloop);
                io::Error::other(format!("PipeWire: {why}"))
            };
            let core =
                (lib.context_connect_fd)(context, grant.fd.into_raw_fd(), std::ptr::null_mut(), 0);
            if core.is_null() {
                return Err(fail("the screen share's remote refused the connection"));
            }
            let props =
                CString::new("media.type=Video media.category=Capture media.role=Screen").unwrap();
            let sname = CString::new("IDN screen").unwrap();
            let stream = (lib.stream_new)(
                core,
                sname.as_ptr(),
                (lib.properties_new_string)(props.as_ptr()),
            );
            if stream.is_null() {
                (lib.core_disconnect)(core);
                return Err(fail("no stream"));
            }
            let state = Box::into_raw(Box::new(State {
                lib,
                stream,
                format: None,
                modifier: None,
                offers: offers.clone(),
                no_dmabuf,
                awaiting,
                tx,
                back,
                pictures: pictures.clone(),
                ended: ended.clone(),
                requeue: requeue.clone(),
            }));
            let events = Box::new(StreamEvents {
                version: 2,
                destroy: None,
                state_changed: Some(on_state),
                control_info: None,
                io_changed: None,
                param_changed: Some(on_param),
                add_buffer: None,
                remove_buffer: Some(on_remove_buffer),
                process: Some(on_process),
                drained: None,
                command: None,
                trigger_done: None,
            });
            let mut hook: Box<SpaHook> = Box::new(std::mem::zeroed());
            (lib.stream_add_listener)(stream, &mut *hook, &*events, state as *mut c_void);
            // DMA-BUF first, where this device imports it (each format with its
            // modifiers), then shared memory.
            let mut pods: Vec<Vec<u8>> = offers
                .iter()
                .map(|(f, m)| pod::enum_format_dmabuf(*f, m, false))
                .collect();
            pods.push(pod::enum_format());
            let mut params: Vec<*const u8> = pods.iter().map(|p| p.as_ptr()).collect();
            // SPA_DIRECTION_INPUT; AUTOCONNECT (buffers mapped here, not by PipeWire).
            let r = (lib.stream_connect)(
                stream,
                0,
                grant.node,
                1 << 0,
                params.as_mut_ptr(),
                params.len() as u32,
            );
            if r < 0 {
                (lib.stream_destroy)(stream);
                (lib.core_disconnect)(core);
                drop(Box::from_raw(state));
                return Err(fail(&format!("the stream didn't connect ({r})")));
            }
            *requeue.live.lock_ok() = Some((lib, tloop as usize, stream as usize));
            (lib.thread_loop_unlock)(tloop);
            Ok(ScreenStream {
                lib,
                tloop,
                context,
                core,
                stream,
                _hook: hook,
                _events: events,
                state,
                requeue,
                pictures,
                ended,
                started: Instant::now(),
            })
        }
    }

    /// `Some` once the share has ended (the user stopped it, the desktop went): why.
    pub fn ended(&self) -> Option<String> {
        self.ended.lock_ok().clone()
    }
}

impl Drop for ScreenStream {
    fn drop(&mut self) {
        // Our thread gives no buffer back from here (it waits for any it is giving back
        // now): the stream goes.
        *self.requeue.live.lock_ok() = None;
        // SAFETY: the objects made in `start`, destroyed once under the loop's lock (no callback
        // runs meanwhile), the State last.
        unsafe {
            (self.lib.thread_loop_lock)(self.tloop);
            (self.lib.stream_destroy)(self.stream);
            (self.lib.core_disconnect)(self.core);
            (self.lib.thread_loop_unlock)(self.tloop);
            (self.lib.thread_loop_stop)(self.tloop);
            (self.lib.context_destroy)(self.context);
            (self.lib.thread_loop_destroy)(self.tloop);
            drop(Box::from_raw(self.state));
        }
    }
}

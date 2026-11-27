//! IDN Source: a network source (IDN or NDI-Compatible) into OBS, with tally back to it.

use crate::ffi;
use crate::{log, setting_opt};
use idn_common::sync::LockOk as _;
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use idn_open_protocol::shq::Fourcc;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

mod properties;
mod receiving;
mod tally;
use properties::{get_defaults, get_properties};
use receiving::receive;
use tally::{recompute_tally, tally_register, tally_unregister};
pub(crate) use tally::{unwatch_frontend, watch_frontend};

const ID: &CStr = c"idn_source";
/// "IDN Source (GPU)" (§E.27).
const ID_GPU: &CStr = c"idn_source_gpu";
const S_SOURCE: &CStr = c"source";
const S_ADDRESS: &CStr = c"address";
const S_PASSPHRASE: &CStr = c"passphrase";
const S_SERVER: &CStr = c"server";
const S_TRUST_DIR: &CStr = c"trust_dir";
const S_USE_NODE: &CStr = c"use_node";
const S_LOW: &CStr = c"low_bandwidth";
const S_RECEIVER: &CStr = c"receiver_name";
/// How OBS times the source's frames. Automatic is buffered for every sender:
/// IDN senders' sound sits within about −12 to +5 ms of the picture
/// buffered, against +14 to +45 ms unbuffered; the frames NDI 6.3 senders seemed to drop
/// under buffered timing didn't happen again in the lab. Kept apart from
/// Buffered so what Automatic means can change without touching saved settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Timing {
    Auto,
    Buffered,
    Unbuffered,
}

const S_TIMING: &CStr = c"timing";
const S_CODEC: &CStr = c"codec_use";
/// `machine` (the IDN Node's setting), `on` or `off`. (Builds before setup2 kept a
/// checkbox under "parallel_decode".)
const S_PARALLEL: &CStr = c"two_at_once";
const S_DECODER: &CStr = c"decoder";
/// SpeedHQ decoding: `auto` (measured), `gpu` or `cpu`.
const S_SHQ: &CStr = c"shq_decode";
const S_HDR: &CStr = c"hdr";
const S_SDR_WHITE: &CStr = c"sdr_white";

/// Shared with the receiving thread: stop, the picture size for OBS, and the tally
/// OBS reports (program = in the program output; preview = in the preview scene).
struct Shared {
    stop: AtomicBool,
    /// What the receiving thread is doing (`Step`), for the log when stopping it takes
    /// long (OBS's exit waited for ever on some source).
    step: std::sync::atomic::AtomicU8,
    width: AtomicU32,
    height: AtomicU32,
    program: AtomicBool,
    preview: AtomicBool,
    tally_changed: AtomicBool,
}

/// The receiving thread's steps, as `Shared::step` holds them.
const STEPS: [&str; 8] = [
    "starting",
    "connecting to the IDN Node",
    "opening the input",
    "waiting for media",
    "handing OBS a picture (obs_source_output_video)",
    "handing OBS sound (obs_source_output_audio)",
    "setting OBS's timing (obs_source_set_async_unbuffered)",
    "closing the input",
];

impl Shared {
    fn step(&self, i: usize) {
        self.step.store(i as u8, Ordering::Relaxed);
    }

    fn new(program: bool, preview: bool) -> Arc<Self> {
        Arc::new(Shared {
            stop: AtomicBool::new(false),
            step: std::sync::atomic::AtomicU8::new(0),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            program: AtomicBool::new(program),
            preview: AtomicBool::new(preview),
            tally_changed: AtomicBool::new(true),
        })
    }

    fn set_tally(&self, program: bool, preview: bool) {
        let changed = self.program.swap(program, Ordering::Relaxed) != program
            || self.preview.swap(preview, Ordering::Relaxed) != preview;
        if changed {
            self.tally_changed.store(true, Ordering::Relaxed);
        }
    }
}

/// One IDN source. `update` runs on OBS's UI thread while `get_width`/`get_height` run
/// on its graphics thread, so the receiving thread's state is swapped under a lock (a
/// plain field replaced by `update` could be read half-freed there).
struct IdnSource {
    source: *mut ffi::obs_source_t,
    /// "IDN Source (GPU)": its GPU and the textures OBS draws (§E.27); `None` for "IDN
    /// Source" (pictures handed over as samples).
    gpu: Option<Arc<crate::gpu_view::GpuView>>,
    shared: Mutex<Arc<Shared>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    /// The settings the receiving thread runs with: OBS calls `update` more than once
    /// for one change (an address typed in: two receiving threads started 0.3 s apart),
    /// and the same settings again don't restart it.
    running: Mutex<Option<Settings>>,
}

/// The source behind OBS's pointer, shared (see [`IdnSource`]).
unsafe fn get<'a>(data: *mut c_void) -> &'a IdnSource {
    // SAFETY: `data` is the Box from `create`, alive until `destroy`; shared, never `&mut`
    // (its fields are under locks or atomic).
    unsafe { &*(data as *const IdnSource) }
}

/// The OBS source pointer, handed to threads and the registry (OBS keeps it valid
/// until `destroy`, which removes it from both first).
#[derive(Clone, Copy)]
struct SourcePtr(*mut ffi::obs_source_t);
// SAFETY: libobs's source calls used with it (output video and sound, active, name) may be
// made from any thread; that the source is still alive is each use's to show.
unsafe impl Send for SourcePtr {}

static WATCHING: AtomicBool = AtomicBool::new(false);

/// What one receiving thread needs, read from the settings.
#[derive(Clone, PartialEq, Eq)]
struct Settings {
    source: String,
    passphrase: Option<String>,
    server: Option<String>,
    trust_dir: Option<String>,
    /// Encryption and identity from the machine's IDN Node.
    use_node: bool,
    low: bool,
    receiver_name: String,
    timing: Timing,
    /// Compressed video from IDN senders that offer it (`codec/1`): never, when the
    /// sender insists (allow), or by preference (Wi-Fi, remote rooms). NDI|HX sources
    /// are compressed regardless.
    codec_use: idn_open_protocol::codec::CodecUse,
    /// PQ/HLG sources: converted to SDR in the plugin (default), or handed to OBS as
    /// HDR frames for OBS's own tone mapping.
    hdr_in_plugin: bool,
    /// The brightness, in nits, an HDR source shows SDR white at (the plugin's tone map).
    sdr_white: u32,
    /// Two pictures decoding at once (4K at 50/60 fps); `None`: the machine's setting.
    parallel_decode: Option<bool>,
    /// Where compressed video is decoded; `None`: the machine's setting.
    decoder: Option<idn_ffmpeg::DecoderChoice>,
    /// SpeedHQ: measured (GPU or CPU decoder), or fixed.
    shq: crate::shq_choice::Prefer,
    /// Video threads for the whole of OBS's IDN sources and outputs; 0 leaves them.
    threads: usize,
}

impl Settings {
    unsafe fn read(settings: *mut ffi::obs_data_t, source: *mut ffi::obs_source_t) -> Self {
        // SAFETY: the caller's settings and source (OBS's, for the callback); the name is
        // copied at once.
        unsafe {
            // A typed-in address wins over the picked source.
            let picked = setting_opt(settings, S_ADDRESS)
                .or_else(|| setting_opt(settings, S_SOURCE))
                .unwrap_or_default();
            // Each OBS source has a receiver name of its own by default: senders keep
            // records by receiver name (sticky encryption, tally), and two IDN sources
            // sharing one name would be taken for one receiver.
            let receiver_name = setting_opt(settings, S_RECEIVER).unwrap_or_else(|| {
                let p = ffi::obs_source_get_name(source);
                let own = if p.is_null() {
                    String::new()
                } else {
                    CStr::from_ptr(p).to_string_lossy().into_owned()
                };
                if own.is_empty() {
                    "OBS".to_owned()
                } else {
                    format!("OBS {own}")
                }
            });
            Settings {
                source: picked,
                passphrase: setting_opt(settings, S_PASSPHRASE),
                server: setting_opt(settings, S_SERVER),
                trust_dir: setting_opt(settings, S_TRUST_DIR),
                use_node: ffi::obs_data_get_bool(settings, S_USE_NODE.as_ptr()),
                low: ffi::obs_data_get_bool(settings, S_LOW.as_ptr()),
                receiver_name,
                timing: match setting_opt(settings, S_TIMING).as_deref() {
                    Some("buffered") => Timing::Buffered,
                    Some("unbuffered") => Timing::Unbuffered,
                    _ => Timing::Auto,
                },
                codec_use: match setting_opt(settings, S_CODEC).as_deref() {
                    Some("never") => idn_open_protocol::codec::CodecUse::Never,
                    Some("prefer") => idn_open_protocol::codec::CodecUse::Prefer,
                    _ => idn_open_protocol::codec::CodecUse::Allow,
                },
                hdr_in_plugin: setting_opt(settings, S_HDR).as_deref() != Some("obs"),
                sdr_white: ffi::obs_data_get_int(settings, S_SDR_WHITE.as_ptr()).clamp(1, 10000)
                    as u32,
                // (`on`, `off`, or `machine`: the IDN Node's setting.)
                parallel_decode: setting_opt(settings, S_PARALLEL)
                    .as_deref()
                    .and_then(idn_common::text::parse_on_off),
                decoder: match setting_opt(settings, S_DECODER).as_deref() {
                    Some("auto") => Some(idn_ffmpeg::DecoderChoice::Auto),
                    Some("gpu") => Some(idn_ffmpeg::DecoderChoice::Gpu),
                    Some("software") => Some(idn_ffmpeg::DecoderChoice::Software),
                    _ => None,
                },
                threads: ffi::obs_data_get_int(settings, crate::S_THREADS.as_ptr()).clamp(0, 64)
                    as usize,
                shq: crate::shq_choice::Prefer::from_setting(
                    setting_opt(settings, S_SHQ).as_deref(),
                ),
            }
        }
    }
}

pub(crate) fn register() {
    // Kept for OBS's lifetime: OBS keeps the pointer.
    let info = Box::leak(Box::new(ffi::obs_source_info {
        id: ID.as_ptr(),
        type_: ffi::obs_source_type_OBS_SOURCE_TYPE_INPUT,
        output_flags: ffi::OBS_SOURCE_ASYNC_VIDEO
            | ffi::OBS_SOURCE_AUDIO
            | ffi::OBS_SOURCE_DO_NOT_DUPLICATE,
        get_name: Some(get_name),
        create: Some(create),
        destroy: Some(destroy),
        get_width: Some(get_width),
        get_height: Some(get_height),
        get_defaults: Some(get_defaults),
        get_properties: Some(get_properties),
        update: Some(update),
        activate: Some(activate),
        deactivate: Some(deactivate),
        show: Some(recompute_cb),
        hide: Some(recompute_cb),
        icon_type: ffi::obs_icon_type_OBS_ICON_TYPE_CAMERA,
        ..Default::default()
    }));
    // SAFETY: `info` is leaked, so it outlives OBS's use of it; its size as built.
    unsafe { ffi::obs_register_source_s(info, std::mem::size_of::<ffi::obs_source_info>()) };
    // "IDN Source (GPU)": drawn by OBS from textures it shares with this plugin (§E.27).
    let gpu_info = Box::leak(Box::new(ffi::obs_source_info {
        id: ID_GPU.as_ptr(),
        type_: ffi::obs_source_type_OBS_SOURCE_TYPE_INPUT,
        // (Custom draw: OBS otherwise calls `video_render` inside its own default effect's
        // pass, where this source's effect drew nothing.)
        output_flags: ffi::OBS_SOURCE_VIDEO
            | ffi::OBS_SOURCE_CUSTOM_DRAW
            | ffi::OBS_SOURCE_AUDIO
            | ffi::OBS_SOURCE_DO_NOT_DUPLICATE,
        get_name: Some(get_name_gpu),
        create: Some(create_gpu),
        video_render: Some(video_render),
        ..*info
    }));
    // SAFETY: as above.
    unsafe { ffi::obs_register_source_s(gpu_info, std::mem::size_of::<ffi::obs_source_info>()) };
}

unsafe extern "C" fn get_name_gpu(_: *mut c_void) -> *const c_char {
    c"IDN Source (GPU)".as_ptr()
}

unsafe extern "C" fn create_gpu(
    settings: *mut ffi::obs_data_t,
    source: *mut ffi::obs_source_t,
) -> *mut c_void {
    let gpu = match crate::gpu_view::GpuView::new() {
        Ok(g) => Some(Arc::new(g)),
        Err(e) => {
            log(ffi::LOG_WARNING, &format!("IDN Source (GPU): {e}"));
            None
        }
    };
    let s = Box::new(IdnSource {
        source,
        gpu,
        shared: Mutex::new(Shared::new(false, false)),
        thread: Mutex::new(None),
        running: Mutex::new(None),
    });
    let p = Box::into_raw(s);
    // SAFETY: `p` was just made from a Box, and the settings are OBS's for this call.
    unsafe { update(p as *mut c_void, settings) };
    p as *mut c_void
}

unsafe extern "C" fn video_render(data: *mut c_void, _effect: *mut ffi::gs_effect_t) {
    // SAFETY: `data` is the source from `create_gpu`, alive until `destroy`.
    if let Some(g) = &unsafe { get(data) }.gpu {
        g.render();
    }
}

unsafe extern "C" fn get_name(_: *mut c_void) -> *const c_char {
    c"IDN Source".as_ptr()
}

unsafe extern "C" fn create(
    settings: *mut ffi::obs_data_t,
    source: *mut ffi::obs_source_t,
) -> *mut c_void {
    let s = Box::new(IdnSource {
        source,
        gpu: None,
        shared: Mutex::new(Shared::new(false, false)),
        thread: Mutex::new(None),
        running: Mutex::new(None),
    });
    let p = Box::into_raw(s);
    // SAFETY: `p` was just made from a Box, and the settings are OBS's for this call.
    unsafe { update(p as *mut c_void, settings) };
    p as *mut c_void
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    // SAFETY: `data` is the Box from `create` (or `create_gpu`); OBS calls `destroy` once,
    // last.
    let s = unsafe { Box::from_raw(data as *mut IdnSource) };
    tally_unregister(s.source);
    s.stop_thread();
}

unsafe extern "C" fn update(data: *mut c_void, settings: *mut ffi::obs_data_t) {
    // SAFETY: `data` is the source from `create`, alive until `destroy`; the settings are
    // OBS's for this call.
    let s = unsafe { get(data) };
    let cfg = unsafe { Settings::read(settings, s.source) };
    {
        let mut running = s.running.lock_ok();
        let alive = s
            .thread
            .lock_ok()
            .as_ref()
            .is_some_and(|t| !t.is_finished());
        if alive && running.as_ref() == Some(&cfg) {
            return;
        }
        *running = Some(cfg.clone());
    }
    s.stop_thread();
    tally_unregister(s.source);
    if cfg.source.is_empty() {
        return;
    }
    // SAFETY: our source, alive until `destroy`.
    unsafe { ffi::obs_source_set_async_unbuffered(s.source, cfg.timing == Timing::Unbuffered) };
    let shared = {
        let mut cur = s.shared();
        *cur = Shared::new(
            cur.program.load(Ordering::Relaxed),
            cur.preview.load(Ordering::Relaxed),
        );
        cur.clone()
    };
    tally_register(s.source, shared.clone());
    let source = SourcePtr(s.source);
    let gpu = s.gpu.clone();
    *s.thread.lock_ok() = std::thread::Builder::new()
        .name("idn-obs-source".into())
        .spawn(move || receive(cfg, shared, source, gpu))
        .ok();
    if WATCHING.load(Ordering::Relaxed) {
        recompute_tally();
    }
}

impl IdnSource {
    fn shared(&self) -> std::sync::MutexGuard<'_, Arc<Shared>> {
        self.shared.lock_ok()
    }

    fn stop_thread(&self) {
        self.shared().stop.store(true, Ordering::Relaxed);
        let t = self.thread.lock_ok().take();
        if let Some(t) = t {
            // Waited for as before, but said in the log when it takes long, and where the
            // thread is.
            let shared = self.shared().clone();
            let t0 = std::time::Instant::now();
            let mut next = Duration::from_secs(2);
            while !t.is_finished() {
                if t0.elapsed() >= next {
                    log(
                        ffi::LOG_WARNING,
                        &format!(
                            "source: its receiving thread is still busy after {} s: {}",
                            t0.elapsed().as_secs(),
                            STEPS[(shared.step.load(Ordering::Relaxed) as usize)
                                .min(STEPS.len() - 1)]
                        ),
                    );
                    next += Duration::from_secs(5);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = t.join();
        }
    }
}

unsafe extern "C" fn activate(data: *mut c_void) {
    // SAFETY: `data` is the source from `create`, alive until `destroy`.
    let s = unsafe { get(data) };
    s.shared().set_tally(true, false);
    if WATCHING.load(Ordering::Relaxed) {
        recompute_tally();
    }
}
unsafe extern "C" fn deactivate(data: *mut c_void) {
    // SAFETY: `data` is the source from `create`, alive until `destroy`.
    let s = unsafe { get(data) };
    let shared = s.shared();
    shared.set_tally(false, shared.preview.load(Ordering::Relaxed));
    drop(shared);
    if WATCHING.load(Ordering::Relaxed) {
        recompute_tally();
    }
}
unsafe extern "C" fn recompute_cb(_: *mut c_void) {
    if WATCHING.load(Ordering::Relaxed) {
        recompute_tally();
    }
}

unsafe extern "C" fn get_width(data: *mut c_void) -> u32 {
    // SAFETY: `data` is the source from `create`, alive until `destroy`.
    let s = unsafe { get(data) };
    match &s.gpu {
        Some(g) => g.size().0,
        None => s.shared().width.load(Ordering::Relaxed),
    }
}
unsafe extern "C" fn get_height(data: *mut c_void) -> u32 {
    // SAFETY: `data` is the source from `create`, alive until `destroy`.
    let s = unsafe { get(data) };
    match &s.gpu {
        Some(g) => g.size().1,
        None => s.shared().height.load(Ordering::Relaxed),
    }
}

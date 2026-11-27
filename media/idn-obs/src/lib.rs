//! OBS Studio plugin on the IDN Open Protocol: IDN sources (the network into OBS) and
//! IDN outputs (OBS's program out to the network). Built against the installed OBS's
//! public plugin API (see `build.rs`); design and status in `README.md`.
//!
//! The plugin as distributed is GPL (it links libobs); the library it uses stays
//! MIT OR Apache-2.0.

#![allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code
)]

use idn_common::sync::LockOk as _;

/// libobs's C API, generated at build time from the installed headers.
pub mod ffi {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

mod gpu_view;
mod output;
mod panel;
mod routed;
mod shq_choice;
mod source;

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};

/// What `OBS_DECLARE_MODULE()` expands to, written out: OBS hands the module its
/// pointer and asks the API version it was built for.
static mut MODULE: *mut ffi::obs_module_t = std::ptr::null_mut();

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_set_pointer(module: *mut ffi::obs_module_t) {
    // SAFETY: OBS calls this once, at load, before any other call into the module.
    unsafe { MODULE = module };
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_current_module() -> *mut ffi::obs_module_t {
    // SAFETY: written once at load (above), before anything reads it; no reference is
    // taken.
    unsafe { MODULE }
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_ver() -> u32 {
    // MAKE_SEMANTIC_VERSION(major, minor, patch) of the headers this was built against.
    (ffi::LIBOBS_API_MAJOR_VER << 24)
        | (ffi::LIBOBS_API_MINOR_VER << 16)
        | ffi::LIBOBS_API_PATCH_VER
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_name() -> *const c_char {
    c"IDN Open Protocol".as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_description() -> *const c_char {
    c"IDN sources and outputs (NDI-Compatible network video) on the IDN Open Protocol".as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_load() -> bool {
    idn_open_protocol::log::set_logger(library_log);
    // Another copy of this plugin loaded first (an old one left in a system folder): its
    // sources are the ones OBS uses, and two copies' threads crashed OBS at exit. This one
    // stays off and says where it is.
    // SAFETY: a NUL-ended id; a plain libobs call during module load.
    if !unsafe { ffi::obs_source_get_display_name(c"idn_source".as_ptr()) }.is_null() {
        log(
            ffi::LOG_WARNING,
            &format!(
                "idn-obs {} ({}): another IDN plugin is already loaded and is the one in use; this copy ({}) stays off. Remove the other one (see the modules loaded above).",
                env!("CARGO_PKG_VERSION"),
                idn_open_protocol::build_line(),
                own_path()
            ),
        );
        return false;
    }
    // Never unloaded: the library's threads (network, discovery, the Node) may still be
    // running when OBS frees its modules at exit.
    pin();
    source::register();
    routed::register();
    output::register();
    log(
        ffi::LOG_INFO,
        &format!(
            "idn-obs {} ({}) loaded (libobs API {}.{}.{})",
            env!("CARGO_PKG_VERSION"),
            idn_open_protocol::build_line(),
            ffi::LIBOBS_API_MAJOR_VER,
            ffi::LIBOBS_API_MINOR_VER,
            ffi::LIBOBS_API_PATCH_VER
        ),
    );
    true
}

/// After the frontend exists: its events drive the sources' tally; the IDN Output's
/// settings page and its Tools menu entry.
#[unsafe(no_mangle)]
pub extern "C" fn obs_module_post_load() {
    source::watch_frontend();
    panel::start();
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_unload() {
    panel::stop();
    source::unwatch_frontend();
    // From here on nothing goes to OBS's log: the library's threads (a receiver's session
    // ending, a link closing) outlive the module, and OBS frees its log handler as it
    // exits; a line written then took OBS down after "Freeing OBS context data".
    UNLOADED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Set by `obs_module_unload`.
static UNLOADED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// This library's file.
#[cfg(unix)]
fn own_path() -> String {
    // SAFETY: `Dl_info` is plain pointers and integers; all zeros is a valid value.
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    let addr = obs_module_load as *const libc::c_void;
    // SAFETY: an address in this library and a `Dl_info` to fill.
    if unsafe { libc::dladdr(addr, &mut info) } == 0 || info.dli_fname.is_null() {
        return "?".into();
    }
    // SAFETY: checked non-null above; the loader's string for this library, which stays
    // loaded.
    unsafe { CStr::from_ptr(info.dli_fname) }
        .to_string_lossy()
        .into_owned()
}

/// Kept loaded until the process ends (`RTLD_NODELETE`).
#[cfg(unix)]
fn pin() {
    let Ok(path) = CString::new(own_path()) else {
        return;
    };
    // A second reference that is never closed, and marks the library not to be unloaded.
    // SAFETY: a NUL-ended path; the handle is kept on purpose (never closed).
    unsafe {
        libc::dlopen(
            path.as_ptr(),
            libc::RTLD_NOW | libc::RTLD_NOLOAD | libc::RTLD_NODELETE,
        )
    };
}

#[cfg(windows)]
fn own_module(flags: u32) -> windows_sys::Win32::Foundation::HMODULE {
    use windows_sys::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GetModuleHandleExW,
    };
    let mut h = std::ptr::null_mut();
    // SAFETY: an address in this module and a handle to fill.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | flags,
            obs_module_load as *const u16,
            &mut h,
        )
    };
    h
}

#[cfg(windows)]
fn own_path() -> String {
    use windows_sys::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleFileNameW,
    };
    let h = own_module(GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT);
    let mut buf = [0u16; 1024];
    // SAFETY: the buffer and its length in u16s; the count returned is within it.
    let n = unsafe { GetModuleFileNameW(h, buf.as_mut_ptr(), buf.len() as u32) } as usize;
    String::from_utf16_lossy(&buf[..n.min(buf.len())])
}

/// Kept loaded until the process ends.
#[cfg(windows)]
fn pin() {
    own_module(windows_sys::Win32::System::LibraryLoader::GET_MODULE_HANDLE_EX_FLAG_PIN);
}

/// The library's log lines, into OBS's log.
fn library_log(msg: &str) {
    log(ffi::LOG_INFO, &format!("idn: {msg}"));
}

/// One line into OBS's log (`blog`), with the text passed as an argument so that '%'
/// in it is harmless.
pub(crate) fn log(level: u32, msg: &str) {
    if UNLOADED.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let Ok(c) = CString::new(msg.replace('\0', " ")) else {
        return;
    };
    // SAFETY: one "%s" in the format and one NUL-ended string for it.
    unsafe { ffi::blog(level as c_int, c"[idn-obs] %s".as_ptr(), c.as_ptr()) };
}

/// A string setting, owned.
pub(crate) unsafe fn setting_str(settings: *mut ffi::obs_data_t, name: &CStr) -> String {
    // SAFETY: the caller's settings (this function's contract) and a NUL-ended name.
    let p = unsafe { ffi::obs_data_get_string(settings, name.as_ptr()) };
    if p.is_null() {
        return String::new();
    }
    // SAFETY: checked non-null; the settings own the string, copied here before returning.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// A non-empty string setting, or `None`.
pub(crate) unsafe fn setting_opt(settings: *mut ffi::obs_data_t, name: &CStr) -> Option<String> {
    // SAFETY: the caller's settings, as `setting_str` asks.
    let s = unsafe { setting_str(settings, name) };
    (!s.trim().is_empty()).then(|| s.trim().to_owned())
}

/// This machine's name, for IDN names ("HOST (name)").
pub(crate) fn hostname() -> String {
    idn_open_protocol::discovery::hostname()
}

/// The encryption settings shared by the source and the output: a passphrase
/// (`secure/1`) and/or site trust through a Discovery Server and a trust directory
/// (`secure/2`). `None` when neither is set.
/// The machine's IDN Node, connected once per OBS process (spec/extensions/idn-node.md
/// §N.5); asked again on each use until one answers.
pub(crate) fn node() -> Option<std::sync::Arc<idn_open_protocol::node::Node>> {
    static NODE: std::sync::Mutex<Option<std::sync::Arc<idn_open_protocol::node::Node>>> =
        std::sync::Mutex::new(None);
    let mut h = NODE.lock_ok();
    if h.is_none() {
        match idn_open_protocol::node::Node::connect("OBS Studio") {
            Ok(x) => {
                log(
                    ffi::LOG_INFO,
                    &format!(
                        "IDN Node: using {}'s site identity ({:?})",
                        x.device_name(),
                        x.enrollment().state()
                    ),
                );
                *h = Some(std::sync::Arc::new(x));
            }
            Err(e) => log(ffi::LOG_INFO, &format!("IDN Node: not used ({e})")),
        }
    }
    h.clone()
}

/// What a source or output connects with: encryption, the `link/1` identity and the
/// Discovery Server; from the IDN Node when `use_node` and one runs, else from the fields.
pub(crate) struct NetConfig {
    pub secure: Option<idn_open_protocol::secure::SecureConfig>,
    pub link: Option<idn_open_protocol::link::LinkConfig>,
    pub server: Option<String>,
}

pub(crate) fn net_config(
    use_node: bool,
    passphrase: Option<&str>,
    server: Option<&str>,
    trust_dir: Option<&str>,
    name: &str,
    role: idn_open_protocol::trust::DeviceRole,
) -> NetConfig {
    if use_node && let Some(h) = node() {
        let node_server = h
            .status()
            .ok()
            .and_then(|s| s.attr("server").map(str::to_owned))
            .filter(|s| !s.is_empty());
        return NetConfig {
            secure: Some(h.secure_config()),
            link: Some(h.link_config()),
            server: server.map(str::to_owned).or(node_server),
        };
    }
    NetConfig {
        secure: secure_config(passphrase, server, trust_dir, name, role),
        link: None,
        server: server.map(str::to_owned),
    }
}

pub(crate) fn secure_config(
    passphrase: Option<&str>,
    server: Option<&str>,
    trust_dir: Option<&str>,
    name: &str,
    role: idn_open_protocol::trust::DeviceRole,
) -> Option<idn_open_protocol::secure::SecureConfig> {
    use idn_open_protocol::secure::SecureConfig;
    if let (Some(dir), Some(server)) = (trust_dir, server) {
        match idn_open_protocol::trust::Enrollment::start(
            idn_open_protocol::trust::EnrollmentConfig {
                discovery_server: server.to_owned(),
                state_dir: dir.into(),
                name: name.to_owned(),
                role,
                passphrase: passphrase.map(str::to_owned),
                admin_code: None,
            },
        ) {
            Ok(e) => return Some(SecureConfig::with_site(e)),
            Err(e) => log(
                ffi::LOG_WARNING,
                &format!("site trust: {e}; using the passphrase only"),
            ),
        }
    }
    let p = passphrase?;
    match SecureConfig::new(p) {
        Ok(c) => Some(c),
        Err(e) => {
            log(
                ffi::LOG_WARNING,
                &format!("passphrase: {e}; not encrypting"),
            );
            None
        }
    }
}

/// Translates OBS's clock (`os_gettime_ns`, monotonic) to IDN's timestamps (100 ns
/// since the Unix epoch) by the two clocks' relation, read side by side, so that a
/// frame's timestamp is the time it was rendered or captured, in wall-clock terms,
/// whatever the pipeline's delay before it reaches the plugin. Picture and sound are
/// translated alike, so they leave with the relation OBS gave them (pinning the first
/// frame to its arrival time put every timestamp 80 ms ahead).
pub(crate) struct ClockMap {
    /// wall (100 ns) − monotonic (100 ns), refreshed now and then (the clocks drift).
    offset: i64,
    measured: std::time::Instant,
}

impl ClockMap {
    pub(crate) fn new() -> Self {
        ClockMap {
            offset: Self::measure(),
            measured: std::time::Instant::now(),
        }
    }

    fn measure() -> i64 {
        // SAFETY: no arguments.
        let mono = unsafe { ffi::os_gettime_ns() } as i64 / 100;
        now_100ns() - mono
    }

    pub(crate) fn map(&mut self, obs_ns: u64) -> i64 {
        if self.measured.elapsed() > std::time::Duration::from_secs(10) {
            self.offset = Self::measure();
            self.measured = std::time::Instant::now();
        }
        (obs_ns / 100) as i64 + self.offset
    }
}

pub(crate) fn now_100ns() -> i64 {
    idn_common::epoch::now().as_nanos() as i64 / 100
}

/// The video threads setting, on sources and outputs alike: the library has one pool
/// for the whole process.
pub(crate) const S_THREADS: &CStr = c"video_threads";

/// Adds the video threads setting to a properties page.
pub(crate) unsafe fn add_threads_property(props: *mut ffi::obs_properties_t) {
    let label = format!(
        "Video threads, shared by every IDN source and output in OBS (0: the machine's \
         setting in the IDN Node, else automatic: {} here; the one set last wins)",
        idn_open_protocol::default_worker_threads()
    );
    let label = std::ffi::CString::new(label).unwrap_or_default();
    // SAFETY: the caller's properties page; NUL-ended strings that outlive the call.
    unsafe {
        ffi::obs_properties_add_int(props, S_THREADS.as_ptr(), label.as_ptr(), 0, 64, 1);
    }
}

/// Sets the library's pool from a source's or output's video threads (0 leaves it).
pub(crate) fn apply_threads(n: usize) {
    if n > 0 && n != idn_open_protocol::worker_threads() {
        idn_open_protocol::set_worker_threads(n);
        log(ffi::LOG_INFO, &format!("video threads: {n}"));
    }
}

/// FFmpeg, probed once: decodes NDI|HX and `codec/1` pictures for the sources and
/// encodes for the outputs. No VA-API device is looked for off Linux.
pub(crate) fn ffmpeg() -> std::sync::Arc<idn_ffmpeg::Ffmpeg> {
    static FF: std::sync::OnceLock<std::sync::Arc<idn_ffmpeg::Ffmpeg>> = std::sync::OnceLock::new();
    FF.get_or_init(|| {
        #[cfg(target_os = "linux")]
        let ff = idn_ffmpeg::Ffmpeg::probe();
        #[cfg(not(target_os = "linux"))]
        let ff = idn_ffmpeg::Ffmpeg::probe_device(None);
        std::sync::Arc::new(ff)
    })
    .clone()
}

/// [`ffmpeg`] decoding as `choice` (one handle per choice, made once).
pub(crate) fn ffmpeg_decoding(
    choice: Option<idn_ffmpeg::DecoderChoice>,
) -> std::sync::Arc<idn_ffmpeg::Ffmpeg> {
    use idn_ffmpeg::DecoderChoice as D;
    // No choice: the probed FFmpeg, which takes the machine's setting at each decoder.
    let Some(choice) = choice else {
        return ffmpeg();
    };
    static BY: std::sync::OnceLock<[std::sync::Arc<idn_ffmpeg::Ffmpeg>; 3]> =
        std::sync::OnceLock::new();
    let all = BY.get_or_init(|| {
        let ff = ffmpeg();
        [D::Auto, D::Gpu, D::Software].map(|c| std::sync::Arc::new(ff.with_decoder(c)))
    });
    all[match choice {
        D::Auto => 0,
        D::Gpu => 1,
        D::Software => 2,
    }]
    .clone()
}

/// A `void*` from `Box::into_raw`, back to a reference.
pub(crate) unsafe fn this<'a, T>(data: *mut c_void) -> &'a mut T {
    // SAFETY: the caller's promise: `data` is a live `Box<T>` from `Box::into_raw`, not
    // borrowed elsewhere meanwhile.
    unsafe { &mut *(data as *mut T) }
}

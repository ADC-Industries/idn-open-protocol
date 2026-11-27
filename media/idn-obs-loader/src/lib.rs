//! `idn_obs.dll` on Windows: the file OBS Studio loads as the IDN plugin.
//!
//! The plugin binds to the FFmpeg OBS ships (it decodes NDI|HX and codec/1 video through
//! it), and that FFmpeg changes with OBS's major versions: OBS 32 ships FFmpeg 7.1
//! (`avcodec-61.dll`), OBS 33 FFmpeg 8.1 (`avcodec-62.dll`). One plugin built for one of
//! them doesn't load in the other (Windows' error 126: a DLL it needs isn't there), and
//! an OBS update broke it silently (OBS 33.0.1).
//!
//! So this small library, without FFmpeg of its own, is what OBS loads. It looks in
//! OBS's own folder for the FFmpeg there, loads the plugin built for it from the plugin's
//! data folder (`data/obs-plugins/idn_obs/bin/idn_obs_ffmpeg8.dll` or
//! `…_ffmpeg7.dll`: not in `obs-plugins/64bit`, where OBS would load them as modules of
//! their own), and passes OBS's module calls on to it. When OBS has neither, the plugin
//! stays off and OBS's log says why.

#![cfg(windows)]

use std::ffi::{CString, c_char, c_void};
use std::os::windows::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicPtr, Ordering};

use windows_sys::Win32::Foundation::{GetLastError, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleFileNameW, GetModuleHandleExW, GetModuleHandleW, GetProcAddress,
    LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LoadLibraryExW,
};

/// The plugin's builds, newest first: the FFmpeg library that marks OBS's FFmpeg, and the
/// plugin built against it.
const BUILDS: [(&str, &str); 2] = [
    ("avcodec-62.dll", "idn_obs_ffmpeg8.dll"),
    ("avcodec-61.dll", "idn_obs_ffmpeg7.dll"),
];

/// libobs's API version the builds were made against (OBS 32.1: `MAKE_SEMANTIC_VERSION`),
/// answered when no build loaded (OBS still asks before it calls `obs_module_load`).
const API_VER: u32 = (32 << 24) | (1 << 16);

/// libobs's `LOG_ERROR`.
const LOG_ERROR: i32 = 100;
/// libobs's LOG_WARNING.
const LOG_WARNING: i32 = 200;

/// The loaded build's module functions.
struct Plugin {
    set_pointer: unsafe extern "C" fn(*mut c_void),
    ver: unsafe extern "C" fn() -> u32,
    load: unsafe extern "C" fn() -> bool,
    post_load: Option<unsafe extern "C" fn()>,
    unload: Option<unsafe extern "C" fn()>,
}

static PLUGIN: OnceLock<Result<Plugin, String>> = OnceLock::new();
static MODULE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

fn plugin() -> Result<&'static Plugin, &'static str> {
    PLUGIN.get_or_init(open).as_ref().map_err(|e| e.as_str())
}

/// A module's file (`None`: the program's).
fn module_file(module: HMODULE) -> Option<PathBuf> {
    let mut buf = vec![0u16; 32768];
    // SAFETY: the buffer and its length in characters; a null module is the program.
    let n = unsafe { GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32) } as usize;
    (n > 0 && n < buf.len()).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..n])))
}

/// This library's own file.
fn own_file() -> Option<PathBuf> {
    let mut module: HMODULE = std::ptr::null_mut();
    // SAFETY: an address inside this library; the handle isn't kept (no reference
    // counted).
    let ok = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            own_file as *const () as *const u16,
            &mut module,
        )
    };
    if ok == 0 { None } else { module_file(module) }
}

/// `p` NUL-ended in UTF-16. (Its own copy of idn_common::win::wide_os: this DLL is OBS's
/// plugin file, kept to windows-sys alone so that it loads in any OBS.)
fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// Whether OBS has this FFmpeg library beside obs64.exe (its own: what decides).
fn obs_ships(dll: &str, obs_bin: Option<&Path>) -> bool {
    obs_bin.is_some_and(|d| d.join(dll).is_file())
}

/// Whether this FFmpeg library is loaded in the process (perhaps another plugin's own).
fn loaded(dll: &str) -> bool {
    let name = wide(Path::new(dll));
    // SAFETY: a NUL-ended name; the handle isn't kept.
    !unsafe { GetModuleHandleW(name.as_ptr()) }.is_null()
}

/// The builds to try, best first (bug run H): those whose FFmpeg OBS ships beside
/// obs64.exe, then those whose FFmpeg is loaded from elsewhere (an FFmpeg 8 another plugin
/// brought into an OBS 32 chose the wrong build, and no other was tried).
fn candidates(obs_bin: Option<&Path>) -> Vec<(&'static str, &'static str)> {
    let mut v: Vec<_> = BUILDS
        .iter()
        .copied()
        .filter(|(f, _)| obs_ships(f, obs_bin))
        .collect();
    for b in BUILDS {
        if loaded(b.0) && !v.contains(&b) {
            v.push(b);
        }
    }
    v
}

/// The build for this OBS's FFmpeg, loaded.
fn open() -> Result<Plugin, String> {
    let obs = module_file(std::ptr::null_mut());
    let obs_bin = obs.as_deref().and_then(Path::parent);
    let own = own_file().ok_or("IDN plugin: couldn't find its own file")?;
    // obs-plugins\64bit\idn_obs.dll → data\obs-plugins\idn_obs\bin
    let root = own
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or("IDN plugin: not in OBS's obs-plugins\\64bit folder")?;
    let bin = root
        .join("data")
        .join("obs-plugins")
        .join("idn_obs")
        .join("bin");
    let builds = candidates(obs_bin);
    if builds.is_empty() {
        return Err(format!(
            "IDN plugin: this OBS Studio has neither FFmpeg 8 (avcodec-62.dll) nor FFmpeg 7 \
             (avcodec-61.dll) in {}; the IDN plugin is made for OBS Studio 32 and 33. \
             Install the IDN Media version made for this OBS Studio.",
            obs_bin.map_or("its folder".into(), |d| d.display().to_string())
        ));
    }
    // Each in turn until one loads (the errors of those that didn't, together).
    let mut errors = Vec::new();
    let mut found = None;
    for (ffmpeg, file) in builds {
        let path = bin.join(file);
        if !path.is_file() {
            errors.push(format!(
                "OBS Studio has {ffmpeg}, but {} is missing; run IDN Media's installer again \
                 (with OBS Studio closed)",
                path.display()
            ));
            continue;
        }
        // The plugin's own folder, then the program's (OBS's FFmpeg) and System32: never
        // the working folder or PATH.
        // SAFETY: a NUL-ended absolute path; no file handle.
        let module = unsafe {
            LoadLibraryExW(
                wide(&path).as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            )
        };
        if module.is_null() {
            // SAFETY: plain call, right after the failed one.
            let err = unsafe { GetLastError() };
            errors.push(format!(
                "couldn't load {} (Windows error {err}{})",
                path.display(),
                if err == 126 {
                    ": a DLL it needs is missing"
                } else {
                    ""
                }
            ));
            continue;
        }
        found = Some((module, path));
        break;
    }
    let Some((module, path)) = found else {
        return Err(format!("IDN plugin: {}", errors.join("; ")));
    };
    if !errors.is_empty() {
        log(
            LOG_WARNING,
            &format!(
                "IDN plugin: {}; loaded {} instead",
                errors.join("; "),
                path.display()
            ),
        );
    }
    let sym = |name: &str| {
        let c = CString::new(name).expect("no NUL");
        // SAFETY: a loaded module and a NUL-ended name.
        unsafe { GetProcAddress(module, c.as_ptr().cast()) }
    };
    let need = |name: &str| {
        sym(name).ok_or_else(|| format!("IDN plugin: {} has no {name}", path.display()))
    };
    // SAFETY: the plugin's exports have these signatures (media/idn-obs/src/lib.rs).
    unsafe {
        Ok(Plugin {
            set_pointer: std::mem::transmute::<
                unsafe extern "system" fn() -> isize,
                unsafe extern "C" fn(*mut c_void),
            >(need("obs_module_set_pointer")?),
            ver: std::mem::transmute::<
                unsafe extern "system" fn() -> isize,
                unsafe extern "C" fn() -> u32,
            >(need("obs_module_ver")?),
            load: std::mem::transmute::<
                unsafe extern "system" fn() -> isize,
                unsafe extern "C" fn() -> bool,
            >(need("obs_module_load")?),
            post_load: sym("obs_module_post_load").map(|f| {
                std::mem::transmute::<unsafe extern "system" fn() -> isize, unsafe extern "C" fn()>(
                    f,
                )
            }),
            unload: sym("obs_module_unload").map(|f| {
                std::mem::transmute::<unsafe extern "system" fn() -> isize, unsafe extern "C" fn()>(
                    f,
                )
            }),
        })
    }
}

/// A line in OBS's log (libobs's `blog`, found at run time: nothing linked).
fn log(level: i32, text: &str) {
    let name = wide(Path::new("obs.dll"));
    // SAFETY: a NUL-ended name; the handle isn't kept.
    let obs = unsafe { GetModuleHandleW(name.as_ptr()) };
    if obs.is_null() {
        return;
    }
    // SAFETY: a loaded module and a NUL-ended name.
    let Some(f) = (unsafe { GetProcAddress(obs, c"blog".as_ptr().cast()) }) else {
        return;
    };
    // SAFETY: libobs's `void blog(int log_level, const char *format, ...)`.
    let blog: unsafe extern "C" fn(i32, *const c_char, ...) = unsafe { std::mem::transmute(f) };
    let text = CString::new(text.replace('\0', " ")).expect("no NUL");
    // SAFETY: "%s" and one NUL-ended string.
    unsafe { blog(level, c"%s".as_ptr(), text.as_ptr()) };
}

// The pointer is OBS's, passed on as it came; nothing here reads through it.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn obs_module_set_pointer(module: *mut c_void) {
    MODULE.store(module, Ordering::Relaxed);
    if let Ok(p) = plugin() {
        // SAFETY: the plugin's export; OBS's module pointer, as OBS hands it.
        unsafe { (p.set_pointer)(module) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_current_module() -> *mut c_void {
    MODULE.load(Ordering::Relaxed)
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_ver() -> u32 {
    match plugin() {
        // SAFETY: the plugin's export.
        Ok(p) => unsafe { (p.ver)() },
        Err(_) => API_VER,
    }
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
    match plugin() {
        // SAFETY: the plugin's export, called once, as OBS calls it.
        Ok(p) => unsafe { (p.load)() },
        Err(e) => {
            log(LOG_ERROR, e);
            false
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_post_load() {
    if let Ok(Plugin {
        post_load: Some(f), ..
    }) = plugin()
    {
        // SAFETY: the plugin's export.
        unsafe { f() };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_unload() {
    if let Ok(Plugin {
        unload: Some(f), ..
    }) = plugin()
    {
        // SAFETY: the plugin's export. The plugin stays loaded (its threads may outlive
        // OBS's unload; it pins itself).
        unsafe { f() };
    }
}

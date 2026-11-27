//! libpipewire, loaded at run time: the few calls and structures the stream uses.

use super::*;

// ---------------------------------------------------------------------------------------
// libpipewire, by hand (its ABI, from spa/ and pipewire/ headers, MIT).

#[repr(C)]
pub(super) struct SpaList {
    pub(super) next: *mut c_void,
    pub(super) prev: *mut c_void,
}

#[repr(C)]
pub(super) struct SpaHook {
    pub(super) link: SpaList,
    pub(super) funcs: *const c_void,
    pub(super) data: *mut c_void,
    pub(super) removed: *mut c_void,
    pub(super) priv_: *mut c_void,
}

pub(super) type Cb = Option<unsafe extern "C" fn()>;

#[repr(C)]
pub(super) struct StreamEvents {
    pub(super) version: u32,
    pub(super) destroy: Cb,
    pub(super) state_changed: Option<unsafe extern "C" fn(*mut c_void, c_int, c_int, *const c_char)>,
    pub(super) control_info: Cb,
    pub(super) io_changed: Cb,
    pub(super) param_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *const u8)>,
    pub(super) add_buffer: Cb,
    pub(super) remove_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut PwBuffer)>,
    pub(super) process: Option<unsafe extern "C" fn(*mut c_void)>,
    pub(super) drained: Cb,
    pub(super) command: Cb,
    pub(super) trigger_done: Cb,
}

#[repr(C)]
pub(super) struct PwBuffer {
    pub(super) buffer: *mut SpaBuffer,
}

#[repr(C)]
pub(super) struct SpaBuffer {
    pub(super) n_metas: u32,
    pub(super) n_datas: u32,
    pub(super) metas: *mut c_void,
    pub(super) datas: *mut SpaData,
}

#[repr(C)]
pub(super) struct SpaData {
    pub(super) kind: u32,
    pub(super) flags: u32,
    pub(super) fd: i64,
    pub(super) mapoffset: u32,
    pub(super) maxsize: u32,
    pub(super) data: *mut c_void,
    pub(super) chunk: *mut SpaChunk,
}

#[repr(C)]
pub(super) struct SpaChunk {
    pub(super) offset: u32,
    pub(super) size: u32,
    pub(super) stride: i32,
    pub(super) flags: i32,
}

pub(super) struct Lib {
    pub(super) _lib: libloading::Library,
    pub(super) init: unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char),
    pub(super) thread_loop_new: unsafe extern "C" fn(*const c_char, *const c_void) -> *mut c_void,
    pub(super) thread_loop_get_loop: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    pub(super) thread_loop_start: unsafe extern "C" fn(*mut c_void) -> c_int,
    pub(super) thread_loop_stop: unsafe extern "C" fn(*mut c_void),
    pub(super) thread_loop_lock: unsafe extern "C" fn(*mut c_void),
    pub(super) thread_loop_unlock: unsafe extern "C" fn(*mut c_void),
    pub(super) thread_loop_destroy: unsafe extern "C" fn(*mut c_void),
    pub(super) context_new: unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void,
    pub(super) context_connect_fd: unsafe extern "C" fn(*mut c_void, c_int, *mut c_void, usize) -> *mut c_void,
    pub(super) context_destroy: unsafe extern "C" fn(*mut c_void),
    pub(super) core_disconnect: unsafe extern "C" fn(*mut c_void) -> c_int,
    pub(super) properties_new_string: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    pub(super) stream_new: unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void) -> *mut c_void,
    pub(super) stream_add_listener:
        unsafe extern "C" fn(*mut c_void, *mut SpaHook, *const StreamEvents, *mut c_void),
    pub(super) stream_connect:
        unsafe extern "C" fn(*mut c_void, c_int, u32, c_int, *mut *const u8, u32) -> c_int,
    pub(super) stream_update_params: unsafe extern "C" fn(*mut c_void, *mut *const u8, u32) -> c_int,
    pub(super) stream_dequeue_buffer: unsafe extern "C" fn(*mut c_void) -> *mut PwBuffer,
    pub(super) stream_queue_buffer: unsafe extern "C" fn(*mut c_void, *mut PwBuffer) -> c_int,
    pub(super) stream_destroy: unsafe extern "C" fn(*mut c_void),
}

pub(super) fn lib() -> io::Result<&'static Lib> {
    static LIB: OnceLock<Result<Lib, String>> = OnceLock::new();
    // SAFETY: libpipewire's entry points, typed as its headers declare them, kept loaded
    // with the pointers; pw_init takes null arguments.
    LIB.get_or_init(|| unsafe {
        let l = libloading::Library::new("libpipewire-0.3.so.0")
            .map_err(|e| format!("libpipewire: {e} (PipeWire isn't installed?)"))?;
        macro_rules! f {
            ($n:literal) => {
                *l.get(concat!($n, "\0").as_bytes())
                    .map_err(|e| format!("libpipewire: {}: {e}", $n))?
            };
        }
        let lib = Lib {
            init: f!("pw_init"),
            thread_loop_new: f!("pw_thread_loop_new"),
            thread_loop_get_loop: f!("pw_thread_loop_get_loop"),
            thread_loop_start: f!("pw_thread_loop_start"),
            thread_loop_stop: f!("pw_thread_loop_stop"),
            thread_loop_lock: f!("pw_thread_loop_lock"),
            thread_loop_unlock: f!("pw_thread_loop_unlock"),
            thread_loop_destroy: f!("pw_thread_loop_destroy"),
            context_new: f!("pw_context_new"),
            context_connect_fd: f!("pw_context_connect_fd"),
            context_destroy: f!("pw_context_destroy"),
            core_disconnect: f!("pw_core_disconnect"),
            properties_new_string: f!("pw_properties_new_string"),
            stream_new: f!("pw_stream_new"),
            stream_add_listener: f!("pw_stream_add_listener"),
            stream_connect: f!("pw_stream_connect"),
            stream_update_params: f!("pw_stream_update_params"),
            stream_dequeue_buffer: f!("pw_stream_dequeue_buffer"),
            stream_queue_buffer: f!("pw_stream_queue_buffer"),
            stream_destroy: f!("pw_stream_destroy"),
            _lib: l,
        };
        (lib.init)(std::ptr::null_mut(), std::ptr::null_mut());
        Ok(lib)
    })
    .as_ref()
    .map_err(|e| io::Error::other(e.clone()))
}

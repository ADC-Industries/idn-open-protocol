//! Tally: the live IDN sources listed, the frontend's scene and studio-mode changes
//! followed, and each source's program and preview tally worked out.

use super::*;

/// A live IDN source for tally: its address (which source it is; never used to reach it)
/// and a weak reference, made strong only for the moment of use (OBS may destroy a source
/// on its own thread while the frontend's events recompute tally).
struct Registered {
    key: *mut ffi::obs_source_t,
    weak: *mut ffi::obs_weak_source_t,
}
// SAFETY: the weak reference is libobs's to share across threads (its calls are
// thread-safe); `key` is only compared, never dereferenced.
unsafe impl Send for Registered {}

impl Drop for Registered {
    fn drop(&mut self) {
        // SAFETY: the weak reference `tally_register` took, released once, here.
        unsafe { ffi::obs_weak_source_release(self.weak) };
    }
}

/// Every live IDN source, for the frontend events to recompute tally.
static SOURCES: Mutex<Vec<(Registered, Arc<Shared>)>> = Mutex::new(Vec::new());

/// `source` listed for tally (a weak reference of its own taken).
pub(super) fn tally_register(source: *mut ffi::obs_source_t, shared: Arc<Shared>) {
    // SAFETY: our source, alive (`update` is called for a live source only).
    let weak = unsafe { ffi::obs_source_get_weak_source(source) };
    if !weak.is_null() {
        SOURCES
            .lock_ok()
            .push((Registered { key: source, weak }, shared));
    }
}

/// `source` taken off the tally list (its weak reference released).
pub(super) fn tally_unregister(source: *mut ffi::obs_source_t) {
    SOURCES.lock_ok().retain(|(r, _)| r.key != source);
}

/// Starts following the frontend's scene and studio-mode changes (after the frontend
/// exists, from `obs_module_post_load`).
pub(crate) fn watch_frontend() {
    if !WATCHING.swap(true, Ordering::Relaxed) {
        // SAFETY: from `obs_module_post_load` on OBS's UI thread; the callback lives as
        // long as the (pinned) library.
        unsafe {
            ffi::obs_frontend_add_event_callback(Some(on_frontend_event), std::ptr::null_mut())
        };
    }
}

pub(crate) fn unwatch_frontend() {
    if WATCHING.swap(false, Ordering::Relaxed) {
        // SAFETY: the callback and data added in `watch_frontend`.
        unsafe {
            ffi::obs_frontend_remove_event_callback(Some(on_frontend_event), std::ptr::null_mut())
        };
    }
}

unsafe extern "C" fn on_frontend_event(event: ffi::obs_frontend_event, _: *mut c_void) {
    use ffi::*;
    if matches!(
        event,
        obs_frontend_event_OBS_FRONTEND_EVENT_SCENE_CHANGED
            | obs_frontend_event_OBS_FRONTEND_EVENT_PREVIEW_SCENE_CHANGED
            | obs_frontend_event_OBS_FRONTEND_EVENT_STUDIO_MODE_ENABLED
            | obs_frontend_event_OBS_FRONTEND_EVENT_STUDIO_MODE_DISABLED
            | obs_frontend_event_OBS_FRONTEND_EVENT_FINISHED_LOADING
            | obs_frontend_event_OBS_FRONTEND_EVENT_TRANSITION_STOPPED
    ) {
        recompute_tally();
    }
}

/// Tally for every IDN source: program = the source is active (in the program output,
/// nested or not); preview = studio mode is on and the preview scene holds it ("shown"
/// alone also meant "in program", and never cleared in studio mode).
pub(super) fn recompute_tally() {
    // Each source made strong under the list's lock (a destroyed one gives null: left
    // out), used after it, released at the end.
    let list: Vec<(SourcePtr, Arc<Shared>)> = SOURCES
        .lock_ok()
        .iter()
        .filter_map(|(r, sh)| {
            // SAFETY: the weak reference is ours until `tally_unregister` (which needs the lock).
            let src = unsafe { ffi::obs_weak_source_get_source(r.weak) };
            (!src.is_null()).then(|| (SourcePtr(src), sh.clone()))
        })
        .collect();
    if list.is_empty() {
        return;
    }
    // SAFETY: frontend calls on OBS's own thread; the sources in `list` are strong references
    // (taken above), released after use.
    unsafe {
        let studio = ffi::obs_frontend_preview_program_mode_active();
        let preview_scene = if studio {
            ffi::obs_frontend_get_current_preview_scene()
        } else {
            std::ptr::null_mut()
        };
        let scene = if preview_scene.is_null() {
            std::ptr::null_mut()
        } else {
            ffi::obs_scene_from_source(preview_scene)
        };
        for (src, shared) in &list {
            let program = ffi::obs_source_active(src.0);
            let mut preview = false;
            if !scene.is_null() {
                let name = ffi::obs_source_get_name(src.0);
                if !name.is_null() {
                    preview = !ffi::obs_scene_find_source_recursive(scene, name).is_null();
                }
            }
            shared.set_tally(program, preview && !program);
        }
        if !preview_scene.is_null() {
            ffi::obs_source_release(preview_scene);
        }
        for (src, _) in &list {
            ffi::obs_source_release(src.0);
        }
    }
}

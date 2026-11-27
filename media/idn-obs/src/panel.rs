//! The IDN Output's settings page: *Tools → IDN Output…* opens it in the browser (and its
//! address works as an OBS *Custom Browser Dock*). It turns OBS's program output on and
//! off, and a second one with the preview (studio mode's preview scene, through an OBS
//! view), sets their names, quality, encoding and network options, shows how they do, and
//! keeps the settings in the plugin's OBS configuration, starting the output with OBS
//! when it was on. OBS gives plugins no dialog for outputs, and a Qt one can't be built
//! for Windows with this plugin's toolchain, so the page is a small local web panel like
//! the IDN Node's: on this machine only (127.0.0.1), changes need the header
//! `X-IDN-OBS: 1`, which no web page elsewhere can send.

use crate::{ffi, log};
use idn_common::sync::LockOk as _;
use idn_open_protocol::api::Output;
use idn_open_protocol::web::{Handler, Request, Response, serve};
use serde_json::{Value, json};
use std::ffi::{CStr, CString};
use std::os::raw::c_void;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, Weak};

/// The page's port (a fixed one, so a browser dock keeps working; another free one if
/// a second OBS has it).
const PORT: u16 = idn_common::ports::OBS_PANEL;

const INDEX: &str = include_str!("panel/index.html");
const APP_JS: &str = include_str!("panel/app.js");
const APP_CSS: &str = include_str!("panel/app.css");

/// The output OBS made for us.
struct Obj(*mut ffi::obs_output_t);
// SAFETY: libobs outputs are reference counted and may be used from any thread; this
// one is only started and stopped on OBS's UI thread (`obs_queue_task`).
unsafe impl Send for Obj {}

/// The preview's output and the view that renders the preview scene for it.
struct PreviewObj {
    out: *mut ffi::obs_output_t,
    view: *mut ffi::obs_view_t,
}
// SAFETY: as `Obj`; the view too is only made, changed and destroyed on OBS's UI thread.
unsafe impl Send for PreviewObj {}

#[derive(Default)]
struct State {
    /// The settings as saved: the output's (`name`, `quality`, `encoding`,
    /// `video_threads`, `use_node`, `passphrase`, `server`, `trust_dir`) and `enabled`.
    settings: Value,
    output: Option<Obj>,
    /// The running output's library side (receivers), from `output.rs`.
    live: Weak<Output>,
    error: Option<String>,
    /// The same for the preview's output (`preview_enabled`, `preview_name`).
    preview: Option<PreviewObj>,
    live_preview: Weak<Output>,
    preview_error: Option<String>,
    url: String,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static STOP: AtomicBool = AtomicBool::new(false);

fn defaults() -> Value {
    json!({
        "enabled": false, "name": "OBS", "quality": 92, "encoding": "machine",
        "preview_enabled": false, "preview_name": "OBS Preview",
        "video_threads": 0, "use_node": true, "passphrase": "", "server": "", "trust_dir": "",
    })
}

/// Where the settings are kept: `idn-output.json` in the plugin's OBS configuration.
fn config_file() -> Option<std::path::PathBuf> {
    // SAFETY: the module pointer OBS gave at load; the path OBS returns is copied, then
    // freed with `bfree` as it asks.
    unsafe {
        let p = ffi::obs_module_get_config_path(
            crate::obs_current_module(),
            c"idn-output.json".as_ptr(),
        );
        if p.is_null() {
            return None;
        }
        let path = CStr::from_ptr(p).to_string_lossy().into_owned();
        ffi::bfree(p as *mut c_void);
        Some(path.into())
    }
}

fn save(settings: &Value) {
    let Some(f) = config_file() else { return };
    if let Some(dir) = f.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // (The site's passphrase is in it: readable by this user only; bug review 3.)
    if let Err(e) = idn_common::fsutil::write_private(
        &f,
        &serde_json::to_vec_pretty(settings).unwrap_or_default(),
    ) {
        log(
            ffi::LOG_WARNING,
            &format!("settings page: saving {}: {e}", f.display()),
        );
    }
}

/// From `output.rs`: the program's (or the preview's) output started (its library side)
/// or stopped or failed.
pub(crate) fn note(preview: bool, live: Option<&Arc<Output>>, error: Option<String>) {
    if let Some(s) = STATE.lock_ok().as_mut() {
        let (l, e) = if preview {
            (&mut s.live_preview, &mut s.preview_error)
        } else {
            (&mut s.live, &mut s.error)
        };
        *l = live.map(Arc::downgrade).unwrap_or_default();
        if error.is_some() || live.is_some() {
            *e = error;
        }
    }
}

/// The settings an output is made with: the shared ones, its name and role.
fn output_settings(settings: &Value, preview: bool) -> Value {
    let mut o = settings.clone();
    if let Some(m) = o.as_object_mut() {
        if preview {
            let name = m
                .get("preview_name")
                .cloned()
                .unwrap_or(json!("OBS Preview"));
            m.insert("name".into(), name);
        }
        for k in ["enabled", "preview_enabled", "preview_name"] {
            m.remove(k);
        }
        m.insert(
            "role".into(),
            json!(if preview { "preview" } else { "program" }),
        );
    }
    o
}

/// After the frontend exists: the settings, the page, the Tools menu entry, and the
/// output started if it was on.
pub(crate) fn start() {
    let mut settings = defaults();
    // A damaged file is set aside (`idn-output.json.bad-SECS-PID-N`), never saved over.
    let saved = match config_file().map(|f| idn_common::fsutil::load_json::<Value>(&f)) {
        Some(Ok(v)) => v,
        Some(Err(e)) => {
            log(ffi::LOG_WARNING, &format!("settings page: {e}"));
            None
        }
        None => None,
    };
    if let Some(saved) = saved
        && let (Some(d), Some(o)) = (settings.as_object_mut(), saved.as_object())
    {
        for (k, v) in o {
            d.insert(k.clone(), v.clone());
        }
    }
    let listener = std::net::TcpListener::bind(("127.0.0.1", PORT))
        .or_else(|_| std::net::TcpListener::bind(("127.0.0.1", 0)));
    let url = match listener {
        Ok(l) => {
            let port = l.local_addr().map(|a| a.port()).unwrap_or(PORT);
            let handler: Handler = Arc::new(handle);
            match serve(l, None, handler, Arc::new(AtomicBool::new(false))) {
                Ok(()) => format!("http://127.0.0.1:{port}/"),
                Err(e) => {
                    log(ffi::LOG_WARNING, &format!("settings page: {e}"));
                    String::new()
                }
            }
        }
        Err(e) => {
            log(ffi::LOG_WARNING, &format!("settings page: {e}"));
            String::new()
        }
    };
    if !url.is_empty() {
        log(
            ffi::LOG_INFO,
            &format!("IDN Output settings: {url} (Tools -> IDN Output...)"),
        );
    }
    *STATE.lock_ok() = Some(State {
        settings,
        url,
        ..Default::default()
    });
    // SAFETY: from `obs_module_post_load` on OBS's UI thread; static strings, and callbacks
    // that live as long as the (pinned) library.
    unsafe {
        ffi::obs_frontend_add_tools_menu_item(
            c"IDN Output…".as_ptr(),
            Some(on_menu),
            std::ptr::null_mut(),
        );
        ffi::obs_frontend_add_event_callback(Some(on_event), std::ptr::null_mut());
    }
}

/// At unload: the output stopped and let go.
pub(crate) fn stop() {
    STOP.store(true, std::sync::atomic::Ordering::Relaxed);
    // SAFETY: the callback and data added in `start`.
    unsafe { ffi::obs_frontend_remove_event_callback(Some(on_event), std::ptr::null_mut()) };
    stop_output();
    stop_preview();
}

unsafe extern "C" fn on_menu(_: *mut c_void) {
    let url = STATE
        .lock_ok()
        .as_ref()
        .map(|s| s.url.clone())
        .unwrap_or_default();
    if url.is_empty() || !idn_open_protocol::web::open_browser(&url) {
        log(
            ffi::LOG_WARNING,
            &format!("settings page: open {url} in a browser"),
        );
    }
}

unsafe extern "C" fn on_event(event: ffi::obs_frontend_event, _: *mut c_void) {
    match event {
        // Started with OBS when it was on.
        ffi::obs_frontend_event_OBS_FRONTEND_EVENT_FINISHED_LOADING => {
            let (on, preview) = STATE.lock_ok().as_ref().map_or((false, false), |s| {
                (
                    s.settings["enabled"] == true,
                    s.settings["preview_enabled"] == true,
                )
            });
            if on {
                start_output();
            }
            if preview {
                start_preview();
            }
        }
        // The preview follows the preview scene (the current scene outside studio mode).
        ffi::obs_frontend_event_OBS_FRONTEND_EVENT_PREVIEW_SCENE_CHANGED
        | ffi::obs_frontend_event_OBS_FRONTEND_EVENT_SCENE_CHANGED
        | ffi::obs_frontend_event_OBS_FRONTEND_EVENT_STUDIO_MODE_ENABLED
        | ffi::obs_frontend_event_OBS_FRONTEND_EVENT_STUDIO_MODE_DISABLED => retarget_preview(),
        ffi::obs_frontend_event_OBS_FRONTEND_EVENT_EXIT => {
            stop_output();
            stop_preview();
        }
        _ => {}
    }
}

/// Starts the output with the saved settings (on OBS's UI thread).
fn start_output() {
    let mut st = STATE.lock_ok();
    let Some(s) = st.as_mut() else { return };
    if s.output.is_some() {
        return;
    }
    let settings = output_settings(&s.settings, false);
    let Ok(text) = CString::new(settings.to_string()) else {
        return;
    };
    // SAFETY: on OBS's UI thread; the data is released once the output has its own, the
    // output is checked non-null and then either kept or released once, and its error text
    // is copied while it lives.
    unsafe {
        let data = ffi::obs_data_create_from_json(text.as_ptr());
        let out = ffi::obs_output_create(
            c"idn_output".as_ptr(),
            c"IDN Output".as_ptr(),
            data,
            std::ptr::null_mut(),
        );
        ffi::obs_data_release(data);
        if out.is_null() {
            s.error = Some("OBS couldn't make the output".into());
            return;
        }
        s.error = None;
        drop(st);
        let ok = ffi::obs_output_start(out);
        let mut st = STATE.lock_ok();
        let Some(s) = st.as_mut() else { return };
        if ok {
            s.output = Some(Obj(out));
        } else {
            let e = ffi::obs_output_get_last_error(out);
            let why = if e.is_null() {
                String::new()
            } else {
                CStr::from_ptr(e).to_string_lossy().into_owned()
            };
            s.error = Some(if why.is_empty() {
                "the output didn't start (see OBS's log)".into()
            } else {
                why
            });
            ffi::obs_output_release(out);
        }
    }
}

/// Stops the output and lets it go (on OBS's UI thread).
fn stop_output() {
    let out = STATE.lock_ok().as_mut().and_then(|s| s.output.take());
    if let Some(Obj(out)) = out {
        // SAFETY: our reference, taken out of the state, so stopped and released once.
        unsafe {
            ffi::obs_output_stop(out);
            ffi::obs_output_release(out);
        }
    }
}

/// The scene the preview shows: studio mode's preview scene, else the current scene (a
/// new reference, or null).
fn preview_scene() -> *mut ffi::obs_source_t {
    // SAFETY: frontend calls on OBS's UI thread; each returns a new reference or null.
    unsafe {
        let s = ffi::obs_frontend_get_current_preview_scene();
        if s.is_null() {
            ffi::obs_frontend_get_current_scene()
        } else {
            s
        }
    }
}

/// Starts the preview's output: a view of the preview scene, rendered at OBS's output
/// size and rate, as a second IDN output with OBS's sound (on OBS's UI thread).
fn start_preview() {
    let mut st = STATE.lock_ok();
    let Some(s) = st.as_mut() else { return };
    if s.preview.is_some() {
        return;
    }
    let settings = output_settings(&s.settings, true);
    let Ok(text) = CString::new(settings.to_string()) else {
        return;
    };
    // SAFETY: on OBS's UI thread; the view is ours, the scene is released after the view
    // takes its own reference, and every pointer is checked before use and let go once on
    // each path.
    unsafe {
        let view = ffi::obs_view_create();
        let scene = preview_scene();
        ffi::obs_view_set_source(view, 0, scene);
        if !scene.is_null() {
            ffi::obs_source_release(scene);
        }
        let video = ffi::obs_view_add(view);
        if video.is_null() {
            ffi::obs_view_destroy(view);
            s.preview_error = Some("OBS couldn't render the preview".into());
            return;
        }
        let data = ffi::obs_data_create_from_json(text.as_ptr());
        let out = ffi::obs_output_create(
            c"idn_output".as_ptr(),
            c"IDN Preview".as_ptr(),
            data,
            std::ptr::null_mut(),
        );
        ffi::obs_data_release(data);
        if out.is_null() {
            ffi::obs_view_remove(view);
            ffi::obs_view_destroy(view);
            s.preview_error = Some("OBS couldn't make the output".into());
            return;
        }
        ffi::obs_output_set_media(out, video, ffi::obs_get_audio());
        s.preview_error = None;
        drop(st);
        let ok = ffi::obs_output_start(out);
        let mut st = STATE.lock_ok();
        let Some(s) = st.as_mut() else { return };
        if ok {
            s.preview = Some(PreviewObj { out, view });
        } else {
            let e = ffi::obs_output_get_last_error(out);
            let why = if e.is_null() {
                String::new()
            } else {
                CStr::from_ptr(e).to_string_lossy().into_owned()
            };
            s.preview_error = Some(if why.is_empty() {
                "the preview's output didn't start (see OBS's log)".into()
            } else {
                why
            });
            ffi::obs_output_release(out);
            ffi::obs_view_remove(view);
            ffi::obs_view_destroy(view);
        }
    }
}

/// Stops the preview's output and its view (on OBS's UI thread).
fn stop_preview() {
    let p = STATE.lock_ok().as_mut().and_then(|s| s.preview.take());
    if let Some(p) = p {
        // SAFETY: our output and view, taken out of the state, so stopped, released and
        // destroyed once.
        unsafe {
            ffi::obs_output_stop(p.out);
            ffi::obs_output_release(p.out);
            ffi::obs_view_remove(p.view);
            ffi::obs_view_set_source(p.view, 0, std::ptr::null_mut());
            ffi::obs_view_destroy(p.view);
        }
    }
}

/// Points the preview's view at the scene now in preview (on OBS's UI thread).
fn retarget_preview() {
    let st = STATE.lock_ok();
    let Some(view) = st.as_ref().and_then(|s| s.preview.as_ref()).map(|p| p.view) else {
        return;
    };
    // SAFETY: the view stays ours while the state lock is held; the scene is released after
    // the view takes its own reference.
    unsafe {
        let scene = preview_scene();
        ffi::obs_view_set_source(view, 0, scene);
        if !scene.is_null() {
            ffi::obs_source_release(scene);
        }
    }
}

/// What to do on OBS's UI thread.
enum Op {
    Start,
    Stop,
    Restart,
    StartPreview,
    StopPreview,
    RestartPreview,
}

unsafe extern "C" fn run_op(param: *mut c_void) {
    // SAFETY: the `Box<Op>` from `on_ui`; OBS runs each task once.
    let op = unsafe { Box::from_raw(param as *mut Op) };
    if STOP.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    match *op {
        Op::Start => start_output(),
        Op::Stop => stop_output(),
        Op::Restart => {
            stop_output();
            start_output();
        }
        Op::StartPreview => start_preview(),
        Op::StopPreview => stop_preview(),
        Op::RestartPreview => {
            stop_preview();
            start_preview();
        }
    }
}

/// Runs `op` on OBS's UI thread and waits for it.
fn on_ui(op: Op) {
    let p = Box::into_raw(Box::new(op)) as *mut c_void;
    // SAFETY: the box goes to `run_op`, which takes it back; called from the page's thread
    // only, so waiting on OBS's UI thread can't wait on itself.
    unsafe { ffi::obs_queue_task(ffi::obs_task_type_OBS_TASK_UI, Some(run_op), p, true) };
}

/// The page's state: the settings (the passphrase only as whether there is one), whether
/// the output runs, its receivers and frames, and the last error.
fn state() -> Value {
    let st = STATE.lock_ok();
    let Some(s) = st.as_ref() else {
        return json!({});
    };
    let mut settings = s.settings.clone();
    let has_pass = settings["passphrase"]
        .as_str()
        .is_some_and(|p| !p.is_empty());
    settings["passphrase"] = json!("");
    let (running, frames, dropped) = match &s.output {
        // SAFETY: the output stays ours while the state lock is held (`stop_output` takes
        // it out under the lock first); these libobs getters may be called from any thread.
        Some(Obj(o)) => unsafe {
            (
                ffi::obs_output_active(*o),
                ffi::obs_output_get_total_frames(*o),
                ffi::obs_output_get_frames_dropped(*o),
            )
        },
        None => (false, 0, 0),
    };
    let live = s.live.upgrade();
    let (p_running, p_frames, p_dropped) = match &s.preview {
        // SAFETY: as above, for the preview's output.
        Some(p) => unsafe {
            (
                ffi::obs_output_active(p.out),
                ffi::obs_output_get_total_frames(p.out),
                ffi::obs_output_get_frames_dropped(p.out),
            )
        },
        None => (false, 0, 0),
    };
    let live_preview = s.live_preview.upgrade();
    json!({
        "preview": {
            "running": p_running,
            "full_name": live_preview.as_ref().map(|o| o.name().to_owned()),
            "receivers": live_preview.as_ref().map_or(0, |o| o.receivers()),
            "frames": p_frames,
            "dropped": p_dropped,
            "error": s.preview_error,
        },
        "settings": settings,
        "passphrase_set": has_pass,
        "running": running,
        "full_name": live.as_ref().map(|o| o.name().to_owned()),
        "receivers": live.as_ref().map_or(0, |o| o.receivers()),
        "frames": frames,
        "dropped": dropped,
        "error": s.error,
        "url": s.url,
    })
}

/// The keys the page may set, and their kinds. Says whether the program's and the
/// preview's outputs need starting again.
fn patch(v: &Value) -> Result<(Value, bool, bool), String> {
    let mut st = STATE.lock_ok();
    let s = st.as_mut().ok_or("not ready")?;
    let before = s.settings.clone();
    let obj = v.as_object().ok_or("a JSON object")?;
    for (k, val) in obj {
        let ok = match k.as_str() {
            "enabled" | "use_node" | "preview_enabled" => val.is_boolean(),
            "quality" => val.as_i64().is_some_and(|q| (50..=100).contains(&q)),
            "video_threads" => val.as_i64().is_some_and(|t| (0..=64).contains(&t)),
            "encoding" => val
                .as_str()
                .is_some_and(|e| ["machine", "auto", "offer", "cpu", "force"].contains(&e)),
            "name" | "preview_name" => val
                .as_str()
                .is_some_and(|n| !n.trim().is_empty() && n.len() <= 100),
            "passphrase" | "server" | "trust_dir" => val.is_string(),
            _ => return Err(format!("no setting {k:?}")),
        };
        if !ok {
            return Err(format!("{k}: not a valid value"));
        }
        s.settings[k] = if k.ends_with("name") {
            json!(val.as_str().unwrap_or("").trim())
        } else {
            val.clone()
        };
    }
    save(&s.settings);
    // The passphrase cleared: gone from the disk too, with any copy of the file set aside
    // as damaged before.
    let has = |v: &Value| v["passphrase"].as_str().is_some_and(|p| !p.is_empty());
    if has(&before)
        && !has(&s.settings)
        && let Some(f) = config_file()
    {
        idn_common::fsutil::remove_set_asides(&f);
    }
    // A running output takes new settings by starting again.
    let changed =
        |preview| output_settings(&before, preview) != output_settings(&s.settings, preview);
    Ok((s.settings.clone(), changed(false), changed(true)))
}

fn handle(r: Request) -> Response {
    let err = |status: &'static str, e: &str| {
        Response::new(
            status,
            "application/json",
            json!({"ok": false, "error": e}).to_string(),
        )
    };
    if let Some(a) = idn_open_protocol::web::ui::asset(&r.path).filter(|_| r.method == "GET") {
        return a;
    }
    match (r.method.as_str(), r.path.as_str()) {
        ("GET", "/") => Response::ok("text/html; charset=utf-8", INDEX),
        ("GET", "/app.js") => Response::ok("text/javascript; charset=utf-8", APP_JS),
        ("GET", "/app.css") => Response::ok("text/css; charset=utf-8", APP_CSS),
        ("GET", "/api/v1/state") => {
            Response::json(state().to_string()).with("Cache-Control", "no-store")
        }
        ("POST", "/api/v1/settings") => {
            if !r.may_change("X-IDN-OBS") {
                return err("403 Forbidden", "missing X-IDN-OBS: 1");
            }
            let v: Value = match serde_json::from_slice(&r.body) {
                Ok(v) => v,
                Err(e) => return err("400 Bad Request", &format!("bad JSON: {e}")),
            };
            let (settings, changed, changed_preview) = match patch(&v) {
                Ok(x) => x,
                Err(e) => return err("400 Bad Request", &e),
            };
            let (running, preview_running) = STATE.lock_ok().as_ref().map_or((false, false), |s| {
                (s.output.is_some(), s.preview.is_some())
            });
            match (settings["enabled"] == true, running) {
                (true, false) => on_ui(Op::Start),
                (false, true) => on_ui(Op::Stop),
                (true, true) if changed => on_ui(Op::Restart),
                _ => {}
            }
            match (settings["preview_enabled"] == true, preview_running) {
                (true, false) => on_ui(Op::StartPreview),
                (false, true) => on_ui(Op::StopPreview),
                (true, true) if changed_preview => on_ui(Op::RestartPreview),
                _ => {}
            }
            Response::json(state().to_string())
        }
        _ => err("404 Not Found", "no such page"),
    }
}

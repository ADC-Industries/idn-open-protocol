//! VST3 plugins through the `vst3` bindings (Steinberg's SDK interfaces, MIT since VST
//! 3.8): the module, its factory, the component with its audio processor and edit
//! controller (one object or two, connected), and what a host must provide itself: the
//! host application (with messages for plugins whose two halves talk), parameter-change
//! queues, a memory stream for state, and a component handler.
//!
//! VST3 parameters are normalised (0–1); the desk sees them that way, with the plugin's
//! own text for each value.

#[cfg(target_os = "linux")]
use crate::runloop::Call;
use crate::runloop::RunLoop;
use crate::window::HostWindow;
use crate::{Param, Plugin, Processor, Ui};
use idn_common::sync::LockOk as _;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_void};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use vst3::Steinberg::IBStream_::IStreamSeekMode_;
#[cfg(target_os = "linux")]
use vst3::Steinberg::Linux::{IEventHandlerTrait, ITimerHandlerTrait};
use vst3::Steinberg::Vst::IAttributeList_::AttrID;
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComWrapper, Interface};

// ---------------------------------------------------------------------------------------
// The module.

/// A loaded `.vst3` module; its exit function runs when the last plugin from it is gone.
struct Module {
    factory: Option<ComPtr<IPluginFactory>>,
    lib: Option<libloading::Library>,
}

impl Drop for Module {
    fn drop(&mut self) {
        // The factory before the library's exit function.
        self.factory = None;
        let lib = self.lib.take();
        if let Some(lib) = lib {
            let exit: &[u8] = if cfg!(windows) {
                b"ExitDll"
            } else if cfg!(target_os = "macos") {
                b"bundleExit"
            } else {
                b"ModuleExit"
            };
            // SAFETY: the module's own exit function, as the VST3 module rules ask.
            if let Ok(f) = unsafe { lib.get::<unsafe extern "C" fn() -> bool>(exit) } {
                // SAFETY: called once, with the factory released above and the library still
                // loaded.
                unsafe { f() };
            }
        }
    }
}

/// The library inside a bundle (`NAME.vst3/Contents/ARCH-OS/NAME.so|.vst3`), or the file.
fn binary(path: &Path) -> PathBuf {
    if !path.is_dir() {
        return path.into();
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };
    if cfg!(windows) {
        path.join("Contents")
            .join(format!("{arch}-win"))
            .join(format!("{stem}.vst3"))
    } else {
        path.join("Contents")
            .join(format!("{arch}-linux"))
            .join(format!("{stem}.so"))
    }
}

fn load(path: &str) -> Result<Arc<Module>, String> {
    let bin = binary(Path::new(path));
    // SAFETY: loading a plugin runs its code; that is what this process is for.
    let lib =
        unsafe { libloading::Library::new(&bin) }.map_err(|e| format!("{}: {e}", bin.display()))?;
    #[cfg(unix)]
    {
        // ModuleEntry gets the library's handle (some plugins find their resources by it).
        let lib = libloading::os::unix::Library::from(lib);
        let handle = lib.into_raw();
        // SAFETY: the handle was just taken from a library we own; it is put back below.
        let lib = unsafe { libloading::os::unix::Library::from_raw(handle) };
        // SAFETY: the module's entry function, given its own handle, called once before anything
        // else in it (the VST3 module rules).
        if let Ok(entry) =
            unsafe { lib.get::<unsafe extern "C" fn(*mut c_void) -> bool>(b"ModuleEntry") }
            && !unsafe { entry(handle) }
        {
            return Err("its ModuleEntry failed".into());
        }
        finish_load(libloading::Library::from(lib))
    }
    #[cfg(windows)]
    {
        // SAFETY: as above: the Windows entry function, once, before the factory.
        if let Ok(init) = unsafe { lib.get::<unsafe extern "system" fn() -> bool>(b"InitDll") }
            && !unsafe { init() }
        {
            return Err("its InitDll failed".into());
        }
        finish_load(lib)
    }
}

fn finish_load(lib: libloading::Library) -> Result<Arc<Module>, String> {
    // SAFETY: the VST3 module's factory function.
    let get = unsafe {
        lib.get::<unsafe extern "system" fn() -> *mut IPluginFactory>(b"GetPluginFactory")
    }
    .map_err(|_| "no GetPluginFactory: not a VST3 plugin")?;
    // SAFETY: GetPluginFactory hands over a counted reference (or null), which the ComPtr now owns.
    let factory = unsafe { ComPtr::from_raw(get()) }.ok_or("no factory")?;
    Ok(Arc::new(Module {
        factory: Some(factory),
        lib: Some(lib),
    }))
}

fn cstr(b: &[c_char]) -> String {
    let bytes: Vec<u8> = b
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn utf16(s: &[u16]) -> String {
    String::from_utf16_lossy(&s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())])
}

/// A class id as VST3 writes it: 32 upper-case hex digits.
fn hex(cid: &TUID) -> String {
    idn_common::hex::hex(&cid.map(|b| b as u8)).to_ascii_uppercase()
}

fn unhex(s: &str) -> Option<TUID> {
    let b: [u8; 16] = idn_common::hex::unhex(s)?.try_into().ok()?;
    Some(b.map(|b| b as c_char))
}

/// The audio effects in a `.vst3` module: (class id, name, vendor).
pub fn describe(path: &str) -> Result<Vec<(String, String, String)>, String> {
    let m = load(path)?;
    let f = m.factory.as_ref().expect("loaded");
    // SAFETY: PFactoryInfo is plain C data (zero is a valid value) for the factory to fill; the
    // factory is alive while `m` is.
    let mut fi: PFactoryInfo = unsafe { std::mem::zeroed() };
    unsafe { f.getFactoryInfo(&mut fi) };
    let vendor = cstr(&fi.vendor);
    let f2 = f.cast::<IPluginFactory2>();
    let mut out = Vec::new();
    // SAFETY: the live factory, as above; a zeroed PClassInfo is plain C data for it to fill.
    for i in 0..unsafe { f.countClasses() } {
        let mut ci: PClassInfo = unsafe { std::mem::zeroed() };
        if unsafe { f.getClassInfo(i, &mut ci) } != kResultOk
            || cstr(&ci.category) != "Audio Module Class"
        {
            continue;
        }
        let v = f2
            .as_ref()
            .and_then(|f2| {
                // SAFETY: as above, for PClassInfo2.
                let mut c2: PClassInfo2 = unsafe { std::mem::zeroed() };
                (unsafe { f2.getClassInfo2(i, &mut c2) } == kResultOk).then(|| cstr(&c2.vendor))
            })
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| vendor.clone());
        out.push((hex(&ci.cid), cstr(&ci.name), v));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// What the host provides.

struct HostApp;

impl Class for HostApp {
    type Interfaces = (IHostApplication,);
}

impl IHostApplicationTrait for HostApp {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        let n: Vec<u16> = "IDN Mixer".encode_utf16().collect();
        // SAFETY: the plugin passes a String128 to fill (the IHostApplication contract); the name
        // is 9 of its 128 units.
        let dst = unsafe { &mut *name };
        dst.fill(0);
        dst[..n.len()].copy_from_slice(&n);
        kResultOk
    }

    unsafe fn createInstance(
        &self,
        cid: *mut TUID,
        iid: *mut TUID,
        obj: *mut *mut c_void,
    ) -> tresult {
        // Messages and attribute lists, for plugins whose processor and controller talk.
        // SAFETY: the plugin's class and interface ids, 16-byte TUIDs valid for the call.
        let guid = |t: *mut TUID| unsafe { (*t).map(|c| c as u8) };
        let (cid, iid) = (guid(cid), guid(iid));
        if cid == IMessage::IID && iid == IMessage::IID {
            let m = ComWrapper::new(Message::default());
            if let Some(p) = m.to_com_ptr::<IMessage>() {
                // SAFETY: `obj` is the plugin's out pointer, valid for the call; it takes the
                // reference `into_raw` gave up.
                unsafe { *obj = p.into_raw() as *mut c_void };
                return kResultOk;
            }
        }
        if cid == IAttributeList::IID && iid == IAttributeList::IID {
            let a = ComWrapper::new(Attributes::default());
            if let Some(p) = a.to_com_ptr::<IAttributeList>() {
                // SAFETY: as above.
                unsafe { *obj = p.into_raw() as *mut c_void };
                return kResultOk;
            }
        }
        // SAFETY: as above.
        unsafe { *obj = std::ptr::null_mut() };
        kResultFalse
    }
}

struct Message {
    /// NUL-terminated, empty to start with: a plugin may read it before setting it, and
    /// an empty Vec's pointer dangles.
    id: RefCell<Vec<u8>>,
    attrs: RefCell<Option<ComPtr<IAttributeList>>>,
}

impl Default for Message {
    fn default() -> Self {
        Message {
            id: RefCell::new(vec![0]),
            attrs: RefCell::new(None),
        }
    }
}

impl Class for Message {
    type Interfaces = (IMessage,);
}

impl IMessageTrait for Message {
    unsafe fn getMessageID(&self) -> FIDString {
        self.id.borrow().as_ptr() as FIDString
    }

    unsafe fn setMessageID(&self, id: FIDString) {
        let mut v = if id.is_null() {
            Vec::new()
        } else {
            // SAFETY: a non-null FIDString is a NUL-ended C string (VST3), copied before the call
            // returns.
            unsafe { CStr::from_ptr(id) }.to_bytes().to_vec()
        };
        v.push(0);
        *self.id.borrow_mut() = v;
    }

    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        let mut a = self.attrs.borrow_mut();
        if a.is_none() {
            *a = ComWrapper::new(Attributes::default()).to_com_ptr::<IAttributeList>();
        }
        a.as_ref().map_or(std::ptr::null_mut(), |p| p.as_ptr())
    }
}

#[derive(Clone)]
enum Attr {
    Int(i64),
    Float(f64),
    Str(Vec<u16>),
    Bin(Vec<u8>),
}

#[derive(Default)]
struct Attributes {
    map: RefCell<HashMap<Vec<u8>, Attr>>,
}

impl Class for Attributes {
    type Interfaces = (IAttributeList,);
}

fn key(id: AttrID) -> Vec<u8> {
    if id.is_null() {
        Vec::new()
    } else {
        // SAFETY: as in `setMessageID`: a non-null id is a NUL-ended C string.
        unsafe { CStr::from_ptr(id) }.to_bytes().to_vec()
    }
}

impl IAttributeListTrait for Attributes {
    unsafe fn setInt(&self, id: AttrID, value: int64) -> tresult {
        self.map.borrow_mut().insert(key(id), Attr::Int(value));
        kResultOk
    }

    unsafe fn getInt(&self, id: AttrID, value: *mut int64) -> tresult {
        match self.map.borrow().get(&key(id)) {
            Some(Attr::Int(v)) => {
                // SAFETY: `value` is the plugin's out pointer, valid for the call.
                unsafe { *value = *v };
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setFloat(&self, id: AttrID, value: f64) -> tresult {
        self.map.borrow_mut().insert(key(id), Attr::Float(value));
        kResultOk
    }

    unsafe fn getFloat(&self, id: AttrID, value: *mut f64) -> tresult {
        match self.map.borrow().get(&key(id)) {
            Some(Attr::Float(v)) => {
                // SAFETY: as above.
                unsafe { *value = *v };
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setString(&self, id: AttrID, string: *const TChar) -> tresult {
        let mut v = Vec::new();
        if !string.is_null() {
            let mut p = string;
            // SAFETY: a non-null TChar string is NUL-ended (VST3); read up to its end.
            unsafe {
                while *p != 0 {
                    v.push(*p);
                    p = p.add(1);
                }
            }
        }
        self.map.borrow_mut().insert(key(id), Attr::Str(v));
        kResultOk
    }

    unsafe fn getString(&self, id: AttrID, string: *mut TChar, size: uint32) -> tresult {
        match self.map.borrow().get(&key(id)) {
            // No room even for the terminator (a size under one TChar): nothing written.
            Some(Attr::Str(_)) if string.is_null() || (size as usize) < 2 => kResultFalse,
            Some(Attr::Str(s)) => {
                let room = (size as usize / 2).saturating_sub(1);
                let n = s.len().min(room);
                // SAFETY: `string` is non-null with `size` bytes (checked above); `n` units and the
                // NUL fit in `size / 2`.
                unsafe {
                    std::ptr::copy_nonoverlapping(s.as_ptr(), string, n);
                    *string.add(n) = 0;
                }
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setBinary(&self, id: AttrID, data: *const c_void, size: uint32) -> tresult {
        let v = if data.is_null() {
            Vec::new()
        } else {
            // SAFETY: non-null `data` holds `size` bytes (the IAttributeList contract), copied
            // before the call returns.
            unsafe { std::slice::from_raw_parts(data as *const u8, size as usize) }.to_vec()
        };
        self.map.borrow_mut().insert(key(id), Attr::Bin(v));
        kResultOk
    }

    unsafe fn getBinary(&self, id: AttrID, data: *mut *const c_void, size: *mut uint32) -> tresult {
        match self.map.borrow().get(&key(id)) {
            // The pointer stays valid while the list does (the VST3 rule).
            Some(Attr::Bin(b)) => {
                // SAFETY: the plugin's out pointers, valid for the call; `b` stays in the map, so
                // the data lives while the list holds that attribute (the VST3 rule).
                unsafe {
                    *data = b.as_ptr() as *const c_void;
                    *size = b.len() as uint32;
                }
                kResultOk
            }
            _ => kResultFalse,
        }
    }
}

type Edits = Arc<Mutex<Vec<(u32, f64)>>>;

/// The controller's calls back: knobs moved in the plugin's own window, passed to its
/// processor at the next block and to the desk.
struct Handler {
    to_audio: Edits,
    edits: Edits,
}

impl Class for Handler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for Handler {
    unsafe fn beginEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }
    unsafe fn performEdit(&self, id: ParamID, value: ParamValue) -> tresult {
        self.to_audio.lock_ok().push((id, value));
        self.edits.lock_ok().push((id, value));
        kResultOk
    }
    unsafe fn endEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }
    unsafe fn restartComponent(&self, _flags: int32) -> tresult {
        kResultOk
    }
}

/// The frame a plugin's view sits in: its size requests, and on Linux the run loop its
/// interface needs (descriptors and timers on the chain's main thread).
struct Frame {
    resize: Rc<Cell<Option<(u32, u32)>>>,
    runloop: Rc<RunLoop>,
    owner: u64,
    /// What it registered: (handler, fd) and (handler, timer id), to unregister.
    fds: RefCell<Vec<(usize, i32)>>,
    timers: RefCell<Vec<(usize, u32)>>,
}

impl Frame {
    /// Removes everything it registered (its view is going).
    fn clear(&self) {
        for (_, fd) in self.fds.borrow_mut().drain(..) {
            self.runloop.remove_fd(self.owner, fd);
        }
        for (_, t) in self.timers.borrow_mut().drain(..) {
            self.runloop.remove_timer(t);
        }
    }
}

#[cfg(target_os = "linux")]
impl Class for Frame {
    type Interfaces = (IPlugFrame, vst3::Steinberg::Linux::IRunLoop);
}

#[cfg(not(target_os = "linux"))]
impl Class for Frame {
    type Interfaces = (IPlugFrame,);
}

impl IPlugFrameTrait for Frame {
    unsafe fn resizeView(&self, view: *mut IPlugView, size: *mut ViewRect) -> tresult {
        if size.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: `size` is non-null (checked) and the plugin's ViewRect for the call.
        let r = unsafe { *size };
        self.resize.set(Some((
            (r.right - r.left).max(1) as u32,
            (r.bottom - r.top).max(1) as u32,
        )));
        // SAFETY: `view` is the plugin's own view, alive for the call (null skipped); onSize gets
        // the rect it asked for, on the main thread, where views are called.
        if let Some(v) = unsafe { vst3::ComRef::from_raw(view) } {
            unsafe { v.onSize(size) };
        }
        kResultOk
    }
}

#[cfg(target_os = "linux")]
impl vst3::Steinberg::Linux::IRunLoopTrait for Frame {
    unsafe fn registerEventHandler(
        &self,
        handler: *mut vst3::Steinberg::Linux::IEventHandler,
        fd: vst3::Steinberg::Linux::FileDescriptor,
    ) -> tresult {
        // SAFETY: `handler` is the plugin's, alive for the call; the ComPtr keeps a reference of
        // its own, so the callback (run on the main thread by the run loop) never outlives it.
        let Some(h) = (unsafe { vst3::ComRef::from_raw(handler) }).map(|r| r.to_com_ptr()) else {
            return kInvalidArgument;
        };
        let call: Rc<dyn Fn()> = Rc::new(move || unsafe { h.onFDIsSet(fd) });
        self.runloop.add_fd(self.owner, fd, 1, Call::Func(call));
        self.fds.borrow_mut().push((handler as usize, fd));
        kResultOk
    }

    unsafe fn unregisterEventHandler(
        &self,
        handler: *mut vst3::Steinberg::Linux::IEventHandler,
    ) -> tresult {
        let mut fds = self.fds.borrow_mut();
        fds.retain(|(h, fd)| {
            let mine = *h == handler as usize;
            if mine {
                self.runloop.remove_fd(self.owner, *fd);
            }
            !mine
        });
        kResultOk
    }

    unsafe fn registerTimer(
        &self,
        handler: *mut vst3::Steinberg::Linux::ITimerHandler,
        ms: vst3::Steinberg::Linux::TimerInterval,
    ) -> tresult {
        // SAFETY: as in `registerEventHandler`.
        let Some(h) = (unsafe { vst3::ComRef::from_raw(handler) }).map(|r| r.to_com_ptr()) else {
            return kInvalidArgument;
        };
        let call: Rc<dyn Fn()> = Rc::new(move || unsafe { h.onTimer() });
        let id =
            self.runloop
                .add_timer(self.owner, ms.min(u32::MAX as u64) as u32, Call::Func(call));
        self.timers.borrow_mut().push((handler as usize, id));
        kResultOk
    }

    unsafe fn unregisterTimer(
        &self,
        handler: *mut vst3::Steinberg::Linux::ITimerHandler,
    ) -> tresult {
        let mut ts = self.timers.borrow_mut();
        ts.retain(|(h, id)| {
            let mine = *h == handler as usize;
            if mine {
                self.runloop.remove_timer(*id);
            }
            !mine
        });
        kResultOk
    }
}

/// A stream in memory, for saving and loading state.
#[derive(Default)]
struct Stream {
    data: RefCell<Vec<u8>>,
    pos: Cell<usize>,
}

impl Class for Stream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for Stream {
    unsafe fn read(&self, buffer: *mut c_void, n: int32, read: *mut int32) -> tresult {
        let d = self.data.borrow();
        let pos = self.pos.get().min(d.len());
        let k = (n.max(0) as usize).min(d.len() - pos);
        // (A null buffer: nothing read, nothing written through it.)
        let k = if buffer.is_null() { 0 } else { k };
        if k > 0 {
            // SAFETY: `d[pos..]` has at least `k` bytes and the plugin's (non-null) `buffer`
            // room for `n` >= `k` (the IBStream contract); they cannot overlap (`d` is ours).
            unsafe { std::ptr::copy_nonoverlapping(d[pos..].as_ptr(), buffer as *mut u8, k) };
        }
        self.pos.set(pos + k);
        if !read.is_null() {
            // SAFETY: a non-null (checked) out pointer of the plugin's, valid for the call.
            unsafe { *read = k as int32 };
        }
        kResultOk
    }

    unsafe fn write(&self, buffer: *mut c_void, n: int32, written: *mut int32) -> tresult {
        // (A null buffer: nothing written.)
        let n = if buffer.is_null() {
            0
        } else {
            n.max(0) as usize
        };
        let mut d = self.data.borrow_mut();
        let pos = self.pos.get();
        if d.len() < pos + n {
            d.resize(pos + n, 0);
        }
        if n > 0 {
            // SAFETY: `d` was grown to `pos + n` bytes above; the plugin's (non-null) `buffer`
            // holds `n` (the IBStream contract).
            unsafe { std::ptr::copy_nonoverlapping(buffer as *const u8, d[pos..].as_mut_ptr(), n) };
        }
        self.pos.set(pos + n);
        if !written.is_null() {
            // SAFETY: as in `read`: a non-null out pointer of the plugin's.
            unsafe { *written = n as int32 };
        }
        kResultOk
    }

    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        // (The constants' type differs between platforms.)
        let base = match mode as i64 {
            m if m == IStreamSeekMode_::kIBSeekCur as i64 => self.pos.get() as i64,
            m if m == IStreamSeekMode_::kIBSeekEnd as i64 => self.data.borrow().len() as i64,
            _ => 0,
        };
        let p = (base + pos).max(0) as usize;
        self.pos.set(p);
        if !result.is_null() {
            // SAFETY: as in `read`.
            unsafe { *result = p as int64 };
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        if !pos.is_null() {
            // SAFETY: as in `read`.
            unsafe { *pos = self.pos.get() as int64 };
        }
        kResultOk
    }
}

fn stream_with(bytes: &[u8]) -> (ComWrapper<Stream>, ComPtr<IBStream>) {
    let s = ComWrapper::new(Stream {
        data: RefCell::new(bytes.to_vec()),
        pos: Cell::new(0),
    });
    let p = s.to_com_ptr::<IBStream>().expect("a stream");
    (s, p)
}

/// One parameter's changes in a block.
struct Queue {
    id: ParamID,
    points: RefCell<Vec<(i32, f64)>>,
}

impl Class for Queue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for Queue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id
    }
    unsafe fn getPointCount(&self) -> int32 {
        self.points.borrow().len() as int32
    }
    unsafe fn getPoint(&self, index: int32, offset: *mut int32, value: *mut ParamValue) -> tresult {
        match self.points.borrow().get(index as usize) {
            Some(&(o, v)) => {
                // SAFETY: the plugin's out pointers, valid for the call (the IParamValueQueue
                // contract).
                unsafe {
                    *offset = o;
                    *value = v;
                }
                kResultOk
            }
            None => kResultFalse,
        }
    }
    unsafe fn addPoint(&self, offset: int32, value: ParamValue, index: *mut int32) -> tresult {
        let mut p = self.points.borrow_mut();
        p.push((offset, value));
        if !index.is_null() {
            // SAFETY: a non-null (checked) out pointer of the plugin's.
            unsafe { *index = p.len() as int32 - 1 };
        }
        kResultOk
    }
}

/// The parameter changes for the next block.
#[derive(Default)]
struct Changes {
    queues: RefCell<Vec<(ComWrapper<Queue>, ComPtr<IParamValueQueue>)>>,
}

impl Class for Changes {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for Changes {
    unsafe fn getParameterCount(&self) -> int32 {
        self.queues.borrow().len() as int32
    }
    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        self.queues
            .borrow()
            .get(index as usize)
            .map_or(std::ptr::null_mut(), |q| q.1.as_ptr())
    }
    unsafe fn addParameterData(
        &self,
        id: *const ParamID,
        index: *mut int32,
    ) -> *mut IParamValueQueue {
        // SAFETY: the plugin's id pointer, valid for the call.
        let id = unsafe { *id };
        let mut qs = self.queues.borrow_mut();
        let i = match qs.iter().position(|q| q.0.id == id) {
            Some(i) => i,
            None => {
                let w = ComWrapper::new(Queue {
                    id,
                    points: RefCell::new(Vec::new()),
                });
                let p = w.to_com_ptr::<IParamValueQueue>().expect("a queue");
                qs.push((w, p));
                qs.len() - 1
            }
        };
        if !index.is_null() {
            // SAFETY: a non-null (checked) out pointer of the plugin's.
            unsafe { *index = i as int32 };
        }
        qs[i].1.as_ptr()
    }
}

// ---------------------------------------------------------------------------------------
// A plugin.

/// The main-thread side: the component and controller, and the plugin's own window.
pub struct Vst3 {
    component: ComPtr<IComponent>,
    processor: ComPtr<IAudioProcessor>,
    controller: Option<ComPtr<IEditController>>,
    /// The controller is an object of its own (initialised and terminated apart).
    separate: bool,
    connection: Option<(ComPtr<IConnectionPoint>, ComPtr<IConnectionPoint>)>,
    name: String,
    /// The audio side until the chain takes it.
    audio: Option<Vst3Processor>,
    to_audio: Edits,
    edits: Edits,
    view: Option<(ComPtr<IPlugView>, ComWrapper<Frame>)>,
    resize: Rc<Cell<Option<(u32, u32)>>>,
    runloop: Rc<RunLoop>,
    owner: u64,
    _host: ComWrapper<HostApp>,
    _handler: ComWrapper<Handler>,
    // Last: the module outlives its plugin's objects.
    _module: Arc<Module>,
}

/// The audio side, on the chain's audio thread.
pub struct Vst3Processor {
    processor: ComPtr<IAudioProcessor>,
    ins: usize,
    outs: usize,
    chans: usize,
    inbuf: Vec<Vec<f32>>,
    outbuf: Vec<Vec<f32>>,
    /// The buffers' pointers, made once (the buffers never grow): no allocation per
    /// block on the audio thread.
    in_ptrs: Vec<*mut f32>,
    out_ptrs: Vec<*mut f32>,
    changes: ComWrapper<Changes>,
    changes_ptr: ComPtr<IParameterChanges>,
    from_main: Edits,
    pending: Vec<(u32, f64)>,
    _module: Arc<Module>,
}

// SAFETY: used by one thread at a time (made on the main thread, then only on the chain's
// audio thread); VST3 processors are called on an audio thread of the host's choosing.
unsafe impl Send for Vst3Processor {}

pub fn open(
    path: &str,
    id: &str,
    channels: usize,
    rate: f64,
    block: usize,
    ui: Ui,
) -> Result<Box<dyn Plugin>, String> {
    let module = load(path)?;
    let cid = unhex(id).ok_or("a VST3 class id is 32 hex digits")?;
    let host = ComWrapper::new(HostApp);
    let host_ptr = host.to_com_ptr::<FUnknown>().ok_or("host")?;
    let f = module.factory.as_ref().expect("loaded");
    let name = {
        let mut name = String::new();
        // SAFETY: the live factory (`module` holds it); a zeroed PClassInfo is plain C data for it
        // to fill.
        for i in 0..unsafe { f.countClasses() } {
            let mut ci: PClassInfo = unsafe { std::mem::zeroed() };
            if unsafe { f.getClassInfo(i, &mut ci) } == kResultOk && ci.cid == cid {
                name = cstr(&ci.name);
            }
        }
        name
    };
    let mut obj: *mut c_void = std::ptr::null_mut();
    // SAFETY: a class id and an IID of 16 bytes each, alive for the call; `obj` receives a counted
    // reference.
    if unsafe {
        f.createInstance(
            cid.as_ptr(),
            IComponent::IID.as_ptr() as *const c_char,
            &mut obj,
        )
    } != kResultOk
    {
        return Err("it couldn't be created".into());
    }
    // SAFETY: createInstance succeeded, so `obj` is a counted IComponent reference (or null);
    // `host_ptr`'s object lives in the Vst3 (`_host`) as long as the plugin does.
    let component = unsafe { ComPtr::from_raw(obj as *mut IComponent) }.ok_or("no component")?;
    if unsafe { component.initialize(host_ptr.as_ptr()) } != kResultOk {
        return Err("it didn't initialise".into());
    }
    // From here an error tears down what was made, in VST3's order, before the module can
    // go (its objects kept each other alive, then ran code unloaded; bug round E, E2-6).
    let mut guard = Teardown {
        component: component.clone(),
        controller: None,
        connection: None,
        handler: None,
        active: None,
        armed: true,
    };
    let processor = component
        .cast::<IAudioProcessor>()
        .ok_or("no audio processor")?;
    // The controller: the same object, or another class to create and connect.
    let (controller, separate) = match component.cast::<IEditController>() {
        Some(c) => (Some(c), false),
        None => {
            let mut ccid: TUID = [0; 16];
            let mut obj: *mut c_void = std::ptr::null_mut();
            // SAFETY: a TUID for the component to fill, then the class created as the component
            // was.
            let made = unsafe { component.getControllerClassId(&mut ccid) } == kResultOk
                && unsafe {
                    f.createInstance(
                        ccid.as_ptr(),
                        IEditController::IID.as_ptr() as *const c_char,
                        &mut obj,
                    )
                } == kResultOk;
            // SAFETY: `obj` is a counted IEditController reference when `made`; initialised with
            // the same host object.
            match made
                .then(|| unsafe { ComPtr::from_raw(obj as *mut IEditController) })
                .flatten()
            {
                Some(c) if unsafe { c.initialize(host_ptr.as_ptr()) } == kResultOk => {
                    (Some(c), true)
                }
                _ => (None, false),
            }
        }
    };
    let connection = if separate {
        match (
            component.cast::<IConnectionPoint>(),
            controller
                .as_ref()
                .and_then(|c| c.cast::<IConnectionPoint>()),
        ) {
            (Some(a), Some(b)) => {
                // SAFETY: both connection points are the plugin's, kept in `connection` until
                // disconnected in the Vst3's drop.
                unsafe {
                    a.connect(b.as_ptr());
                    b.connect(a.as_ptr());
                }
                Some((a, b))
            }
            _ => None,
        }
    } else {
        None
    };
    guard.controller = controller.as_ref().map(|c| (c.clone(), separate));
    guard.connection = connection.clone();
    let (to_audio, edits): (Edits, Edits) = (Arc::default(), Arc::default());
    let handler = ComWrapper::new(Handler {
        to_audio: to_audio.clone(),
        edits: edits.clone(),
    });
    if let Some(c) = &controller {
        // The controller learns the component's state, and gets the handler.
        let (_s, sp) = stream_with(&[]);
        // SAFETY: the stream (`_s` and `sp`) is ours and outlives these calls; component and
        // controller are initialised, on the main thread.
        if unsafe { component.getState(sp.as_ptr()) } == kResultOk {
            unsafe {
                sp.seek(
                    0,
                    IStreamSeekMode_::kIBSeekSet as int32,
                    std::ptr::null_mut(),
                )
            };
            unsafe { c.setComponentState(sp.as_ptr()) };
        }
        if let Some(h) = handler.to_com_ptr::<IComponentHandler>() {
            // SAFETY: `h` points at the handler kept in the Vst3 (`_handler`) until it is unset in
            // its drop.
            unsafe { c.setComponentHandler(h.as_ptr()) };
            guard.handler = Some(h);
        }
    }
    // The main buses: stereo where the chain is, else mono.
    let main = |dir: BusDirection| -> usize {
        // SAFETY: the initialised component, on the main thread; a zeroed BusInfo is plain C data
        // for it to fill.
        if unsafe { component.getBusCount(MediaTypes_::kAudio as MediaType, dir) } == 0 {
            return 0;
        }
        let mut bi: BusInfo = unsafe { std::mem::zeroed() };
        unsafe { component.getBusInfo(MediaTypes_::kAudio as MediaType, dir, 0, &mut bi) };
        unsafe { component.activateBus(MediaTypes_::kAudio as MediaType, dir, 0, 1) };
        bi.channelCount.max(0) as usize
    };
    let (mut ins, mut outs) = (
        main(BusDirections_::kInput as BusDirection),
        main(BusDirections_::kOutput as BusDirection),
    );
    let want = if channels >= 2 {
        SpeakerArr::kStereo
    } else {
        SpeakerArr::kMono
    };
    let (mut a_in, mut a_out) = (want, want);
    // SAFETY: two speaker arrangements on the stack, their counts (0 or 1) matching.
    if unsafe {
        processor.setBusArrangements(
            &mut a_in,
            (ins > 0) as int32,
            &mut a_out,
            (outs > 0) as int32,
        )
    } == kResultOk
    {
        let n = if channels >= 2 { 2 } else { 1 };
        if ins > 0 {
            ins = n;
        }
        if outs > 0 {
            outs = n;
        }
    }
    if outs == 0 {
        return Err("it has no audio output".into());
    }
    let mut setup = ProcessSetup {
        processMode: ProcessModes_::kRealtime as int32,
        symbolicSampleSize: SymbolicSampleSizes_::kSample32 as int32,
        maxSamplesPerBlock: block as int32,
        sampleRate: rate,
    };
    // SAFETY: a ProcessSetup on the stack.
    if unsafe { processor.setupProcessing(&mut setup) } != kResultOk {
        return Err("it refused the processing setup".into());
    }
    // SAFETY: set up above; activated in VST3's order (setActive, then setProcessing).
    unsafe {
        component.setActive(1);
        processor.setProcessing(1);
    }
    guard.active = Some(processor.clone());
    let changes = ComWrapper::new(Changes::default());
    let changes_ptr = changes.to_com_ptr::<IParameterChanges>().ok_or("changes")?;
    let mut inbuf = vec![vec![0.0; block]; ins.max(1)];
    let mut outbuf = vec![vec![0.0; block]; outs];
    let audio = Vst3Processor {
        processor: processor.clone(),
        ins,
        outs,
        chans: channels,
        in_ptrs: inbuf.iter_mut().map(|b| b.as_mut_ptr()).collect(),
        out_ptrs: outbuf.iter_mut().map(|b| b.as_mut_ptr()).collect(),
        inbuf,
        outbuf,
        changes,
        changes_ptr,
        from_main: to_audio.clone(),
        pending: Vec::new(),
        _module: module.clone(),
    };
    // Made whole: the Vst3's own drop tears it down from here.
    guard.armed = false;
    Ok(Box::new(Vst3 {
        component,
        processor,
        controller,
        separate,
        connection,
        name,
        audio: Some(audio),
        to_audio,
        edits,
        view: None,
        resize: Rc::default(),
        runloop: ui.runloop,
        owner: ui.owner,
        _host: host,
        _handler: handler,
        _module: module,
    }))
}

/// What a VST3 plugin being opened has made so far, torn down if the opening fails.
struct Teardown {
    component: ComPtr<IComponent>,
    controller: Option<(ComPtr<IEditController>, bool)>,
    connection: Option<(ComPtr<IConnectionPoint>, ComPtr<IConnectionPoint>)>,
    handler: Option<ComPtr<IComponentHandler>>,
    active: Option<ComPtr<IAudioProcessor>>,
    armed: bool,
}

impl Drop for Teardown {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // SAFETY: the plugin's objects, held here; torn down in VST3's order (as the
        // Vst3's own drop) on the main thread, before the module they come from is let go.
        unsafe {
            if let Some(p) = self.active.take() {
                p.setProcessing(0);
                self.component.setActive(0);
            }
            if let Some((a, b)) = self.connection.take() {
                a.disconnect(b.as_ptr());
                b.disconnect(a.as_ptr());
            }
            if let Some((c, separate)) = self.controller.take() {
                c.setComponentHandler(std::ptr::null_mut());
                if separate {
                    c.terminate();
                }
            }
            self.handler.take();
            self.component.terminate();
        }
    }
}

impl Drop for Vst3 {
    fn drop(&mut self) {
        self.close_editor();
        // The audio side is gone (or never handed over) before processing stops.
        drop(self.audio.take());
        // SAFETY: the plugin's objects, still held here; its audio side is gone first (above, or by
        // the chain's audio thread, which main.rs waits for before dropping a plugin); torn down in
        // VST3's order on the main thread.
        unsafe {
            self.processor.setProcessing(0);
            self.component.setActive(0);
            if let Some((a, b)) = self.connection.take() {
                a.disconnect(b.as_ptr());
                b.disconnect(a.as_ptr());
            }
            if let Some(c) = &self.controller {
                c.setComponentHandler(std::ptr::null_mut());
                if self.separate {
                    c.terminate();
                }
            }
            self.component.terminate();
        }
    }
}

impl Plugin for Vst3 {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn latency(&mut self) -> u32 {
        // SAFETY: the live processor, on the main thread, where VST3 asks for the latency.
        unsafe { self.processor.getLatencySamples() }
    }

    fn params(&mut self) -> Vec<Param> {
        let Some(c) = &self.controller else {
            return Vec::new();
        };
        let mut out = Vec::new();
        // SAFETY: the live controller, on the main thread; a zeroed ParameterInfo is plain C data
        // to fill, `text` the String128 getParamStringByValue fills.
        for i in 0..unsafe { c.getParameterCount() } {
            let mut pi: ParameterInfo = unsafe { std::mem::zeroed() };
            if unsafe { c.getParameterInfo(i, &mut pi) } != kResultOk {
                continue;
            }
            let value = unsafe { c.getParamNormalized(pi.id) };
            let mut text: String128 = [0; 128];
            unsafe { c.getParamStringByValue(pi.id, value, &mut text) };
            let units = utf16(&pi.units);
            let mut t = utf16(&text);
            if !units.is_empty() {
                t = format!("{t} {units}");
            }
            out.push(Param {
                id: pi.id,
                name: utf16(&pi.title),
                min: 0.0,
                max: 1.0,
                default: pi.defaultNormalizedValue,
                value,
                text: t,
            });
        }
        out
    }

    fn set_param(&mut self, id: u32, value: f64) {
        let v = value.clamp(0.0, 1.0);
        if let Some(c) = &self.controller {
            // SAFETY: the live controller, on the main thread.
            unsafe { c.setParamNormalized(id, v) };
        }
        self.to_audio.lock_ok().push((id, v));
    }

    fn save(&mut self) -> Vec<u8> {
        // The component's state, then the controller's, each with its length.
        let mut out = Vec::new();
        let (s, sp) = stream_with(&[]);
        // SAFETY: our stream, alive past the call; the plugin's live objects, on the main thread.
        unsafe { self.component.getState(sp.as_ptr()) };
        let comp = s.data.borrow().clone();
        out.extend_from_slice(&(comp.len() as u32).to_le_bytes());
        out.extend_from_slice(&comp);
        if let Some(c) = &self.controller {
            let (s, sp) = stream_with(&[]);
            // SAFETY: as above.
            unsafe { c.getState(sp.as_ptr()) };
            out.extend_from_slice(&s.data.borrow());
        }
        out
    }

    fn load(&mut self, state: &[u8]) -> Result<(), String> {
        let n = state
            .get(..4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
            .ok_or("short state")?;
        let comp = state.get(4..4 + n).ok_or("short state")?;
        let ctrl = &state[4 + n..];
        let (_s, sp) = stream_with(comp);
        // SAFETY: our streams, alive past each call; the plugin's live objects, on the main thread.
        if unsafe { self.component.setState(sp.as_ptr()) } != kResultOk {
            return Err("it refused its state".into());
        }
        if let Some(c) = &self.controller {
            let (_s, sp) = stream_with(comp);
            // SAFETY: as above.
            unsafe { c.setComponentState(sp.as_ptr()) };
            if !ctrl.is_empty() {
                let (_s, sp) = stream_with(ctrl);
                unsafe { c.setState(sp.as_ptr()) };
            }
        }
        Ok(())
    }

    fn processor(&mut self) -> Box<dyn Processor> {
        Box::new(self.audio.take().expect("the audio side is taken once"))
    }

    fn edits(&mut self) -> Vec<(u32, f64)> {
        std::mem::take(&mut *self.edits.lock_ok())
    }

    fn has_editor(&mut self) -> bool {
        // Whether it makes a view is known only by asking it for one.
        self.controller.is_some()
    }

    fn open_editor(&mut self, win: &HostWindow) -> Result<(u32, u32), String> {
        let c = self
            .controller
            .as_ref()
            .ok_or("this plugin has no window of its own")?;
        // SAFETY: createView hands over a counted reference (or null), which the ComPtr now owns.
        let view = unsafe { ComPtr::from_raw(c.createView(ViewType::kEditor)) }
            .ok_or("this plugin has no window of its own")?;
        let platform = if cfg!(windows) {
            kPlatformTypeHWND
        } else {
            kPlatformTypeX11EmbedWindowID
        };
        // SAFETY: the live view, on the main thread.
        if unsafe { view.isPlatformTypeSupported(platform) } != kResultOk {
            return Err(format!(
                "this plugin can't show its window in a {} window",
                crate::window::PLATFORM
            ));
        }
        let frame = ComWrapper::new(Frame {
            resize: self.resize.clone(),
            runloop: self.runloop.clone(),
            owner: self.owner,
            fds: RefCell::default(),
            timers: RefCell::default(),
        });
        let fp = frame.to_com_ptr::<IPlugFrame>().ok_or("frame")?;
        // SAFETY: the frame stays with the view (`self.view`) until setFrame(null) in close_editor;
        // `win` is our window, which outlives the editor (the chain closes the editor before
        // dropping the window).
        unsafe { view.setFrame(fp.as_ptr()) };
        if unsafe { view.attached(win.handle() as usize as *mut c_void, platform) } != kResultOk {
            unsafe { view.setFrame(std::ptr::null_mut()) };
            frame.clear();
            return Err("it didn't attach its window".into());
        }
        // SAFETY: a zeroed ViewRect is plain data for the attached view to fill.
        let mut r: ViewRect = unsafe { std::mem::zeroed() };
        let size = if unsafe { view.getSize(&mut r) } == kResultOk {
            (
                (r.right - r.left).max(1) as u32,
                (r.bottom - r.top).max(1) as u32,
            )
        } else {
            (640, 480)
        };
        self.view = Some((view, frame));
        Ok(size)
    }

    fn close_editor(&mut self) {
        if let Some((view, frame)) = self.view.take() {
            // SAFETY: the view held in `self.view`, removed then unframed in VST3's order; its
            // frame lives until after.
            unsafe {
                view.removed();
                view.setFrame(std::ptr::null_mut());
            }
            frame.clear();
        }
    }

    fn take_resize(&mut self) -> Option<(u32, u32)> {
        self.resize.take()
    }
}

impl Processor for Vst3Processor {
    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        let frames = frames.min(self.outbuf.first().map_or(0, Vec::len));
        for (j, b) in self.inbuf.iter_mut().enumerate() {
            b[..frames].copy_from_slice(&io[j % self.chans][..frames]);
        }
        // Changes from the desk and from the plugin's window; never waits for them.
        if let Some(mut q) = self.from_main.try_lock_ok() {
            self.pending.append(&mut q);
        }
        {
            self.changes.queues.borrow_mut().clear();
            for (id, v) in self.pending.drain(..) {
                let mut idx = 0;
                // SAFETY: `changes_ptr` is our Changes, alive in `self`; `id` and `idx` are locals;
                // the queue it returns stays in Changes until the next block.
                let q = unsafe { self.changes_ptr.addParameterData(&id, &mut idx) };
                if let Some(q) = unsafe { vst3::ComRef::from_raw(q) } {
                    unsafe { q.addPoint(0, v, &mut idx) };
                }
            }
        }
        let mut input = AudioBusBuffers {
            numChannels: self.ins as int32,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 {
                channelBuffers32: self.in_ptrs.as_mut_ptr(),
            },
        };
        let mut output = AudioBusBuffers {
            numChannels: self.outs as int32,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 {
                channelBuffers32: self.out_ptrs.as_mut_ptr(),
            },
        };
        let mut data = ProcessData {
            processMode: ProcessModes_::kRealtime as int32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as int32,
            numSamples: frames as int32,
            numInputs: (self.ins > 0) as int32,
            numOutputs: 1,
            inputs: if self.ins > 0 {
                &mut input
            } else {
                std::ptr::null_mut()
            },
            outputs: &mut output,
            inputParameterChanges: self.changes_ptr.as_ptr(),
            outputParameterChanges: std::ptr::null_mut(),
            inputEvents: std::ptr::null_mut(),
            outputEvents: std::ptr::null_mut(),
            processContext: std::ptr::null_mut(),
        };
        // SAFETY: `data` and what it points at live through the call: `in_ptrs`/`out_ptrs` hold
        // `ins`/`outs` pointers into `inbuf`/`outbuf`, each `block` long, and `frames` <= block
        // (above); this is the chain's audio thread, the only one that calls process.
        if unsafe { self.processor.process(&mut data) } != kResultOk {
            return; // passed on unchanged
        }
        for (c, o) in io.iter_mut().enumerate().take(self.chans) {
            o[..frames].copy_from_slice(&self.outbuf[c % self.outs][..frames]);
        }
    }
}

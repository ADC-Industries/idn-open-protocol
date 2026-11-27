//! The ASIO driver object hosts load (spec/extensions/asio-interface.md; Windows: COM,
//! in-process, through its class factory here; Linux: cwASIO's `instantiateDriver`,
//! linux.rs): written in Rust on idn-asio-abi's definitions (from cwASIO's headers), with
//! no Steinberg SDK. Every call goes to the driver in lib.rs as plain values; the
//! interface's structures are filled here.

// The interface's own names (ST_Int16LSB, kAsio…), as cwASIO spells them.
#![allow(non_upper_case_globals)]

use crate::Switch;
use idn_asio_abi::*;
use idn_common::sync::LockOk as _;
use std::ffi::{c_char, c_long, c_ulong, c_void};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, Ordering};
#[cfg(windows)]
use windows_sys::core::{GUID, HRESULT};
/// COM's result codes (Linux has no COM; `queryInterface` answers with the same values).
#[cfg(not(windows))]
#[allow(clippy::upper_case_acronyms)] // COM's name
type HRESULT = i32;

/// The driver's class id, which is also the interface id ASIO hosts ask for.
pub const CLSID: Guid = Guid {
    data1: 0x3567_6bb1,
    data2: 0xb34f,
    data3: 0x4615,
    data4: [0x92, 0x9e, 0x85, 0xfc, 0x4b, 0xd4, 0x0d, 0x3b],
};
const IID_IUNKNOWN: Guid = Guid {
    data1: 0,
    data2: 0,
    data3: 0,
    data4: [0xc0, 0, 0, 0, 0, 0, 0, 0x46],
};
#[cfg(windows)]
const IID_ICLASSFACTORY: Guid = Guid {
    data1: 1,
    data2: 0,
    data3: 0,
    data4: [0xc0, 0, 0, 0, 0, 0, 0, 0x46],
};

const S_OK: HRESULT = 0;
#[cfg(windows)]
const S_FALSE: HRESULT = 1;
const E_NOINTERFACE: HRESULT = 0x8000_4002_u32 as i32;
const E_POINTER: HRESULT = 0x8000_4003_u32 as i32;
#[cfg(windows)]
const E_OUTOFMEMORY: HRESULT = 0x8007_000E_u32 as i32;
#[cfg(windows)]
const CLASS_E_NOAGGREGATION: HRESULT = 0x8004_0110_u32 as i32;
#[cfg(windows)]
const CLASS_E_CLASSNOTAVAILABLE: HRESULT = 0x8004_0111_u32 as i32;

/// Driver objects alive, and the server's locks (`DllCanUnloadNow`).
static OBJECTS: AtomicI32 = AtomicI32::new(0);
#[cfg(windows)]
static LOCKS: AtomicI32 = AtomicI32::new(0);

/// A host's `long` as the driver's `i32` (`long` is 64 bits on Linux): out of range is -1,
/// which every call taking it refuses.
#[allow(clippy::useless_conversion, clippy::unnecessary_fallible_conversions)]
fn int(v: c_long) -> i32 {
    i32::try_from(v).unwrap_or(-1)
}

#[cfg(windows)]
fn guid(g: *const GUID) -> Option<Guid> {
    // SAFETY: COM passes a valid GUID or null.
    let g = unsafe { g.as_ref()? };
    Some(Guid {
        data1: g.data1,
        data2: g.data2,
        data3: g.data3,
        data4: g.data4,
    })
}

/// One driver object: the vtable pointer first (the interface's layout).
#[repr(C)]
struct Object {
    vtbl: *const DriverVtbl,
    refs: AtomicU32,
    /// The driver (lib.rs): made again for another instance before `init` (see
    /// `future`), then kept.
    drv: AtomicPtr<c_void>,
    /// Its instance (cwASIO's registration name, §A.6; at most 31 bytes): what
    /// `getDriverName` says.
    instance: std::sync::Mutex<String>,
    /// `init` was called: the instance stays.
    inited: AtomicBool,
    /// Drivers replaced by another instance's before `init` (a host thread may still be
    /// in a call to one): freed with the object.
    retired: std::sync::Mutex<Vec<usize>>,
    /// The host's callbacks while its buffers exist, and whether it takes time info.
    callbacks: AtomicPtr<Callbacks>,
    time_info: AtomicBool,
    /// The host takes `kAsioLatenciesChanged`, and the input latency it was last told
    /// (frames): a change (a desk on this machine starting to clock it, the site's
    /// latency) is told so recordings stay lined up (bug run 2026-11-25).
    latency_note: AtomicBool,
    told_latency: AtomicI32,
}

impl Object {
    /// The driver (lib.rs).
    fn drv(&self) -> *mut c_void {
        self.drv.load(Ordering::Acquire)
    }
}

/// # Safety
/// `this` is a live [`Object`] (hosts call through the pointer they were given).
unsafe fn me<'a>(this: *mut Driver) -> &'a Object {
    unsafe { &*(this as *const Object) }
}

unsafe extern "system" fn query_interface(
    this: *mut Driver,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> c_long {
    if out.is_null() {
        return E_POINTER as c_long;
    }
    // SAFETY: as COM passes them.
    let iid = unsafe { iid.as_ref() }.copied();
    // Linux: cwASIO asks with no id for the object's own (ASIO) interface.
    let default = cfg!(not(windows)) && iid.is_none();
    if default || iid == Some(IID_IUNKNOWN) || iid == Some(CLSID) {
        unsafe {
            add_ref(this);
            *out = this.cast();
        }
        return S_OK as c_long;
    }
    unsafe { *out = std::ptr::null_mut() };
    E_NOINTERFACE as c_long
}

unsafe extern "system" fn add_ref(this: *mut Driver) -> c_ulong {
    // (`c_ulong`: 32 bits on Windows, 64 on Linux.)
    (unsafe { me(this) }.refs.fetch_add(1, Ordering::AcqRel) + 1) as c_ulong
}

unsafe extern "system" fn release(this: *mut Driver) -> c_ulong {
    // (Not below zero: a count that wrapped would free the object again.)
    let Ok(was) = unsafe { me(this) }
        .refs
        .try_update(Ordering::AcqRel, Ordering::Acquire, |r| r.checked_sub(1))
    else {
        return 0;
    };
    let left = was - 1;
    if left == 0 {
        // On a clock thread (inside a program's callback, this driver's or another IDN
        // driver's): a stop there only tells the clock to end, so the object can't go
        // yet (that clock may be about to call `on_switch` with it). Let go on a thread of
        // its own, whose stop waits for the clock first (bug run 2026-11-26).
        if crate::on_a_clock() {
            let this = this as usize;
            if std::thread::Builder::new()
                .name("idn-asio-release".into())
                .spawn(move || unsafe { teardown(this as *mut Driver) })
                .is_ok()
            {
                return 0;
            }
        }
        unsafe { teardown(this) };
    }
    left as c_ulong
}

/// The object let go (its last reference): the clock stopped and its thread gone first,
/// through a shared reference (a host releasing it mid-switch has `on_switch` still
/// holding one, bug run 2026-11-25), then the object and its drivers freed.
///
/// # Safety
/// `this` is a live object with no references left.
unsafe fn teardown(this: *mut Driver) {
    let drv = unsafe { me(this) }.drv();
    unsafe { crate::idn_asio_stop(drv) };
    // SAFETY: the last reference: made by `Box::into_raw` in `new_object`.
    let o = unsafe { Box::from_raw(this as *mut Object) };
    unsafe { crate::idn_asio_free(o.drv()) };
    for d in o.retired.lock_ok().drain(..) {
        unsafe { crate::idn_asio_free(d as *mut c_void) };
    }
    OBJECTS.fetch_sub(1, Ordering::AcqRel);
}

unsafe extern "system" fn init(this: *mut Driver, _sys: *mut c_void) -> AsioBool {
    let o = unsafe { me(this) };
    // (Under the instance's lock: a name set meanwhile is before this, or refused.)
    o.inited.store(true, Ordering::Release);
    drop(o.instance.lock_ok());
    if unsafe { crate::idn_asio_init(o.drv()) } != 0 {
        ASIO_TRUE
    } else {
        ASIO_FALSE
    }
}

unsafe extern "system" fn get_driver_name(this: *mut Driver, name: *mut c_char) {
    if name.is_null() {
        return;
    }
    let instance = unsafe { me(this) }.instance.lock_ok().clone();
    // SAFETY: the interface's 32-byte buffer.
    put_text(
        unsafe { std::slice::from_raw_parts_mut(name, 32) },
        &instance,
    );
}

unsafe extern "system" fn get_driver_version(_this: *mut Driver) -> c_long {
    1
}

unsafe extern "system" fn get_error_message(this: *mut Driver, text: *mut c_char) {
    if text.is_null() {
        return;
    }
    unsafe { crate::idn_asio_error(me(this).drv(), text, 124) };
}

unsafe extern "system" fn start(this: *mut Driver) -> AsioError {
    let o = unsafe { me(this) };
    if o.callbacks.load(Ordering::Acquire).is_null() || crate::on_another_clock(o.drv()) {
        return ASE_InvalidMode;
    }
    let sw: Switch = on_switch;
    if unsafe { crate::idn_asio_start(o.drv(), sw, this.cast()) } != 0 {
        ASE_OK
    } else {
        ASE_HWMalfunction
    }
}

unsafe extern "system" fn stop(this: *mut Driver) -> AsioError {
    unsafe { crate::idn_asio_stop(me(this).drv()) };
    ASE_OK
}

unsafe extern "system" fn get_channels(
    this: *mut Driver,
    inputs: *mut c_long,
    outputs: *mut c_long,
) -> AsioError {
    if inputs.is_null() || outputs.is_null() {
        return ASE_InvalidParameter;
    }
    let (mut i, mut o) = (0, 0);
    unsafe {
        crate::idn_asio_channels(me(this).drv(), &mut i, &mut o);
        *inputs = i as c_long;
        *outputs = o as c_long;
    }
    ASE_OK
}

unsafe extern "system" fn get_latencies(
    this: *mut Driver,
    input: *mut c_long,
    output: *mut c_long,
) -> AsioError {
    if input.is_null() || output.is_null() {
        return ASE_InvalidParameter;
    }
    let (mut i, mut o) = (0, 0);
    unsafe {
        crate::idn_asio_latencies(me(this).drv(), &mut i, &mut o);
        *input = i as c_long;
        *output = o as c_long;
        // What the host knows now.
        me(this).told_latency.store(i, Ordering::Release);
    }
    ASE_OK
}

unsafe extern "system" fn get_buffer_size(
    _this: *mut Driver,
    min: *mut c_long,
    max: *mut c_long,
    preferred: *mut c_long,
    granularity: *mut c_long,
) -> AsioError {
    if min.is_null() || max.is_null() || preferred.is_null() || granularity.is_null() {
        return ASE_InvalidParameter;
    }
    let (mut a, mut b, mut c) = (0, 0, 0);
    unsafe {
        crate::idn_asio_buffer_sizes(&mut a, &mut b, &mut c);
        *min = a as c_long;
        *max = b as c_long;
        *preferred = c as c_long;
        // Powers of two.
        *granularity = -1;
    }
    ASE_OK
}

unsafe extern "system" fn can_sample_rate(_this: *mut Driver, rate: f64) -> AsioError {
    if crate::idn_asio_can_rate(rate) != 0 {
        ASE_OK
    } else {
        ASE_NoClock
    }
}

unsafe extern "system" fn get_sample_rate(this: *mut Driver, rate: *mut f64) -> AsioError {
    if rate.is_null() {
        return ASE_InvalidParameter;
    }
    unsafe { *rate = crate::idn_asio_rate(me(this).drv()) };
    ASE_OK
}

unsafe extern "system" fn set_sample_rate(this: *mut Driver, rate: f64) -> AsioError {
    let o = unsafe { me(this) };
    if crate::idn_asio_can_rate(rate) == 0 {
        return ASE_NoClock;
    }
    if unsafe { crate::idn_asio_rate(o.drv()) } == rate {
        return ASE_OK;
    }
    if unsafe { crate::idn_asio_set_rate(o.drv(), rate) } == 0 {
        return ASE_InvalidMode;
    }
    // (Not `sampleRateDidChange`: that is for changes the driver finds itself; called
    // back inside the host's own call it could re-enter or deadlock it; bug run
    // 2026-11-25.)
    ASE_OK
}

unsafe extern "system" fn get_clock_sources(
    _this: *mut Driver,
    clocks: *mut ClockSource,
    num: *mut c_long,
) -> AsioError {
    // SAFETY: the host's array of `*num` entries.
    if clocks.is_null() || num.is_null() || unsafe { *num } < 1 {
        return ASE_InvalidParameter;
    }
    let c = unsafe { &mut *clocks };
    c.index = 0;
    c.associated_channel = -1;
    c.associated_group = -1;
    c.is_current_source = ASIO_TRUE;
    put_text(&mut c.name, "This computer");
    unsafe { *num = 1 };
    ASE_OK
}

unsafe extern "system" fn set_clock_source(_this: *mut Driver, reference: c_long) -> AsioError {
    if reference == 0 {
        ASE_OK
    } else {
        ASE_InvalidParameter
    }
}

unsafe extern "system" fn get_sample_position(
    this: *mut Driver,
    pos: *mut Int64,
    stamp: *mut Int64,
) -> AsioError {
    if pos.is_null() || stamp.is_null() {
        return ASE_InvalidParameter;
    }
    let (mut s, mut n) = (0u64, 0u64);
    unsafe {
        crate::idn_asio_position(me(this).drv(), &mut s, &mut n);
        *pos = Int64::new(s);
        *stamp = Int64::new(n);
    }
    ASE_OK
}

unsafe extern "system" fn get_channel_info(this: *mut Driver, info: *mut ChannelInfo) -> AsioError {
    // SAFETY: the host's structure, its channel and direction filled in.
    let Some(info) = (unsafe { info.as_mut() }) else {
        return ASE_InvalidParameter;
    };
    let mut name = [0 as c_char; 32];
    let input = i32::from(info.is_input != ASIO_FALSE);
    if unsafe {
        crate::idn_asio_channel_name(
            me(this).drv(),
            input,
            int(info.channel),
            name.as_mut_ptr(),
            32,
        )
    } == 0
    {
        return ASE_InvalidParameter;
    }
    info.is_active = ASIO_TRUE;
    info.channel_group = 0;
    info.sample_type = ST_Float32LSB;
    info.name = name;
    ASE_OK
}

unsafe extern "system" fn create_buffers(
    this: *mut Driver,
    infos: *mut BufferInfo,
    n: c_long,
    size: c_long,
    callbacks: *const Callbacks,
) -> AsioError {
    let o = unsafe { me(this) };
    // From another IDN driver's callback: refused, nothing changed (its clock can't be
    // waited for there, and may still be using the buffers and the callbacks).
    if crate::on_another_clock(o.drv()) {
        return ASE_InvalidMode;
    }
    // The clock stopped first (no switch with the callbacks about to go), then the
    // previous buffers' callbacks are gone whatever happens now, a refusal included (a
    // failed call must not leave them, perhaps freed, in place).
    unsafe { crate::idn_asio_stop(o.drv()) };
    o.callbacks.store(std::ptr::null_mut(), Ordering::Release);
    if callbacks.is_null() || infos.is_null() || n < 0 {
        return ASE_InvalidParameter;
    }
    if unsafe { crate::idn_asio_create_buffers(o.drv(), int(size)) } == 0 {
        return ASE_InvalidMode;
    }
    // SAFETY: the host's array of `n` entries.
    let infos = unsafe { std::slice::from_raw_parts_mut(infos, n as usize) };
    for info in infos.iter_mut() {
        for half in 0..2 {
            let p = unsafe {
                crate::idn_asio_buffer(
                    o.drv(),
                    i32::from(info.is_input != ASIO_FALSE),
                    int(info.channel_num),
                    half,
                )
            };
            if p.is_null() {
                unsafe { crate::idn_asio_dispose_buffers(o.drv()) };
                return ASE_InvalidParameter;
            }
            info.buffers[half as usize] = p.cast();
        }
    }
    // SAFETY: the host's callbacks, which it keeps until `disposeBuffers`.
    let cb = unsafe { &*callbacks };
    let ti = cb
        .asio_message
        .is_some_and(|m| unsafe { m(kAsioSupportsTimeInfo, 0, std::ptr::null_mut(), std::ptr::null_mut()) } == 1);
    o.time_info.store(ti, Ordering::Release);
    // Whether the host takes latency changes; what it reads now is what it knows.
    let note = cb.asio_message.is_some_and(|m| unsafe {
        m(
            kAsioSelectorSupported,
            kAsioLatenciesChanged,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 1);
    let (mut i, mut out) = (0, 0);
    unsafe { crate::idn_asio_latencies(o.drv(), &mut i, &mut out) };
    o.told_latency.store(i, Ordering::Release);
    o.latency_note.store(note, Ordering::Release);
    o.callbacks
        .store(callbacks as *mut Callbacks, Ordering::Release);
    ASE_OK
}

unsafe extern "system" fn dispose_buffers(this: *mut Driver) -> AsioError {
    let o = unsafe { me(this) };
    // From another IDN driver's callback: refused, nothing changed (as createBuffers).
    if crate::on_another_clock(o.drv()) {
        return ASE_InvalidMode;
    }
    // The clock stopped first (from inside the callback too: it was left sending the
    // last buffers on and on).
    unsafe { crate::idn_asio_stop(o.drv()) };
    unsafe { crate::idn_asio_dispose_buffers(o.drv()) };
    o.callbacks.store(std::ptr::null_mut(), Ordering::Release);
    ASE_OK
}

unsafe extern "system" fn control_panel(_this: *mut Driver) -> AsioError {
    crate::idn_asio_control_panel();
    ASE_OK
}

unsafe extern "system" fn future(this: *mut Driver, selector: c_long, p: *mut c_void) -> AsioError {
    match selector {
        // (Not overloads: none are reported.)
        kAsioCanTimeInfo => ASE_SUCCESS,
        kcwASIOsetInstanceName => unsafe { set_instance(me(this), p as *const c_char) },
        _ => ASE_NotPresent,
    }
}

/// cwASIO's instance name (§A.6), before `init`: the driver made again with that
/// instance's settings (no threads or network yet: nothing else holds it). A null or
/// empty name only asks whether there are instances; the same name again, after `init`
/// too, is how a host reads it back.
///
/// # Safety
/// `name` is null or a NUL-terminated string.
unsafe fn set_instance(o: &Object, name: *const c_char) -> AsioError {
    if name.is_null() {
        return ASE_SUCCESS;
    }
    // SAFETY: the caller's NUL-terminated string.
    let Ok(name) = unsafe { std::ffi::CStr::from_ptr(name) }.to_str() else {
        return ASE_InvalidParameter;
    };
    if name.is_empty() {
        return ASE_SUCCESS;
    }
    if name.len() > 31 {
        return ASE_NotPresent;
    }
    let mut instance = o.instance.lock_ok();
    if *instance == name {
        return ASE_SUCCESS;
    }
    // Only while nothing of the driver is in use: before `init`, buffers and a clock.
    if o.inited.load(Ordering::Acquire)
        || !o.callbacks.load(Ordering::Acquire).is_null()
        || !crate::idle(o.drv())
    {
        return ASE_InvalidMode;
    }
    let drv = crate::idn_asio_new_instance(name);
    if drv.is_null() {
        return ASE_NoMemory;
    }
    let old = o.drv.swap(drv, Ordering::AcqRel);
    // Kept, not freed: another host thread may be in a call to it ("Both"); freed with
    // the object.
    o.retired.lock_ok().push(old as usize);
    *instance = name.to_owned();
    ASE_SUCCESS
}

unsafe extern "system" fn output_ready(_this: *mut Driver) -> AsioError {
    ASE_NotPresent
}

/// The clock's call, every buffer: the host's buffer switch, with time info when it takes
/// it.
unsafe extern "C" fn on_switch(ctx: *mut c_void, half: i32, samples: u64, nanos: u64) {
    let o = unsafe { me(ctx.cast()) };
    let cb = o.callbacks.load(Ordering::Acquire);
    // SAFETY: the host's callbacks, valid while its buffers exist.
    let Some(cb) = (unsafe { cb.as_ref() }) else {
        return;
    };
    // The latency changed since the host asked: it is told, before the switch (atomics
    // only). Nothing of this object is touched once the host's switch returns: its last
    // Release may come from inside it (bug run 2026-11-25).
    if o.latency_note.load(Ordering::Acquire)
        && let Some(m) = cb.asio_message
    {
        let (mut i, mut out) = (0, 0);
        unsafe { crate::idn_asio_latencies(o.drv(), &mut i, &mut out) };
        if o.told_latency.swap(i, Ordering::AcqRel) != i {
            unsafe {
                m(
                    kAsioLatenciesChanged,
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
        }
    }
    let rate = unsafe { crate::idn_asio_rate(o.drv()) };
    if o.time_info.load(Ordering::Acquire)
        && let Some(f) = cb.buffer_switch_time_info
    {
        let mut t = AsioTime::zeroed();
        t.time_info.speed = 1.0;
        t.time_info.sample_rate = rate;
        t.time_info.sample_position = Int64::new(samples);
        t.time_info.system_time = Int64::new(nanos);
        t.time_info.flags = kSystemTimeValid | kSamplePositionValid | kSampleRateValid;
        unsafe { f(&mut t, half as c_long, ASIO_FALSE) };
    } else if let Some(f) = cb.buffer_switch {
        unsafe { f(half as c_long, ASIO_FALSE) };
    }
}

/// A new driver object, its one reference the caller's (null: the driver couldn't be
/// made). Windows: the class factory's; Linux: `instantiateDriver`.
pub(crate) fn new_object() -> *mut Driver {
    let drv = crate::idn_asio_new();
    if drv.is_null() {
        return std::ptr::null_mut();
    }
    let obj = Box::into_raw(Box::new(Object {
        vtbl: &VTBL,
        refs: AtomicU32::new(1),
        drv: AtomicPtr::new(drv),
        instance: std::sync::Mutex::new(crate::DEFAULT_INSTANCE.to_owned()),
        inited: AtomicBool::new(false),
        retired: std::sync::Mutex::new(Vec::new()),
        callbacks: AtomicPtr::new(std::ptr::null_mut()),
        time_info: AtomicBool::new(false),
        latency_note: AtomicBool::new(false),
        told_latency: AtomicI32::new(0),
    }));
    OBJECTS.fetch_add(1, Ordering::AcqRel);
    obj as *mut Driver
}

static VTBL: DriverVtbl = DriverVtbl {
    query_interface,
    add_ref,
    release,
    init,
    get_driver_name,
    get_driver_version,
    get_error_message,
    start,
    stop,
    get_channels,
    get_latencies,
    get_buffer_size,
    can_sample_rate,
    get_sample_rate,
    set_sample_rate,
    get_clock_sources,
    set_clock_source,
    get_sample_position,
    get_channel_info,
    create_buffers,
    dispose_buffers,
    control_panel,
    future,
    output_ready,
};

#[cfg(windows)]
/// The class factory (one, static).
#[repr(C)]
struct Factory {
    vtbl: *const FactoryVtbl,
}

#[cfg(windows)]
#[repr(C)]
struct FactoryVtbl {
    query_interface:
        unsafe extern "system" fn(*mut Factory, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut Factory) -> c_ulong,
    release: unsafe extern "system" fn(*mut Factory) -> c_ulong,
    create_instance: unsafe extern "system" fn(
        *mut Factory,
        outer: *mut c_void,
        iid: *const GUID,
        out: *mut *mut c_void,
    ) -> HRESULT,
    lock_server: unsafe extern "system" fn(*mut Factory, lock: i32) -> HRESULT,
}

#[cfg(windows)]
/// The static factory, shared: its vtable is static and it has no state.
struct Shared(Factory);
#[cfg(windows)]
// SAFETY: a pointer to a static vtable, never written.
unsafe impl Sync for Shared {}

#[cfg(windows)]
static FACTORY_VTBL: FactoryVtbl = FactoryVtbl {
    query_interface: factory_query,
    add_ref: factory_add_ref,
    release: factory_release,
    create_instance,
    lock_server,
};
#[cfg(windows)]
static FACTORY: Shared = Shared(Factory {
    vtbl: &FACTORY_VTBL,
});

#[cfg(windows)]
unsafe extern "system" fn factory_query(
    this: *mut Factory,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    let iid = guid(iid);
    if iid == Some(IID_IUNKNOWN) || iid == Some(IID_ICLASSFACTORY) {
        unsafe { *out = this.cast() };
        return S_OK;
    }
    unsafe { *out = std::ptr::null_mut() };
    E_NOINTERFACE
}

#[cfg(windows)]
unsafe extern "system" fn factory_add_ref(_this: *mut Factory) -> c_ulong {
    2
}

#[cfg(windows)]
unsafe extern "system" fn factory_release(_this: *mut Factory) -> c_ulong {
    1
}

#[cfg(windows)]
unsafe extern "system" fn create_instance(
    _this: *mut Factory,
    outer: *mut c_void,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    unsafe { *out = std::ptr::null_mut() };
    if !outer.is_null() {
        return CLASS_E_NOAGGREGATION;
    }
    let this = new_object();
    if this.is_null() {
        return E_OUTOFMEMORY;
    }
    let iid = guid(iid).unwrap_or_default();
    // SAFETY: the object just made; the query adds a reference if it hands it out, and the
    // first one is let go (the object goes if the interface was wrong).
    unsafe {
        let hr = query_interface(this, &iid, out);
        release(this);
        hr
    }
}

#[cfg(windows)]
unsafe extern "system" fn lock_server(_this: *mut Factory, lock: i32) -> HRESULT {
    if lock != 0 {
        LOCKS.fetch_add(1, Ordering::AcqRel);
    } else {
        // (Not below zero on an unbalanced unlock.)
        let _ = LOCKS.try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n > 0).then(|| n - 1)
        });
    }
    S_OK
}

#[cfg(windows)]
/// `DllGetClassObject`'s work: the factory for this driver's class.
pub fn class_object(clsid: *const GUID, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    if guid(clsid) != Some(CLSID) {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    // SAFETY: the static factory; the query only reads `iid` and writes `out`.
    unsafe { factory_query(&FACTORY.0 as *const Factory as *mut Factory, iid, out) }
}

#[cfg(windows)]
/// `DllCanUnloadNow`'s answer.
pub fn can_unload() -> HRESULT {
    if OBJECTS.load(Ordering::Acquire) == 0 && LOCKS.load(Ordering::Acquire) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}

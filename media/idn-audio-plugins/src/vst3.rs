//! The plugins as VST3 (`GetPluginFactory`): for each of IDN Send and IDN Return a
//! processor (the audio) and a controller (the "Number" parameter, 1–64, as 64 steps), the
//! number kept in the processor's state.
#![allow(non_snake_case)]
// The bindings' integer types differ between Windows and the others (enum constants,
// TChar): casts that are no-ops on one are needed on the other.
#![allow(clippy::unnecessary_cast)]

use crate::engine::{self, Engine, Kind};
use std::ffi::{CString, c_char, c_void};
use std::ptr;
use std::slice;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComRef, ComWrapper, uid};

/// The number's parameter id.
const NUMBER: u32 = 1;

const SEND_PROCESSOR: TUID = uid(0x49444E53, 0x656E6450, 0x726F6331, 0x41444349);
const SEND_CONTROLLER: TUID = uid(0x49444E53, 0x656E6443, 0x74726C31, 0x41444349);
const RETURN_PROCESSOR: TUID = uid(0x49444E52, 0x65745072, 0x6F633141, 0x44434921);
const RETURN_CONTROLLER: TUID = uid(0x49444E52, 0x65744374, 0x726C3141, 0x44434921);

fn copy_cstring(src: &str, dst: &mut [c_char]) {
    let c = CString::new(src).unwrap_or_default();
    let bytes = c.as_bytes_with_nul();
    for (s, d) in bytes.iter().zip(dst.iter_mut()) {
        *d = *s as c_char;
    }
    if bytes.len() > dst.len()
        && let Some(last) = dst.last_mut()
    {
        *last = 0;
    }
}

fn copy_wstring(src: &str, dst: &mut [TChar]) {
    let mut len = 0;
    for (s, d) in src.encode_utf16().zip(dst.iter_mut()) {
        *d = s as TChar;
        len += 1;
    }
    if len < dst.len() {
        dst[len] = 0;
    } else if let Some(last) = dst.last_mut() {
        *last = 0;
    }
}

/// A NUL-ended UTF-16 string, at most `max` units.
unsafe fn read_wstring(p: *const TChar, max: usize) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    // SAFETY: the host's string, read up to its end or `max`.
    while n < max && unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    // SAFETY: the `n` units just read, all before the NUL.
    String::from_utf16_lossy(unsafe { slice::from_raw_parts(p as *const u16, n) })
}

/// The host's name, from the context `initialize` gets (`IHostApplication`).
unsafe fn host_name(context: *mut FUnknown) -> Option<String> {
    // SAFETY: the host's context, valid for the `initialize` call this runs in (null gives None).
    let ctx = unsafe { ComRef::from_raw(context) }?;
    let app = ctx.cast::<IHostApplication>()?;
    let mut name: String128 = [0; 128];
    // SAFETY: a String128 for the host to fill.
    (unsafe { app.getName(&mut name) } == kResultOk)
        // SAFETY: our own array, 128 units.
        .then(|| unsafe { read_wstring(name.as_ptr(), 128) })
}

/// The 4 bytes of a state, from an `IBStream`.
unsafe fn read_state(state: *mut IBStream) -> Option<u32> {
    // SAFETY: the host's stream, valid for the setState/setComponentState call this runs in.
    let s = unsafe { ComRef::from_raw(state) }?;
    let mut b = [0u8; 4];
    let mut got = 0;
    // SAFETY: 4 bytes into `b`.
    let r = unsafe { s.read(b.as_mut_ptr() as *mut c_void, 4, &mut got) };
    (r == kResultOk && got == 4)
        .then_some(())
        .and_then(|_| engine::load_state(&b))
}

unsafe fn write_state(state: *mut IBStream, slot: u32) -> tresult {
    // SAFETY: the host's stream, valid for the getState call this runs in.
    let Some(s) = (unsafe { ComRef::from_raw(state) }) else {
        return kInvalidArgument;
    };
    let b = engine::save_state(slot);
    let mut put = 0;
    // SAFETY: 4 bytes from `b`.
    let r = unsafe { s.write(b.as_ptr() as *mut c_void, 4, &mut put) };
    if r == kResultOk && put == 4 {
        kResultOk
    } else {
        kResultFalse
    }
}

// ---------------------------------------------------------------------------------------
// The processor.

struct Processor {
    engine: Arc<Engine>,
    rate: AtomicU32,
    max_frames: AtomicU32,
}

impl Class for Processor {
    type Interfaces = (IComponent, IAudioProcessor, IProcessContextRequirements);
}

impl Processor {
    fn new(kind: Kind) -> Processor {
        Processor {
            engine: Engine::new(kind, &engine::host_label(None)),
            rate: AtomicU32::new(48_000),
            max_frames: AtomicU32::new(4096),
        }
    }

    fn controller(&self) -> TUID {
        match self.engine.kind() {
            Kind::Send => SEND_CONTROLLER,
            Kind::Return => RETURN_CONTROLLER,
        }
    }
}

impl IPluginBaseTrait for Processor {
    unsafe fn initialize(&self, context: *mut FUnknown) -> tresult {
        // SAFETY: the host's context for this call, as `host_name` needs.
        if let Some(h) = unsafe { host_name(context) } {
            self.engine.set_host(&engine::host_label(Some(&h)));
        }
        kResultOk
    }

    unsafe fn terminate(&self) -> tresult {
        self.engine.deactivate();
        kResultOk
    }
}

impl IComponentTrait for Processor {
    unsafe fn getControllerClassId(&self, class_id: *mut TUID) -> tresult {
        // SAFETY: the host's out pointer, valid for the call (the IComponent contract).
        unsafe { *class_id = self.controller() };
        kResultOk
    }

    unsafe fn setIoMode(&self, _mode: IoMode) -> tresult {
        kResultOk
    }

    unsafe fn getBusCount(&self, media_type: MediaType, _dir: BusDirection) -> i32 {
        (media_type as MediaTypes == MediaTypes_::kAudio) as i32
    }

    unsafe fn getBusInfo(
        &self,
        media_type: MediaType,
        dir: BusDirection,
        index: i32,
        bus: *mut BusInfo,
    ) -> tresult {
        if media_type as MediaTypes != MediaTypes_::kAudio || index != 0 {
            return kInvalidArgument;
        }
        // SAFETY: the host's BusInfo to fill, valid and unaliased for the call.
        let bus = unsafe { &mut *bus };
        bus.mediaType = MediaTypes_::kAudio as MediaType;
        bus.direction = dir;
        bus.channelCount = engine::CHANNELS as i32;
        copy_wstring(
            if dir as BusDirections == BusDirections_::kInput {
                "Input"
            } else {
                "Output"
            },
            &mut bus.name,
        );
        bus.busType = BusTypes_::kMain as BusType;
        bus.flags = BusInfo_::BusFlags_::kDefaultActive as u32;
        kResultOk
    }

    unsafe fn getRoutingInfo(&self, _i: *mut RoutingInfo, _o: *mut RoutingInfo) -> tresult {
        kNotImplemented
    }

    unsafe fn activateBus(&self, _t: MediaType, _d: BusDirection, _i: i32, _s: TBool) -> tresult {
        kResultOk
    }

    unsafe fn setActive(&self, state: TBool) -> tresult {
        if state != 0 {
            self.engine.activate(
                self.rate.load(Ordering::Relaxed),
                self.max_frames.load(Ordering::Relaxed) as usize,
            );
        } else {
            self.engine.deactivate();
        }
        kResultOk
    }

    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        // SAFETY: the host's stream for this call, as `read_state` needs.
        match unsafe { read_state(state) } {
            Some(n) => {
                self.engine.set_slot(n);
                kResultOk
            }
            None => kResultFalse,
        }
    }

    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        // SAFETY: the host's stream for this call, as `write_state` needs.
        unsafe { write_state(state, self.engine.slot()) }
    }
}

impl IAudioProcessorTrait for Processor {
    unsafe fn setBusArrangements(
        &self,
        inputs: *mut SpeakerArrangement,
        num_ins: i32,
        outputs: *mut SpeakerArrangement,
        num_outs: i32,
    ) -> tresult {
        // Stereo only (a mono track is the host's to fold).
        let ok = num_ins == 1
            && num_outs == 1
            // SAFETY: one arrangement each way (counts checked first, `&&` short-circuits), the
            // host's arrays.
            && unsafe { *inputs == SpeakerArr::kStereo && *outputs == SpeakerArr::kStereo };
        if ok { kResultTrue } else { kResultFalse }
    }

    unsafe fn getBusArrangement(
        &self,
        _dir: BusDirection,
        index: i32,
        arr: *mut SpeakerArrangement,
    ) -> tresult {
        if index != 0 {
            return kInvalidArgument;
        }
        // SAFETY: the host's out pointer, valid for the call.
        unsafe { *arr = SpeakerArr::kStereo };
        kResultOk
    }

    unsafe fn canProcessSampleSize(&self, size: i32) -> tresult {
        if size as SymbolicSampleSizes == SymbolicSampleSizes_::kSample32 {
            kResultOk
        } else {
            kResultFalse
        }
    }

    unsafe fn getLatencySamples(&self) -> u32 {
        0
    }

    unsafe fn setupProcessing(&self, setup: *mut ProcessSetup) -> tresult {
        // SAFETY: the host's ProcessSetup, valid for the call.
        let s = unsafe { &*setup };
        self.rate
            .store(s.sampleRate.round().max(8000.0) as u32, Ordering::Relaxed);
        self.max_frames
            .store(s.maxSamplesPerBlock.max(1) as u32, Ordering::Relaxed);
        self.engine
            .set_offline(s.processMode as ProcessModes == ProcessModes_::kOffline);
        kResultOk
    }

    unsafe fn setProcessing(&self, _state: TBool) -> tresult {
        kResultOk
    }

    unsafe fn process(&self, data: *mut ProcessData) -> tresult {
        // SAFETY: the host's ProcessData, valid for the call; what it points at is read only below.
        let d = unsafe { &*data };
        // The number, at its last point in this block.
        // SAFETY: the host's parameter changes and their queues, alive for this process call; read
        // on the audio thread that called it.
        if let Some(changes) = unsafe { ComRef::from_raw(d.inputParameterChanges) } {
            for i in 0..unsafe { changes.getParameterCount() } {
                let Some(q) = (unsafe { ComRef::from_raw(changes.getParameterData(i)) }) else {
                    continue;
                };
                let n = unsafe { q.getPointCount() };
                if unsafe { q.getParameterId() } != NUMBER || n == 0 {
                    continue;
                }
                let (mut offset, mut v) = (0, 0.0);
                if unsafe { q.getPoint(n - 1, &mut offset, &mut v) } == kResultOk {
                    self.engine.set_slot(engine::normalized_to_slot(v));
                }
            }
        }
        let frames = d.numSamples.max(0) as usize;
        if d.numInputs < 1 || d.numOutputs < 1 || frames == 0 {
            return kResultOk;
        }
        // SAFETY: at least one input and one output bus (checked above), the host's.
        let (ins, outs) = unsafe { (&*d.inputs, &*d.outputs) };
        let n = (outs.numChannels.max(0) as usize).min(engine::CHANNELS);
        let in_n = ins.numChannels.max(0) as usize;
        // SAFETY: 32-bit buffers: canProcessSampleSize accepts only kSample32, so the host sets
        // that union field.
        let (ip, op) = unsafe {
            (
                ins.__field0.channelBuffers32,
                outs.__field0.channelBuffers32,
            )
        };
        if op.is_null() || n == 0 {
            return kResultOk;
        }
        // The slices below are made only for buffers that don't overlap other ones: each
        // output its own, and an input either its own or its channel's output (as hosts share
        // them). A host aliasing otherwise gets this block skipped, not overlapping
        // references.
        // SAFETY: `n` and `in_n` are within the buses' channel arrays (the host's).
        let out_at = |c: usize| unsafe { *op.add(c) } as *const f32;
        let in_at = |c: usize| -> *const f32 {
            if ip.is_null() || c >= in_n {
                std::ptr::null()
            } else {
                // SAFETY: as above.
                unsafe { *ip.add(c) }
            }
        };
        for c in 0..n {
            for k in 0..n {
                if (k != c && out_at(k) == out_at(c)) || (k != c && !in_at(c).is_null() && in_at(c) == out_at(k)) {
                    return kResultOk;
                }
            }
        }
        let mut chans: Vec<&mut [f32]> = Vec::with_capacity(engine::CHANNELS);
        for c in 0..n {
            // SAFETY: the host's buffers, `frames` long; input and output may be the same.
            let o = unsafe { slice::from_raw_parts_mut(*op.add(c), frames) };
            if !ip.is_null() && c < in_n {
                // SAFETY: `c` < the input bus's channel count, so in the host's array.
                let i = unsafe { *ip.add(c) };
                if !std::ptr::eq(i, o.as_ptr()) {
                    // SAFETY: the host's input, `frames` long, not the output it goes to nor any
                    // other channel's output (both checked above).
                    let i = unsafe { slice::from_raw_parts(i, frames) };
                    o.copy_from_slice(i);
                }
            } else {
                o.fill(0.0);
            }
            chans.push(o);
        }
        self.engine.process(&mut chans, frames);
        kResultOk
    }

    unsafe fn getTailSamples(&self) -> u32 {
        0
    }
}

impl IProcessContextRequirementsTrait for Processor {
    unsafe fn getProcessContextRequirements(&self) -> u32 {
        0
    }
}

// ---------------------------------------------------------------------------------------
// The controller.

struct Controller {
    slot: AtomicU32,
}

impl Class for Controller {
    type Interfaces = (IEditController,);
}

impl IPluginBaseTrait for Controller {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }

    unsafe fn terminate(&self) -> tresult {
        kResultOk
    }
}

impl IEditControllerTrait for Controller {
    unsafe fn setComponentState(&self, state: *mut IBStream) -> tresult {
        // SAFETY: the host's stream for this call, as `read_state` needs.
        if let Some(n) = unsafe { read_state(state) } {
            self.slot.store(n, Ordering::Relaxed);
        }
        kResultOk
    }

    unsafe fn setState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }

    unsafe fn getState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }

    unsafe fn getParameterCount(&self) -> i32 {
        1
    }

    unsafe fn getParameterInfo(&self, index: i32, info: *mut ParameterInfo) -> tresult {
        if index != 0 {
            return kInvalidArgument;
        }
        // SAFETY: the host's ParameterInfo to fill, valid and unaliased for the call.
        let info = unsafe { &mut *info };
        info.id = NUMBER;
        copy_wstring("Number", &mut info.title);
        copy_wstring("Number", &mut info.shortTitle);
        copy_wstring("", &mut info.units);
        info.stepCount = (engine::SLOTS.end() - engine::SLOTS.start()) as i32;
        info.defaultNormalizedValue = engine::slot_to_normalized(1);
        info.unitId = 0;
        info.flags = ParameterInfo_::ParameterFlags_::kCanAutomate as i32;
        kResultOk
    }

    unsafe fn getParamStringByValue(&self, id: u32, v: f64, s: *mut String128) -> tresult {
        if id != NUMBER {
            return kInvalidArgument;
        }
        // SAFETY: the host's String128 to fill, valid for the call.
        copy_wstring(&engine::normalized_to_slot(v).to_string(), unsafe {
            &mut *s
        });
        kResultOk
    }

    unsafe fn getParamValueByString(&self, id: u32, s: *mut TChar, v: *mut f64) -> tresult {
        if id != NUMBER {
            return kInvalidArgument;
        }
        // SAFETY: the host's NUL-ended string (read at most 128 units, a String128's size) and out
        // pointer, valid for the call.
        match unsafe { read_wstring(s, 128) }.trim().parse::<u32>() {
            Ok(n) if engine::SLOTS.contains(&n) => {
                unsafe { *v = engine::slot_to_normalized(n) };
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn normalizedParamToPlain(&self, _id: u32, v: f64) -> f64 {
        engine::normalized_to_slot(v) as f64
    }

    unsafe fn plainParamToNormalized(&self, _id: u32, plain: f64) -> f64 {
        engine::slot_to_normalized(plain.round().max(0.0) as u32)
    }

    unsafe fn getParamNormalized(&self, id: u32) -> f64 {
        if id == NUMBER {
            engine::slot_to_normalized(self.slot.load(Ordering::Relaxed))
        } else {
            0.0
        }
    }

    unsafe fn setParamNormalized(&self, id: u32, v: f64) -> tresult {
        if id != NUMBER {
            return kInvalidArgument;
        }
        self.slot
            .store(engine::normalized_to_slot(v), Ordering::Relaxed);
        kResultOk
    }

    unsafe fn setComponentHandler(&self, _handler: *mut IComponentHandler) -> tresult {
        kResultOk
    }

    unsafe fn createView(&self, _name: *const c_char) -> *mut IPlugView {
        ptr::null_mut()
    }
}

// ---------------------------------------------------------------------------------------
// The factory.

struct Factory;

impl Class for Factory {
    type Interfaces = (IPluginFactory, IPluginFactory2);
}

/// The classes: (id, category, name, processor's kind).
const CLASSES: [(TUID, &str, &str, Option<Kind>); 4] = [
    (
        SEND_PROCESSOR,
        "Audio Module Class",
        "IDN Send",
        Some(Kind::Send),
    ),
    (
        SEND_CONTROLLER,
        "Component Controller Class",
        "IDN Send",
        None,
    ),
    (
        RETURN_PROCESSOR,
        "Audio Module Class",
        "IDN Return",
        Some(Kind::Return),
    ),
    (
        RETURN_CONTROLLER,
        "Component Controller Class",
        "IDN Return",
        None,
    ),
];

impl IPluginFactoryTrait for Factory {
    unsafe fn getFactoryInfo(&self, info: *mut PFactoryInfo) -> tresult {
        // SAFETY: the host's PFactoryInfo to fill, valid and unaliased for the call.
        let info = unsafe { &mut *info };
        copy_cstring("ADC Industries", &mut info.vendor);
        copy_cstring("", &mut info.url);
        copy_cstring("", &mut info.email);
        info.flags = PFactoryInfo_::FactoryFlags_::kUnicode as int32;
        kResultOk
    }

    unsafe fn countClasses(&self) -> i32 {
        CLASSES.len() as i32
    }

    unsafe fn getClassInfo(&self, index: i32, info: *mut PClassInfo) -> tresult {
        let Some((cid, category, name, _)) = CLASSES.get(index as usize) else {
            return kInvalidArgument;
        };
        // SAFETY: the host's PClassInfo to fill, valid and unaliased for the call.
        let info = unsafe { &mut *info };
        info.cid = *cid;
        info.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as int32;
        copy_cstring(category, &mut info.category);
        copy_cstring(name, &mut info.name);
        kResultOk
    }

    unsafe fn createInstance(
        &self,
        cid: FIDString,
        iid: FIDString,
        obj: *mut *mut c_void,
    ) -> tresult {
        // SAFETY: a class id is 16 bytes (FIDString to a TUID, as VST3 passes it; align 1).
        let cid = unsafe { *(cid as *const TUID) };
        let instance = if cid == SEND_PROCESSOR || cid == RETURN_PROCESSOR {
            let kind = if cid == SEND_PROCESSOR {
                Kind::Send
            } else {
                Kind::Return
            };
            ComWrapper::new(Processor::new(kind)).to_com_ptr::<FUnknown>()
        } else if cid == SEND_CONTROLLER || cid == RETURN_CONTROLLER {
            ComWrapper::new(Controller {
                slot: AtomicU32::new(1),
            })
            .to_com_ptr::<FUnknown>()
        } else {
            None
        };
        match instance {
            Some(i) => {
                let p = i.as_ptr();
                // SAFETY: `p` is the object just made, alive while `i` is; queryInterface gets the
                // host's IID and out pointer and adds the reference it hands over before `i` drops
                // ours.
                unsafe { ((*(*p).vtbl).queryInterface)(p, iid as *mut TUID, obj) }
            }
            None => kInvalidArgument,
        }
    }
}

impl IPluginFactory2Trait for Factory {
    unsafe fn getClassInfo2(&self, index: i32, info: *mut PClassInfo2) -> tresult {
        let Some((cid, category, name, kind)) = CLASSES.get(index as usize) else {
            return kInvalidArgument;
        };
        // SAFETY: the host's PClassInfo2 to fill, valid and unaliased for the call.
        let info = unsafe { &mut *info };
        info.cid = *cid;
        info.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as int32;
        copy_cstring(category, &mut info.category);
        copy_cstring(name, &mut info.name);
        info.classFlags = 0;
        copy_cstring(
            if kind.is_some() { "Fx|Network" } else { "" },
            &mut info.subCategories,
        );
        copy_cstring("ADC Industries", &mut info.vendor);
        copy_cstring(env!("CARGO_PKG_VERSION"), &mut info.version);
        copy_cstring("VST 3.7", &mut info.sdkVersion);
        kResultOk
    }
}

#[cfg(target_os = "windows")]
#[unsafe(no_mangle)]
extern "system" fn InitDll() -> bool {
    true
}

#[cfg(target_os = "windows")]
#[unsafe(no_mangle)]
extern "system" fn ExitDll() -> bool {
    true
}

#[cfg(target_os = "linux")]
#[unsafe(no_mangle)]
extern "system" fn ModuleEntry(_library_handle: *mut c_void) -> bool {
    true
}

#[cfg(target_os = "linux")]
#[unsafe(no_mangle)]
extern "system" fn ModuleExit() -> bool {
    true
}

#[cfg(target_os = "macos")]
#[unsafe(no_mangle)]
extern "system" fn BundleEntry(_bundle_ref: *mut c_void) -> bool {
    true
}

#[cfg(target_os = "macos")]
#[unsafe(no_mangle)]
extern "system" fn BundleExit() -> bool {
    true
}

#[unsafe(no_mangle)]
extern "system" fn GetPluginFactory() -> *mut IPluginFactory {
    // Never unloaded once used: its threads (QUIC, discovery) outlive the plug-ins.
    idn_common::module::pin_own_module();
    ComWrapper::new(Factory)
        .to_com_ptr::<IPluginFactory>()
        .map_or(ptr::null_mut(), |p| p.into_raw())
}

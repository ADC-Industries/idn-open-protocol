#![cfg(windows)]
//! IDN Camera's media source (spec/extensions/idn-display.md §D.5): the DLL Windows'
//! Frame Server loads for the virtual camera `idn-display --virtual-camera` creates.
//!
//! The media source Windows' Frame Server loads for IDN Camera: one video stream, NV12 at
//! the camera's size and rate ([`crate::shm`]), its pictures from the shared memory
//! `idn-display` writes (black while nothing does). A COM in-process server
//! (`DllGetClassObject`, `DllRegisterServer`), live, no pause.

use idn_common::sync::LockOk as _;
use idn_common::win::wide;
use idn_vcam::shm;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use windows::Win32::Foundation::*;
use windows::Win32::Media::KernelStreaming::{
    IKsControl, IKsControl_Impl, KSIDENTIFIER, PINNAME_VIDEO_CAPTURE,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleFileNameW, GetModuleHandleExW,
};
use windows::Win32::System::Memory::*;
use windows::Win32::System::Registry::*;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::core::{GUID, HRESULT, IUnknown, Interface, PCWSTR, Ref, Result, implement, w};
use windows_core::{BOOL, IUnknownImpl};

/// IDN Camera's media source.
pub const CLSID: GUID = GUID::from_u128(idn_vcam::CLSID_U128);
use idn_vcam::CLSID_TEXT;

fn err(hr: HRESULT) -> windows::core::Error {
    windows::core::Error::from_hresult(hr)
}

/// The shared memory, made here (a service may make `Global\` names; the user's program may
/// then open it) for the session at the screen: the Frame Server, administrators and that
/// session's user may read and write it, no other user (bug round E, E3-7: any
/// interactive user could, and so watch or replace another's camera).
struct Shm {
    map: HANDLE,
    view: *mut u8,
    len: usize,
}

// SAFETY: the view is plain memory shared between processes; access is by copies and a
// sequence number.
unsafe impl Send for Shm {}

impl Shm {
    fn open() -> Result<Shm> {
        let len = shm::size(shm::WIDTH, shm::HEIGHT);
        // SAFETY: no arguments; 0xFFFFFFFF when no session is at the screen.
        let session =
            unsafe { windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId() };
        if session == u32::MAX {
            return Err(err(E_FAIL));
        }
        let user = session_user_sid(session).ok_or_else(|| err(E_ACCESSDENIED))?;
        let sddl = wide(&format!(
            "D:(A;;GA;;;SY)(A;;GA;;;LS)(A;;GA;;;BA)(A;;GRGW;;;{user})"
        ));
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: Win32 calls; the descriptor is freed once the mapping is made.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )?;
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd.0,
                bInheritHandle: false.into(),
            };
            let name = wide(&shm::name(session));
            let map = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                Some(&sa),
                PAGE_READWRITE,
                0,
                len as u32,
                PCWSTR(name.as_ptr()),
            );
            // The descriptor freed once used (one leaked at every try; bug round E, E3-8).
            let _ = LocalFree(Some(HLOCAL(sd.0)));
            let map = map?;
            let view = MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, len);
            if view.Value.is_null() {
                let _ = CloseHandle(map);
                return Err(windows::core::Error::from_thread());
            }
            let p = view.Value as *mut u8;
            // A fresh one: its header.
            if std::slice::from_raw_parts(p, 4) != shm::MAGIC {
                let h = shm::header(shm::WIDTH, shm::HEIGHT, shm::FPS);
                std::ptr::copy_nonoverlapping(h.as_ptr(), p, h.len());
            }
            Ok(Shm { map, view: p, len })
        }
    }

    /// The newest picture into `out` (NV12 of the camera's size): `false` if there is none
    /// (no writer, or none for a while).
    fn read(&self, out: &mut [u8]) -> bool {
        // SAFETY: the view is `len` bytes; the sequence number is 8-aligned at its offset.
        unsafe {
            let seq = &*(self.view.add(shm::OFF_SEQ) as *const AtomicU64);
            let written = &*(self.view.add(shm::OFF_WRITTEN) as *const AtomicU64);
            let w = written.load(Ordering::Acquire);
            if w == 0 || GetTickCount64().saturating_sub(w) > shm::STALE_MS {
                return false;
            }
            let n = out.len().min(self.len - shm::HEADER);
            for _ in 0..3 {
                let a = seq.load(Ordering::Acquire);
                if a % 2 == 1 {
                    std::thread::yield_now();
                    continue;
                }
                std::ptr::copy_nonoverlapping(self.view.add(shm::HEADER), out.as_mut_ptr(), n);
                if seq.load(Ordering::Acquire) == a {
                    return true;
                }
            }
            false
        }
    }
}

/// The SID (text, `S-1-5-21-…`) of the user signed in to Windows session `session`.
fn session_user_sid(session: u32) -> Option<String> {
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{LookupAccountNameW, PSID, SID_NAME_USE};
    use windows::Win32::System::RemoteDesktop::{
        WTS_CURRENT_SERVER_HANDLE, WTS_INFO_CLASS, WTSDomainName, WTSFreeMemory,
        WTSQuerySessionInformationW, WTSUserName,
    };
    use windows::core::PWSTR;
    // SAFETY: Win32 calls; every buffer is sized as the call says and freed after.
    unsafe {
        let info = |class: WTS_INFO_CLASS| -> Option<String> {
            let mut buf = PWSTR::null();
            let mut n = 0u32;
            WTSQuerySessionInformationW(
                Some(WTS_CURRENT_SERVER_HANDLE),
                session,
                class,
                &mut buf,
                &mut n,
            )
            .ok()?;
            let s = buf.to_string().ok();
            WTSFreeMemory(buf.0 as *mut c_void);
            s.filter(|s| !s.is_empty())
        };
        let user = info(WTSUserName)?;
        let account = wide(&match info(WTSDomainName) {
            Some(d) => format!("{d}\\{user}"),
            None => user,
        });
        let (mut sid_len, mut dom_len) = (0u32, 0u32);
        let mut kind = SID_NAME_USE::default();
        let _ = LookupAccountNameW(
            PCWSTR::null(),
            PCWSTR(account.as_ptr()),
            None,
            &mut sid_len,
            None,
            &mut dom_len,
            &mut kind,
        );
        if sid_len == 0 {
            return None;
        }
        let mut sid = vec![0u8; sid_len as usize];
        let mut dom = vec![0u16; dom_len.max(1) as usize];
        LookupAccountNameW(
            PCWSTR::null(),
            PCWSTR(account.as_ptr()),
            Some(PSID(sid.as_mut_ptr() as *mut c_void)),
            &mut sid_len,
            Some(PWSTR(dom.as_mut_ptr())),
            &mut dom_len,
            &mut kind,
        )
        .ok()?;
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(PSID(sid.as_mut_ptr() as *mut c_void), &mut text).ok()?;
        let s = text.to_string().ok();
        let _ = LocalFree(Some(HLOCAL(text.0 as *mut c_void)));
        // (Only a SID's characters: it goes into a security descriptor's text.)
        s.filter(|s| {
            s.starts_with("S-")
                && s.chars()
                    .all(|c| c.is_ascii_digit() || c == '-' || c == 'S')
        })
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        // SAFETY: the view and handle are ours.
        unsafe {
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.view as *mut c_void,
            });
            let _ = CloseHandle(self.map);
        }
    }
}

/// The video stream's media type: NV12, the camera's size and rate.
fn media_type() -> Result<IMFMediaType> {
    // SAFETY: Media Foundation calls on objects made here.
    unsafe {
        let mt = MFCreateMediaType()?;
        mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        mt.SetUINT64(
            &MF_MT_FRAME_SIZE,
            ((shm::WIDTH as u64) << 32) | shm::HEIGHT as u64,
        )?;
        mt.SetUINT64(&MF_MT_FRAME_RATE, ((shm::FPS as u64) << 32) | 1)?;
        mt.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
        mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        mt.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        mt.SetUINT32(&MF_MT_DEFAULT_STRIDE, shm::WIDTH)?;
        Ok(mt)
    }
}

/// What the Frame Server wants to know of a camera's stream.
fn stream_attributes(a: &IMFAttributes) -> Result<()> {
    // SAFETY: Media Foundation calls.
    unsafe {
        a.SetGUID(&MF_DEVICESTREAM_STREAM_CATEGORY, &PINNAME_VIDEO_CAPTURE)?;
        a.SetUINT32(&MF_DEVICESTREAM_STREAM_ID, 0)?;
        a.SetUINT32(&MF_DEVICESTREAM_FRAMESERVER_SHARED, 1)?;
        a.SetUINT32(
            &MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES,
            MFFrameSourceTypes_Color.0 as u32,
        )?;
    }
    Ok(())
}

fn attributes(n: u32) -> Result<IMFAttributes> {
    let mut a = None;
    // SAFETY: Media Foundation call.
    unsafe { MFCreateAttributes(&mut a, n)? };
    a.ok_or_else(|| err(E_FAIL))
}

fn no_ks() -> Result<()> {
    Err(err(HRESULT::from_win32(ERROR_SET_NOT_FOUND.0)))
}

// --- The stream -----------------------------------------------------------------------

#[implement(IMFMediaStream2, IKsControl)]
struct Stream {
    source: Mutex<Option<IMFMediaSource>>,
    sd: IMFStreamDescriptor,
    queue: IMFMediaEventQueue,
    state: Mutex<MF_STREAM_STATE>,
    shm: Mutex<Option<Shm>>,
    frame: Mutex<Vec<u8>>,
}

impl Stream {
    fn new(source: IMFMediaSource, sd: IMFStreamDescriptor) -> Result<Stream> {
        Ok(Stream {
            source: Mutex::new(Some(source)),
            sd,
            // SAFETY: Media Foundation call.
            queue: unsafe { MFCreateEventQueue()? },
            state: Mutex::new(MF_STREAM_STATE_STOPPED),
            shm: Mutex::new(Shm::open().ok()),
            frame: Mutex::new(shm::black(shm::WIDTH, shm::HEIGHT)),
        })
    }

    fn shutdown(&self) {
        // SAFETY: Media Foundation call.
        let _ = unsafe { self.queue.Shutdown() };
        *self.source.lock_ok() = None;
        *self.shm.lock_ok() = None;
    }
}

impl IMFMediaEventGenerator_Impl for Stream_Impl {
    fn GetEvent(&self, flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> Result<IMFMediaEvent> {
        // SAFETY: Media Foundation call on our own queue.
        unsafe { self.queue.GetEvent(flags.0) }
    }
    fn BeginGetEvent(&self, cb: Ref<IMFAsyncCallback>, state: Ref<IUnknown>) -> Result<()> {
        // SAFETY: Media Foundation call; the caller's callback and state passed on as given.
        unsafe { self.queue.BeginGetEvent(cb.as_ref(), state.as_ref()) }
    }
    fn EndGetEvent(&self, result: Ref<IMFAsyncResult>) -> Result<IMFMediaEvent> {
        // SAFETY: Media Foundation call; the caller's result passed on as given.
        unsafe { self.queue.EndGetEvent(result.as_ref()) }
    }
    fn QueueEvent(
        &self,
        met: u32,
        ext: *const GUID,
        hr: HRESULT,
        value: *const PROPVARIANT,
    ) -> Result<()> {
        // SAFETY: Media Foundation call; COM's caller gives valid (or null) `ext` and `value`.
        unsafe { self.queue.QueueEventParamVar(met, ext, hr, value) }
    }
}

impl IMFMediaStream_Impl for Stream_Impl {
    fn GetMediaSource(&self) -> Result<IMFMediaSource> {
        self.source
            .lock_ok()
            .clone()
            .ok_or_else(|| err(MF_E_SHUTDOWN))
    }
    fn GetStreamDescriptor(&self) -> Result<IMFStreamDescriptor> {
        Ok(self.sd.clone())
    }
    fn RequestSample(&self, token: Ref<IUnknown>) -> Result<()> {
        if self.source.lock_ok().is_none() {
            return Err(err(MF_E_SHUTDOWN));
        }
        if *self.state.lock_ok() != MF_STREAM_STATE_RUNNING {
            return Err(err(MF_E_MEDIA_SOURCE_WRONGSTATE));
        }
        let len = shm::nv12_len(shm::WIDTH, shm::HEIGHT);
        {
            // The shared memory may come later (a source made before idn-display ran).
            let mut s = self.shm.lock_ok();
            // (Tried once a second while it isn't there, not at every frame.)
            static TRIED: std::sync::Mutex<Option<std::time::Instant>> =
                std::sync::Mutex::new(None);
            if s.is_none() {
                let mut t = TRIED.lock_ok();
                if t.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(1)) {
                    *t = Some(std::time::Instant::now());
                    *s = Shm::open().ok();
                }
            }
            let mut frame = self.frame.lock_ok();
            let fresh = s.as_ref().is_some_and(|s| s.read(&mut frame));
            if !fresh {
                // Black made once, not per sample (else 3 MB a request while no writer runs).
                static BLACK: std::sync::LazyLock<Vec<u8>> =
                    std::sync::LazyLock::new(|| shm::black(shm::WIDTH, shm::HEIGHT));
                frame.copy_from_slice(&BLACK);
            }
        }
        // SAFETY: Media Foundation calls on a buffer of `len` bytes made here.
        unsafe {
            let buf = MFCreateMemoryBuffer(len as u32)?;
            let mut p = std::ptr::null_mut();
            buf.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(self.frame.lock_ok().as_ptr(), p, len);
            buf.Unlock()?;
            buf.SetCurrentLength(len as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buf)?;
            sample.SetSampleTime(MFGetSystemTime())?;
            sample.SetSampleDuration(10_000_000 / shm::FPS as i64)?;
            if let Some(t) = token.as_ref() {
                sample.SetUnknown(&MFSampleExtension_Token, t)?;
            }
            self.queue
                .QueueEventParamUnk(MEMediaSample.0 as u32, &GUID::zeroed(), S_OK, &sample)
        }
    }
}

impl IMFMediaStream2_Impl for Stream_Impl {
    fn SetStreamState(&self, value: MF_STREAM_STATE) -> Result<()> {
        *self.state.lock_ok() = value;
        Ok(())
    }
    fn GetStreamState(&self) -> Result<MF_STREAM_STATE> {
        Ok(*self.state.lock_ok())
    }
}

impl IKsControl_Impl for Stream_Impl {
    fn KsProperty(
        &self,
        _: *const KSIDENTIFIER,
        _: u32,
        _: *mut c_void,
        _: u32,
        _: *mut u32,
    ) -> Result<()> {
        no_ks()
    }
    fn KsMethod(
        &self,
        _: *const KSIDENTIFIER,
        _: u32,
        _: *mut c_void,
        _: u32,
        _: *mut u32,
    ) -> Result<()> {
        no_ks()
    }
    fn KsEvent(
        &self,
        _: *const KSIDENTIFIER,
        _: u32,
        _: *mut c_void,
        _: u32,
        _: *mut u32,
    ) -> Result<()> {
        no_ks()
    }
}

// --- The source -----------------------------------------------------------------------

#[implement(IMFMediaSourceEx, IMFGetService, IKsControl)]
struct Source {
    queue: IMFMediaEventQueue,
    attributes: IMFAttributes,
    stream_attributes: IMFAttributes,
    pd: IMFPresentationDescriptor,
    sd: IMFStreamDescriptor,
    stream: Mutex<Option<IMFMediaStream2>>,
    shut: Mutex<bool>,
}

impl Source {
    fn new() -> Result<Source> {
        // SAFETY: Media Foundation calls on objects made here.
        unsafe {
            let mt = media_type()?;
            let sd = MFCreateStreamDescriptor(0, &[Some(mt.clone())])?;
            sd.GetMediaTypeHandler()?.SetCurrentMediaType(&mt)?;
            stream_attributes(&sd.cast()?)?;
            let pd = MFCreatePresentationDescriptor(Some(&[Some(sd.clone())]))?;
            pd.SelectStream(0)?;
            let sa = attributes(4)?;
            stream_attributes(&sa)?;
            Ok(Source {
                queue: MFCreateEventQueue()?,
                attributes: attributes(1)?,
                stream_attributes: sa,
                pd,
                sd,
                stream: Mutex::new(None),
                shut: Mutex::new(false),
            })
        }
    }
}

impl IMFMediaEventGenerator_Impl for Source_Impl {
    fn GetEvent(&self, flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> Result<IMFMediaEvent> {
        // SAFETY: Media Foundation call on our own queue.
        unsafe { self.queue.GetEvent(flags.0) }
    }
    fn BeginGetEvent(&self, cb: Ref<IMFAsyncCallback>, state: Ref<IUnknown>) -> Result<()> {
        // SAFETY: Media Foundation call; the caller's callback and state passed on as given.
        unsafe { self.queue.BeginGetEvent(cb.as_ref(), state.as_ref()) }
    }
    fn EndGetEvent(&self, result: Ref<IMFAsyncResult>) -> Result<IMFMediaEvent> {
        // SAFETY: Media Foundation call; the caller's result passed on as given.
        unsafe { self.queue.EndGetEvent(result.as_ref()) }
    }
    fn QueueEvent(
        &self,
        met: u32,
        ext: *const GUID,
        hr: HRESULT,
        value: *const PROPVARIANT,
    ) -> Result<()> {
        // SAFETY: Media Foundation call; COM's caller gives valid (or null) `ext` and `value`.
        unsafe { self.queue.QueueEventParamVar(met, ext, hr, value) }
    }
}

impl IMFMediaSource_Impl for Source_Impl {
    fn GetCharacteristics(&self) -> Result<u32> {
        if *self.shut.lock_ok() {
            return Err(err(MF_E_SHUTDOWN));
        }
        Ok(MFMEDIASOURCE_IS_LIVE.0 as u32)
    }

    fn CreatePresentationDescriptor(&self) -> Result<IMFPresentationDescriptor> {
        if *self.shut.lock_ok() {
            return Err(err(MF_E_SHUTDOWN));
        }
        // SAFETY: Media Foundation call on our own descriptor.
        unsafe { self.pd.Clone() }
    }

    fn Start(
        &self,
        _pd: Ref<IMFPresentationDescriptor>,
        _format: *const GUID,
        _start: *const PROPVARIANT,
    ) -> Result<()> {
        if *self.shut.lock_ok() {
            return Err(err(MF_E_SHUTDOWN));
        }
        let me: IMFMediaSource = self.to_interface::<IMFMediaSourceEx>().cast()?;
        let mut slot = self.stream.lock_ok();
        let (stream, event) = match slot.as_ref() {
            Some(s) => (s.clone(), MEUpdatedStream),
            None => {
                let s: IMFMediaStream2 = Stream::new(me, self.sd.clone())?.into();
                *slot = Some(s.clone());
                (s, MENewStream)
            }
        };
        drop(slot);
        // SAFETY: Media Foundation calls.
        unsafe {
            self.queue
                .QueueEventParamUnk(event.0 as u32, &GUID::zeroed(), S_OK, &stream)?;
            stream.SetStreamState(MF_STREAM_STATE_RUNNING)?;
            stream.QueueEvent(
                MEStreamStarted.0 as u32,
                &GUID::zeroed(),
                S_OK,
                &PROPVARIANT::default(),
            )?;
            self.queue.QueueEventParamVar(
                MESourceStarted.0 as u32,
                &GUID::zeroed(),
                S_OK,
                &PROPVARIANT::default(),
            )
        }
    }

    fn Stop(&self) -> Result<()> {
        if *self.shut.lock_ok() {
            return Err(err(MF_E_SHUTDOWN));
        }
        if let Some(s) = self.stream.lock_ok().as_ref() {
            // SAFETY: Media Foundation calls.
            unsafe {
                s.SetStreamState(MF_STREAM_STATE_STOPPED)?;
                s.QueueEvent(
                    MEStreamStopped.0 as u32,
                    &GUID::zeroed(),
                    S_OK,
                    &PROPVARIANT::default(),
                )?;
            }
        }
        // SAFETY: Media Foundation call.
        unsafe {
            self.queue.QueueEventParamVar(
                MESourceStopped.0 as u32,
                &GUID::zeroed(),
                S_OK,
                &PROPVARIANT::default(),
            )
        }
    }

    fn Pause(&self) -> Result<()> {
        Err(err(MF_E_INVALID_STATE_TRANSITION))
    }

    fn Shutdown(&self) -> Result<()> {
        *self.shut.lock_ok() = true;
        if let Some(s) = self.stream.lock_ok().take() {
            // Our own stream: its queue ends and it lets go of the source.
            if let Ok(obj) = s.cast_object_ref::<Stream>() {
                obj.shutdown();
            }
        }
        // SAFETY: Media Foundation call.
        unsafe { self.queue.Shutdown() }
    }
}

impl IMFMediaSourceEx_Impl for Source_Impl {
    fn GetSourceAttributes(&self) -> Result<IMFAttributes> {
        Ok(self.attributes.clone())
    }
    fn GetStreamAttributes(&self, id: u32) -> Result<IMFAttributes> {
        if id != 0 {
            return Err(err(MF_E_INVALIDSTREAMNUMBER));
        }
        Ok(self.stream_attributes.clone())
    }
    fn SetD3DManager(&self, _: Ref<IUnknown>) -> Result<()> {
        Ok(())
    }
}

impl IMFGetService_Impl for Source_Impl {
    fn GetService(&self, _: *const GUID, _: *const GUID, _: *mut *mut c_void) -> Result<()> {
        Err(err(MF_E_UNSUPPORTED_SERVICE))
    }
}

impl IKsControl_Impl for Source_Impl {
    fn KsProperty(
        &self,
        _: *const KSIDENTIFIER,
        _: u32,
        _: *mut c_void,
        _: u32,
        _: *mut u32,
    ) -> Result<()> {
        no_ks()
    }
    fn KsMethod(
        &self,
        _: *const KSIDENTIFIER,
        _: u32,
        _: *mut c_void,
        _: u32,
        _: *mut u32,
    ) -> Result<()> {
        no_ks()
    }
    fn KsEvent(
        &self,
        _: *const KSIDENTIFIER,
        _: u32,
        _: *mut c_void,
        _: u32,
        _: *mut u32,
    ) -> Result<()> {
        no_ks()
    }
}

// --- COM in-process server -------------------------------------------------------------

#[implement(IClassFactory)]
struct Factory;

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<IUnknown>,
        riid: *const GUID,
        out: *mut *mut c_void,
    ) -> Result<()> {
        if outer.is_some() {
            return Err(err(CLASS_E_NOAGGREGATION));
        }
        let source: IMFMediaSourceEx = Source::new()?.into();
        // SAFETY: COM's query on an interface made here.
        unsafe { source.query(riid, out).ok() }
    }
    fn LockServer(&self, _: BOOL) -> Result<()> {
        Ok(())
    }
}

/// COM: the class factory for IDN Camera's media source.
#[unsafe(no_mangle)]
extern "system" fn DllGetClassObject(
    clsid: *const GUID,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: COM passes valid pointers.
    if unsafe { *clsid } != CLSID {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    let f: IClassFactory = Factory.into();
    // SAFETY: COM passes a valid IID pointer and out slot (as to `CreateInstance` above).
    unsafe { f.query(riid, out) }
}

/// COM: kept loaded (the Frame Server holds it for the camera's life).
#[unsafe(no_mangle)]
extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}

/// This DLL's own path.
fn dll_path() -> Result<Vec<u16>> {
    let mut module = HMODULE::default();
    // SAFETY: an address inside this DLL names it.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(dll_path as *const u16),
            &mut module,
        )?;
        let mut buf = vec![0u16; 1024];
        let n = GetModuleFileNameW(Some(module), &mut buf) as usize;
        buf.truncate(n);
        buf.push(0);
        Ok(buf)
    }
}

fn set_value(key: HKEY, name: PCWSTR, value: &[u16]) -> Result<()> {
    // SAFETY: a wide string with its terminating zero.
    unsafe {
        RegSetValueExW(
            key,
            name,
            None,
            REG_SZ,
            Some(std::slice::from_raw_parts(
                value.as_ptr() as *const u8,
                value.len() * 2,
            )),
        )
        .ok()
    }
}

/// COM: registers the media source (the installer runs `regsvr32 idn_vcam.dll`, as an
/// administrator): `HKLM\SOFTWARE\Classes\CLSID\{…}\InprocServer32`.
#[unsafe(no_mangle)]
extern "system" fn DllRegisterServer() -> HRESULT {
    let r = (|| -> Result<()> {
        let path = dll_path()?;
        let base = wide(&format!("SOFTWARE\\Classes\\CLSID\\{CLSID_TEXT}"));
        let inproc = wide(&format!(
            "SOFTWARE\\Classes\\CLSID\\{CLSID_TEXT}\\InprocServer32"
        ));
        // SAFETY: registry calls with keys opened here and closed below.
        unsafe {
            let mut k = HKEY::default();
            RegCreateKeyExW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(base.as_ptr()),
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &mut k,
                None,
            )
            .ok()?;
            set_value(k, PCWSTR::null(), &wide("IDN Camera"))?;
            let _ = RegCloseKey(k);
            RegCreateKeyExW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(inproc.as_ptr()),
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &mut k,
                None,
            )
            .ok()?;
            set_value(k, PCWSTR::null(), &path)?;
            set_value(k, w!("ThreadingModel"), &wide("Both"))?;
            let _ = RegCloseKey(k);
        }
        Ok(())
    })();
    r.map_or_else(|e| e.code(), |()| S_OK)
}

/// COM: removes the registration.
#[unsafe(no_mangle)]
extern "system" fn DllUnregisterServer() -> HRESULT {
    let base = wide(&format!("SOFTWARE\\Classes\\CLSID\\{CLSID_TEXT}"));
    // SAFETY: a registry call.
    let r = unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, PCWSTR(base.as_ptr())) };
    if r == ERROR_SUCCESS || r == ERROR_FILE_NOT_FOUND {
        S_OK
    } else {
        HRESULT::from_win32(r.0)
    }
}

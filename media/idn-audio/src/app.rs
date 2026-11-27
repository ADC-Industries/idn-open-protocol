//! One program's sound (Windows 10 2004 and later): WASAPI process loopback, with no
//! virtual sound card. The program is found by its executable's name or its process id;
//! its child processes are included (browsers play from a child).

use crate::capture::{State, Work};
use idn_common::sync::LockOk as _;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, ActivateAudioInterfaceAsync,
    IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
    IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{BLOB, COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    CreateEventW, OpenProcess, PROCESS_SYNCHRONIZE, SetEvent, WaitForSingleObject,
};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{IUnknown, Interface, implement};

const RATE: u32 = 48_000;
const CHANNELS: usize = 2;

/// The process id of `name_or_pid`: a number, or an executable's name (with or without
/// `.exe`, any case). Of a program that runs as several processes of that name (Chrome,
/// Edge, Spotify, Discord), the top one of its tree: its sound is captured with every
/// process under it (`PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE`), and the first
/// one found was often a helper whose own tree held no sound, or that soon exited.
pub(crate) fn find_process(name_or_pid: &str) -> io::Result<u32> {
    if let Ok(pid) = name_or_pid.trim().parse::<u32>() {
        return Ok(pid);
    }
    let want = name_or_pid.trim().to_lowercase();
    let want_exe = if want.ends_with(".exe") {
        want.clone()
    } else {
        format!("{want}.exe")
    };
    // (pid, parent) of every process of that name.
    let mut found: Vec<(u32, u32)> = Vec::new();
    // SAFETY: the snapshot handle is ours and closed here; `e` has its dwSize set, as
    // Process32FirstW/NextW require.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).map_err(io::Error::other)?;
        let mut e = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let n = e
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(e.szExeFile.len());
                let exe = String::from_utf16_lossy(&e.szExeFile[..n]).to_lowercase();
                if exe == want_exe {
                    found.push((e.th32ProcessID, e.th32ParentProcessID));
                }
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    crate::top_of_tree(&found)
        .ok_or_else(|| io::Error::other(format!("no program {name_or_pid:?} is running")))
}

/// Signals an event when Windows has activated the loopback client.
#[implement(
    IActivateAudioInterfaceCompletionHandler,
    windows::Win32::System::Com::IAgileObject
)]
struct Activated(HANDLE);

// The handler owns its event: Windows may complete an activation after `open` stopped
// waiting (5 s), and the event must still be there then (closed, its number could already
// name another handle in the process).
impl Drop for Activated {
    fn drop(&mut self) {
        // SAFETY: the event this handler owns, closed once, when it goes.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

// A marker interface: the handler may be called from any thread.
impl windows::Win32::System::Com::IAgileObject_Impl for Activated_Impl {}

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(
        &self,
        _op: windows::core::Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        // SAFETY: the event this handler owns, alive as long as `self`.
        unsafe { SetEvent(self.0) }
    }
}

/// The audio client for process `pid`'s sound, started, with its event.
unsafe fn open(pid: u32) -> windows::core::Result<(IAudioClient, IAudioCaptureClient, HANDLE)> {
    // SAFETY: COM is initialised on the calling thread (`start`); `params` lives through
    // ActivateAudioInterfaceAsync, which takes its parameters during the call (Microsoft's loopback
    // sample frees them on return); `done` is the handler's, alive while it is.
    unsafe {
        let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        };
        // Never dropped: windows-rs's PROPVARIANT clears itself when dropped
        // (`PropVariantClear`), which frees a VT_BLOB's data with CoTaskMemFree, and this
        // blob is `params` on the stack: the process crashed, silently, as `open` returned
        // (the program's sound restarting for ever).
        let pv = std::mem::ManuallyDrop::new(PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                    vt: VT_BLOB,
                    Anonymous: PROPVARIANT_0_0_0 {
                        blob: BLOB {
                            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                            pBlobData: &mut params as *mut _ as *mut u8,
                        },
                    },
                    ..Default::default()
                }),
            },
        });
        let done = CreateEventW(None, false, false, None)?;
        let handler: IActivateAudioInterfaceCompletionHandler = Activated(done).into();
        let op = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*pv),
            &handler,
        )?;
        let waited = WaitForSingleObject(done, 5000);
        // (`done` is the handler's, closed when it goes.)
        if waited != WAIT_OBJECT_0 {
            return Err(windows::core::Error::from_hresult(
                windows::Win32::Foundation::E_FAIL,
            ));
        }
        let mut hr = windows::core::HRESULT(0);
        let mut unk: Option<IUnknown> = None;
        op.GetActivateResult(&mut hr, &mut unk)?;
        hr.ok()?;
        let client: IAudioClient = unk
            .ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))?
            .cast()?;
        // Process loopback offers no format of its own: ask for 48 kHz stereo float.
        let fmt = WAVEFORMATEX {
            wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
            nChannels: CHANNELS as u16,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * 4 * CHANNELS as u32,
            nBlockAlign: 4 * CHANNELS as u16,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK
                | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            200_000, // 20 ms, in 100 ns
            0,
            &fmt,
            None,
        )?;
        let ready = CreateEventW(None, false, false, None)?;
        client.SetEventHandle(ready)?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        Ok((client, capture, ready))
    }
}

/// Starts capturing process `pid` on a thread of its own, into `work`, until `stop`.
pub(crate) fn start(
    pid: u32,
    mut work: Work,
    stop: Arc<AtomicBool>,
    state: Arc<State>,
) -> io::Result<std::thread::JoinHandle<()>> {
    // Opened on the thread (COM there), reported back before returning.
    let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let stop_thread = stop.clone();
    let thread = std::thread::Builder::new()
        .name("idn-audio-app".into())
        // SAFETY: COM is initialised on this thread first; the client, capture and handles from
        // `open` are used here only and closed at the end; a buffer from GetBuffer holds `frames`
        // frames of the 32-bit stereo float asked for, until ReleaseBuffer.
        .spawn(move || unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let (client, capture, ready) = match open(pid) {
                Ok(x) => {
                    let _ = tx.send(Ok(()));
                    x
                }
                Err(e) => {
                    let _ = tx.send(Err(format!("program sound (process {pid}): {e}")));
                    CoUninitialize();
                    return;
                }
            };
            // The program itself, to notice when it exits (a restarted program is a new
            // process: whoever runs this starts again and finds it).
            let program = OpenProcess(PROCESS_SYNCHRONIZE, false, pid).ok();
            // Windows sends nothing while the program is quiet: the gaps are filled with
            // silence, so the stream keeps time (and the drift servo its footing). Only
            // while it is quiet: measured against this computer's clock while sound
            // comes, a device clock a little slow would have silence put into the sound
            // every quarter hour or so; the drift servo takes that difference.
            let mut last_sound = Instant::now();
            let mut padded: u64 = 0;
            let silence = vec![0f32; 480 * CHANNELS];
            while !stop_thread.load(Ordering::Relaxed) {
                let _ = WaitForSingleObject(ready, 20);
                if program.is_some_and(|p| WaitForSingleObject(p, 0) == WAIT_OBJECT_0) {
                    state.stats.lock_ok().ended =
                        Some(format!("the program (process {pid}) exited"));
                    break;
                }
                loop {
                    let Ok(n) = capture.GetNextPacketSize() else {
                        break;
                    };
                    if n == 0 {
                        break;
                    }
                    let mut data = std::ptr::null_mut();
                    let (mut frames, mut flags) = (0u32, 0u32);
                    if capture
                        .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                        .is_err()
                    {
                        state.stats.lock_ok().errors += 1;
                        break;
                    }
                    let samples = frames as usize * CHANNELS;
                    if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                        let z = vec![0f32; samples];
                        work.feed(&z);
                    } else {
                        work.feed(std::slice::from_raw_parts(data as *const f32, samples));
                    }
                    last_sound = Instant::now();
                    padded = 0;
                    let _ = capture.ReleaseBuffer(frames);
                }
                // Quiet for more than 30 ms: silence up to 10 ms behind the clock.
                let due = (last_sound.elapsed().as_secs_f64() * RATE as f64) as u64;
                while due > padded + RATE as u64 * 30 / 1000 {
                    work.feed(&silence);
                    padded += 480;
                    if due <= padded + RATE as u64 / 100 {
                        break;
                    }
                }
            }
            let _ = client.Stop();
            let _ = CloseHandle(ready);
            if let Some(p) = program {
                let _ = CloseHandle(p);
            }
            drop(capture);
            drop(client);
            CoUninitialize();
        })?;
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(())) => Ok(thread),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(io::Error::other(e))
        }
        Err(_) => {
            // Not joined (it may still be in Windows' hands), but it stops if it starts.
            stop.store(true, Ordering::Relaxed);
            Err(io::Error::other("program sound: Windows didn't answer"))
        }
    }
}

/// The programs with sound open now (to pick one from a list):
/// every playback device's audio sessions (WASAPI), by executable (`spotify`, without
/// `.exe`) and a name to show, the playing ones first; system sounds left out.
pub fn programs_with_sound() -> io::Result<Vec<(String, String, bool)>> {
    use windows::Win32::Media::Audio::{
        AudioSessionStateActive, DEVICE_STATE_ACTIVE, IAudioSessionControl2, IAudioSessionManager2,
        IMMDeviceEnumerator, MMDeviceEnumerator, eRender,
    };
    use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
    let mut by_pid: std::collections::BTreeMap<u32, bool> = std::collections::BTreeMap::new();
    // SAFETY: COM on this thread for the calls below, uninitialised at the end.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let r = (|| -> windows::core::Result<()> {
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let devices = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
            for i in 0..devices.GetCount()? {
                let dev = devices.Item(i)?;
                let Ok(mgr) = dev.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
                    continue;
                };
                let sessions = mgr.GetSessionEnumerator()?;
                for j in 0..sessions.GetCount()? {
                    let Ok(s) = sessions.GetSession(j) else {
                        continue;
                    };
                    let Ok(s2) = s.cast::<IAudioSessionControl2>() else {
                        continue;
                    };
                    if s2.IsSystemSoundsSession().0 == 0 {
                        continue; // S_OK: the system's sounds
                    }
                    let Ok(pid) = s2.GetProcessId() else { continue };
                    if pid == 0 {
                        continue;
                    }
                    let active = s.GetState().is_ok_and(|st| st == AudioSessionStateActive);
                    let e = by_pid.entry(pid).or_insert(false);
                    *e |= active;
                }
            }
            Ok(())
        })();
        CoUninitialize();
        r.map_err(io::Error::other)?;
    }
    // Their executables (a browser's sound comes from a child: the child's name).
    let mut names: std::collections::BTreeMap<u32, String> = std::collections::BTreeMap::new();
    // SAFETY: the snapshot handle is ours and closed here; `e` has its dwSize set, as
    // Process32FirstW/NextW require.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).map_err(io::Error::other)?;
        let mut e = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                if by_pid.contains_key(&e.th32ProcessID) {
                    let n = e
                        .szExeFile
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(e.szExeFile.len());
                    names.insert(e.th32ProcessID, String::from_utf16_lossy(&e.szExeFile[..n]));
                }
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    let mut out: Vec<(String, String, bool)> = Vec::new();
    for (pid, active) in by_pid {
        let Some(exe) = names.get(&pid) else { continue };
        let stem = exe
            .strip_suffix(".exe")
            .or_else(|| exe.strip_suffix(".EXE"))
            .unwrap_or(exe)
            .to_lowercase();
        if let Some(x) = out.iter_mut().find(|x| x.0 == stem) {
            x.2 |= active;
            continue;
        }
        out.push((stem.clone(), crate::display_name(&stem), active));
    }
    out.sort_by(|a, b| b.2.cmp(&a.2).then(a.1.cmp(&b.1)));
    Ok(out)
}

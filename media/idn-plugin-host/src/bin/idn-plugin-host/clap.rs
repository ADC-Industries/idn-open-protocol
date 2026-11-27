//! CLAP plugins through `clack-host`: the instance on the chain's main thread (its
//! parameters, state and own window, with the host's GUI, timer and POSIX fd support), its
//! started audio processor on the chain's audio thread. Parameter changes go to the audio
//! side through a short queue; values the plugin reports from processing (a knob turned in
//! its window) come back the same way.

use idn_common::sync::LockOk as _;
use crate::runloop::{Call, RunLoop};
use crate::window::HostWindow;
use crate::{Param, Plugin, Processor, Ui};
use clack_extensions::audio_ports::{AudioPortInfoBuffer, PluginAudioPorts};
use clack_extensions::gui::{
    GuiApiType, GuiConfiguration, GuiSize, HostGui, HostGuiImpl, PluginGui, Window,
};
use clack_extensions::latency::PluginLatency;
use clack_extensions::params::{ParamInfoBuffer, PluginParams};
use clack_extensions::state::PluginState;
use clack_extensions::timer::{HostTimer, HostTimerImpl, PluginTimer, TimerId};
use clack_host::events::event_types::ParamValueEvent;
use clack_host::prelude::*;
use std::ffi::CString;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Handlers;

/// What the plugin asks of the host from any thread.
#[derive(Default)]
struct Requests {
    resize: Mutex<Option<(u32, u32)>>,
    closed: AtomicBool,
    callback: AtomicBool,
}

struct Shared(Arc<Requests>);

impl<'a> SharedHandler<'a> for Shared {
    fn request_restart(&self) {}
    fn request_process(&self) {}
    fn request_callback(&self) {
        self.0.callback.store(true, Ordering::Relaxed);
    }
}

impl HostGuiImpl for Shared {
    fn resize_hints_changed(&self) {}
    fn request_resize(&self, size: GuiSize) -> Result<(), HostError> {
        *self.0.resize.lock_ok() = Some((size.width, size.height));
        Ok(())
    }
    fn request_show(&self) -> Result<(), HostError> {
        Ok(())
    }
    fn request_hide(&self) -> Result<(), HostError> {
        Ok(())
    }
    fn closed(&self, _was_destroyed: bool) {
        self.0.closed.store(true, Ordering::Relaxed);
    }
}

/// The host's main-thread side: the chain's run loop.
struct MainThread {
    runloop: Rc<RunLoop>,
    owner: u64,
}

impl<'a> MainThreadHandler<'a> for MainThread {}

impl HostTimerImpl for MainThread {
    fn register_timer(&self, period_ms: u32) -> Result<TimerId, HostError> {
        Ok(TimerId(self.runloop.add_timer(
            self.owner,
            period_ms,
            Call::Owner(self.owner),
        )))
    }
    fn unregister_timer(&self, id: TimerId) -> Result<(), HostError> {
        self.runloop
            .remove_timer(id.0)
            .then_some(())
            .ok_or(HostError::Message("no such timer"))
    }
}

#[cfg(unix)]
mod fd {
    use super::*;
    use clack_extensions::posix_fd::{FdFlags, HostPosixFdImpl};
    use std::os::unix::io::RawFd;

    impl HostPosixFdImpl for MainThread {
        fn register_fd(&self, fd: RawFd, flags: FdFlags) -> Result<(), HostError> {
            self.runloop
                .add_fd(self.owner, fd, flags.bits(), Call::Owner(self.owner));
            Ok(())
        }
        fn modify_fd(&self, fd: RawFd, flags: FdFlags) -> Result<(), HostError> {
            self.runloop
                .modify_fd(self.owner, fd, flags.bits())
                .then_some(())
                .ok_or(HostError::Message("no such fd"))
        }
        fn unregister_fd(&self, fd: RawFd) -> Result<(), HostError> {
            self.runloop
                .remove_fd(self.owner, fd)
                .then_some(())
                .ok_or(HostError::Message("no such fd"))
        }
    }
}

impl HostHandlers for Handlers {
    type Shared<'a> = Shared;
    type MainThread<'a> = MainThread;
    type AudioProcessor<'a> = ();

    fn declare_extensions(builder: &mut HostExtensions<Self>, _shared: &Self::Shared<'_>) {
        builder.register::<HostGui>().register::<HostTimer>();
        #[cfg(unix)]
        builder.register::<clack_extensions::posix_fd::HostPosixFd>();
    }
}

fn host_info() -> HostInfo {
    HostInfo::new(
        "IDN Mixer",
        "ADC Industries",
        "https://github.com/ADC-Industries",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("no NUL bytes")
}

/// The plugins in a `.clap` file: (id, name, vendor).
pub fn describe(path: &str) -> Result<Vec<(String, String, String)>, String> {
    // SAFETY: loading a plugin library runs its code; that is what this process is for.
    let entry = unsafe { PluginEntry::load(path) }.map_err(|e| format!("{e:?}"))?;
    let factory = entry.get_plugin_factory().ok_or("no plugin factory")?;
    let s = |c: Option<&std::ffi::CStr>| {
        c.map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    Ok(factory
        .plugin_descriptors()
        .map(|d| (s(d.id()), s(d.name()), s(d.vendor())))
        .collect())
}

type Queue = Arc<Mutex<Vec<(u32, f64)>>>;

pub struct Clap {
    // The instance before the entry: dropped first.
    instance: PluginInstance<Handlers>,
    /// The audio side until the chain takes it.
    audio: Option<ClapProcessor>,
    /// The audio side, stopped, back for deactivation.
    stopped: Receiver<StoppedPluginAudioProcessor<Handlers>>,
    _entry: PluginEntry,
    name: String,
    to_audio: Queue,
    edits: Queue,
    requests: Arc<Requests>,
    gui: bool,
}

/// The audio side, on the chain's audio thread.
pub struct ClapProcessor {
    processor: Option<StartedPluginAudioProcessor<Handlers>>,
    back: Sender<StoppedPluginAudioProcessor<Handlers>>,
    /// The plugin's main input and output port channel counts, and the chain's.
    outs: usize,
    chans: usize,
    in_ports: AudioPorts,
    out_ports: AudioPorts,
    inbuf: Vec<Vec<f32>>,
    outbuf: Vec<Vec<f32>>,
    events: EventBuffer,
    out_events: EventBuffer,
    from_main: Queue,
    edits: Queue,
    pending: Vec<(u32, f64)>,
    steady: u64,
}

pub fn open(
    path: &str,
    id: &str,
    channels: usize,
    rate: f64,
    block: usize,
    ui: Ui,
) -> Result<Box<dyn Plugin>, String> {
    // SAFETY: as in `describe`.
    let entry = unsafe { PluginEntry::load(path) }.map_err(|e| format!("{e:?}"))?;
    let factory = entry.get_plugin_factory().ok_or("no plugin factory")?;
    let desc = factory
        .plugin_descriptors()
        .find(|d| d.id().is_some_and(|i| i.to_bytes() == id.as_bytes()))
        .ok_or_else(|| format!("no plugin {id:?} in it"))?;
    let name = desc
        .name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| id.to_owned());
    let cid = CString::new(id).map_err(|e| e.to_string())?;
    let requests = Arc::new(Requests::default());
    let (rq, rl, owner) = (requests.clone(), ui.runloop.clone(), ui.owner);
    let mut instance = PluginInstance::<Handlers>::new(
        |_| Shared(rq),
        move |_| MainThread { runloop: rl, owner },
        &entry,
        &cid,
        &host_info(),
    )
    .map_err(|e| format!("{e:?}"))?;
    // The main ports' sizes (a mono plugin on a stereo strip, or the reverse).
    let (ins, outs) = {
        let h = instance.plugin_handle();
        match h.get_extension::<PluginAudioPorts>() {
            Some(ap) => {
                let mut buf = AudioPortInfoBuffer::new();
                let mut main = |input: bool| {
                    (ap.count(&h, input) > 0)
                        .then(|| {
                            ap.get(&h, 0, input, &mut buf)
                                .map(|p| p.channel_count as usize)
                        })
                        .flatten()
                        .unwrap_or(channels)
                };
                (main(true), main(false))
            }
            None => (channels, channels),
        }
    };
    let (ins, outs) = (ins.max(1), outs.max(1));
    let stopped = instance
        .activate(
            |_, _| (),
            PluginAudioConfiguration {
                sample_rate: rate,
                min_frames_count: 1,
                max_frames_count: block as u32,
            },
        )
        .map_err(|e| format!("activate: {e:?}"))?;
    let processor = stopped
        .start_processing()
        .map_err(|_| "start processing failed".to_owned())?;
    let (back, stopped) = channel();
    let (to_audio, edits): (Queue, Queue) = (Arc::default(), Arc::default());
    let audio = ClapProcessor {
        processor: Some(processor),
        back,
        outs,
        chans: channels,
        in_ports: AudioPorts::with_capacity(ins, 1),
        out_ports: AudioPorts::with_capacity(outs, 1),
        inbuf: vec![vec![0.0; block]; ins],
        outbuf: vec![vec![0.0; block]; outs],
        events: EventBuffer::new(),
        out_events: EventBuffer::new(),
        from_main: to_audio.clone(),
        edits: edits.clone(),
        pending: Vec::new(),
        steady: 0,
    };
    Ok(Box::new(Clap {
        instance,
        audio: Some(audio),
        stopped,
        _entry: entry,
        name,
        to_audio,
        edits,
        requests,
        gui: false,
    }))
}

impl Drop for ClapProcessor {
    fn drop(&mut self) {
        // Stopped on the audio thread (its own call), deactivated on the main thread.
        if let Some(p) = self.processor.take() {
            let _ = self.back.send(p.stop_processing());
        }
    }
}

impl Drop for Clap {
    fn drop(&mut self) {
        self.close_editor();
        // An audio side never handed over stops here.
        drop(self.audio.take());
        if let Ok(p) = self.stopped.recv_timeout(Duration::from_secs(2)) {
            self.instance.deactivate(p);
        }
    }
}

impl Plugin for Clap {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn latency(&mut self) -> u32 {
        let h = self.instance.plugin_handle();
        h.get_extension::<PluginLatency>().map_or(0, |l| l.get(&h))
    }

    fn params(&mut self) -> Vec<Param> {
        let h = self.instance.plugin_handle();
        let Some(p) = h.get_extension::<PluginParams>() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut buf = ParamInfoBuffer::new();
        for i in 0..p.count(&h) {
            let Some(info) = p.get_info(&h, i, &mut buf) else {
                continue;
            };
            let (id, name, min, max, default) = (
                info.id,
                String::from_utf8_lossy(info.name).into_owned(),
                info.min_value,
                info.max_value,
                info.default_value,
            );
            let value = p.get_value(&h, id).unwrap_or(default);
            let mut tb = [0u8; 128];
            let text = p
                .value_to_text(&h, id, value, &mut tb)
                .map(|t| {
                    String::from_utf8_lossy(t.split(|&b| b == 0).next().unwrap_or(&[])).into_owned()
                })
                .unwrap_or_default();
            out.push(Param {
                id: id.get(),
                name,
                min,
                max,
                default,
                value,
                text,
            });
        }
        out
    }

    fn set_param(&mut self, id: u32, value: f64) {
        self.to_audio.lock_ok().push((id, value));
    }

    fn save(&mut self) -> Vec<u8> {
        let h = self.instance.plugin_handle();
        let mut out = Vec::new();
        if let Some(s) = h.get_extension::<PluginState>() {
            let _ = s.save(&h, &mut out);
        }
        out
    }

    fn load(&mut self, state: &[u8]) -> Result<(), String> {
        let h = self.instance.plugin_handle();
        let s = h
            .get_extension::<PluginState>()
            .ok_or("this plugin keeps no state")?;
        s.load(&h, &mut &state[..]).map_err(|e| format!("{e:?}"))
    }

    fn processor(&mut self) -> Box<dyn Processor> {
        Box::new(self.audio.take().expect("the audio side is taken once"))
    }

    fn edits(&mut self) -> Vec<(u32, f64)> {
        if self.requests.callback.swap(false, Ordering::Relaxed) {
            self.instance.call_on_main_thread_callback();
        }
        std::mem::take(&mut *self.edits.lock_ok())
    }

    fn has_editor(&mut self) -> bool {
        let api = if cfg!(windows) {
            GuiApiType::WIN32
        } else {
            GuiApiType::X11
        };
        let h = self.instance.plugin_handle();
        h.get_extension::<PluginGui>().is_some_and(|g| {
            g.is_api_supported(
                &h,
                GuiConfiguration {
                    api_type: api,
                    is_floating: false,
                },
            )
        })
    }

    fn open_editor(&mut self, win: &HostWindow) -> Result<(u32, u32), String> {
        let api = if cfg!(windows) {
            GuiApiType::WIN32
        } else {
            GuiApiType::X11
        };
        let h = self.instance.plugin_handle();
        let gui = h
            .get_extension::<PluginGui>()
            .ok_or("this plugin has no window of its own")?;
        let embedded = GuiConfiguration {
            api_type: api,
            is_floating: false,
        };
        if !gui.is_api_supported(&h, embedded) {
            return Err(format!(
                "this plugin can't show its window in a {} window",
                crate::window::PLATFORM
            ));
        }
        gui.create(&h, embedded)
            .map_err(|e| format!("its window: {e}"))?;
        let window = if cfg!(windows) {
            // SAFETY: our window lives until the interface is destroyed (close_editor).
            unsafe { Window::from_win32_hwnd(win.handle() as *mut std::ffi::c_void) }
        } else {
            Window::from_x11_handle(win.handle() as _)
        };
        // SAFETY: as above.
        if let Err(e) = unsafe { gui.set_parent(&h, window) } {
            gui.destroy(&h);
            return Err(format!("its window: {e}"));
        }
        let size = gui.get_size(&h).map_or((640, 480), |s| (s.width, s.height));
        let _ = gui.show(&h);
        self.gui = true;
        self.requests.closed.store(false, Ordering::Relaxed);
        Ok(size)
    }

    fn close_editor(&mut self) {
        if self.gui {
            let h = self.instance.plugin_handle();
            if let Some(gui) = h.get_extension::<PluginGui>() {
                let _ = gui.hide(&h);
                gui.destroy(&h);
            }
            self.gui = false;
        }
    }

    fn take_resize(&mut self) -> Option<(u32, u32)> {
        let r = self.requests.resize.lock_ok().take()?;
        // Accepted as asked: the plugin is told its new size.
        let h = self.instance.plugin_handle();
        if let Some(gui) = h.get_extension::<PluginGui>() {
            let _ = gui.set_size(
                &h,
                GuiSize {
                    width: r.0,
                    height: r.1,
                },
            );
        }
        Some(r)
    }

    fn editor_gone(&mut self) -> bool {
        self.requests.closed.swap(false, Ordering::Relaxed)
    }

    fn on_timer(&mut self, id: u32) {
        let h = self.instance.plugin_handle();
        if let Some(t) = h.get_extension::<PluginTimer>() {
            t.on_timer(&h, TimerId(id));
        }
    }

    #[cfg(unix)]
    fn on_fd(&mut self, fd: i32, flags: u32) {
        use clack_extensions::posix_fd::{FdFlags, PluginPosixFd};
        let h = self.instance.plugin_handle();
        if let Some(p) = h.get_extension::<PluginPosixFd>() {
            p.on_fd(&h, fd, FdFlags::from_bits_truncate(flags));
        }
    }
}

impl Processor for ClapProcessor {
    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        let Some(proc) = self.processor.as_mut() else {
            return;
        };
        let frames = frames.min(self.inbuf.first().map_or(0, Vec::len));
        // The chain's channels onto the plugin's (a mono chain feeds both of a stereo
        // plugin's inputs).
        for (j, b) in self.inbuf.iter_mut().enumerate() {
            b[..frames].copy_from_slice(&io[j % self.chans][..frames]);
        }
        // Changes from the main thread; never waits for it.
        if let Some(mut q) = self.from_main.try_lock_ok() {
            self.pending.append(&mut q);
        }
        self.events.clear();
        for (id, v) in self.pending.drain(..) {
            self.events.push(&ParamValueEvent::new(
                0,
                ClapId::new(id),
                Pckn::match_all(),
                v,
            ));
        }
        self.out_events.clear();
        let input = self.in_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                self.inbuf
                    .iter_mut()
                    .map(|b| InputChannel::variable(&mut b[..frames])),
            ),
        }]);
        let mut output = self.out_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(
                self.outbuf.iter_mut().map(|b| &mut b[..frames]),
            ),
        }]);
        let ok = proc
            .process(
                &input,
                &mut output,
                &InputEvents::from_buffer(&self.events),
                &mut OutputEvents::from_buffer(&mut self.out_events),
                Some(self.steady),
                None,
            )
            .is_ok();
        self.steady += frames as u64;
        // Values the plugin reports (its window's knobs), for the desk.
        let moved: Vec<(u32, f64)> = self
            .out_events
            .iter()
            .filter_map(|e| e.as_event::<ParamValueEvent>())
            .filter_map(|p| Some((p.param_id()?.get(), p.value())))
            .collect();
        if !moved.is_empty()
            && let Some(mut q) = self.edits.try_lock_ok()
        {
            q.extend(moved);
        }
        if !ok {
            return; // passed on unchanged
        }
        // The plugin's outputs onto the chain (a mono plugin's on every channel).
        for (c, o) in io.iter_mut().enumerate().take(self.chans) {
            o[..frames].copy_from_slice(&self.outbuf[c % self.outs][..frames]);
        }
    }
}

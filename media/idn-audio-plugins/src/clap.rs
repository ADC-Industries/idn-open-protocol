//! The plugins as CLAP (`clap_entry`): IDN Send and IDN Return, stereo effects with one
//! stepped parameter, "Number" (1–64), kept in the project's state.

use crate::engine::{self, Engine, Kind};
use clack_extensions::audio_ports::{
    AudioPortFlags, AudioPortInfo, AudioPortInfoWriter, AudioPortType, PluginAudioPorts,
    PluginAudioPortsImpl,
};
use clack_extensions::params::{
    ParamDisplayWriter, ParamInfo, ParamInfoFlags, ParamInfoWriter, PluginAudioProcessorParams,
    PluginMainThreadParams, PluginParams,
};
use clack_extensions::render::{PluginRender, PluginRenderImpl, RenderMode};
use clack_extensions::state::{PluginState, PluginStateImpl};
use clack_plugin::entry::prelude::*;
use clack_plugin::events::event_types::ParamValueEvent;
use clack_plugin::prelude::*;
use clack_plugin::stream::{InputStream, OutputStream};
use std::ffi::CStr;
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::sync::Arc;

/// The number's parameter id.
const NUMBER: u32 = 1;

pub const SEND_ID: &str = "com.adc-industries.idn.send";
pub const RETURN_ID: &str = "com.adc-industries.idn.return";

pub struct IdnPlugin;

pub struct Shared {
    engine: Arc<Engine>,
}

impl PluginShared<'_> for Shared {}

pub struct Main<'a> {
    shared: &'a Shared,
}

impl<'a> PluginMainThread<'a, Shared> for Main<'a> {}

impl Plugin for IdnPlugin {
    type AudioProcessor<'a> = Processor<'a>;
    type Shared<'a> = Shared;
    type MainThread<'a> = Main<'a>;

    fn declare_extensions(builder: &mut PluginExtensions<Self>, _shared: Option<&Shared>) {
        builder
            .register::<PluginParams>()
            .register::<PluginState>()
            .register::<PluginAudioPorts>()
            .register::<PluginRender>();
    }
}

pub struct Processor<'a> {
    shared: &'a Shared,
}

impl<'a> PluginAudioProcessor<'a, Shared, Main<'a>> for Processor<'a> {
    fn activate(
        _host: HostAudioProcessorHandle<'a>,
        _main: &Main<'a>,
        shared: &'a Shared,
        cfg: PluginAudioConfiguration,
    ) -> Result<Self, PluginError> {
        shared.engine.activate(
            cfg.sample_rate.round() as u32,
            cfg.max_frames_count as usize,
        );
        Ok(Processor { shared })
    }

    fn deactivate(self, _main: &Main<'a>) {
        self.shared.engine.deactivate();
    }

    fn process(
        &mut self,
        _process: Process,
        mut audio: Audio,
        events: Events,
    ) -> Result<ProcessStatus, PluginError> {
        for e in events.input {
            set_number(self.shared, e);
        }
        let frames = audio.frames_count() as usize;
        let Some(mut port) = audio.port_pair(0) else {
            return Ok(ProcessStatus::Continue);
        };
        let Some(mut pairs) = port.channels()?.into_f32() else {
            return Ok(ProcessStatus::Continue);
        };
        // Each channel as one buffer holding the track's sound, the engine working in place.
        let mut bufs: [Option<&mut [f32]>; engine::CHANNELS] = [None, None];
        for (c, pair) in pairs.iter_mut().enumerate().take(engine::CHANNELS) {
            bufs[c] = match pair {
                ChannelPair::InputOutput(i, o) => {
                    o[..frames].copy_from_slice(&i[..frames]);
                    Some(o)
                }
                ChannelPair::InPlace(b) => Some(b),
                ChannelPair::OutputOnly(o) => {
                    o[..frames].fill(0.0);
                    Some(o)
                }
                ChannelPair::InputOnly(_) => None,
            };
        }
        let [a, b] = bufs;
        match (a, b) {
            (Some(a), Some(b)) => self.shared.engine.process(&mut [a, b], frames),
            (Some(a), None) | (None, Some(a)) => self.shared.engine.process(&mut [a], frames),
            (None, None) => {}
        }
        Ok(ProcessStatus::Continue)
    }
}

/// A parameter event for the number, wherever it arrives.
fn set_number(shared: &Shared, e: &UnknownEvent) {
    if let Some(p) = e.as_event::<ParamValueEvent>()
        && p.param_id().map(|i| i.get()) == Some(NUMBER)
    {
        shared.engine.set_slot(p.value().round() as u32);
    }
}

impl PluginAudioProcessorParams for Processor<'_> {
    fn flush(&mut self, input: &InputEvents, _output: &mut OutputEvents) {
        for e in input {
            set_number(self.shared, e);
        }
    }
}

impl PluginMainThreadParams for Main<'_> {
    fn count(&self) -> u32 {
        1
    }

    fn get_info(&self, index: u32, info: &mut ParamInfoWriter) {
        if index == 0 {
            info.set(&ParamInfo {
                id: ClapId::new(NUMBER),
                flags: ParamInfoFlags::IS_STEPPED,
                cookie: Default::default(),
                name: b"Number",
                module: b"",
                min_value: *engine::SLOTS.start() as f64,
                max_value: *engine::SLOTS.end() as f64,
                default_value: 1.0,
            });
        }
    }

    fn get_value(&self, id: ClapId) -> Option<f64> {
        (id.get() == NUMBER).then(|| self.shared.engine.slot() as f64)
    }

    fn value_to_text(
        &self,
        _id: ClapId,
        value: f64,
        writer: &mut ParamDisplayWriter,
    ) -> std::fmt::Result {
        write!(writer, "{}", value.round() as u32)
    }

    fn text_to_value(&self, _id: ClapId, text: &CStr) -> Option<f64> {
        text.to_str()
            .ok()?
            .trim()
            .parse::<u32>()
            .ok()
            .map(f64::from)
    }

    fn flush(&self, input: &InputEvents, _output: &mut OutputEvents) {
        for e in input {
            set_number(self.shared, e);
        }
    }
}

impl PluginStateImpl for Main<'_> {
    fn save(&self, output: &mut OutputStream) -> Result<(), PluginError> {
        output.write_all(&engine::save_state(self.shared.engine.slot()))?;
        Ok(())
    }

    fn load(&self, input: &mut InputStream) -> Result<(), PluginError> {
        let mut b = [0u8; 4];
        input.read_exact(&mut b)?;
        if let Some(n) = engine::load_state(&b) {
            self.shared.engine.set_slot(n);
        }
        Ok(())
    }
}

impl PluginAudioPortsImpl for Main<'_> {
    fn count(&self, _is_input: bool) -> u32 {
        1
    }

    fn get(&self, index: u32, _is_input: bool, writer: &mut AudioPortInfoWriter) {
        if index == 0 {
            writer.set(&AudioPortInfo {
                id: ClapId::new(0),
                name: b"main",
                channel_count: engine::CHANNELS as u32,
                flags: AudioPortFlags::IS_MAIN,
                port_type: Some(AudioPortType::STEREO),
                in_place_pair: Some(ClapId::new(0)),
            });
        }
    }
}

impl PluginRenderImpl for Main<'_> {
    fn has_hard_realtime_requirement(&self) -> bool {
        false
    }

    fn set(&self, mode: RenderMode) -> Result<(), PluginError> {
        self.shared.engine.set_offline(mode == RenderMode::Offline);
        Ok(())
    }
}

/// Both plugins.
pub struct Factory {
    send: PluginDescriptor,
    ret: PluginDescriptor,
}

impl Factory {
    fn new() -> Factory {
        use clack_plugin::plugin::features::{AUDIO_EFFECT, STEREO, UTILITY};
        let d = |id: &str, name: &str, what: &str| {
            PluginDescriptor::new(id, name)
                .with_vendor("ADC Industries")
                .with_version(env!("CARGO_PKG_VERSION"))
                .with_description(what)
                .with_features([AUDIO_EFFECT, UTILITY, STEREO])
        };
        Factory {
            send: d(
                SEND_ID,
                "IDN Send",
                "Sends the track as the IDN source \"HOST (DAW Send N)\"",
            ),
            ret: d(
                RETURN_ID,
                "IDN Return",
                "Adds what the site routes to the receiver \"DAW Return N\"",
            ),
        }
    }
}

impl PluginFactoryImpl for Factory {
    fn plugin_count(&self) -> u32 {
        2
    }

    fn plugin_descriptor(&self, index: u32) -> Option<&PluginDescriptor> {
        match index {
            0 => Some(&self.send),
            1 => Some(&self.ret),
            _ => None,
        }
    }

    fn create_plugin<'a>(
        &'a self,
        host_info: HostInfo<'a>,
        plugin_id: &CStr,
    ) -> Option<PluginInstance<'a>> {
        let (kind, desc) = if Some(plugin_id) == self.send.id() {
            (Kind::Send, &self.send)
        } else if Some(plugin_id) == self.ret.id() {
            (Kind::Return, &self.ret)
        } else {
            return None;
        };
        let host = engine::host_label(host_info.name().and_then(|n| n.to_str().ok()));
        Some(PluginInstance::new::<IdnPlugin>(
            host_info,
            desc,
            move |_host| {
                Ok(Shared {
                    engine: Engine::new(kind, &host),
                })
            },
            |_host, shared| Ok(Main { shared }),
        ))
    }
}

pub struct IdnEntry {
    factory: PluginFactoryWrapper<Factory>,
}

impl Entry for IdnEntry {
    fn new(_bundle_path: Option<&CStr>) -> Result<Self, EntryLoadError> {
        // Never unloaded once used: its threads (QUIC, discovery) outlive the plug-ins.
        idn_common::module::pin_own_module();
        Ok(IdnEntry {
            factory: PluginFactoryWrapper::new(Factory::new()),
        })
    }

    fn declare_factories<'a>(&'a self, builder: &mut EntryFactories<'a>) {
        builder.register_factory(&self.factory);
    }
}

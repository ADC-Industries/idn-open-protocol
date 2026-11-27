//! A small CLAP plugin for the plugin host's tests: a stereo gain with one parameter
//! ("Gain", 0–2, linear, default 0.5) and its state (the gain). Built as a library
//! (`cargo build -p idn-plugin-host --examples`), loaded like any CLAP plugin. For the
//! host's own tests it misbehaves on cue: a block starting with the sample 1234 makes it
//! crash (abort), one starting with 4321 makes it hang.

use clack_extensions::audio_ports::{
    AudioPortFlags, AudioPortInfo, AudioPortInfoWriter, AudioPortType, PluginAudioPorts,
    PluginAudioPortsImpl,
};
use clack_extensions::params::{
    ParamDisplayWriter, ParamInfo, ParamInfoFlags, ParamInfoWriter, PluginAudioProcessorParams,
    PluginMainThreadParams, PluginParams,
};
use clack_extensions::state::{PluginState, PluginStateImpl};
use clack_plugin::events::event_types::ParamValueEvent;
use clack_plugin::prelude::*;
use clack_plugin::stream::{InputStream, OutputStream};
use std::ffi::CStr;
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU32, Ordering};

const GAIN: u32 = 1;

pub struct Gain;

pub struct Shared {
    gain: AtomicU32,
}

impl Shared {
    fn get(&self) -> f64 {
        f32::from_bits(self.gain.load(Ordering::Relaxed)) as f64
    }
    fn set(&self, v: f64) {
        self.gain
            .store((v.clamp(0.0, 2.0) as f32).to_bits(), Ordering::Relaxed)
    }
}

impl PluginShared<'_> for Shared {}

pub struct Main<'a> {
    shared: &'a Shared,
}

impl<'a> PluginMainThread<'a, Shared> for Main<'a> {}

impl Plugin for Gain {
    type AudioProcessor<'a> = Processor<'a>;
    type Shared<'a> = Shared;
    type MainThread<'a> = Main<'a>;

    fn declare_extensions(builder: &mut PluginExtensions<Self>, _shared: Option<&Shared>) {
        builder
            .register::<PluginParams>()
            .register::<PluginState>()
            .register::<PluginAudioPorts>();
    }
}

impl DefaultPluginFactory for Gain {
    fn get_descriptor() -> PluginDescriptor {
        PluginDescriptor::new("com.adc-industries.idn.test-gain", "IDN Test Gain")
            .with_vendor("ADC Industries")
    }

    fn new_shared(_host: HostSharedHandle<'_>) -> Result<Shared, PluginError> {
        // As some plugins do: the host keeps this off its pipe to the desk.
        println!("IDN Test Gain loaded");
        Ok(Shared {
            gain: AtomicU32::new(0.5f32.to_bits()),
        })
    }

    fn new_main_thread<'a>(
        _host: HostMainThreadHandle<'a>,
        shared: &'a Shared,
    ) -> Result<Main<'a>, PluginError> {
        Ok(Main { shared })
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
        _cfg: PluginAudioConfiguration,
    ) -> Result<Self, PluginError> {
        Ok(Processor { shared })
    }

    fn process(
        &mut self,
        _process: Process,
        mut audio: Audio,
        events: Events,
    ) -> Result<ProcessStatus, PluginError> {
        for e in events.input {
            if let Some(p) = e.as_event::<ParamValueEvent>()
                && p.param_id().map(|i| i.get()) == Some(GAIN)
            {
                self.shared.set(p.value());
            }
        }
        let g = self.shared.get() as f32;
        let mut checked = false;
        for mut port in &mut audio {
            let Some(pairs) = port.channels()?.into_f32() else {
                continue;
            };
            for pair in pairs {
                // The host's crash and hang tests: cued by the block's first sample.
                if !std::mem::replace(&mut checked, true) {
                    cue(match &pair {
                        ChannelPair::InputOutput(i, _) => i.first().copied(),
                        ChannelPair::InPlace(b) => b.first().copied(),
                        _ => None,
                    });
                }
                match pair {
                    ChannelPair::InputOutput(i, o) => i.iter().zip(o).for_each(|(i, o)| *o = i * g),
                    ChannelPair::InPlace(b) => b.iter_mut().for_each(|s| *s *= g),
                    ChannelPair::OutputOnly(o) => o.fill(0.0),
                    ChannelPair::InputOnly(_) => {}
                }
            }
        }
        Ok(ProcessStatus::Continue)
    }
}

impl PluginAudioProcessorParams for Processor<'_> {
    fn flush(&mut self, input: &InputEvents, _output: &mut OutputEvents) {
        for e in input {
            if let Some(p) = e.as_event::<ParamValueEvent>() {
                self.shared.set(p.value());
            }
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
                id: ClapId::new(GAIN),
                flags: ParamInfoFlags::IS_AUTOMATABLE,
                cookie: Default::default(),
                name: b"Gain",
                module: b"",
                min_value: 0.0,
                max_value: 2.0,
                default_value: 0.5,
            });
        }
    }

    fn get_value(&self, id: ClapId) -> Option<f64> {
        (id.get() == GAIN).then(|| self.shared.get())
    }

    fn value_to_text(
        &self,
        _id: ClapId,
        value: f64,
        writer: &mut ParamDisplayWriter,
    ) -> std::fmt::Result {
        write!(writer, "{value:.2}x")
    }

    fn text_to_value(&self, _id: ClapId, text: &CStr) -> Option<f64> {
        text.to_str().ok()?.trim_end_matches('x').parse().ok()
    }

    fn flush(&self, input: &InputEvents, _output: &mut OutputEvents) {
        for e in input {
            if let Some(p) = e.as_event::<ParamValueEvent>() {
                self.shared.set(p.value());
            }
        }
    }
}

impl PluginStateImpl for Main<'_> {
    fn save(&self, output: &mut OutputStream) -> Result<(), PluginError> {
        output.write_all(&(self.shared.get() as f32).to_le_bytes())?;
        Ok(())
    }

    fn load(&self, input: &mut InputStream) -> Result<(), PluginError> {
        let mut b = [0u8; 4];
        input.read_exact(&mut b)?;
        self.shared.set(f32::from_le_bytes(b) as f64);
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
                channel_count: 2,
                flags: AudioPortFlags::IS_MAIN,
                port_type: Some(AudioPortType::STEREO),
                in_place_pair: None,
            });
        }
    }
}

clack_export_entry!(SinglePluginEntry<Gain>);

/// A block starting with 1234 crashes the plugin (and its host), one with 4321 hangs it.
fn cue(first: Option<f32>) {
    match first {
        Some(1234.0) => std::process::abort(),
        Some(4321.0) => loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        },
        _ => {}
    }
}

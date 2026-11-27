//! Playing a mix through the system's sound card, and the microphone for talkback, with
//! cpal (feature `play`): Linux (ALSA), Windows (WASAPI), macOS (Core Audio). A phone app
//! uses its own low-latency output (AAudio/Oboe, AVAudioEngine) and
//! [`Receiver::render`](crate::receiver::Receiver::render) instead.

use crate::receiver::Receiver;
use crate::receiver::talk::Resampler;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// The mix playing on a sound card until dropped.
pub struct Player {
    _stream: cpal::Stream,
    /// The device's rate and channels.
    pub rate: u32,
    pub channels: u16,
    pub device: String,
}

fn device_name(d: &cpal::Device) -> String {
    d.description()
        .map(|d| d.name().to_owned())
        .unwrap_or_else(|_| "(unnamed)".into())
}

fn find(host: &cpal::Host, name: Option<&str>, input: bool) -> Result<cpal::Device, String> {
    match name {
        None if input => host.default_input_device().ok_or("no sound input".into()),
        None => host.default_output_device().ok_or("no sound output".into()),
        Some(n) => {
            let all = if input {
                host.input_devices()
            } else {
                host.output_devices()
            }
            .map_err(|e| e.to_string())?;
            all.into_iter()
                .find(|d| device_name(d).contains(n))
                .ok_or(format!("no sound device {n:?}"))
        }
    }
}

/// The mix at 48 kHz made the device's rate: two converters and what they gave.
struct Out {
    rx: Receiver,
    conv: Option<(Resampler, Resampler)>,
    l: Vec<f32>,
    r: Vec<f32>,
    pos: usize,
    block: Vec<f32>,
    mono_l: Vec<f32>,
    mono_r: Vec<f32>,
}

impl Out {
    /// The next `frames` frames as (left, right).
    fn fill(&mut self, frames: usize, mut put: impl FnMut(usize, f32, f32)) {
        for k in 0..frames {
            while self.pos >= self.l.len() {
                self.l.clear();
                self.r.clear();
                self.pos = 0;
                self.block.resize(2 * crate::FRAME_SAMPLES, 0.0);
                self.rx.render(&mut self.block);
                match &mut self.conv {
                    None => {
                        for [l, r] in self.block.as_chunks::<2>().0 {
                            self.l.push(*l);
                            self.r.push(*r);
                        }
                    }
                    Some((a, b)) => {
                        self.mono_l.clear();
                        self.mono_r.clear();
                        for [l, r] in self.block.as_chunks::<2>().0 {
                            self.mono_l.push(*l);
                            self.mono_r.push(*r);
                        }
                        a.process(&self.mono_l, &mut self.l);
                        b.process(&self.mono_r, &mut self.r);
                        let n = self.l.len().min(self.r.len());
                        self.l.truncate(n);
                        self.r.truncate(n);
                    }
                }
            }
            put(k, self.l[self.pos], self.r[self.pos]);
            self.pos += 1;
        }
    }
}

fn build_out<T: cpal::SizedSample + cpal::FromSample<f32> + Send + 'static>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut out: Out,
) -> Result<cpal::Stream, cpal::Error> {
    let ch = usize::from(config.channels.max(1));
    let rate = config.sample_rate;
    let rx = out.rx.clone();
    device.build_output_stream::<T, _, _>(
        config,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            let ts = info.timestamp();
            // (Until heard, and the block itself.)
            let d = ts.playback.duration_since(ts.callback);
            let buffered = data.len() / ch;
            rx.set_output_latency_ms(
                d.as_secs_f32() * 1000.0 + buffered as f32 * 1000.0 / rate as f32,
            );
            let frames = data.len() / ch;
            out.fill(frames, |k, l, r| {
                let f = &mut data[k * ch..(k + 1) * ch];
                if ch == 1 {
                    f[0] = T::from_sample(0.5 * (l + r));
                } else {
                    f[0] = T::from_sample(l);
                    f[1] = T::from_sample(r);
                    for x in &mut f[2..] {
                        *x = T::from_sample(0.0);
                    }
                }
            });
        },
        |e| eprintln!("idn-iem: sound output: {e}"),
        None,
    )
}

impl Player {
    /// Plays `rx`'s mix on `device` (a part of its name; `None`: the default output), at
    /// 48 kHz where the device takes it, else converted to its rate.
    pub fn start(rx: &Receiver, device: Option<&str>) -> Result<Player, String> {
        let host = cpal::default_host();
        let dev = find(&host, device, false)?;
        let def = dev.default_output_config().map_err(|e| e.to_string())?;
        let channels = def.channels().max(1);
        // About 5 ms a callback (the system may give more).
        let mut tries = vec![(48_000u32, cpal::BufferSize::Fixed(240))];
        tries.push((48_000, cpal::BufferSize::Default));
        if def.sample_rate() != 48_000 {
            tries.push((def.sample_rate(), cpal::BufferSize::Default));
        }
        let mut last = String::new();
        for (rate, buffer_size) in tries {
            let config = cpal::StreamConfig {
                channels,
                sample_rate: rate,
                buffer_size,
            };
            let out = Out {
                rx: rx.clone(),
                conv: (rate != crate::RATE).then(|| {
                    (
                        Resampler::new(crate::RATE, rate),
                        Resampler::new(crate::RATE, rate),
                    )
                }),
                l: Vec::new(),
                r: Vec::new(),
                pos: 0,
                block: Vec::new(),
                mono_l: Vec::new(),
                mono_r: Vec::new(),
            };
            let s = match def.sample_format() {
                cpal::SampleFormat::I16 => build_out::<i16>(&dev, config, out),
                cpal::SampleFormat::I32 => build_out::<i32>(&dev, config, out),
                cpal::SampleFormat::U16 => build_out::<u16>(&dev, config, out),
                _ => build_out::<f32>(&dev, config, out),
            };
            match s {
                Ok(s) => {
                    s.play().map_err(|e| e.to_string())?;
                    return Ok(Player {
                        _stream: s,
                        rate,
                        channels,
                        device: device_name(&dev),
                    });
                }
                Err(e) => last = e.to_string(),
            }
        }
        Err(format!("sound output {}: {last}", device_name(&dev)))
    }
}

/// The microphone feeding `rx`'s talkback (heard only while talking) until dropped.
pub struct Mic {
    _stream: cpal::Stream,
    pub device: String,
}

fn build_in<T: cpal::SizedSample + Send + 'static>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    rx: Receiver,
) -> Result<cpal::Stream, cpal::Error>
where
    f32: cpal::FromSample<T>,
{
    let (rate, ch) = (config.sample_rate, config.channels);
    let mut buf: Vec<f32> = Vec::new();
    device.build_input_stream::<T, _, _>(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            buf.clear();
            buf.extend(data.iter().map(|s| s.to_sample::<f32>()));
            let _ = rx.talk_feed(&buf, rate, ch);
        },
        |e| eprintln!("idn-iem: sound input: {e}"),
        None,
    )
}

impl Mic {
    /// `device` (a part of its name; `None`: the default input) into `rx`'s talkback.
    pub fn start(rx: &Receiver, device: Option<&str>) -> Result<Mic, String> {
        let host = cpal::default_host();
        let dev = find(&host, device, true)?;
        let def = dev.default_input_config().map_err(|e| e.to_string())?;
        let config = cpal::StreamConfig {
            channels: def.channels(),
            sample_rate: def.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        };
        let rx = rx.clone();
        let s = match def.sample_format() {
            cpal::SampleFormat::I16 => build_in::<i16>(&dev, config, rx),
            cpal::SampleFormat::I32 => build_in::<i32>(&dev, config, rx),
            cpal::SampleFormat::U16 => build_in::<u16>(&dev, config, rx),
            _ => build_in::<f32>(&dev, config, rx),
        }
        .map_err(|e| e.to_string())?;
        s.play().map_err(|e| e.to_string())?;
        Ok(Mic {
            _stream: s,
            device: device_name(&dev),
        })
    }
}

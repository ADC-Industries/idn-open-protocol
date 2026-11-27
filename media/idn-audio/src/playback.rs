//! A source's sound on a sound card's output. The buffering, drift correction and
//! mixing of several sources are the library's ([`idn_open_protocol::audio`]); this opens
//! the device and feeds its callback from a mixer.

use cpal::traits::{DeviceTrait, StreamTrait};
use idn_open_protocol::audio::{LaneConfig, Mixer};
use std::io;
use std::sync::Arc;

pub use idn_open_protocol::audio::Lane;
/// Counters and the drift correction, from [`Playback::stats`] and [`Lane::stats`].
pub use idn_open_protocol::audio::LaneStats as PlaybackStats;

/// How to play.
#[derive(Debug, Clone)]
pub struct PlaybackConfig {
    /// The device's name or identifier ([`crate::devices`]), or the default playback
    /// device.
    pub device: Option<String>,
    /// Sound held before the device, in milliseconds: the network's timing jitter must
    /// fit in it. Default 60 (10 in the low-latency mode). The device is asked for a
    /// buffer of about a quarter of it.
    pub latency_ms: u32,
    /// The largest drift correction, in parts per million (0 switches it off).
    pub max_correction_ppm: f64,
    /// The device's buffer in frames, instead of a quarter of the latency (at least
    /// 5 ms); for the ultra-low mode (idn-routing.md §A.12) on a device that keeps up.
    pub device_buffer: Option<u32>,
}

impl Default for PlaybackConfig {
    fn default() -> Self {
        PlaybackConfig {
            device: None,
            latency_ms: 60,
            max_correction_ppm: 1000.0,
            device_buffer: None,
        }
    }
}

impl PlaybackConfig {
    fn lane(&self) -> LaneConfig {
        LaneConfig {
            latency_ms: self.latency_ms,
            max_correction_ppm: self.max_correction_ppm,
            ..Default::default()
        }
    }
}

/// What plays the mixer: a cpal stream, or Windows' low-latency shared mode.
enum Out {
    Cpal {
        _s: cpal::Stream,
    },
    #[cfg(windows)]
    Low {
        _l: crate::wasapi_low::LowLatency,
    },
}

/// A running playback; drop it to stop.
pub struct Playback {
    _stream: Out,
    mixer: Arc<Mixer>,
    lane_cfg: LaneConfig,
    /// The lane [`Playback::push`] plays on.
    main: Lane,
    /// Its errors' words and its progress, for [`Playback::status`].
    health: crate::health::SharedHealth,
}

/// Makes [`Lane`]s on a playback's device, from [`Playback::lanes`]. Lanes play while the
/// playback runs.
#[derive(Clone)]
pub struct LaneMaker {
    mixer: Arc<Mixer>,
    cfg: LaneConfig,
}

impl LaneMaker {
    /// A new lane (see [`Playback::lane`]).
    pub fn lane(&self) -> Lane {
        self.mixer.lane(&self.cfg)
    }

    /// A new lane that adapts to its path (roadmap R.30), starting at `start_ms` (the
    /// path's, [`idn_common::net::Path::start_ms`]), in line with the device's other
    /// adapting lanes.
    pub fn lane_adaptive(&self, start_ms: u32) -> Lane {
        self.mixer.lane(&LaneConfig {
            latency_ms: start_ms,
            adaptive: true,
            ..self.cfg.clone()
        })
    }

    /// The device's channel count.
    pub fn device_channels(&self) -> u16 {
        self.mixer.channels()
    }

    /// The mixer its lanes play on (for a receiver's sync with the site, §A.14).
    pub fn mixer(&self) -> Arc<Mixer> {
        self.mixer.clone()
    }

    /// The device's clock (frames played, when, its nominal rate): see
    /// [`idn_open_protocol::audio::Mixer::clock`].
    pub fn clock(&self) -> Option<(u64, i64, u32)> {
        self.mixer.clock()
    }
}

impl Playback {
    /// Opens the device and starts; sound starts once [`push`](Self::push) has filled the
    /// buffer to its target.
    pub fn start(cfg: &PlaybackConfig) -> io::Result<Playback> {
        let device = crate::find_device(cfg.device.as_deref(), false)?;
        let default = device.default_output_config().map_err(crate::cpal_err)?;
        let dev_rate = default.sample_rate();
        let dev_ch = default.channels().max(1);
        let mixer = Mixer::new(dev_rate, dev_ch, &crate::name_of(&device));
        let health = crate::health::SharedHealth::default();
        // A device buffer of about a quarter of the latency: ALSA otherwise opens some
        // devices with their largest (hundreds of ms), which the latency can't cover. At
        // least 5 ms: at 10 ms latency a 2.5 ms one (120 frames) underran about 30 times a
        // second on the lab host (ALSA loopback), 5 ms none.
        let want = cfg.device_buffer.map_or(
            (dev_rate as u64 * cfg.latency_ms.max(4) as u64 / 4000)
                .max(dev_rate as u64 / 200)
                .clamp(64, 1 << 16) as u32,
            |f| f.clamp(16, 1 << 16),
        );
        // Windows: under the shared engine's 10 ms, its low-latency mode (R.30 step 6),
        // when the device's mix format is what cpal reports (it is: cpal opens WASAPI in it).
        #[cfg(windows)]
        if crate::wasapi_low::wanted(want, dev_rate) {
            let low = crate::wasapi_low::mix_format(
                cfg.device
                    .as_deref()
                    .map(|_| crate::name_of(&device))
                    .as_deref(),
            )
            .and_then(|(r, c)| {
                if (r, c) != (dev_rate, dev_ch) {
                    return Err(io::Error::other(format!(
                        "mix format {r} Hz {c} ch, cpal {dev_rate} Hz {dev_ch} ch"
                    )));
                }
                crate::wasapi_low::start(
                    cfg.device
                        .as_deref()
                        .map(|_| crate::name_of(&device))
                        .as_deref(),
                    mixer.clone(),
                    want,
                )
            });
            match low {
                Ok(l) => {
                    let lane_cfg = cfg.lane();
                    let main = mixer.lane(&lane_cfg);
                    return Ok(Playback {
                        _stream: Out::Low { _l: l },
                        mixer,
                        lane_cfg,
                        main,
                        health,
                    });
                }
                Err(e) => idn_open_protocol::log::log(&format!(
                    "audio playback: {e}; using the shared engine"
                )),
            }
        }
        let buffer = crate::fixed_buffer(default.buffer_size(), want);
        let fmt = default.sample_format();
        let mut stream = None;
        for size in [
            buffer.map(cpal::BufferSize::Fixed),
            Some(cpal::BufferSize::Default),
        ]
        .into_iter()
        .flatten()
        {
            let config = cpal::StreamConfig {
                channels: dev_ch,
                sample_rate: dev_rate,
                buffer_size: size,
            };
            let err_mixer = mixer.clone();
            let err_health = health.clone();
            let frames = match size {
                cpal::BufferSize::Fixed(n) => Some(n),
                cpal::BufferSize::Default => None,
            };
            // The first error said, then at most one line every 10 s with the count: a
            // device that can't keep its buffer (snd-aloop at 128 frames: about 30 a
            // second) would otherwise fill the log.
            let mut said =
                idn_open_protocol::log::Throttle::new(std::time::Duration::from_secs(10));
            let on_err = move |e: cpal::Error| {
                err_mixer.note_error();
                err_health.error_of(&e);
                let hint = frames.map_or(String::new(), |n| {
                    format!(" (a {n}-frame device buffer: if these keep coming, a larger --device-buffer)")
                });
                said.log(&format!("audio playback: {e}{hint}"), "errors");
            };
            let r = match fmt {
                cpal::SampleFormat::I16 => build::<i16>(&device, config, &mixer, on_err),
                cpal::SampleFormat::I32 => build::<i32>(&device, config, &mixer, on_err),
                cpal::SampleFormat::U16 => build::<u16>(&device, config, &mixer, on_err),
                _ => build::<f32>(&device, config, &mixer, on_err),
            };
            match r {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) if matches!(size, cpal::BufferSize::Fixed(_)) => {
                    idn_open_protocol::log::log(&format!(
                        "audio playback: device buffer of {want} frames refused ({e}); using the default"
                    ));
                }
                Err(e) => return Err(crate::cpal_err(e)),
            }
        }
        let stream = stream.expect("built or returned");
        stream.play().map_err(crate::cpal_err)?;
        let lane_cfg = cfg.lane();
        let main = mixer.lane(&lane_cfg);
        Ok(Playback {
            _stream: Out::Cpal { _s: stream },
            mixer,
            lane_cfg,
            main,
            health,
        })
    }

    /// Queues a block of planar f32 sound (an `AudioIn`'s `channels` at its
    /// `header.sample_rate`). Channels map one to one onto the device's; a mono source
    /// plays on the first two. Never blocks for long.
    pub fn push(&self, sample_rate: u32, channels: &[Vec<f32>]) {
        self.main.push_all(sample_rate, channels);
    }

    /// [`push`](Self::push) with the block's site time (`idn_open_protocol::audio::
    /// site_time`): played by time while this machine's site clock is usable and the
    /// source is on the same grandmaster (spec/extensions/idn-clock.md §C.4).
    pub fn push_timed(&self, sample_rate: u32, channels: &[Vec<f32>], site_ns: Option<i128>) {
        self.main.push_timed(sample_rate, channels, None, site_ns);
    }

    /// How long after the device asks for a block it is heard (its buffer and output
    /// latency, from its callback timestamps; zero until the first). A lane's
    /// [`latency`](idn_open_protocol::audio::mixer::Lane::latency) and
    /// [`heard_at`](idn_open_protocol::audio::mixer::Lane::heard_at) count it in: when a
    /// sample pushed now is heard.
    pub fn output_delay(&self) -> std::time::Duration {
        self.mixer.output_delay()
    }

    /// The device's health: running, failing (recent errors) or lost (it stopped taking
    /// sound), with why.
    pub fn status(&self) -> crate::DeviceStatus {
        self.health.status(
            self.mixer.clock().map_or(0, |c| c.0),
            self.mixer.errors(),
            None,
        )
    }

    /// Calls `f` from a thread of its own with the status at once and at each change of
    /// state (polled every 250 ms), until this playback is dropped: to reopen a device
    /// that was lost, and show why.
    pub fn on_status(&self, f: impl Fn(&crate::DeviceStatus) + Send + 'static) {
        let (health, mixer) = (Arc::downgrade(&self.health), self.mixer.clone());
        crate::health::watch(
            move || {
                let h = health.upgrade()?;
                Some(h.status(mixer.clock().map_or(0, |c| c.0), mixer.errors(), None))
            },
            f,
        );
    }

    /// Counters and the drift correction (of what [`push`](Self::push) plays).
    pub fn stats(&self) -> PlaybackStats {
        self.main.stats()
    }

    /// A new lane for another source on this device (audio routing): its own buffer and
    /// servo at this playback's latency.
    pub fn lane(&self) -> Lane {
        self.lanes().lane()
    }

    /// Makes lanes on this device from any thread (the playback itself stays where it
    /// was started: some audio systems tie the device stream to its thread).
    pub fn lanes(&self) -> LaneMaker {
        LaneMaker {
            mixer: self.mixer.clone(),
            cfg: self.lane_cfg.clone(),
        }
    }

    /// The device's channel count.
    pub fn device_channels(&self) -> u16 {
        self.mixer.channels()
    }
}

/// The output stream, its callback pulling from the mixer in the device's sample format.
fn build<T: cpal::SizedSample + cpal::FromSample<f32>>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mixer: &Arc<Mixer>,
    on_err: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream, cpal::Error> {
    let mixer = mixer.clone();
    // The callback's own mixing buffer, so it doesn't allocate after the first block.
    let mut mix: Vec<f32> = Vec::new();
    device.build_output_stream::<T, _, _>(
        config,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            // How long after this callback its block is heard: lanes playing by time
            // count it in (spec/extensions/idn-clock.md §C.4).
            let ts = info.timestamp();
            mixer.set_output_delay(ts.playback.duration_since(ts.callback));
            mix.resize(data.len(), 0.0);
            mixer.fill(&mut mix);
            for (d, s) in data.iter_mut().zip(&mix) {
                *d = T::from_sample(*s);
            }
        },
        on_err,
        None,
    )
}

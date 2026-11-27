//! A sound card's input, as planar f32 blocks for an IDN output.

use cpal::Sample;
use cpal::traits::{DeviceTrait, StreamTrait};
use idn_common::sync::LockOk as _;
use idn_open_protocol::api::AudioBlock;
use idn_open_protocol::audio::resample::{Rate, deinterleave};
use idn_open_protocol::audio::servo::Servo;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// How to capture.
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// The device's name ([`crate::devices`]), or the default capture device.
    pub device: Option<String>,
    /// Channels to send (the device's first ones; more than it has are silent). Default:
    /// the device's.
    pub channels: Option<u16>,
    /// The rate to send at; the device's rate is converted when it differs. Default 48000.
    pub sample_rate: u32,
    /// Block length to send, in milliseconds (10 → 480 samples at 48 kHz). 0 (the
    /// default): the device's period, each as it comes: packets as small as the
    /// card's own (a receiver's buffer can't be smaller than one packet: 10 ms blocks were
    /// 31 ms from card to card on one machine, 128-frame ones 9).
    pub block_ms: u32,
    /// The largest drift correction, in parts per million (0 switches it off). Sound
    /// cards are usually within ±100.
    pub max_correction_ppm: f64,
    /// Capture what a playback device plays (`device` names a playback device; `None`:
    /// the system's): this computer's own sound, with no virtual sound card (Windows:
    /// WASAPI loopback; Linux: the default output's monitor through the sound server,
    /// `device` must be `None`).
    pub loopback: bool,
    /// Capture one program's sound only: its executable's name (`spotify.exe`, `Spotify`)
    /// or its process id, with its child processes (Windows 10 2004 and later: process
    /// loopback). `device` is ignored; the sound comes at 48 kHz stereo.
    pub app: Option<String>,
    /// The device's period in frames (32, 64, 128, 256, 512, 1024 …), as audio programs
    /// offer it. Default: 128 with `block_ms` 0; else the power of two nearest the block
    /// (960 → 1024, 480 → 512), which lines up with sound servers' cycles (PipeWire's is
    /// 1024), the blocks sent staying `block_ms` whatever the period.
    pub device_buffer: Option<u32>,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        CaptureConfig {
            device: None,
            channels: None,
            sample_rate: 48000,
            // The device's period (10 ms before, 20 before R.30 step 3):
            // a block's length is latency at every receiver, two or three times over.
            block_ms: 0,
            max_correction_ppm: 1000.0,
            loopback: false,
            app: None,
            device_buffer: None,
        }
    }
}

/// Counters and the drift correction, from [`Capture::stats`].
#[derive(Debug, Clone, Default)]
pub struct CaptureStats {
    /// The device in use.
    pub device: String,
    /// The device's rate and channels.
    pub device_rate: u32,
    pub device_channels: u16,
    /// What is sent: rate and channels.
    pub sample_rate: u32,
    pub channels: u16,
    /// Frames read from the device.
    pub frames_in: u64,
    /// Blocks handed to the sink.
    pub blocks_out: u64,
    /// The resampling correction in use, in ppm: how much faster than its label the
    /// device's clock runs, as measured against this machine's.
    pub correction_ppm: f64,
    /// How far the sent samples lead the clock, in ms (smoothed; near zero when settled).
    pub lead_ms: f64,
    /// Device errors reported by the audio system.
    pub errors: u64,
    /// Times the lead jumped by more than [`RESYNC_MS`] (input lost or held back, not
    /// drift) and the servo started again from there.
    pub resyncs: u64,
    /// The capture has ended for good (one program's sound: the program exited).
    pub ended: Option<String>,
}

/// A lead change this large is lost or delayed input (an overrun, a stalled device), not
/// clock drift, which moves it by well under a millisecond a second: steering it away at
/// the correction's limit would only run the lead off further.
pub const RESYNC_MS: f64 = 30.0;

pub(crate) struct State {
    pub(crate) stats: Mutex<CaptureStats>,
    /// Its errors' words and its progress, for [`Capture::status`].
    pub(crate) health: crate::health::Health,
}

/// What delivers the samples: a sound card's stream, or a thread of ours (one program's
/// sound), stopped when dropped.
enum Feed {
    Stream {
        _s: cpal::Stream,
    },
    /// Windows' low-latency shared mode (R.30 step 6).
    #[cfg(windows)]
    Low {
        _l: crate::wasapi_low::LowLatency,
    },
    // (Made for one program's sound, which is Windows' only.)
    #[cfg_attr(not(windows), allow(dead_code))]
    Thread(
        Arc<std::sync::atomic::AtomicBool>,
        Option<std::thread::JoinHandle<()>>,
    ),
}

impl Drop for Feed {
    fn drop(&mut self) {
        if let Feed::Thread(stop, t) = self {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(t) = t.take() {
                let _ = t.join();
            }
        }
    }
}

/// A running capture; drop it to stop.
pub struct Capture {
    _feed: Feed,
    state: Arc<State>,
}

impl Capture {
    /// Opens the device and starts: `sink` gets each block with its timecode (100 ns
    /// since the Unix epoch, counted in samples from the start so it never jumps,
    /// whatever size the device delivers in; pass it to `Output::send_audio`). It runs on
    /// the audio thread and must not block — `Output::send_audio` doesn't.
    pub fn start(
        cfg: &CaptureConfig,
        sink: impl FnMut(AudioBlock, i64) + Send + 'static,
    ) -> io::Result<Capture> {
        if let Some(app) = &cfg.app {
            return Capture::start_app(cfg, app, Box::new(sink));
        }
        // Linux: what the desktop plays, through the sound server's ALSA device aimed at
        // the default output's monitor (no device named: the computer's own sound).
        #[cfg(target_os = "linux")]
        let aimed = if cfg.loopback {
            if cfg.device.is_some() {
                return Err(io::Error::other(
                    "on Linux, loopback captures the desktop's default output; for another device capture its monitor (idn audio devices lists it)",
                ));
            }
            Some(crate::virtual_dev::aim_at_desktop_sound()?)
        } else {
            None
        };
        // The aim is for this device alone: taken back once it is open (or failed), so
        // that streams this process opens later (a Wayland screen share's) aren't
        // aimed at the desktop's sound too.
        #[cfg(target_os = "linux")]
        let _unaim = aimed.map(|_| crate::virtual_dev::Unaim);
        #[cfg(not(target_os = "linux"))]
        let aimed: Option<&str> = None;
        if cfg.loopback && aimed.is_none() && !cfg!(windows) {
            return Err(io::Error::other("loopback capture: Windows and Linux only"));
        }
        // Windows' loopback: a playback device, opened for capture in its own format.
        let loopback = cfg.loopback && aimed.is_none();
        let device = match aimed {
            Some(name) => crate::find_device(Some(name), true)?,
            None => crate::find_device(cfg.device.as_deref(), !loopback)?,
        };
        let default = if loopback {
            device.default_output_config()
        } else {
            device.default_input_config()
        }
        .map_err(crate::cpal_err)?;
        let dev_rate = default.sample_rate();
        let send_ch = cfg
            .channels
            .unwrap_or(default.channels().max(1))
            .clamp(1, 256);
        // The device opens with the channels asked for when it offers them at its rate
        // (a multichannel interface, Dante Virtual Soundcard); otherwise with its default
        // count, and the stream pads with silence or takes the first channels.
        let offers = |n: u16| {
            device
                .supported_input_configs()
                .is_ok_and(|mut cs| cs.any(|c| c.channels() == n && c.contains_rate(dev_rate)))
        };
        let dev_ch = if !loopback && send_ch != default.channels() && offers(send_ch) {
            send_ch
        } else {
            default.channels().max(1)
        };
        let out_rate = if cfg.sample_rate == 0 {
            48000
        } else {
            cfg.sample_rate
        };
        // A device period of about a block (a power of two unless asked): ALSA otherwise
        // opens some devices with their largest (hundreds of ms), which arrives in bursts.
        // Blocks of the period itself with `block_ms` 0 (128 frames unless asked).
        let period =
            cfg.device_buffer
                .map(|f| f.clamp(16, 1 << 16))
                .unwrap_or(if cfg.block_ms == 0 {
                    128
                } else {
                    nearest_pow2(
                        (dev_rate as usize * cfg.block_ms.clamp(1, 1000) as usize / 1000) as u32,
                    )
                });
        let chunk = if cfg.block_ms == 0 {
            (period as usize).max(32)
        } else {
            (dev_rate as usize * cfg.block_ms.clamp(1, 1000) as usize / 1000).max(32)
        };
        let state = Arc::new(State {
            health: Default::default(),
            stats: Mutex::new(CaptureStats {
                device: crate::name_of(&device),
                device_rate: dev_rate,
                device_channels: dev_ch,
                sample_rate: out_rate,
                channels: send_ch,
                ..Default::default()
            }),
        });
        let work = Work::new(
            Rate::new(
                dev_rate,
                out_rate,
                send_ch as usize,
                chunk,
                0.002 + cfg.max_correction_ppm * 1e-6,
            )?,
            Servo::new(cfg.max_correction_ppm),
            dev_ch as usize,
            send_ch as usize,
            out_rate,
            Box::new(sink),
            state.clone(),
        );
        let buffer = crate::fixed_buffer(default.buffer_size(), period);
        let work = Arc::new(Mutex::new(work));
        // Windows: a card's input under the shared engine's 10 ms in its low-latency mode
        // (R.30 step 6), in the mix format cpal would open.
        #[cfg(windows)]
        if !loopback && crate::wasapi_low::wanted(period, dev_rate) {
            let name = cfg.device.as_deref().map(|_| crate::name_of(&device));
            let low = crate::wasapi_low::capture_format(name.as_deref()).and_then(|(r, c)| {
                if (r, c) != (dev_rate, dev_ch) {
                    return Err(io::Error::other(format!(
                        "mix format {r} Hz {c} ch, cpal {dev_rate} Hz {dev_ch} ch"
                    )));
                }
                let (w, st) = (work.clone(), state.clone());
                crate::wasapi_low::start_capture(
                    name.as_deref(),
                    period,
                    move |d: &[f32]| w.lock_ok().feed(d),
                    move || st.stats.lock_ok().errors += 1,
                )
            });
            match low {
                Ok(l) => {
                    return Ok(Capture {
                        _feed: Feed::Low { _l: l },
                        state,
                    });
                }
                Err(e) => idn_open_protocol::log::log(&format!(
                    "audio capture: {e}; using the shared engine"
                )),
            }
        }
        let fmt = default.sample_format();
        let mut stream = None;
        for size in [buffer, None] {
            let Some(size) = size
                .map(cpal::BufferSize::Fixed)
                .or_else(|| size.is_none().then_some(cpal::BufferSize::Default))
            else {
                continue;
            };
            let config = cpal::StreamConfig {
                channels: dev_ch,
                sample_rate: dev_rate,
                buffer_size: size,
            };
            let err_state = state.clone();
            let on_err = move |e: cpal::Error| {
                err_state.stats.lock_ok().errors += 1;
                err_state.health.error_of(&e);
                idn_open_protocol::log::log(&format!("audio capture: {e}"));
            };
            let w = work.clone();
            let r = match fmt {
                cpal::SampleFormat::I16 => device.build_input_stream::<i16, _, _>(
                    config,
                    move |d, _| w.lock_ok().feed(d),
                    on_err,
                    None,
                ),
                cpal::SampleFormat::I32 => device.build_input_stream::<i32, _, _>(
                    config,
                    move |d, _| w.lock_ok().feed(d),
                    on_err,
                    None,
                ),
                cpal::SampleFormat::U16 => device.build_input_stream::<u16, _, _>(
                    config,
                    move |d, _| w.lock_ok().feed(d),
                    on_err,
                    None,
                ),
                _ => device.build_input_stream::<f32, _, _>(
                    config,
                    move |d, _| w.lock_ok().feed(d),
                    on_err,
                    None,
                ),
            };
            match r {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) if matches!(size, cpal::BufferSize::Fixed(_)) => {
                    idn_open_protocol::log::log(&format!(
                        "audio capture: device buffer of {chunk} frames refused ({e}); using the default"
                    ));
                }
                Err(e) => return Err(crate::cpal_err(e)),
            }
        }
        let stream = stream.expect("built or returned");
        // (An attempt given up on that ended later isn't this stream's end.)
        state.health.clear_ended();
        stream.play().map_err(crate::cpal_err)?;
        Ok(Capture {
            _feed: Feed::Stream { _s: stream },
            state,
        })
    }

    /// One program's sound (Windows process loopback): 48 kHz stereo float from Windows,
    /// through the same rate conversion, drift correction and timecodes as a card's.
    fn start_app(
        cfg: &CaptureConfig,
        app: &str,
        sink: Box<dyn FnMut(AudioBlock, i64) + Send>,
    ) -> io::Result<Capture> {
        #[cfg(not(windows))]
        {
            let _ = (cfg, app, sink);
            Err(io::Error::other(
                "one program's sound is Windows' (process loopback)",
            ))
        }
        #[cfg(windows)]
        {
            let pid = crate::app::find_process(app)?;
            let (dev_rate, dev_ch) = (48_000u32, 2usize);
            let send_ch = cfg.channels.unwrap_or(2).clamp(1, 256) as usize;
            let out_rate = if cfg.sample_rate == 0 {
                48000
            } else {
                cfg.sample_rate
            };
            // (Process loopback hands over 10 ms at a time: its blocks with `block_ms` 0.)
            let ms = if cfg.block_ms == 0 {
                10
            } else {
                cfg.block_ms.clamp(1, 1000)
            };
            let chunk = (dev_rate as usize * ms as usize / 1000).max(32);
            let state = Arc::new(State {
                health: Default::default(),
                stats: Mutex::new(CaptureStats {
                    device: format!("{app} (process {pid})"),
                    device_rate: dev_rate,
                    device_channels: dev_ch as u16,
                    sample_rate: out_rate,
                    channels: send_ch as u16,
                    ..Default::default()
                }),
            });
            let work = Work::new(
                Rate::new(
                    dev_rate,
                    out_rate,
                    send_ch,
                    chunk,
                    0.002 + cfg.max_correction_ppm * 1e-6,
                )?,
                Servo::new(cfg.max_correction_ppm),
                dev_ch,
                send_ch,
                out_rate,
                sink,
                state.clone(),
            );
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let thread = crate::app::start(pid, work, stop.clone(), state.clone())?;
            Ok(Capture {
                _feed: Feed::Thread(stop, Some(thread)),
                state,
            })
        }
    }

    /// Counters and the drift correction.
    pub fn stats(&self) -> CaptureStats {
        self.state.stats.lock_ok().clone()
    }

    /// The device's health: running, failing (recent errors) or lost (no sound for a
    /// second, or ended), with why.
    pub fn status(&self) -> crate::DeviceStatus {
        status_of(&self.state)
    }

    /// Calls `f` from a thread of its own with the status at once and at each change of
    /// state (polled every 250 ms), until this capture is dropped.
    pub fn on_status(&self, f: impl Fn(&crate::DeviceStatus) + Send + 'static) {
        let state = Arc::downgrade(&self.state);
        crate::health::watch(move || state.upgrade().map(|s| status_of(&s)), f);
    }
}

fn status_of(st: &State) -> crate::DeviceStatus {
    let s = st.stats.lock_ok().clone();
    st.health.status(s.frames_in, s.errors, s.ended.as_deref())
}

/// The power of two nearest `frames` (halfway: the larger), 32 to 65536.
pub(crate) fn nearest_pow2(frames: u32) -> u32 {
    let f = frames.clamp(32, 1 << 16);
    let up = f.next_power_of_two();
    let down = up / 2;
    if f - down < up - f { down } else { up }
}

/// The audio thread's state.
pub(crate) struct Work {
    rate: Rate,
    servo: Servo,
    planar: Vec<Vec<f32>>,
    /// A device's integer samples as f32, reused (no allocation on the audio thread).
    conv: Vec<f32>,
    dev_ch: usize,
    out_rate: u32,
    /// Frames produced at the output rate.
    out_frames: u64,
    start: Option<Instant>,
    last: Option<Instant>,
    /// The lead's standing offset (buffering), averaged over the second and third
    /// seconds and subtracted from later readings; the servo runs from then.
    baseline: Option<f64>,
    baseline_acc: (f64, u32),
    /// The timecode of the first sample sent (100 ns since the epoch); later blocks
    /// count samples from it.
    base_tc: Option<i64>,
    sink: Box<dyn FnMut(AudioBlock, i64) + Send>,
    state: Arc<State>,
}

impl Work {
    fn new(
        rate: Rate,
        servo: Servo,
        dev_ch: usize,
        send_ch: usize,
        out_rate: u32,
        sink: Box<dyn FnMut(AudioBlock, i64) + Send>,
        state: Arc<State>,
    ) -> Self {
        Work {
            rate,
            servo,
            planar: vec![Vec::new(); send_ch],
            conv: Vec::new(),
            dev_ch,
            out_rate,
            out_frames: 0,
            start: None,
            last: None,
            baseline: None,
            baseline_acc: (0.0, 0),
            base_tc: None,
            sink,
            state,
        }
    }

    pub(crate) fn feed<T: cpal::SizedSample + 'static>(&mut self, data: &[T])
    where
        f32: cpal::FromSample<T>,
    {
        let now = Instant::now();
        let start = *self.start.get_or_insert(now);
        let base_tc = *self
            .base_tc
            .get_or_insert_with(|| (idn_common::epoch::now().as_nanos() / 100) as i64);
        let frames = data.len() / self.dev_ch.max(1);
        // Device samples → planar f32, the channels we send.
        for p in &mut self.planar {
            p.clear();
        }
        if std::any::TypeId::of::<T>() == std::any::TypeId::of::<f32>() {
            // SAFETY: T is f32 (checked just above); same size and alignment.
            let d = unsafe { std::slice::from_raw_parts(data.as_ptr() as *const f32, data.len()) };
            deinterleave(d, self.dev_ch, &mut self.planar);
        } else {
            self.conv.clear();
            self.conv.extend(data.iter().map(|s| f32::from_sample(*s)));
            deinterleave(&self.conv, self.dev_ch, &mut self.planar);
        }
        for p in &mut self.planar {
            p.resize(frames, 0.0); // channels the device lacks: silence
        }
        self.rate.push(&self.planar);
        let (out_rate, mut produced) = (self.out_rate, 0u64);
        let sink = &mut self.sink;
        let st = &self.state;
        let sent = self.out_frames;
        let r = self.rate.pull(|planar, n| {
            let tc = base_tc + ((sent + produced) as i128 * 10_000_000 / out_rate as i128) as i64;
            produced += n as u64;
            let block = AudioBlock {
                sample_rate: out_rate,
                channels: planar.iter().map(|c| c[..n].to_vec()).collect(),
            };
            st.stats.lock_ok().blocks_out += 1;
            sink(block, tc);
        });
        if r.is_err() {
            st.stats.lock_ok().errors += 1;
        }
        self.out_frames += produced;
        // The drift servo: produced samples against the clock, after a second of settling.
        let elapsed = now.duration_since(start).as_secs_f64();
        let lead_ms = (self.out_frames as f64 / out_rate as f64 - elapsed) * 1000.0;
        if (1.0..3.0).contains(&elapsed) && self.baseline.is_none() {
            self.baseline_acc.0 += lead_ms;
            self.baseline_acc.1 += 1;
        } else if elapsed >= 3.0 {
            let (sum, n) = self.baseline_acc;
            let mut base =
                *self
                    .baseline
                    .get_or_insert(if n > 0 { sum / n as f64 } else { lead_ms });
            if (lead_ms - base).abs() > RESYNC_MS {
                base = lead_ms;
                self.baseline = Some(base);
                self.servo.reset();
                st.stats.lock_ok().resyncs += 1;
            }
            let dt = self
                .last
                .map_or(0.0, |l| now.duration_since(l).as_secs_f64());
            let rel = self.servo.update(lead_ms - base, dt);
            self.rate.set_relative(rel);
        }
        self.last = Some(now);
        let mut s = st.stats.lock_ok();
        s.frames_in += frames as u64;
        s.correction_ppm = self.servo.ppm();
        s.lead_ms = self.servo.lead_ms();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn device_periods_are_powers_of_two() {
        for (block, want) in [
            (960, 1024),
            (480, 512),
            (96, 128),
            (882, 1024),
            (720, 512),
            (10, 32),
        ] {
            assert_eq!(nearest_pow2(block), want, "block {block}");
        }
    }

    /// `Work` fed straight (no device): the sink's blocks joined back together must be
    /// the input, sample for sample after the filter's delay, whatever size the device
    /// delivers in, and the timecodes must count samples exactly.
    fn run(calls: &[usize]) -> (Vec<f32>, Vec<(i64, usize)>, Vec<f32>) {
        let (tx, rx) = mpsc::channel();
        let state = Arc::new(State {
            health: Default::default(),
            stats: Mutex::new(CaptureStats::default()),
        });
        let mut w = Work::new(
            Rate::new(48000, 48000, 1, 960, 0.002).unwrap(),
            Servo::new(0.0), // no clock in a test: no correction
            2,
            1,
            48000,
            Box::new(move |b: AudioBlock, tc: i64| tx.send((tc, b.channels[0].clone())).unwrap()),
            state,
        );
        let mut input = Vec::new();
        let mut t = 0usize;
        for &n in calls {
            let data: Vec<f32> = (0..n)
                .flat_map(|i| {
                    let v = ((t + i) as f32 * 2.0 * std::f32::consts::PI * 997.0 / 48000.0).sin();
                    [v, -v]
                })
                .collect();
            t += n;
            input.extend((0..n).map(|i| data[2 * i]));
            w.feed(&data);
        }
        let mut out = Vec::new();
        let mut tcs = Vec::new();
        while let Ok((tc, b)) = rx.try_recv() {
            tcs.push((tc, b.len()));
            out.extend(b);
        }
        (input, tcs, out)
    }

    fn check(calls: &[usize]) {
        let (input, tcs, out) = run(calls);
        let total: usize = calls.iter().sum();
        assert!(out.len() >= total - 960, "{} of {total}", out.len());
        // The best lag is the filter delay; at it, no sample may be off (a lost frame
        // shows as a phase jump of a whole sample: error ~0.13 at 997 Hz).
        let err_at = |d: usize| {
            (d + 2000..out.len())
                .map(|i| (out[i] - input[i - d]).abs())
                .fold(0.0f32, f32::max)
        };
        let (d, err) = (0..400usize)
            .map(|d| (d, err_at(d)))
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap();
        assert!(err < 0.02, "{calls:?}: max error {err} at lag {d}");
        // Timecodes: each block starts where the previous ended, to the sample.
        let mut expect = tcs[0].0;
        for (tc, n) in &tcs {
            assert!((tc - expect).abs() <= 1, "timecode {tc} expected {expect}");
            expect = tc + (*n as i64 * 10_000_000 / 48000);
        }
    }

    #[test]
    fn blocks_of_any_device_size_are_continuous() {
        check(&[960; 60]);
        check(&[480; 120]);
        check(&[16384; 4]); // a device with a huge buffer, delivering in bursts
        check(&[1000, 24, 960, 3000, 1, 960, 960, 7000]);
    }
}

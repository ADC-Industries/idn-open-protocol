//! A sound card as the program's sound (spec/extensions/idn-switcher.md §S.6, stage 3):
//! two of its inputs (a mono card's one on both sides), drift-corrected by `idn-audio`,
//! into the program's sound like any other source. And the monitor's sound card (§S.24).

use idn_audio::{Capture, CaptureConfig, Playback, PlaybackConfig};
use idn_common::sync::LockOk as _;
use idn_open_protocol::audio::Lane;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A card's capture, on a thread of its own (some audio systems tie a device's stream to
/// the thread that opened it); stopped when dropped.
pub struct CardIn {
    pub device: Option<String>,
    pub first: u16,
    /// Where its sound goes (its latency follows the sound's delay).
    pub lane: Arc<Lane>,
    /// Its thread, stopped and waited for with it.
    _worker: idn_common::worker::Worker,
    status: Arc<Mutex<Value>>,
}

impl CardIn {
    /// Opens `device` (`None`: the default capture device), its inputs `first` and
    /// `first + 1` (from 0) as left and right (the last input alone, or a mono card's
    /// only one, on both), into `lane`.
    pub fn start(device: Option<String>, first: u16, lane: Lane) -> Result<CardIn, String> {
        let lane = Arc::new(lane);
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(json!({})));
        let (tx, rx) = channel::<Result<(), String>>();
        let thread = {
            let (dev, lane, stop, status) =
                (device.clone(), lane.clone(), stop.clone(), status.clone());
            std::thread::Builder::new()
                .name("idn-switcher-card".into())
                .spawn(move || {
                    let cfg = CaptureConfig { device: dev, channels: None, sample_rate: 48_000, block_ms: 10, ..Default::default() };
                    // Opened (and again when lost: the same lane).
                    let open = || {
                        let lane = lane.clone();
                        let mut map: Vec<(usize, u16)> = Vec::new();
                        Capture::start(&cfg, move |block, _timecode| {
                            if map.is_empty() {
                                // Inputs `first` and the next; the last one alone on both
                                // sides (it took the card's first instead).
                                let n = block.channels.len().max(1);
                                let l = (first as usize).min(n - 1);
                                let r = if l + 1 < n { l + 1 } else { l };
                                map = vec![(l, 0), (r, 1)];
                            }
                            lane.push(block.sample_rate, &block.channels, &map);
                        })
                    };
                    let r = open();
                    let mut cap = match r {
                        Ok(c) => {
                            let _ = tx.send(Ok(()));
                            Some(c)
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e.to_string()));
                            return;
                        }
                    };
                    let mut again = idn_audio::reopen::Reopen::default();
                    while !stop.load(Ordering::Relaxed) {
                        // Lost (its driver ended the stream, unplugged): opened again
                        // (idn_audio::reopen; bug run 2026-11-25: it stayed silent).
                        let (lost, moved) = match &cap {
                            Some(c) => (c.status().state == idn_audio::DeviceState::Lost, c.stats().frames_in > 0),
                            None => (true, true),
                        };
                        if again.due(lost, moved) {
                            cap = None;
                            let line = match open() {
                                Ok(c) => {
                                    cap = Some(c);
                                    "switcher: the sound card opened again".to_owned()
                                }
                                Err(e) => format!("switcher: the sound card: {e} (tried again later)"),
                            };
                            again.opened();
                            if again.say(&line) {
                                idn_open_protocol::log::log(&line);
                            }
                        }
                        *status.lock_ok() = match &cap {
                            Some(cap) => {
                                let s = cap.stats();
                                json!({"device": s.device, "channels": s.device_channels,
                                    "correction_ppm": s.correction_ppm, "errors": s.errors, "resyncs": s.resyncs})
                            }
                            None => json!({"error": "lost: opening it again"}),
                        };
                        std::thread::sleep(Duration::from_millis(500));
                    }
                })
                .map_err(|e| e.to_string())?
        };
        match rx.recv() {
            Ok(Ok(())) => Ok(CardIn {
                device,
                first,
                lane,
                _worker: idn_common::worker::Worker::from_parts(stop, thread),
                status,
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(format!("sound card: {e}"))
            }
            Err(_) => Err("sound card: its thread stopped".into()),
        }
    }

    /// The device, its channels, the drift correction.
    pub fn status(&self) -> Value {
        self.status.lock_ok().clone()
    }
}

/// The monitor on a sound card (§S.24), on a thread of its own like [`CardIn`]; stopped
/// when dropped.
pub struct CardOut {
    pub device: Option<String>,
    /// What the monitor plays into (its numbers are the card's): a new one when the card
    /// is opened again.
    lane: Arc<Mutex<Arc<Lane>>>,
    /// Its thread, stopped and waited for with it.
    _worker: idn_common::worker::Worker,
    status: Arc<Mutex<Value>>,
}

impl CardOut {
    /// Opens `device` (`None`: the default playback device), the monitor's left and
    /// right on its first two outputs, holding `latency_ms` against the frame clock's
    /// jitter.
    pub fn start(device: Option<String>, latency_ms: u32) -> Result<CardOut, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(json!({})));
        let (tx, rx) = channel::<Result<Arc<Mutex<Arc<Lane>>>, String>>();
        let thread = {
            let (dev, stop, status) = (device.clone(), stop.clone(), status.clone());
            std::thread::Builder::new()
                .name("idn-switcher-monitor".into())
                .spawn(move || {
                    let cfg = PlaybackConfig { device: dev, latency_ms, max_correction_ppm: 1000.0, device_buffer: None };
                    let p = match Playback::start(&cfg) {
                        Ok(p) => p,
                        Err(e) => {
                            let _ = tx.send(Err(e.to_string()));
                            return;
                        }
                    };
                    // A lane of its own: its numbers are the monitor's (the playback's
                    // own carries nothing).
                    let lane = Arc::new(Mutex::new(Arc::new(p.lane())));
                    let _ = tx.send(Ok(lane.clone()));
                    let mut p = Some(p);
                    let mut again = idn_audio::reopen::Reopen::default();
                    while !stop.load(Ordering::Relaxed) {
                        // Lost: opened again (idn_audio::reopen), its new lane taking the
                        // old one's place (bug run 2026-11-25).
                        let lost = p.as_ref().is_none_or(|p| p.status().state == idn_audio::DeviceState::Lost);
                        if again.due(lost, false) {
                            p = None;
                            let line = match Playback::start(&cfg) {
                                Ok(n) => {
                                    *lane.lock_ok() = Arc::new(n.lane());
                                    p = Some(n);
                                    "switcher: the monitor's sound card opened again".to_owned()
                                }
                                Err(e) => format!("switcher: the monitor's sound card: {e} (tried again later)"),
                            };
                            again.opened();
                            if again.say(&line) {
                                idn_open_protocol::log::log(&line);
                            }
                        }
                        *status.lock_ok() = match &p {
                            Some(p) => {
                                let s = lane.lock_ok().stats();
                                json!({"device": s.device, "channels": p.device_channels(),
                                    "underruns": s.underruns, "fill_ms": s.fill_ms, "correction_ppm": s.correction_ppm})
                            }
                            None => json!({"error": "lost: opening it again"}),
                        };
                        std::thread::sleep(Duration::from_millis(500));
                    }
                })
                .map_err(|e| e.to_string())?
        };
        match rx.recv() {
            Ok(Ok(lane)) => Ok(CardOut {
                device,
                lane,
                _worker: idn_common::worker::Worker::from_parts(stop, thread),
                status,
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(format!("monitor sound card: {e}"))
            }
            Err(_) => Err("monitor sound card: its thread stopped".into()),
        }
    }

    /// The device, its buffer and drift correction.
    pub fn status(&self) -> Value {
        self.status.lock_ok().clone()
    }

    /// What the monitor plays into now.
    pub fn lane(&self) -> Arc<Lane> {
        self.lane.lock_ok().clone()
    }
}

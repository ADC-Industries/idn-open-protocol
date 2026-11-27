//! A sound card on the desk directly (spec/extensions/idn-mixer.md §M.6), without the
//! routing matrix: its inputs on channels of the desk, the desk's outputs on its outputs.
//! Both are drift-corrected to this machine's clock by `idn-audio`.

use idn_audio::{Capture, CaptureConfig, Lane, Playback, PlaybackConfig};
use idn_common::sync::LockOk as _;
use idn_open_protocol::audio::{LaneConfig, Mixer};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A card's input on the desk.
#[derive(Debug, Clone, Default)]
pub struct CardIn {
    /// The device ([`idn_audio::devices`]); `None`: the default capture device.
    pub device: Option<String>,
    /// How many of its inputs (default: all it opens with).
    pub channels: Option<u16>,
    /// The desk channel its first input arrives on (from 0).
    pub first: usize,
}

/// The desk's outputs on a card: output 1 (main L) on its first channel, and on in
/// order as far as it has channels (a stereo card plays the main mix).
#[derive(Debug, Clone, Default)]
pub struct CardOut {
    /// `None`: the default playback device.
    pub device: Option<String>,
}

/// A card's clock as the desk reads it (roadmap R.26): frames the card took or gave, when
/// (`idn_open_protocol::ptp::mono_ns`), and its nominal rate. `None` before it ran.
pub type CardTime = idn_open_protocol::audio::pace::ClockRef;

/// The cards, running on a thread of their own (some audio systems tie a device's stream
/// to the thread that opened it); stopped when dropped.
pub struct Cards {
    /// Its thread, stopped and waited for with it.
    _worker: idn_common::worker::Worker,
    /// What the clock plays the outputs on (its state is the card output's).
    pub out: Option<Arc<Lane>>,
    /// The card the desk follows: its output when it has one (the desk's sound goes
    /// there), else its input, read without drift correction.
    pub time: Option<CardTime>,
    status: Arc<Mutex<Value>>,
}

/// How long the output card takes no sound before the desk starts again for it.
const OUT_LOST_FOR: Duration = Duration::from_secs(10);

/// Frames counted as an input card delivers them, and when (one pair under a lock: two
/// atomics could be read as a new time with the old count, a block off in the clock's
/// fit; set once a period, read every 100 ms, it is never contended).
#[derive(Default)]
struct Counted(Mutex<(u64, i64)>);

impl Cards {
    /// Opens the cards asked for; one that can't be opened says why in [`Cards::status`].
    pub fn start(
        input: Option<CardIn>,
        output: Option<CardOut>,
        desk_in: Arc<Mixer>,
        latency_ms: u32,
        block: usize,
    ) -> Cards {
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(json!({})));
        let (tx, rx) = channel::<(Option<Arc<Lane>>, Option<CardTime>)>();
        // An input card alone is the desk's clock: its blocks as the card gives them, not
        // corrected to this machine's (the desk follows it, so there is nothing to
        // correct).
        let input_clocks = output.is_none();
        let counted = Arc::new(Counted::default());
        let thread = {
            let (stop, status) = (stop.clone(), status.clone());
            std::thread::Builder::new()
                .name("idn-mixer-cards".into())
                .spawn(move || {
                    let lane_cfg = LaneConfig { latency_ms, max_correction_ppm: 1000.0, ..Default::default() };
                    let counting = counted.clone();
                    // (Its lane's state in the status too: what the card's sound waits in.)
                    let mut in_lane: Option<Arc<idn_open_protocol::audio::Lane>> = None;
                    // The capture, opened (and again when lost: the lane and the count go on).
                    let open_capture = |c: &CardIn, lane: Arc<idn_open_protocol::audio::Lane>, counting: Arc<Counted>| {
                        let first = c.first;
                        let cfg = CaptureConfig {
                            device: c.device.clone(),
                            channels: c.channels,
                            sample_rate: crate::desk::rate_hz(),
                            // The card's period the desk's block (bug run 5: 128 frames
                            // whatever the block), each handed on as it comes.
                            block_ms: 0,
                            device_buffer: Some(block as u32),
                            max_correction_ppm: if input_clocks { 0.0 } else { 1000.0 },
                            ..Default::default()
                        };
                        let mut map = Vec::new();
                        Capture::start(&cfg, move |block, _timecode| {
                            let n = block.channels.first().map_or(0, Vec::len) as u64;
                            {
                                let mut c = counting.0.lock_ok();
                                c.0 += n;
                                c.1 = idn_open_protocol::ptp::mono_ns();
                            }
                            if map.len() != block.channels.len() {
                                map = (0..block.channels.len()).map(|k| (k, (first + k) as u16)).collect();
                            }
                            lane.push(block.sample_rate, &block.channels, &map);
                        })
                    };
                    let mut capture = input.map(|c| {
                        let lane = Arc::new(desk_in.lane(&lane_cfg));
                        in_lane = Some(lane.clone());
                        let r = open_capture(&c, lane, counting.clone());
                        (c, r)
                    });
                    // Its tries when lost (idn_audio::reopen: a start grace, backing off).
                    let mut again = idn_audio::reopen::Reopen::default();
                    // Since when the output card stopped taking sound.
                    let mut out_lost: Option<std::time::Instant> = None;
                    let playback = output.map(|o| {
                        let r = Playback::start(&PlaybackConfig { device: o.device.clone(), latency_ms, max_correction_ppm: 1000.0, device_buffer: None });
                        (o, r)
                    });
                    // The desk's outputs go into a lane of their own: its numbers are the
                    // output's (the playback's own lane carries nothing).
                    let out = playback.as_ref().and_then(|(_, p)| p.as_ref().ok()).map(|p| Arc::new(p.lane()));
                    let time: Option<CardTime> = match playback.as_ref().and_then(|(_, p)| p.as_ref().ok()) {
                        Some(p) => {
                            let lanes = p.lanes();
                            Some(Arc::new(move || lanes.clock()))
                        }
                        // (Also when it didn't open: once opened again it is followed.)
                        None if capture.is_some() => {
                            let c = counted.clone();
                            Some(Arc::new(move || {
                                let (frames, at) = *c.0.lock_ok();
                                (at != 0).then_some((frames, at, crate::desk::rate_hz()))
                            }))
                        }
                        None => None,
                    };
                    let _ = tx.send((out.clone(), time));
                    let mut first = true;
                    while !stop.load(Ordering::Relaxed) {
                        // The input card lost (its driver ended the stream, it was
                        // unplugged) or not open: opened again (idn_audio::reopen), on the
                        // same lane (bug run 2026-11-25: it stayed silent for good).
                        if let (Some((c, r)), Some(lane)) = (&mut capture, &in_lane) {
                            let (lost, moved) = match r {
                                Ok(cap) => (cap.status().state == idn_audio::DeviceState::Lost, cap.stats().frames_in > 0),
                                Err(_) => (true, true),
                            };
                            if again.due(lost, moved) {
                                // The old one closed first (a driver may take one at a time).
                                *r = Err(std::io::Error::other("opening again"));
                                // Its count from 0: the desk measures the card afresh (a
                                // count going on past a gap read as a wild rate for 30 s).
                                *counting.0.lock_ok() = (0, 0);
                                *r = open_capture(c, lane.clone(), counting.clone());
                                again.opened();
                                let line = match r {
                                    Ok(_) => "mixer: sound card input opened again".to_owned(),
                                    Err(e) => format!("mixer: sound card input: {e} (tried again later)"),
                                };
                                if again.say(&line) {
                                    idn_open_protocol::log::log(&line);
                                }
                            }
                        }
                        let mut st = json!({});
                        if let Some((c, r)) = &capture {
                            st["input"] = match r {
                                Ok(cap) => {
                                    let s = cap.stats();
                                    let l = in_lane.as_ref().map(|l| l.stats());
                                    // Lost: said as an error (the failure watch's card_lost).
                                    let cs = cap.status();
                                    let lost = (cs.state == idn_audio::DeviceState::Lost).then_some(cs.text);
                                    json!({"device": s.device, "channels": s.channels, "first_channel": c.first + 1, "error": lost,
                                           "device_rate": s.device_rate, "correction_ppm": s.correction_ppm,
                                           "errors": s.errors, "resyncs": s.resyncs,
                                           "fill_ms": l.as_ref().map(|l| l.fill_ms), "target_ms": l.as_ref().map(|l| l.target_ms),
                                           "underruns": l.as_ref().map(|l| l.underruns), "dropped_ms": l.as_ref().map(|l| l.dropped_ms),
                                           "lane_correction_ppm": l.as_ref().map(|l| l.correction_ppm)})
                                }
                                Err(e) => json!({"device": c.device, "error": e.to_string()}),
                            };
                        }
                        if let Some((o, r)) = &playback {
                            st["output"] = match (r, &out) {
                                (Ok(p), Some(lane)) => {
                                    let s = lane.stats();
                                    // Ended for good (its driver invalidated the stream), or
                                    // taking no sound for OUT_LOST_FOR (unplugged): the desk
                                    // starts again to open it anew (its clock and outputs
                                    // are the card's; workers.rs). Lost: said as an error.
                                    let ps = p.status();
                                    let lost = ps.state == idn_audio::DeviceState::Lost;
                                    if !lost {
                                        out_lost = None;
                                    }
                                    let long = lost && out_lost.get_or_insert_with(std::time::Instant::now).elapsed() >= OUT_LOST_FOR;
                                    let ended = lost && (ps.text.starts_with("ended:") || long);
                                    json!({"device": s.device, "channels": p.device_channels(), "underruns": s.underruns, "ended": ended,
                                           "error": lost.then_some(ps.text),
                                           "device_errors": s.errors, "fill_ms": s.fill_ms, "margin_ms": s.margin_ms,
                                           "correction_ppm": s.correction_ppm, "dropped_ms": s.dropped_ms,
                                           "through": s.through, "through_fixed_ms": s.through_fixed_ms,
                                           "calm_drift_ppm": s.calm_drift_ppm, "through_ended": s.through_ended})
                                }
                                (Ok(_), None) => json!({"device": o.device, "error": "no lane"}),
                                (Err(e), _) => json!({"device": o.device, "error": e.to_string()}),
                            };
                        }
                        if first {
                            for (k, v) in st.as_object().into_iter().flatten() {
                                if let Some(e) = v["error"].as_str() {
                                    idn_open_protocol::log::log(&format!("mixer: sound card {k}: {e}"));
                                }
                            }
                            first = false;
                        }
                        *status.lock_ok() = st;
                        std::thread::sleep(Duration::from_millis(500));
                    }
                })
                .expect("a thread")
        };
        let (out, time) = rx.recv().unwrap_or((None, None));
        Cards {
            _worker: idn_common::worker::Worker::from_parts(stop, thread),
            out,
            time,
            status,
        }
    }

    /// The cards' state: devices, drift correction, errors.
    pub fn status(&self) -> Value {
        self.status.lock_ok().clone()
    }

    /// Where [`Cards::status`] is kept (updated twice a second), for a watch that
    /// outlives the borrow (the desk's failure watch, §M.27).
    pub fn status_cell(&self) -> Arc<Mutex<Value>> {
        self.status.clone()
    }
}

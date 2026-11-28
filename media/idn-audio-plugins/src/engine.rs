//! What the plugins do, the same in CLAP and VST3 (idn-routing.md §A.13).
//!
//! **IDN Send** passes its track through and sends it as the IDN source `HOST (DAW Send N)`:
//! a transmitter in every routing matrix. **IDN Return** is the routable receiver
//! `DAW Return N` on this machine's IDN Node: what the Site Manager (or the Node's panel)
//! routes to it is added to its track. `N` is the plugin's one parameter (1–64), so that
//! a project keeps its sends and returns by number in any host, without a window.
//!
//! The audio thread only copies: a send's blocks go into a ring that a pump thread sends
//! from every 2 ms (every 4 ms while the host sends nothing); the Node and the receiving
//! live on a control thread, and the audio thread takes their results without waiting
//! (`try_lock`); a block that finds them busy is skipped (a send) or passes dry (a
//! return). Rendering offline (faster than real time) neither sends nor returns.

use idn_common::sync::LockOk as _;
use idn_open_protocol::api::{AudioBlock, Output, OutputConfig, Publisher, PublisherConfig};
use idn_open_protocol::audio::{Lane, LaneConfig, Mixer};
use idn_open_protocol::node::Node;
use idn_open_protocol::node::routing::{AudioReceiver, Router, RouterConfig, RouterEvent};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The numbers a plugin may take: `DAW Send 1` to `DAW Send 64`.
pub const SLOTS: std::ops::RangeInclusive<u32> = 1..=64;
/// Channels of each plugin (a stereo track).
pub const CHANNELS: usize = 2;
/// What a send's ring holds, at most (interleaved stereo frames): half a second at 96 kHz.
const RING_FRAMES: usize = 48_000;
/// How often the pump sends what the ring holds.
const PUMP: Duration = Duration::from_millis(2);
/// How often the pump looks at the ring once no sound has come for [`QUIET_AFTER`] (the
/// host stopped, or not playing): the first block after it waits up to this long.
const PUMP_QUIET: Duration = Duration::from_millis(4);
/// No sound into the ring for this long: the pump looks less often ([`PUMP_QUIET`]).
const QUIET_AFTER: Duration = Duration::from_millis(500);

/// A return's latency unless the site sets the receiver's (§A.12): a DAW takes its sound
/// in its own buffer's steps (often 512 frames, 10.7 ms), which the lane must ride over.
pub const RETURN_LATENCY_MS: u32 = 20;

/// Which plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Send,
    Return,
}

impl Kind {
    /// `Send` or `Return`, as in the names.
    pub fn word(self) -> &'static str {
        match self {
            Kind::Send => "Send",
            Kind::Return => "Return",
        }
    }
}

static NODE: Mutex<Option<Arc<Node>>> = Mutex::new(None);

/// Uses `node` rather than connecting to the machine's (tests; a program embedding the
/// engine).
pub fn use_node(node: Arc<Node>) {
    *NODE.lock_ok() = Some(node);
}

/// The machine's IDN Node, connected once per host process (as the OBS plugin does) and
/// asked again on each use until one answers.
fn node() -> Option<Arc<Node>> {
    let mut n = NODE.lock_ok();
    if n.is_none() {
        match Node::connect("IDN audio plugins") {
            Ok(x) => *n = Some(Arc::new(x)),
            Err(e) => log(&format!("IDN Node: not used ({e})")),
        }
    }
    n.clone()
}

fn log(m: &str) {
    idn_open_protocol::log::log(&format!("idn-audio-plugins: {m}"));
}

/// The host's name as it goes in the plugins' names: its own name ("REAPER", "Bitwig
/// Studio"), else the program's, without characters that names can't hold.
pub fn host_label(given: Option<&str>) -> String {
    let from_exe = || {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
    };
    let raw = given
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(from_exe)
        .unwrap_or_else(|| "DAW".into());
    let clean: String = raw
        .chars()
        .filter(|c| !matches!(c, '(' | ')' | '"' | '\\' | '/' | ':' | '|') && !c.is_control())
        .take(32)
        .collect();
    match clean.trim() {
        "" => "DAW".into(),
        s => s.to_owned(),
    }
}

/// While active: the stop flag of its threads (control, and a send's pump), and them.
type Running = (Arc<AtomicBool>, Vec<std::thread::JoinHandle<()>>);

/// What one plugin instance shares between the host's threads.
pub struct Engine {
    kind: Kind,
    host: Mutex<String>,
    slot: AtomicU32,
    rate: AtomicU32,
    offline: AtomicBool,
    /// Send: the output, while one is up.
    output: Mutex<Option<Arc<Output>>>,
    /// Return: the lanes' mixer at the host's rate, and where its block goes before it is
    /// added to the track (interleaved).
    mixer: Mutex<Option<Arc<Mixer>>>,
    scratch: Mutex<Vec<f32>>,
    /// Send: the track's sound (interleaved), copied in by the audio thread, sent by the
    /// pump. Its room is made once: the audio thread never allocates.
    ring: Mutex<std::collections::VecDeque<f32>>,
    control: Mutex<Option<Running>>,
    /// What the control thread last did, for the log and the tests.
    status: Arc<Mutex<String>>,
}

impl Engine {
    /// An instance of `kind` in the host `host` ([`host_label`]), number 1.
    pub fn new(kind: Kind, host: &str) -> Arc<Engine> {
        Arc::new(Engine {
            kind,
            host: Mutex::new(host.to_owned()),
            slot: AtomicU32::new(1),
            rate: AtomicU32::new(48_000),
            offline: AtomicBool::new(false),
            output: Mutex::new(None),
            mixer: Mutex::new(None),
            scratch: Mutex::new(Vec::new()),
            ring: Mutex::new(std::collections::VecDeque::with_capacity(
                RING_FRAMES * CHANNELS,
            )),
            control: Mutex::new(None),
            status: Arc::default(),
        })
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// The plugin's number (1–64).
    pub fn slot(&self) -> u32 {
        self.slot.load(Ordering::Relaxed)
    }

    /// A new number: the control thread changes the source or receiver to match.
    pub fn set_slot(&self, n: u32) {
        self.slot
            .store(n.clamp(*SLOTS.start(), *SLOTS.end()), Ordering::Relaxed);
    }

    /// The host's name, once the host says it (VST3 tells it after the plugin is made); the
    /// control thread renames the source or receiver.
    pub fn set_host(&self, host: &str) {
        *self.host.lock_ok() = host.to_owned();
    }

    /// The name of the source (send: `HOST (name)` on the network) or the receiver.
    pub fn name(&self) -> String {
        format!(
            "{} {} {}",
            self.host.lock_ok(),
            self.kind.word(),
            self.slot()
        )
    }

    /// A send's port while it is up (receivers reach it there by address).
    pub fn port(&self) -> Option<u16> {
        self.output.lock_ok().as_ref().map(|o| o.port())
    }

    /// What the control thread last did.
    pub fn status(&self) -> String {
        self.status.lock_ok().clone()
    }

    /// Rendering offline: nothing is sent or returned until it is real time again.
    pub fn set_offline(&self, offline: bool) {
        self.offline.store(offline, Ordering::Relaxed);
    }

    /// The host starts processing at `rate` in blocks of up to `max_frames`: the control
    /// thread starts (off the audio thread).
    pub fn activate(self: &Arc<Self>, rate: u32, max_frames: usize) {
        self.deactivate();
        self.rate.store(rate.max(8000), Ordering::Relaxed);
        if self.kind == Kind::Return {
            let host = self.host.lock_ok().clone();
            *self.mixer.lock_ok() = Some(Mixer::new(rate.max(8000), CHANNELS as u16, &host));
            *self.scratch.lock_ok() = vec![0.0; max_frames.max(1) * CHANNELS];
        }
        let stop = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();
        let control = {
            let (me, stop) = (self.clone(), stop.clone());
            std::thread::Builder::new()
                .name("idn-audio-plugins".into())
                .spawn(move || me.control_loop(&stop))
        };
        match control {
            Ok(t) => threads.push(t),
            Err(e) => log(&format!("control thread: {e}")),
        }
        if self.kind == Kind::Send {
            self.ring.lock_ok().clear();
            let (me, stop) = (self.clone(), stop.clone());
            match std::thread::Builder::new()
                .name("idn-audio-plugins-send".into())
                .spawn(move || me.pump(&stop))
            {
                Ok(t) => threads.push(t),
                Err(e) => log(&format!("send thread: {e}")),
            }
        }
        *self.control.lock_ok() = Some((stop, threads));
    }

    /// The host stops processing: the source or receiver goes away.
    pub fn deactivate(&self) {
        if let Some((stop, threads)) = self.control.lock_ok().take() {
            stop.store(true, Ordering::Relaxed);
            for t in threads {
                let _ = t.join();
            }
        }
        *self.output.lock_ok() = None;
        *self.mixer.lock_ok() = None;
    }

    /// Keeps the source or receiver as the number says, until stopped.
    fn control_loop(&self, stop: &AtomicBool) {
        // Send: the publisher with its output. Return: the receiver and its router.
        let mut live: Option<(String, Box<dyn std::any::Any>)> = None;
        let mut failed_at: Option<(String, std::time::Instant)> = None;
        while !stop.load(Ordering::Relaxed) {
            let slot = self.name();
            let retry = failed_at
                .as_ref()
                .is_none_or(|(s, t)| *s != slot || t.elapsed() >= Duration::from_secs(5));
            if live.as_ref().map(|(s, _)| s) != Some(&slot) && retry {
                // The old one first: a return's receiver name must be free again.
                *self.output.lock_ok() = None;
                live = None;
                let made = match self.kind {
                    Kind::Send => self.start_send(),
                    Kind::Return => self.start_return(),
                };
                let note = match made {
                    Ok(x) => {
                        live = Some((slot, x));
                        failed_at = None;
                        format!("{} up", self.name())
                    }
                    Err(e) => {
                        failed_at = Some((slot, std::time::Instant::now()));
                        format!("{}: {e}", self.name())
                    }
                };
                log(&note);
                *self.status.lock_ok() = note;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        *self.output.lock_ok() = None;
        drop(live);
    }

    /// A send's pump: every 2 ms, what the ring holds goes out as one block (every
    /// [`PUMP_QUIET`], 4 ms, while no sound has come for half a second: the host stopped).
    fn pump(&self, stop: &AtomicBool) {
        let mut buf: Vec<f32> = Vec::new();
        let mut last_sound = std::time::Instant::now();
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(if last_sound.elapsed() >= QUIET_AFTER {
                PUMP_QUIET
            } else {
                PUMP
            });
            {
                let mut r = self.ring.lock_ok();
                buf.clear();
                buf.extend(r.drain(..));
            }
            let frames = buf.len() / CHANNELS;
            if frames == 0 {
                continue;
            }
            last_sound = std::time::Instant::now();
            let Some(out) = self.output.lock_ok().clone() else {
                continue;
            };
            let block = AudioBlock {
                sample_rate: self.rate.load(Ordering::Relaxed),
                channels: (0..CHANNELS)
                    .map(|c| buf.iter().skip(c).step_by(CHANNELS).copied().collect())
                    .collect(),
            };
            let _ = out.send_audio(block, None);
        }
    }

    /// The source `HOST (DAW Send N)`, with the Node's site identity when one runs.
    fn start_send(&self) -> Result<Box<dyn std::any::Any>, String> {
        let mut cfg = PublisherConfig::default();
        if let Some(n) = node() {
            cfg.secure = Some(n.secure_config());
            cfg.link = Some(n.link_config());
            cfg.discovery_server = n
                .status()
                .ok()
                .and_then(|s| s.attr("server").map(str::to_owned))
                .filter(|s| !s.is_empty());
        }
        let p = Publisher::new(cfg).map_err(|e| format!("publisher: {e}"))?;
        let out = Output::new(
            &p,
            OutputConfig {
                audio_labels: vec!["L".into(), "R".into()],
                ..OutputConfig::new(&self.name())
            },
        )
        .map_err(|e| format!("output: {e}"))?;
        *self.output.lock_ok() = Some(Arc::new(out));
        Ok(Box::new(p))
    }

    /// The routable receiver `DAW Return N` on the Node, its routes played into the mixer.
    fn start_return(&self) -> Result<Box<dyn std::any::Any>, String> {
        let node = node().ok_or("a return needs the IDN Node on this machine")?;
        let mixer = self.mixer.lock_ok().clone().ok_or("not active")?;
        let rx = Arc::new(
            AudioReceiver::register_as(
                &node,
                &self.name(),
                CHANNELS as u16,
                &[],
                idn_open_protocol::node::routing::ReceiverKind::Program,
            )
            .map_err(|e| e.to_string())?,
        );
        // In line with the site's other receivers of its sources (§A.14).
        rx.attach_mixer(&mixer);
        let lanes: Arc<Mutex<BTreeMap<String, Lane>>> = Arc::default();
        let router = Router::start(
            node,
            rx.clone(),
            RouterConfig { lossless: true },
            move |ev| match ev {
                RouterEvent::Block(b) => {
                    let mut l = lanes.lock_ok();
                    // Its own latency, unless the site sets the receiver's (§A.12).
                    // Adapting to the path (R.30), from its start (at most the return's
                    // own), in line with the return's other sources.
                    let lane = l.entry(b.tx.to_owned()).or_insert_with(|| {
                        mixer.lane(&LaneConfig {
                            latency_ms: b
                                .latency_ms
                                .unwrap_or(b.path.start_ms().min(RETURN_LATENCY_MS)),
                            max_correction_ppm: 1000.0,
                            adaptive: true,
                            ..Default::default()
                        })
                    });
                    lane.follow_site(b.latency_ms);
                    let at = idn_open_protocol::audio::site_time(
                        b.audio.header.timestamp,
                        b.audio.header.metadata.as_deref(),
                    );
                    lane.push_timed(
                        b.audio.header.sample_rate,
                        &b.audio.channels,
                        Some(b.map),
                        at,
                    );
                }
                RouterEvent::Gone(tx) => {
                    lanes.lock_ok().remove(tx);
                }
            },
        )
        .map_err(|e| e.to_string())?;
        // The router first: it holds the receiver.
        Ok(Box::new((router, rx)))
    }

    /// The audio thread, a block of `frames` frames in place: each channel holds the
    /// track's sound and keeps it (send), or gets the routed sound added (return).
    pub fn process(&self, channels: &mut [&mut [f32]], frames: usize) {
        if self.offline.load(Ordering::Relaxed) || frames == 0 {
            return;
        }
        match self.kind {
            Kind::Send => {
                // Into the ring (no allocation: its room was made); the pump sends it.
                let Some(mut ring) = self.ring.try_lock_ok() else {
                    return;
                };
                if ring.len() + frames * CHANNELS > ring.capacity() {
                    return; // the pump is far behind: this block is skipped
                }
                for i in 0..frames {
                    for c in 0..CHANNELS {
                        let ch = channels.get(c).or(channels.first());
                        ring.push_back(ch.map_or(0.0, |s| s[i]));
                    }
                }
            }
            Kind::Return => {
                let (Some(mixer), Some(mut scratch)) =
                    (self.mixer.try_lock_ok(), self.scratch.try_lock_ok())
                else {
                    return;
                };
                let Some(mixer) = mixer.as_ref() else { return };
                if scratch.len() < frames * CHANNELS {
                    // A block longer than the host said (some hosts do): this once.
                    scratch.resize(frames * CHANNELS, 0.0);
                }
                let buf = &mut scratch[..frames * CHANNELS];
                mixer.fill(buf);
                let n = channels.len();
                for (c, ch) in channels.iter_mut().enumerate() {
                    for (i, s) in ch[..frames].iter_mut().enumerate() {
                        // A mono track takes both sides.
                        *s += if n == 1 {
                            0.5 * (buf[i * CHANNELS] + buf[i * CHANNELS + 1])
                        } else {
                            buf[i * CHANNELS + c.min(CHANNELS - 1)]
                        };
                    }
                }
            }
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.deactivate();
    }
}

/// The plugin's state as saved in a project: its number, as 4 bytes (little-endian).
pub fn save_state(slot: u32) -> [u8; 4] {
    slot.to_le_bytes()
}

/// A saved state's number, if it is one.
pub fn load_state(b: &[u8]) -> Option<u32> {
    let n = u32::from_le_bytes(b.get(..4)?.try_into().ok()?);
    SLOTS.contains(&n).then_some(n)
}

/// The number's parameter as hosts show it: plain 1–64, or 0–1 normalized (VST3).
pub fn slot_to_normalized(n: u32) -> f64 {
    (n.clamp(*SLOTS.start(), *SLOTS.end()) - SLOTS.start()) as f64
        / (SLOTS.end() - SLOTS.start()) as f64
}

pub fn normalized_to_slot(v: f64) -> u32 {
    SLOTS.start() + (v.clamp(0.0, 1.0) * (SLOTS.end() - SLOTS.start()) as f64).round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_numbers_and_state() {
        assert_eq!(host_label(Some("REAPER")), "REAPER");
        assert_eq!(host_label(Some("Bitwig (Studio)")), "Bitwig Studio");
        assert!(!host_label(None).is_empty());
        let e = Engine::new(Kind::Return, "REAPER");
        e.set_slot(3);
        assert_eq!(e.name(), "REAPER Return 3");
        e.set_slot(500);
        assert_eq!(e.slot(), 64);
        assert_eq!(load_state(&save_state(17)), Some(17));
        assert_eq!(load_state(&[0, 0, 0, 0]), None);
        for n in SLOTS {
            assert_eq!(normalized_to_slot(slot_to_normalized(n)), n);
        }
    }

    /// The audio thread's cost per block (a profile): `cargo test --release -p idn-audio-plugins
    /// block_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "a profile"]
    fn block_cost() {
        for kind in [Kind::Send, Kind::Return] {
            let e = Engine::new(kind, "Bench");
            // As active, without the control and pump threads (no network here).
            *e.mixer.lock_ok() = Some(Mixer::new(48_000, 2, "Bench"));
            *e.scratch.lock_ok() = vec![0.0; 128 * CHANNELS];
            let (mut l, mut r) = (vec![0.1f32; 128], vec![0.2f32; 128]);
            let n = 200_000;
            let t = std::time::Instant::now();
            for _ in 0..n {
                e.process(&mut [&mut l[..], &mut r[..]], 128);
                if kind == Kind::Send {
                    e.ring.lock_ok().clear(); // the pump's part, not timed apart
                }
            }
            let per = t.elapsed().as_secs_f64() / n as f64;
            eprintln!(
                "{kind:?}: {:.2} us a 128-frame block ({:.3}% of the block's 2.67 ms)",
                per * 1e6,
                per / (128.0 / 48_000.0) * 100.0
            );
        }
    }

    /// A live send's whole cost, the pump included (a profile): 5 s of 128-frame blocks at
    /// real time into an output, the process's CPU over it.
    #[test]
    #[ignore = "a profile"]
    fn send_cost() {
        let cpu = || {
            let s = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
            let f: Vec<f64> = s
                .rsplit_once(')')
                .map(|(_, r)| {
                    r.split_whitespace()
                        .filter_map(|x| x.parse().ok())
                        .collect()
                })
                .unwrap_or_default();
            (f.get(11).copied().unwrap_or(0.0) + f.get(12).copied().unwrap_or(0.0)) / 100.0
        };
        let e = Engine::new(Kind::Send, "Bench");
        e.activate(48_000, 128);
        idn_common::testing::wait_until(std::time::Duration::from_secs(2), || e.port().is_some());
        let (mut l, mut r) = (vec![0.1f32; 128], vec![0.2f32; 128]);
        let (c0, t0) = (cpu(), std::time::Instant::now());
        let mut next = t0;
        while t0.elapsed() < std::time::Duration::from_secs(5) {
            e.process(&mut [&mut l[..], &mut r[..]], 128);
            next += std::time::Duration::from_micros(2667);
            if let Some(d) = next.checked_duration_since(std::time::Instant::now()) {
                std::thread::sleep(d);
            }
        }
        let used = cpu() - c0;
        eprintln!(
            "a live send (process, pump, output): {:.1}% of a core",
            used / t0.elapsed().as_secs_f64() * 100.0
        );
        e.deactivate();
    }
}

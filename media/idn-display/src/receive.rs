//! The source: its newest picture for the screen, its sound for a sound card.

use idn_common::sync::LockOk as _;
use idn_open_protocol::color::{AlphaMode, ColorConfig, Transfer};
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// A picture to show: its Y'CbCr planes, turned into RGB by the GPU as it draws (they
/// were converted to RGBA here first: 12% of a core at 1080p30 and twice the upload),
/// or RGBA where the CPU must convert (HDR, tone-mapped).
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub kind: Kind,
}

pub enum Kind {
    /// Y, Cb, Cr (8 bits), the chroma planes' size, and how they become RGB.
    Planes {
        y: Vec<u8>,
        cb: Vec<u8>,
        cr: Vec<u8>,
        chroma: (u32, u32),
        rgb: ColorConfig,
    },
    /// `width * height * 4` bytes.
    Rgba(Vec<u8>),
    /// Y, Cb, Cr on the display's GPU, from a source on this machine (§E.27): copied into
    /// the textures there, never through this program's memory.
    Gpu {
        picture: idn_open_protocol::gpu::GpuPicture,
        chroma: (u32, u32),
        rgb: ColorConfig,
    },
}

/// The newest picture, waiting to be drawn (older ones not drawn are dropped: the screen
/// shows the latest, never a queue).
#[derive(Default)]
pub struct Latest {
    slot: Mutex<Option<Picture>>,
    ready: Condvar,
}

impl Latest {
    pub fn put(&self, p: Picture) {
        *self.slot.lock_ok() = Some(p);
        self.ready.notify_all();
    }

    /// The newest picture not yet taken.
    pub fn take(&self) -> Option<Picture> {
        self.slot.lock_ok().take()
    }

    /// Waits up to `timeout` for a picture.
    #[cfg(target_os = "linux")]
    pub fn wait(&self, timeout: Duration) -> Option<Picture> {
        let g = self.slot.lock_ok();
        let (mut g, _) = self
            .ready
            .wait_timeout_while(g, timeout, |p| p.is_none())
            .unwrap();
        g.take()
    }
}

/// The connection as the window sees it, for remote control (R.25): the input (once
/// open), and whether its source takes keyboard and mouse (`idn_input="true"`).
#[derive(Default)]
pub struct Link {
    pub input: Mutex<Option<Arc<Input>>>,
    /// The source takes keyboard and mouse, over a session with a site member or a linked
    /// computer (and `peer`, if given): a source's claim alone let anyone on the LAN
    /// announcing a screen be sent this keyboard.
    pub controllable: AtomicBool,
    /// The computer control is meant for (its key's fingerprint), if known.
    pub peer: Option<String>,
    /// Latency probes on their way (R.25 stage 2: Ctrl+Alt+P), when sent; and the round
    /// trips of those answered, in ms.
    pub probes: Mutex<std::collections::HashMap<u32, std::time::Instant>>,
    pub probe_ms: Mutex<Vec<f64>>,
    /// What the receiver has seen, for the display service's status (§D.8).
    pub seen: Mutex<Seen>,
    /// The sound: the source's channels on the card's first outputs and the gain (`None`:
    /// left and right, as they come).
    pub sound: Mutex<Option<(Vec<u16>, f32)>>,
}

/// What a feed's receiver has seen (§D.8): its connection, its pictures.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    /// Connected to the source (or to the one a routing source sent it to).
    pub connected: bool,
    /// Why it isn't, the last time it said.
    pub reason: String,
    /// The source a routing source sent it to (§1.8).
    pub routed_to: Option<String>,
    /// The last picture's arrival, and how many came.
    pub last_picture: Option<std::time::Instant>,
    pub pictures: u64,
    /// The last picture's size and the frame rate it gave.
    pub size: Option<(u32, u32)>,
    pub frame_rate: Option<f64>,
    /// It came on the GPU (handed over, or SpeedHQ reconstructed there).
    pub on_gpu: bool,
    /// How it came (`squic`, `quic`, `tcp`, …).
    pub via: &'static str,
}

/// What to receive and where its sound goes.
pub struct Config {
    /// The source, `HOST (name)` or `address:port`.
    pub source: String,
    /// This display's name as the source sees it.
    pub receiver_name: String,
    /// Play its sound on this sound card (`Some("")`: the default).
    pub audio: Option<String>,
    pub secure: Option<idn_open_protocol::secure::SecureConfig>,
    pub discovery_server: Option<String>,
    /// The display's GPU, for pictures handed over GPU to GPU (§E.27).
    pub gpu: Option<idn_open_protocol::gpu::GpuHandIn>,
    /// For games (R.25 stage 2): this thread at real-time priority, the fewest pictures
    /// waiting.
    pub game: bool,
    /// Presented in step with the other screens of a video wall (`sync/1`, §E.14).
    pub sync: Option<idn_open_protocol::sync::SyncConfig>,
}

/// Receives until `stop`, handing pictures to `latest` and calling `woken` for each
/// (the window's event loop draws then).
pub fn run(
    cfg: Config,
    latest: Arc<Latest>,
    link: Arc<Link>,
    stop: Arc<AtomicBool>,
    woken: impl Fn() + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("idn-display-in".into())
        .spawn(move || {
            let input = match Input::open(InputConfig {
                receiver_name: cfg.receiver_name.clone(),
                secure: cfg.secure.clone(),
                discovery_server: cfg.discovery_server.clone(),
                // The newest picture only: a late one is dropped, not shown late.
                skip_late: true,
                // For games: none waiting behind the one being shown.
                queue: if cfg.game {
                    2
                } else {
                    InputConfig::new("").queue
                },
                gpu: cfg.gpu.clone(),
                // SpeedHQ's inverse DCT on the GPU, its pictures left there (§E.27):
                // the CPU only reads the bitstream. `IDN_DISPLAY_GPU_DECODE=0`: decoded
                // on the CPU (to compare).
                coefficients: cfg.gpu.is_some()
                    && idn_common::switches::on("IDN_DISPLAY_GPU_DECODE"),
                sync: cfg.sync.clone(),
                ..InputConfig::new(&cfg.source)
            }) {
                Ok(i) => Arc::new(i),
                Err(e) => {
                    eprintln!("idn-display: {}: {e}", cfg.source);
                    link.seen.lock_ok().reason = e.to_string();
                    return;
                }
            };
            *link.input.lock_ok() = Some(input.clone());
            if cfg.game && !idn_common::rt::audio_this_thread() {
                eprintln!(
                    "idn-display: no real-time priority here (an rtprio limit or RealtimeKit)"
                );
            }
            let playback = cfg.audio.as_ref().and_then(|d| {
                let pc = idn_audio::PlaybackConfig {
                    device: Some(d.clone()).filter(|d| !d.is_empty() && d != "default"),
                    latency_ms: 40,
                    max_correction_ppm: 1000.0,
                    device_buffer: None,
                };
                match idn_audio::Playback::start(&pc) {
                    Ok(p) => Some(p),
                    Err(e) => {
                        eprintln!("idn-display: sound card {d:?}: {e}");
                        None
                    }
                }
            });
            let lane = playback.as_ref().map(|p| p.lane());
            let mut on_gpu_said = false;
            let mut recon = cfg.gpu.as_ref().map(|h| {
                idn_shq_gpu::Reconstructor::new(Arc::new(idn_shq_gpu::Gpu::adopt(
                    h.device.clone(),
                    h.queue.clone(),
                    "IDN_DISPLAY_IDCT",
                )))
            });
            while !stop.load(Ordering::Relaxed) {
                match input.recv(Duration::from_millis(200)) {
                    Some(InputEvent::Video(mut v)) => {
                        let (w, h) = (v.picture.width, v.picture.height);
                        // On the GPU (§E.27). Read back only for what needs the samples
                        // here: HDR tone-mapped on the CPU.
                        let mut on_gpu = v.gpu.take();
                        // SpeedHQ as coefficients: reconstructed there, as a picture
                        // handed over there; on the CPU once the GPU fails.
                        if let Some(c) = v.picture.coefficients.take() {
                            match recon.as_mut().map(|r| r.reconstruct_gpu(&c)) {
                                Some(Ok(g)) => {
                                    if !on_gpu_said {
                                        eprintln!(
                                            "idn-display: SpeedHQ reconstructed on the GPU"
                                        );
                                        on_gpu_said = true;
                                    }
                                    v.picture.planes = c
                                        .planes
                                        .iter()
                                        .map(|p| idn_open_protocol::shq::Plane {
                                            width: p.width,
                                            height: p.height,
                                            data: Vec::new(),
                                        })
                                        .collect();
                                    on_gpu = Some(g);
                                }
                                failed => {
                                    if let Some(Err(e)) = failed {
                                        eprintln!(
                                            "idn-display: SpeedHQ reconstructed on the CPU from now ({e})"
                                        );
                                        recon = None;
                                    }
                                    v.picture = idn_open_protocol::shq::reconstruct(&c);
                                }
                            }
                        }
                        if let Some(g) = &on_gpu
                            && v.rgb.transfer != Transfer::Sdr
                        {
                            let Some(hand) = &cfg.gpu else { continue };
                            match g.read_planes(hand) {
                                Ok(data) => {
                                    for (pl, d) in v.picture.planes.iter_mut().zip(data) {
                                        pl.data = d;
                                    }
                                    on_gpu = None;
                                }
                                Err(_) => continue,
                            }
                        }
                        let p = &v.picture.planes;
                        if p.len() < 3 || w == 0 || h == 0 {
                            continue;
                        }
                        let kind = if let Some(picture) = on_gpu {
                            let cb = &v.picture.planes[1];
                            Kind::Gpu {
                                chroma: (cb.width as u32, cb.height as u32),
                                picture,
                                rgb: v.rgb,
                            }
                        } else if v.rgb.transfer == Transfer::Sdr {
                            let mut it = v.picture.planes.into_iter();
                            let (y, cb, cr) =
                                (it.next().unwrap(), it.next().unwrap(), it.next().unwrap());
                            Kind::Planes {
                                chroma: (cb.width as u32, cb.height as u32),
                                y: y.data,
                                cb: cb.data,
                                cr: cr.data,
                                rgb: v.rgb,
                            }
                        } else {
                            // PQ / HLG: tone-mapped to SDR on the CPU, as the library does.
                            let mut rgba = Vec::new();
                            idn_open_protocol::color::planes_to_rgba_into(
                                p,
                                w,
                                h,
                                AlphaMode::Opaque,
                                &v.rgb,
                                &mut rgba,
                            );
                            Kind::Rgba(rgba)
                        };
                        {
                            let mut s = link.seen.lock_ok();
                            s.last_picture = Some(std::time::Instant::now());
                            s.pictures += 1;
                            s.size = Some((w as u32, h as u32));
                            s.frame_rate = (v.header.frame_rate_d > 0).then(|| {
                                v.header.frame_rate_n as f64 / v.header.frame_rate_d as f64
                            });
                            s.on_gpu = matches!(kind, Kind::Gpu { .. });
                            s.via = v.via;
                        }
                        latest.put(Picture {
                            width: w as u32,
                            height: h as u32,
                            kind,
                        });
                        woken();
                    }
                    // The source's capabilities: whether it takes keyboard and mouse.
                    Some(InputEvent::Metadata { xml, .. }) if xml.contains("ndi_capabilities") => {
                        let (k, v) = idn_open_protocol::remote::CAPABILITY;
                        let takes = xml.contains(&format!("{k}=\"{v}\""));
                        let st = input.stats();
                        let trusted = st.encrypted
                            && (st.site_trust || st.linked)
                            && link
                                .peer
                                .as_deref()
                                .is_none_or(|p| st.peer_fp.as_deref() == Some(p));
                        if takes && !trusted {
                            eprintln!(
                                "idn-display: the source takes keyboard and mouse, but not over a session with {}: not sent",
                                if link.peer.is_some() {
                                    "the computer chosen"
                                } else {
                                    "a site member or a linked computer"
                                }
                            );
                        }
                        link.controllable.store(takes && trusted, Ordering::Relaxed);
                    }
                    // A latency probe's answer (R.25 stage 2): its round trip.
                    Some(InputEvent::Metadata { xml, .. })
                        if let Some(id) = idn_open_protocol::remote::probe_of(&xml) =>
                    {
                        if let Some(t) = link.probes.lock_ok().remove(&id) {
                            link.probe_ms
                                .lock_ok()
                                .push(t.elapsed().as_secs_f64() * 1e3);
                        }
                    }
                    Some(InputEvent::State(st)) => {
                        // A new session (or none): control waits for its capabilities.
                        link.controllable.store(false, Ordering::Relaxed);
                        use idn_open_protocol::input::ConnectionState as C;
                        let mut s = link.seen.lock_ok();
                        match st {
                            C::Connected(_) => s.connected = true,
                            C::Routed { name, .. } => {
                                s.connected = true;
                                s.routed_to = Some(name);
                            }
                            C::Connecting => s.connected = false,
                            C::Lost(why) => {
                                s.connected = false;
                                s.reason = why;
                            }
                        }
                    }
                    Some(InputEvent::Audio(a)) => {
                        if let Some(lane) = &lane {
                            let sound = link.sound.lock_ok().clone();
                            match sound {
                                // Left and right on the card's first two outputs.
                                None => {
                                    let map: Vec<(usize, u16)> = (0..a.channels.len().min(2))
                                        .map(|c| (c, c as u16))
                                        .collect();
                                    lane.push(a.header.sample_rate, &a.channels, &map);
                                }
                                // The channels chosen, in order, at their level (§D.8).
                                Some((chosen, gain)) => {
                                    let picked: Vec<Vec<f32>> = chosen
                                        .iter()
                                        .filter_map(|&c| a.channels.get(c as usize))
                                        .map(|ch| ch.iter().map(|x| x * gain).collect())
                                        .collect();
                                    let map: Vec<(usize, u16)> =
                                        (0..picked.len()).map(|k| (k, k as u16)).collect();
                                    lane.push(a.header.sample_rate, &picked, &map);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        })
        .expect("a thread")
}

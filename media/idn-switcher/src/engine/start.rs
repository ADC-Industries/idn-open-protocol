//! Starting the switcher: its program output appears on the network at once, black until
//! an input is put on program.

use super::*;

impl Switcher {
    /// Starts: the program output appears on the network at once, black until an input
    /// is put on program.
    pub fn start(mut cfg: SwitcherConfig) -> io::Result<Switcher> {
        // With the IDN Node's encryption, its Discovery Server unless one is set here, as
        // the protocol's inputs and outputs take it (idn-node.md §N.1): the desks and IDN
        // Data feeds of an enrolled site are listed there, not on mDNS.
        if cfg.discovery_server.is_none() {
            cfg.discovery_server = cfg
                .secure
                .as_ref()
                .and_then(|s| s.site.as_ref())
                .and_then(|e| e.node_machine())
                .and_then(|m| m.discovery_server);
        }
        let publisher = Publisher::new(PublisherConfig {
            mdns: cfg.mdns,
            secure: cfg.secure.clone(),
            discovery_server: cfg.discovery_server.clone(),
            ..Default::default()
        })?;
        let mut program_cfg = fed_output_config(&cfg.name, &cfg.out_codec);
        let hdr = cfg.hdr != idn_open_protocol::color::Transfer::Sdr;
        if hdr {
            // Its colour said with every picture (§E.7), BT.2100.
            program_cfg.color_info =
                Some(idn_open_protocol::color::to_hdr::hdr_color_info(cfg.hdr));
        }
        let program_out = Arc::new(Output::new(&publisher, program_cfg)?);
        program_out.set_frame_rate(cfg.fps);
        let program_hdr = hdr.then(|| hdr_sender::HdrSender::new(program_out.clone(), cfg.hdr));
        let preview_out = if cfg.preview_output {
            let o = Output::new(
                &publisher,
                output_config(&format!("{} (Preview)", cfg.name)),
            )?;
            o.set_frame_rate(cfg.fps);
            Some(o)
        } else {
            None
        };
        let multiview_out = if cfg.multiview_output {
            let o = Output::new(
                &publisher,
                OutputConfig::new(&format!("{} (Multiview)", cfg.name)),
            )?;
            o.set_frame_rate(multiview_rate(cfg.fps));
            Some(Arc::new(o))
        } else {
            None
        };
        // The monitor's sound on the network (§S.25), for a desk's headphones.
        let monitor_net = Output::new(
            &publisher,
            output_config(&format!("{} (Monitor)", cfg.name)),
        )?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                version: 1,
                program: None,
                preview: None,
                kind: Kind::Mix,
                duration: 1.0,
                position: 0.0,
                running: false,
                ftb: false,
                ftb_level: 0.0,
                audio: AudioSource::FollowProgram,
                gain_db: 0.0,
                delay_ms: 0,
                audio_denoise: false,
                take_feed: false,
                triggers: Vec::new(),
                stingers: vec![None; STINGERS],
                cut_done: false,
                mixes: BTreeMap::new(),
                monitor: Monitor::default(),
            }),
            changed: Condvar::new(),
            inputs: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(1),
            program_out,
            program_hdr,
            preview_out,
            multiview_out: Mutex::new(multiview_out),
            multiview_layout: Mutex::new(None),
            multiview: Mutex::new((0, None)),
            multiview_wanted: Mutex::new(Instant::now()),
            thumbs_wanted: Mutex::new(BTreeMap::new()),
            multiview_jpeg: Mutex::new(None),
            mixer: Mixer::new(rate(), 2, "switcher"),
            tracks: Mutex::new(Vec::new()),
            scenes: Mutex::new(BTreeMap::new()),
            globals: Mutex::new(BTreeMap::new()),
            next_global: AtomicU64::new(1),
            layer_fades: Mutex::new(BTreeMap::new()),
            titles: Mutex::new(BTreeMap::new()),
            title_texts: Mutex::new(BTreeMap::new()),
            title_pictures: Mutex::new(BTreeMap::new()),
            timers: Mutex::new(BTreeMap::new()),
            title_data: Mutex::new(BTreeMap::new()),
            cameras: Mutex::new(BTreeMap::new()),
            views: Mutex::new(BTreeMap::new()),
            frame_clock: Mutex::new(None),
            views_on_gpu: AtomicBool::new(false),
            #[cfg(feature = "gpu")]
            gpu_made: Mutex::new(None),
            #[cfg(feature = "gpu")]
            gpu_hand: Mutex::new(None),
            #[cfg(feature = "gpu")]
            gpu_seed: Mutex::new(None),
            clock_runs: std::sync::atomic::AtomicU64::new(0),
            mixes_on_gpu: AtomicBool::new(false),
            hdr_path: std::sync::atomic::AtomicU8::new(0),
            fonts: Mutex::new(None),
            slides: Mutex::new(BTreeMap::new()),
            loader: Mutex::new(None),
            on_stinger: Mutex::new(None),
            publisher: publisher.clone(),
            mix_outs: Mutex::new(BTreeMap::new()),
            next_mix: AtomicU64::new(1),
            routed: Arc::default(),
            routed_on: Arc::new(AtomicBool::new(false)),
            routed_delay: Arc::default(),
            router: Mutex::new(None),
            receiver: Mutex::new(None),
            routed_mixer: Mixer::new(rate(), FEED_CHANNELS as u16, "switcher routed"),
            feeds: Mutex::new(BTreeMap::new()),
            desks: Mutex::new(BTreeMap::new()),
            text_sources: Mutex::new(Vec::new()),
            other_sources: Mutex::new(Vec::new()),
            listed: Mutex::new(None),
            captions: Mutex::new(Captions::default()),
            stop: AtomicBool::new(false),
            #[cfg(feature = "card")]
            card: Mutex::new(None),
            monitor_mixer: Mixer::new(rate(), 2, "switcher monitor"),
            monitor_net,
            preview_mixer: Mixer::new(rate(), 2, "switcher preview"),
            monitor_out: Mutex::new(MonitorOut::default()),
            monitor_ready: Condvar::new(),
            #[cfg(feature = "card")]
            monitor_card: Mutex::new(None),
            compositor: Mutex::new("CPU".into()),
            frame_us: AtomicU64::new(0),
            parts: Default::default(),
            gpu_parts: Default::default(),
            scene_parts: Default::default(),
            heard: Mutex::new(Vec::new()),
            late: AtomicU64::new(0),
            frame_beat: idn_common::heartbeat::Heartbeat::new(),
            worst: Mutex::new(Worst::default()),
            cfg,
        });
        // The routable receiver: routes come from the Node, sound into lanes while chosen.
        if let Some(node) = &shared.cfg.node {
            use idn_open_protocol::node::routing::{
                AudioReceiver, ReceiverKind, Router, RouterConfig, RouterEvent,
            };
            let labels: Vec<String> = ["Program L", "Program R"]
                .map(str::to_owned)
                .into_iter()
                .chain((1..=MIXES).flat_map(|k| [format!("Mix {k} L"), format!("Mix {k} R")]))
                .collect();
            match AudioReceiver::register_as(
                node,
                &shared.cfg.name,
                FEED_CHANNELS as u16,
                &labels,
                ReceiverKind::Switcher,
            ) {
                Ok(rx) => {
                    let rx = Arc::new(rx);
                    *shared.receiver.lock_ok() = Some(rx.clone());
                    let (lanes, on, mixer, delay) = (
                        shared.routed.clone(),
                        shared.routed_on.clone(),
                        shared.routed_mixer.clone(),
                        shared.routed_delay.clone(),
                    );
                    // In line with the site's other receivers of its sources (§A.14).
                    rx.attach_mixer(&mixer);
                    let r =
                        Router::start(
                            node.clone(),
                            rx,
                            RouterConfig::default(),
                            move |ev| match ev {
                                RouterEvent::Block(b) => {
                                    let mut l = lanes.lock_ok();
                                    if !on.load(Ordering::Relaxed) {
                                        l.clear();
                                        return;
                                    }
                                    // Its own, unless the site sets the receiver's (§A.12).
                                    // Adapting to the path (R.30), from its start (at
                                    // most the switcher's own), in line with its other
                                    // sources; with a stream delay, held at it instead
                                    // (the site's latency, too, plus the delay).
                                    let d = delay.load(Ordering::Relaxed);
                                    let lane = l.entry(b.tx.to_owned()).or_insert_with(|| {
                                        mixer.lane(&LaneConfig {
                                            latency_ms: if d > 0 {
                                                SOUND_BUFFER_MS + d
                                            } else {
                                                b.latency_ms.unwrap_or(
                                                    b.path.start_ms().min(SOUND_BUFFER_MS),
                                                )
                                            },
                                            max_correction_ppm: 1000.0,
                                            adaptive: d == 0,
                                            ..Default::default()
                                        })
                                    });
                                    lane.follow_site(b.latency_ms.map(|m| m + d));
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
                        )?;
                    *shared.router.lock_ok() = Some(r);
                }
                Err(e) => {
                    idn_open_protocol::log::log(&format!("switcher: no routable receiver: {e}"))
                }
            }
        }
        // The GPU compositor now, before any input opens: inputs keep their pictures on its
        // GPU (§E.27), and decoders decode there.
        #[cfg(feature = "gpu")]
        if shared.cfg.gpu {
            let made = make_gpu(&shared);
            *shared.gpu_seed.lock_ok() = Some(match &made {
                Ok(g) => {
                    *shared.gpu_hand.lock_ok() = Some(g.hand_in());
                    Ok(g.seed())
                }
                Err(e) => Err(e.clone()),
            });
            *shared.gpu_made.lock_ok() = Some(made);
        }
        let clock = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("idn-switcher-clock".into())
                // Started again after a panic: the program goes on, the status says how
                // often (the crash report has why).
                .spawn(move || {
                    idn_common::heartbeat::keep_running(
                        "the switcher's frame clock",
                        &shared.stop,
                        &shared.frame_beat,
                        || frame_clock(&shared),
                    )
                })?
        };
        let triggers = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("idn-switcher-triggers".into())
                .spawn(move || triggers_watch(&shared))?
        };
        // Desks and IDN Data feeds are found with or without a Node (without one they
        // were never listed, and a Node that couldn't be reached went unnoticed); only the
        // desks' sound feeds need it.
        let desks = {
            let shared = shared.clone();
            Some(
                std::thread::Builder::new()
                    .name("idn-switcher-desks".into())
                    .spawn(move || watch_desks(&shared))?,
            )
        };
        let filters = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("idn-switcher-filters".into())
                .spawn(move || filters_tick(&shared))?
        };
        let titles = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("idn-switcher-titles".into())
                .spawn(move || titles_tick(&shared))?
        };
        let data = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("idn-switcher-data".into())
                .spawn(move || data_tick(&shared))?
        };
        Ok(Switcher {
            shared,
            _publisher: publisher,
            clock: Some(clock),
            triggers: Some(triggers),
            desks,
            filters: Some(filters),
            titles: Some(titles),
            data: Some(data),
        })
    }
}

//! The service's start and run: its folder, problems watched, audit, and close.

use super::*;

impl Service {
    /// Starts the switcher and what runs it: its saved setup restored, its show, its
    /// streams' watch. One a process (its folder and the engine's rate are the process's).
    pub fn start(sc: ServiceConfig) -> Result<Arc<Service>, String> {
        let ServiceConfig {
            switcher: mut cfg,
            dir,
            inputs,
            node,
            rate: own_rate,
            machine_rate,
        } = sc;
        // The folder (none given: the default) is set once the start can no longer fail
        // (bug run O-L9: a failed first start kept its folder for good, and a later one
        // with none given took it silently).
        let dir = dir.unwrap_or_else(|| idn_common::paths::program_dir("switcher"));
        if STATE_DIR.get().is_some_and(|had| *had != dir) {
            return Err("a switcher service with another folder runs in this process".into());
        }
        let _ = std::fs::create_dir_all(&dir);
        if let Some(n) = &node {
            cfg.secure = Some(n.secure_config());
            cfg.node = Some(n.clone());
        }
        crate::engine::set_rate(
            own_rate
                .or(machine_rate)
                .or(node.as_ref().map(|n| idn_open_protocol::node::rate::ask(n)))
                .unwrap_or(idn_open_protocol::node::rate::DEFAULT),
        );
        // The stream sound's suppression delay at this rate, measured now (bug run J2-4).
        let _ = idn_dsp::denoise::latency(crate::engine::rate());
        #[cfg(feature = "ffmpeg")]
        let ff = Arc::new(idn_ffmpeg::Ffmpeg::probe());
        #[cfg(feature = "ffmpeg")]
        {
            // NDI|HX and compressed IDN sources decode to pictures; SpeedHQ is still asked for.
            // Nothing takes an input's pictures faster than the program's rate (program,
            // monitors and ISOs all run at it): a 4K60 input on a 30 program hands over 30.
            cfg.codec = Some(idn_open_protocol::codec::CodecInput::new(
                Arc::new(ff.with_picture_rate(cfg.fps)),
                idn_open_protocol::codec::CodecUse::Allow,
            ));
            // One format forced on this machine (§N.13): the program and mixes send it to
            // every IDN receiver that decodes it (encoded from the GPU's pictures); otherwise
            // SpeedHQ, as always.
            cfg.out_codec = idn_ffmpeg::output_codec(&ff, None, None).filter(|c| c.fixed);
        }
        let sw = Arc::new(Switcher::start(cfg.clone()).map_err(|e| e.to_string())?);
        for s in &inputs {
            if let Err(e) = sw.add_input(s, Source::Network(s.clone())) {
                idn_open_protocol::log::log(&format!("switcher: input {s:?}: {e}"));
            }
        }
        #[cfg(feature = "ffmpeg")]
        let gateway = Arc::new(idn_ffmpeg::gateway::Gateway::new(ff.clone()));
        // The program's sound rate, known before its first sound (bug run L4-1).
        #[cfg(feature = "ffmpeg")]
        gateway.set_rate(crate::engine::rate());
        #[cfg(feature = "ffmpeg")]
        let _tap = gateway.attach(sw.program_output());
        // Typed failure events (§S.37): to the Site Manager's log through the Node.
        let events = idn_open_protocol::web::problems::Events::new();
        if let Some(n) = &node {
            events.log.to_node(n.clone());
        }
        let streams = Streams {
            events: events.clone(),
            seen: Mutex::default(),
            #[cfg(feature = "ffmpeg")]
            gateway,
            #[cfg(feature = "ffmpeg")]
            ff: ff.clone(),
            #[cfg(feature = "ffmpeg")]
            isos: Mutex::new(std::collections::BTreeMap::new()),
            version: Mutex::new(0),
            pairs: Mutex::default(),
            records: Mutex::default(),
            live_since: Mutex::default(),
            names: Mutex::default(),
            watch: Mutex::default(),
            alerts: Mutex::default(),
            history: Mutex::default(),
            node: node.clone(),
        };
        let media = Media {
            #[cfg(all(feature = "ffmpeg", feature = "gpu"))]
            gpu: sw.gpu().filter(|_| idn_common::switches::on("IDN_SWITCHER_GPU_INPUTS")),
            #[cfg(feature = "ffmpeg")]
            ff: ff.clone(),
            #[cfg(feature = "ffmpeg")]
            fps: cfg.fps,
            #[cfg(feature = "ffmpeg")]
            // The machine's site identity: on a site-mode Node, the switcher's inputs take only
            // members' sources.
            // Unlisted, in a group of their own: the switcher takes these outputs by address,
            // and nobody else should see them in their source lists (with the Node's identity
            // they were registered with the site's server).
            publisher: idn_open_protocol::api::Publisher::new(idn_open_protocol::api::PublisherConfig {
                unlisted: true,
                secure: cfg.secure.clone(),
                groups: vec!["idn-switcher-media".into()],
                monitoring: false,
                ..Default::default()
            })
            .map_err(|e| format!("the media's outputs: {e}"))?,
            #[cfg(feature = "ffmpeg")]
            players: Mutex::default(),
            node: node.clone(),
            library: Mutex::new(None),
            #[cfg(feature = "ffmpeg")]
            lists: Mutex::default(),
            streams_in: Mutex::default(),
            #[cfg(feature = "ffmpeg")]
            looping: Mutex::default(),
        };
        if STATE_DIR.set(dir.clone()).is_err() && STATE_DIR.get() != Some(&dir) {
            return Err("a switcher service with another folder runs in this process".into());
        }
        let displays = {
            let sw = sw.clone();
            crate::displays::Displays::start(
                crate::displays::display_exe(),
                Box::new(move |name: &str| {
                    sw.outputs()
                        .into_iter()
                        .find(|o| o.1 == name)
                        .map(|o| format!("127.0.0.1:{}", o.2))
                }),
            )
        };
        // (Unreadable: set aside and said; an action this build can't read: left out and
        // said, the file copied aside, not the whole show lost; bug round 2026-11-09.)
        let show_path = state_dir().join(SHOW_FILE);
        let show = match idn_common::fsutil::load_saved(&show_path, |b| {
            serde_json::from_slice::<Value>(b).ok()
        }) {
            Ok(Some(v)) => {
                let (show, notes) = crate::show::Show::from_saved(&v);
                if !notes.is_empty() {
                    let copy = show_path.with_extension("json.bad");
                    let _ = std::fs::copy(&show_path, &copy);
                    idn_open_protocol::log::log(&format!(
                        "switcher: the show: loaded with {} cue(s); what this build can't read was left out of it (the file as it was kept as {}): {}",
                        show.cues.len(),
                        copy.display(),
                        notes.join("; ")
                    ));
                }
                show
            }
            Ok(None) => Default::default(),
            Err(why) => {
                idn_open_protocol::log::log(&format!("switcher: the show: {why}"));
                Default::default()
            }
        };
        let api = Arc::new(Service {
            show: Mutex::new(crate::show::Runner::new(show)),
            show_errors: Arc::default(),
            show_version: Arc::default(),
            cpu_seen: Mutex::new(None),
            gpu_seen: Mutex::default(),
            undo: Mutex::default(),
            replaying: AtomicBool::new(false),
            setup_gen: std::sync::atomic::AtomicU64::new(0),
            displays,
            sw: sw.clone(),
            pending_shots: Mutex::new(Vec::new()),
            unrestored: Mutex::new(Vec::new()),
            unplaced_items: Mutex::new(std::collections::BTreeMap::new()),
            unplaced_globals: Mutex::new(std::collections::BTreeMap::new()),
            streams,
            media,
            seq: Mutex::default(),
            mqtt: std::sync::OnceLock::new(),
            audit: idn_open_protocol::web::audit::Audit::for_engine(Some(&dir)),
            tokens: node.as_ref().map(|n| {
                idn_open_protocol::web::TokenCheck::from_enrollment(
                    "switcher",
                    n.enrollment().clone(),
                )
            }),
            events,
            watcher: Mutex::default(),
        });
        // The failure watch (§S.37), each second.
        {
            let api_w = Arc::downgrade(&api);
            std::thread::Builder::new()
                .name("idn-switcher-problems".into())
                .spawn(move || {
                    while let Some(api) = api_w.upgrade() {
                        api.watch_problems();
                        drop(api);
                        std::thread::sleep(Duration::from_secs(1));
                    }
                })
                .expect("a thread");
        }
        // MQTT (§S.33): its state, commands and triggers, and the cues' MQTT actions.
        let _ = api
            .mqtt
            .set(mqtt_link::MqttLink::start(Arc::downgrade(&api)));
        // Slideshows read their pictures with FFmpeg's decoders, their names as a file
        // input's (in the media folder, or the site's library).
        #[cfg(feature = "ffmpeg")]
        {
            let api = Arc::downgrade(&api);
            sw.set_picture_loader(Arc::new(move |path: &str| {
                let path = match api.upgrade() {
                    Some(a) => a.media.resolve(path)?,
                    None => path.to_owned(),
                };
                load_picture(&path)
            }));
        }
        // The titles, slideshows and scenes kept from the last run, once the inputs named on
        // the command line exist (titles and slideshows first: scenes show them).
        api.restore_timers();
        api.restore_monitor();
        api.restore_sources();
        api.restore_globals();
        api.restore_scenes();
        api.restore_mixes();
        api.restore_multiview(true);
        // Screens last, once the mixes they may show exist.
        api.restore_screens();
        // Cues that follow on by themselves (§S.27).
        {
            let api = Arc::downgrade(&api);
            std::thread::Builder::new()
                .name("idn-switcher-show".into())
                .spawn(move || {
                    while let Some(api) = api.upgrade() {
                        // The cue it follows on to, fired only if it is still the next one:
                        // a Go pressed meanwhile fired it already, and this fired the one
                        // after at once (deep bug run 2026-11-09).
                        let due = {
                            let r = api.show.lock_ok();
                            r.follow_due().then_some(r.next).flatten()
                        };
                        if let Some(k) = due {
                            let _ = api.command("cue_go", &json!({ "if_next": k }));
                        }
                        drop(api);
                        std::thread::sleep(Duration::from_millis(100));
                    }
                })
                .expect("a thread");
        }
        api.keep_mixes_saved();
        api.keep_pending_shots();
        // Inputs' filters (§S.17) and cameras' control (§S.21), by label.
        api.keep_by_label(FILTERS_FILE, "filters", Value::clone, |sw, id, v| {
            crate::filter::items_from_json(v).and_then(|f| sw.set_filters(id, f))
        });
        api.keep_by_label(
            CAMERAS_FILE,
            "camera",
            |v| v["control"].clone(),
            |sw, id, v| crate::ptz::Control::from_json(v).and_then(|c| sw.set_camera(id, Some(c))),
        );
        // Inputs kept receiving while not in use (§S.3), by label.
        api.keep_by_label(
            KEEP_ACTIVE_FILE,
            "keep_active",
            Value::clone,
            |sw, id, v| sw.keep_active(id, v.as_bool().unwrap_or(false)),
        );
        // Scenes' desk snapshots recalled as they go live, by label.
        api.keep_by_label(
            SCENE_SNAPSHOTS_FILE,
            "on_air_snapshot",
            Value::clone,
            |sw, id, v| {
                sw.scene_snapshot(
                    id,
                    v["desk"]
                        .as_str()
                        .zip(v["snapshot"].as_str())
                        .map(|(d, s)| (d.to_owned(), s.to_owned())),
                )
            },
        );
        // Inputs' sound sync offsets (§S.28), by label.
        api.keep_by_label(
            AUDIO_OFFSETS_FILE,
            "audio_offset_ms",
            Value::clone,
            |sw, id, v| sw.input_audio_offset(id, v.as_u64().unwrap_or(0).min(500) as u32),
        );
        // A stinger whose clip is a file input plays it from its start (§S.14).
        #[cfg(feature = "ffmpeg")]
        {
            let api_w = Arc::downgrade(&api);
            sw.on_stinger(move |input| {
                if let Some(api) = api_w.upgrade()
                    && let Some((_, p)) = api.media.players.lock_ok().get(&input)
                {
                    p.seek(Duration::ZERO);
                    p.resume();
                }
            });
        }
        // The streams' and recordings' health (§S.30), every 2 s.
        #[cfg(feature = "ffmpeg")]
        {
            let api_w = Arc::downgrade(&api);
            std::thread::Builder::new()
                .name("idn-switcher-watch".into())
                .spawn(move || {
                    while let Some(api) = api_w.upgrade() {
                        api.streams.reattach_mixes(&api.sw);
                        api.streams.watch();
                        drop(api);
                        std::thread::sleep(Duration::from_secs(2));
                    }
                })
                .expect("a thread");
        }
        Ok(api)
    }

    /// Its folder.
    pub fn dir(&self) -> std::path::PathBuf {
        state_dir()
    }

    /// The switcher's typed events (§S.37): for a host's own push channel or log.
    pub fn failure_events(&self) -> &Arc<idn_open_protocol::web::problems::Events> {
        &self.events
    }

    /// One round of the failure watch (§S.37): the engine's state against the last
    /// round's, the sources on the network, the load; each change an event.
    pub(super) fn watch_problems(&self) {
        let st = self.sw.state();
        let now = Instant::now();
        let listed = self.sw.listed_sources();
        let evs = {
            let mut w = self.watcher.lock_ok();
            let mut evs = problems::transitions(&w.before, &st);
            evs.extend(w.sources(&st, listed.as_deref(), now));
            evs.extend(w.load(&st, self.streams.skipping(), now));
            w.before = st;
            evs
        };
        for (name, level, text, f) in evs {
            self.events.emit(name, level, &text, f);
        }
    }

    /// What is wrong now (§S.37): in the state `s` (the inputs, cameras, feeds, streams),
    /// the sources gone, the load.
    pub(super) fn problems_of(&self, s: &Value) -> Vec<Value> {
        use idn_open_protocol::web::problems::problem;
        let mut out = problems::problems(s);
        let w = self.watcher.lock_ok();
        out.extend(w.sources.values().cloned());
        if let Some((since, part)) = w.overloaded {
            out.push(problem(
                "overloaded",
                "warning",
                &format!("the {part} can't keep up"),
                json!({"part": part, "for_s": since.elapsed().as_secs()}),
            ));
        }
        drop(w);
        for d in s["streams"].as_array().into_iter().flatten() {
            let name = d["name"].as_str().unwrap_or("");
            match (d["reason"].as_str(), d["state"].as_str()) {
                (Some("auth_expired"), _) => out.push(problem(
                    "stream_auth_expired",
                    "error",
                    &format!("{name}: the platform refused the stream's key or login"),
                    json!({"destination": d["id"], "error": d["error"]}),
                )),
                (_, Some("failed")) => out.push(problem(
                    "stream_failed",
                    "error",
                    &format!("{name}: {}", d["error"].as_str().unwrap_or("failed")),
                    json!({"destination": d["id"], "error": d["error"]}),
                )),
                _ => {}
            }
        }
        out
    }

    /// Its audit: every change, who made it and from where (§S.35).
    pub fn audit(&self) -> &Arc<idn_open_protocol::web::audit::Audit> {
        &self.audit
    }

    /// What checks site-signed tokens for this switcher (with a Node), for a network
    /// front ([`idn_open_protocol::web::PasswordGate::with_tokens`]).
    pub fn token_check(&self) -> Option<Arc<idn_open_protocol::web::TokenCheck>> {
        self.tokens.clone()
    }

    /// The engine.
    pub fn switcher(&self) -> &Arc<Switcher> {
        &self.sw
    }

    /// Recordings finish their last fragment and streams close (within `within`): before
    /// the program ends.
    pub fn close(&self, within: Duration) {
        #[cfg(feature = "ffmpeg")]
        self.streams.gateway.close(within);
        #[cfg(not(feature = "ffmpeg"))]
        let _ = within;
    }
}

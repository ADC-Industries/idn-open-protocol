//! The switcher's API for its inputs: added, removed, renamed, tapped, their filters,
//! cameras and PTZ, virtual shots, sound offsets, pictures.

use super::*;

impl Switcher {
    /// Adds an input (no limit on how many, §S.3); its id. A network input connects only
    /// while it is in use (in a scene on a bus, on the multiview, kept active…). A scene
    /// starts with a copy of each global layer (§S.5); the first scenes fill program and
    /// preview.
    pub fn add_input(&self, label: &str, source: Source) -> io::Result<u32> {
        let sh = &self.shared;
        if let Source::Delay { input, by_ms } = &source {
            if !sh.inputs.lock_ok().contains_key(input) {
                return Err(idn_common::err::invalid_input(format!(
                    "delay: input {input} does not exist"
                )));
            }
            if *by_ms == 0 || *by_ms > 30_000 {
                return Err(idn_common::err::invalid_input("delay: 1 to 30000 ms"));
            }
        }
        if let Source::Ptz { camera: src, .. } | Source::Virtual { input: src, .. } = &source
            && !sh.inputs.lock_ok().contains_key(src)
        {
            return Err(idn_common::err::invalid_input(format!(
                "input {src} does not exist"
            )));
        }
        if let Source::Ptz { preset, .. } = &source
            && !(1..=crate::ptz::PRESETS).contains(preset)
        {
            return Err(idn_common::err::invalid_input(format!(
                "preset: 1 to {}",
                crate::ptz::PRESETS
            )));
        }
        if let Source::Slideshow(show) = &source {
            let bad = |e: &str| Err(idn_common::err::invalid_input(format!("slideshow: {e}")));
            if show.pictures.is_empty() || show.pictures.len() > MAX_SLIDES {
                return bad(&format!("1 to {MAX_SLIDES} pictures"));
            }
            if !(0.0..=3600.0).contains(&show.every) || !(0.0..=10.0).contains(&show.fade) {
                return bad("every 0–3600 s, fade 0–10 s");
            }
        }
        let id = sh.next_id.fetch_add(1, Ordering::Relaxed) as u32;
        if let Source::Virtual { view, .. } = &source {
            let v = view.clamped();
            sh.views.lock_ok().insert(
                id,
                ViewRun {
                    from: v,
                    to: v,
                    start: Instant::now(),
                    secs: 0.0,
                },
            );
        }
        let input_cfg = match &source {
            Source::Network(name) => Some(InputConfig {
                receiver_name: sh.cfg.name.clone(),
                secure: sh.cfg.secure.clone(),
                discovery_server: sh.cfg.discovery_server.clone(),
                codec: sh.cfg.codec.clone(),
                // Small until it's used (§S.3).
                max_size: Some((640, 360)),
                // SpeedHQ as coefficients, reconstructed on the GPU (perf queue 2b);
                // `IDN_SWITCHER_COEFS=0` decodes on the CPU as before.
                coefficients: cfg!(feature = "gpu") && sh.cfg.gpu && gpu_pictures(),
                // Its own file players and lists: their pictures as they are, not
                // through a video encode and decode (a 4K60 file).
                // `IDN_SWITCHER_LOCAL=0` receives them over the network as before.
                local: idn_common::switches::on("IDN_SWITCHER_LOCAL"),
                // No more pictures than the program uses: a 4K60 SpeedHQ camera on a
                // 2160p30 program had every picture decoded.
                picture_rate: Some(sh.cfg.fps),
                // An HDR program (R.24 stage 2): HDR and 10-bit sources decoded with
                // their extra precision (the others still as coefficients).
                precise_hdr: (sh.cfg.hdr != idn_open_protocol::color::Transfer::Sdr)
                    .then_some(sh.cfg.hdr),
                // The compositor's GPU (set as it connects: `connection`): a source on this
                // machine's pictures there, and a compressed one's decoded there (§E.27).
                ..InputConfig::new(name)
            }),
            Source::Colour(_)
            | Source::Still { .. }
            | Source::Feed
            | Source::Delay { .. }
            | Source::Scene
            | Source::Title
            | Source::Slideshow(_)
            | Source::Ptz { .. }
            | Source::Virtual { .. } => None,
        };
        let slot = Arc::new(Slot {
            id,
            label: Mutex::new(label.to_owned()),
            source,
            input: Mutex::new(None),
            input_cfg,
            keep_active: AtomicBool::new(false),
            on_air_snapshot: Mutex::new(None),
            used_at: Mutex::new(None),
            use_changed: Condvar::new(),
            active: AtomicBool::new(false),
            tile_small: Mutex::new(None),
            latest: Mutex::new(None),
            raw: Mutex::new(None),
            filters: Mutex::new(None),
            ptz_capable: AtomicBool::new(false),
            decode_path: std::sync::atomic::AtomicU8::new(0),
            last_error: Mutex::new(None),
            lost: Mutex::new(None),
            seq: AtomicU64::new(0),
            fps: std::sync::atomic::AtomicU32::new(0),
            fitted: Mutex::new(None),
            connected: AtomicBool::new(false),
            running: AtomicBool::new(false),
            lane: Mutex::new(None),
            gain: std::sync::atomic::AtomicU32::new(1f32.to_bits()),
            picture_ts: std::sync::atomic::AtomicI64::new(0),
            audio_tail: Mutex::new(None),
            shown_ts: std::sync::atomic::AtomicI64::new(0),
            told: Mutex::new(None),
            tile: Mutex::new(None),
            audio_offset_ms: std::sync::atomic::AtomicU32::new(0),
            iso: Mutex::new(Vec::new()),
            iso_audio: Mutex::new(Vec::new()),
            iso_rate: std::sync::atomic::AtomicU32::new(0),
            track_lanes: Mutex::new(Vec::new()),
            monitor_lane: Mutex::new(None),
            preview_lane: Mutex::new(None),
            meter: Mutex::new(Meter::default()),
            stop: AtomicBool::new(false),
            thread: Mutex::new(None),
        });
        let fixed = match &slot.source {
            Source::Colour(c) => Some(Arc::new(Frame::solid(sh.cfg.width, sh.cfg.height, *c))),
            Source::Still { frame, .. } => Some(frame.clone()),
            // A scene starts empty: transparent, drawn each frame it is in use.
            // A title too, until its layers are set.
            Source::Scene | Source::Title => {
                Some(Arc::new(crate::scene::canvas(sh.cfg.width, sh.cfg.height)))
            }
            Source::Network(_)
            | Source::Feed
            | Source::Delay { .. }
            | Source::Slideshow(_)
            | Source::Ptz { .. }
            | Source::Virtual { .. } => None,
        };
        if let Some(f) = fixed {
            store(&slot, f, None);
            slot.connected.store(true, Ordering::Relaxed);
        } else {
            let (s2, sh2) = (slot.clone(), Arc::downgrade(sh));
            let ctl = match &slot.source {
                Source::Slideshow(show) => {
                    let ctl = Arc::new(SlideCtl::default());
                    {
                        let mut run = ctl.run.lock_ok();
                        run.order = slide_order(show.pictures.len(), show.shuffle, id as u64);
                        run.playing = show.every > 0.0;
                    }
                    sh.slides.lock_ok().insert(id, ctl.clone());
                    Some(ctl)
                }
                _ => None,
            };
            *slot.thread.lock_ok() = Some(
                std::thread::Builder::new()
                    .name("idn-switcher-in".into())
                    .spawn(move || match (&s2.source, ctl) {
                        (Source::Delay { .. }, _) => delay_worker(&s2, &sh2),
                        (Source::Ptz { .. } | Source::Virtual { .. }, _) => {
                            follow_worker(&s2, &sh2)
                        }
                        (Source::Slideshow(show), Some(ctl)) => {
                            slideshow_worker(&s2, &sh2, &ctl, show)
                        }
                        _ => connection(&s2, &sh2),
                    })?,
            );
        }
        let scene = matches!(slot.source, Source::Scene);
        {
            let mut inputs = sh.inputs.lock_ok();
            match slot.source {
                Source::Scene => {
                    // A copy of every global layer, in their order (§S.5).
                    let mut items: Vec<crate::scene::Item> = sh
                        .globals
                        .lock_ok()
                        .values()
                        .map(|def| {
                            let mut it = crate::scene::Item {
                                visible: def.visible,
                                ..Default::default()
                            };
                            crate::scene::apply_global(&mut it, def);
                            it
                        })
                        .collect();
                    crate::scene::assign_ids(&[], &mut items);
                    sh.scenes.lock_ok().insert(id, items);
                }
                Source::Title => {
                    sh.titles.lock_ok().insert(id, Default::default());
                }
                _ => {}
            }
            inputs.insert(id, slot);
        }
        let mut st = sh.state.lock_ok();
        // The first scenes fill the buses (only scenes go there, §S.3).
        if scene {
            if st.program.is_none() {
                st.program = Some(id);
            } else if st.preview.is_none() {
                st.preview = Some(id);
            }
        }
        bump(&mut st, &sh.changed);
        Ok(id)
    }

    /// Keeps an input receiving while it isn't in use (§S.3: it connects at once and
    /// stays connected), or lets it disconnect when unused (the default).
    pub fn keep_active(&self, id: u32, on: bool) -> Result<(), String> {
        let slot = self.slot(id)?;
        slot.keep_active.store(on, Ordering::Relaxed);
        if on {
            *slot.used_at.lock_ok() = Some(Instant::now());
            slot.use_changed.notify_all();
        }
        self.edit(|_| Ok(()))
    }

    /// Scene `id`'s desk snapshot recalled when it goes to program (`None`: none): desk
    /// `desk` (its source name) recalls `snapshot` (§S.27).
    pub fn scene_snapshot(&self, id: u32, to: Option<(String, String)>) -> Result<(), String> {
        let slot = self.slot(id)?;
        if !matches!(slot.source, Source::Scene) {
            return Err(format!("input {id} is not a scene"));
        }
        *slot.on_air_snapshot.lock_ok() = to;
        self.edit(|_| Ok(()))
    }

    /// Removes an input (off every bus; a scene's or global layer showing it shows
    /// nothing there).
    pub fn remove_input(&self, id: u32) -> Result<(), String> {
        let slot = self
            .shared
            .inputs
            .lock_ok()
            .remove(&id)
            .ok_or("no such input")?;
        // A scene's items go with it; scenes showing this input show nothing there.
        self.shared.scenes.lock_ok().remove(&id);
        self.shared
            .layer_fades
            .lock_ok()
            .retain(|(s, _), _| *s != id);
        self.shared.titles.lock_ok().remove(&id);
        self.shared.title_data.lock_ok().remove(&id);
        self.shared.cameras.lock_ok().remove(&id);
        self.shared.views.lock_ok().remove(&id);
        slot.stop.store(true, Ordering::Relaxed);
        slot.wake_unused();
        if let Some(ctl) = self.shared.slides.lock_ok().remove(&id) {
            ctl.wake.notify_all();
        }
        if let Some(t) = slot.thread.lock_ok().take() {
            let _ = t.join();
        }
        self.edit(|s| {
            if s.program == Some(id) {
                s.program = None;
            }
            if s.preview == Some(id) {
                s.preview = None;
            }
            for m in s.mixes.values_mut() {
                if m.program == Some(id) {
                    m.program = None;
                }
                if m.preview == Some(id) {
                    m.preview = None;
                }
            }
            if s.audio == AudioSource::Input(id) {
                s.audio = AudioSource::FollowProgram;
            }
            s.monitor.solo.retain(|&i| i != id);
            Ok(())
        })
    }

    /// Renames an input (its label in the state).
    pub fn rename_input(&self, id: u32, label: &str) -> Result<(), String> {
        let slot = self.slot(id)?;
        *slot.label.lock_ok() = label.to_owned();
        self.edit(|_| Ok(()))
    }

    /// Taps an input for ISO recording: every program frame, `f` gets that input's newest
    /// picture at full size (kept full while tapped), stamped with the program frame's
    /// timecode and timestamp — so a recording of it lines up with the program's — and its
    /// own sound since the frame before. `f` runs on a worker thread of its own (the frame
    /// clock only hands the picture over); if it falls behind, frames are skipped. Hand the
    /// frames to `idn-ffmpeg`'s gateway (`Gateway::send`) to record them. Drop the
    /// [`InputTap`] to stop.
    pub fn tap_input(
        &self,
        id: u32,
        f: impl Fn(&idn_open_protocol::api::TappedFrame) + Send + 'static,
    ) -> Result<InputTap, String> {
        let slot = self.slot(id)?;
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let tap = NEXT.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::sync_channel::<IsoItem>(8);
        let fps = self.shared.cfg.fps;
        // An input unused until now comes at its multiview tile's size until its sender
        // has the request for full size: those pictures are not recorded (the recording
        // starts at full size, not with a file of a few small pictures),
        // for 2 s at most (a source no bigger than its tile).
        let small = slot.told.lock_ok().and_then(|t| t.2);
        let since = Instant::now();
        let worker = std::thread::Builder::new()
            .name("idn-switcher-iso".into())
            .spawn(move || {
                use idn_open_protocol::shq::Fourcc;
                let mut waiting = small;
                for (pic, audio, rate, tc, ts) in rx {
                    if let Some((w, h)) = waiting {
                        if pic.width as u32 <= w
                            && pic.height as u32 <= h
                            && since.elapsed() < Duration::from_secs(2)
                        {
                            continue;
                        }
                        waiting = None;
                    }
                    // Coefficients (for the GPU) as samples; else the planes as they are.
                    let frame = if pic.deferred() {
                        pic.pixels()
                    } else {
                        (*pic).clone()
                    };
                    let alpha = frame.a.as_ref().is_some_and(|a| !a.is_empty());
                    let (w, h) = (frame.width, frame.height);
                    let tf = idn_open_protocol::api::TappedFrame::picture(
                        if alpha { Fourcc::Shq7 } else { Fourcc::Shq2 },
                        w,
                        h,
                        Arc::new(frame.into_planes()),
                        fps,
                        tc,
                        ts,
                    )
                    .with_audio(audio, rate);
                    f(&tf);
                }
            })
            .map_err(|e| e.to_string())?;
        slot.iso.lock_ok().push((tap, tx, Vec::new()));
        Ok(InputTap {
            shared: Arc::downgrade(&self.shared),
            input: id,
            id: tap,
            worker: Some(worker),
        })
    }

    /// Taps the stream's sound with extra tracks, for a multi-track recording: every program
    /// frame, `f` gets sound alone (a [`TappedFrame`](idn_open_protocol::api::TappedFrame)
    /// without a picture), stamped as the program's sound: channels 1–2 the program sound as
    /// streamed, then two per input in `inputs` — that input's own sound, through a lane at
    /// the latency it would have as the stream's, so the tracks line up with the program
    /// (within the inputs' own network timing). Stems are raw: no gain, fade or noise
    /// suppression. Up to 7 inputs. Feed `f`'s frames with the program's pictures (its
    /// sound left out) to a gateway set for `1 + inputs.len()` tracks. Drop the [`TracksTap`]
    /// to stop.
    pub fn tap_sound_tracks(
        &self,
        inputs: &[u32],
        f: impl Fn(&idn_open_protocol::api::TappedFrame) + Send + 'static,
    ) -> Result<TracksTap, String> {
        if inputs.len() > 7 {
            return Err("at most 7 input tracks".into());
        }
        let slots = inputs
            .iter()
            .map(|&i| self.slot(i))
            .collect::<Result<Vec<_>, _>>()?;
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let global = SOUND_BUFFER_MS + self.shared.state.lock_ok().delay_ms;
        let mixers: Vec<Arc<Mixer>> = slots
            .iter()
            .map(|slot| {
                let m = Mixer::new(rate(), 2, "switcher track");
                let lat = global + slot.audio_offset_ms.load(Ordering::Relaxed);
                let lane = m.lane(&LaneConfig {
                    latency_ms: lat,
                    max_correction_ppm: 1000.0,
                    // (An ISO track's sound lined up with its pictures as the program's:
                    // no step a few seconds in, lab 467-B.)
                    keep_start_level: true,
                    ..Default::default()
                });
                slot.track_lanes.lock_ok().push((id, lane, lat));
                m
            })
            .collect();
        let (tx, rx) = std::sync::mpsc::sync_channel(16);
        let worker = std::thread::Builder::new()
            .name("idn-switcher-tracks".into())
            .spawn(move || {
                for tf in rx {
                    f(&tf);
                }
            })
            .map_err(|e| e.to_string())?;
        self.shared
            .tracks
            .lock_ok()
            .push(TracksReg { id, mixers, tx });
        Ok(TracksTap {
            shared: Arc::downgrade(&self.shared),
            id,
            worker: Some(worker),
        })
    }

    /// An input's newest picture as a JPEG `width` wide (its shape kept; at most 1920), over
    /// a grey chequerboard where it is transparent: a title designer's preview.
    /// A scene's picture as a JPEG `width` wide (the program's shape), drawn now whether
    /// it is on a bus or not: a client UI's scene cards. Its inputs come into use (at a
    /// thumbnail's size) for 5 s, so asking again shows them live.
    pub fn scene_jpeg(&self, id: u32, width: usize) -> Result<Vec<u8>, String> {
        let sh = &self.shared;
        let slot = self.slot(id)?;
        if !matches!(slot.source, Source::Scene) {
            return Err(format!("input {id} is not a scene"));
        }
        sh.thumbs_wanted
            .lock_ok()
            .insert(id, Instant::now() + Duration::from_secs(5));
        let w = width.clamp(64, 1280) & !1;
        let h = ((w * sh.cfg.height.max(1)) / sh.cfg.width.max(1)).max(2) & !1;
        let defs = effective_scenes(sh);
        let inputs = sh.inputs.lock_ok().clone();
        let mut drawn = BTreeMap::new();
        let mut drawer = SceneDrawer::default();
        super::mixing::draw_scene(id, &defs, &inputs, w, h, 0, &mut drawn, &mut drawer);
        let f = drawn
            .get(&id)
            .cloned()
            .unwrap_or_else(|| Arc::new(Frame::black(w, h)));
        let p = if (f.width, f.height) == (w, h) {
            (*f).clone()
        } else {
            f.fitted(w, h)
        };
        Ok(crate::jpeg::encode(&p, 75))
    }

    pub fn input_jpeg(&self, id: u32, width: usize) -> Result<Vec<u8>, String> {
        let slot = self.slot(id)?;
        let f = slot.latest.lock_ok().clone().ok_or("no picture yet")?;
        // At most 1920 × 2160: a tall, narrow picture is fitted by its height (64 × 8192
        // at 1920 wide was 245 760 tall, 1.4 GB, and wrapped the JPEG's 16-bit height).
        let mut w = width.clamp(64, 1920) & !1;
        let mut h = ((w * f.height.max(1)) / f.width.max(1)).max(2);
        if h > 2160 {
            w = ((2160 * f.width.max(1) / f.height.max(1)).max(2)) & !1;
            h = 2160;
        }
        let mut p = f.fitted(w, h);
        if let Some(a) = p.a.take() {
            let cw = Frame::chroma_width(w);
            for y in 0..h {
                for x in 0..w {
                    let k = a[y * w + x] as u32;
                    let check = if (x / 16 + y / 16) % 2 == 0 { 90 } else { 130 };
                    let v = &mut p.y[y * w + x];
                    *v = ((*v as u32 * k + check * (255 - k) + 127) / 255) as u8;
                }
                for c in 0..cw {
                    let k = a[y * w + (2 * c).min(w - 1)] as i32;
                    for v in [&mut p.cb[y * cw + c], &mut p.cr[y * cw + c]] {
                        *v = (128 + (*v as i32 - 128) * k / 255) as u8;
                    }
                }
            }
        }
        Ok(crate::jpeg::encode(&p, 80))
    }

    /// An input's sound sync offset (ms, 0–500, OBS-style per-source sync): while its sound
    /// is the stream's (`Input`, or `FollowProgram` with it on program) it is delayed by
    /// this on top of the stream's delay, to put a camera's sound back with its picture.
    pub fn input_audio_offset(&self, id: u32, ms: u32) -> Result<(), String> {
        let slot = self.slot(id)?;
        slot.audio_offset_ms.store(ms.min(500), Ordering::Relaxed);
        self.edit(|_| Ok(()))
    }

    /// Sets an input's filters (§S.17), as a whole, in order (an empty list: none). They
    /// run on each new picture as it arrives, so the input is filtered wherever it shows; a
    /// still is filtered again at once. A LUT file or a mask picture that can't be read is
    /// an error, and the filters stay as they were.
    pub fn set_filters(&self, id: u32, items: Vec<crate::filter::Item>) -> Result<(), String> {
        let slot = self.slot(id)?;
        let loader = self.shared.loader.lock_ok().clone();
        let load = loader.as_ref().map(|l| l.as_ref() as crate::filter::Load);
        let chain = crate::filter::Chain::new(items, load)?;
        let keep = !chain.items().is_empty();
        *slot.filters.lock_ok() = keep.then(|| Arc::new(chain));
        // The last picture again, through the new filters (a still has no next one); a
        // scene is drawn again each frame anyway.
        let last = slot.raw.lock_ok().clone();
        if !matches!(slot.source, Source::Scene)
            && let Some((f, _)) = last
        {
            store(&slot, f, None);
        }
        self.edit(|_| Ok(()))
    }

    /// Sets how a camera input is moved (§S.21): over its own connection (an NDI/IDN PTZ
    /// camera) or VISCA; `None` forgets it.
    pub fn set_camera(&self, id: u32, control: Option<crate::ptz::Control>) -> Result<(), String> {
        let slot = self.slot(id)?;
        let mut cams = self.shared.cameras.lock_ok();
        match control {
            None => {
                cams.remove(&id);
            }
            Some(c) => {
                if c == crate::ptz::Control::Network && slot.input_cfg.is_none() {
                    return Err(format!(
                        "input {id} has no network connection to move it by"
                    ));
                }
                let visca = match &c {
                    crate::ptz::Control::Visca { how, address } => {
                        Some(Arc::new(crate::ptz::ViscaLink::new(*how, address)))
                    }
                    crate::ptz::Control::Network => None,
                };
                cams.insert(
                    id,
                    Camera {
                        control: c,
                        visca,
                        last_preset: None,
                        error: None,
                    },
                );
            }
        }
        drop(cams);
        self.edit(|_| Ok(()))
    }

    /// Moves a camera (§S.21): the camera input, or one of its preset inputs.
    pub fn ptz(&self, id: u32, m: crate::ptz::Move) -> Result<(), String> {
        let camera = match self.slot(id)?.source {
            Source::Ptz { camera, .. } => camera,
            _ => id,
        };
        camera_move(&self.shared, camera, m)?;
        self.edit(|_| Ok(()))
    }

    /// Moves a virtual shot to `view` over `secs` seconds (0: at once), eased in and out.
    pub fn virtual_view(&self, id: u32, view: View, secs: f32) -> Result<(), String> {
        let mut views = self.shared.views.lock_ok();
        let run = views
            .get_mut(&id)
            .ok_or_else(|| format!("input {id} is not a virtual shot"))?;
        let (now, _) = run.now();
        *run = ViewRun {
            from: now,
            to: view.clamped(),
            start: Instant::now(),
            secs: if secs.is_finite() {
                secs.clamp(0.0, 60.0)
            } else {
                0.0
            },
        };
        drop(views);
        self.edit(|_| Ok(()))
    }

    pub(super) fn slot(&self, id: u32) -> Result<Arc<Slot>, String> {
        self.shared
            .inputs
            .lock_ok()
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("no input {id}"))
    }

    /// Shows a picture this process renders on a live-feed input (one added with
    /// [`Source::Feed`]) — a key, a lower third, a source your app composes — with no
    /// network hop. Call it for each new picture; the switcher shows the newest, at the
    /// program's size (fitted like any input). `Err` if the id is not a live feed.
    pub fn feed(&self, id: u32, picture: Arc<Frame>) -> Result<(), String> {
        let slot = self.slot(id)?;
        if !matches!(slot.source, Source::Feed) {
            return Err(format!("input {id} is not a live feed"));
        }
        slot.running.store(true, Ordering::Relaxed);
        store(&slot, picture, None);
        // The first picture connects the input; the state shows that.
        if !slot.connected.swap(true, Ordering::Relaxed) {
            let mut s = self.shared.state.lock_ok();
            bump(&mut s, &self.shared.changed);
        }
        Ok(())
    }

    /// The sources on the network that aren't data feeds, by name ("HOST (name)"), to
    /// pick a network input from.
    pub fn network_sources(&self) -> Vec<String> {
        self.shared.other_sources.lock_ok().clone()
    }

    /// Every source listed on the network with its address (`IP:port`, "" unknown);
    /// `None` until the list was first read (§S.37: a source gone or renamed).
    pub fn listed_sources(&self) -> Option<Vec<(String, String)>> {
        self.shared.listed.lock_ok().clone()
    }
}

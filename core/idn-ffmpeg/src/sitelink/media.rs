//! Sources over the link: subscribing, sending one, receiving the far side's.

use super::*;

impl Session {
    pub(super) fn subscribe(self: &Arc<Self>, source: &str) {
        let refuse =
            |why: String| self.send_ctl(json!({"t": "refused", "source": source, "why": why}));
        if !self.catalogue.lock_ok().contains_key(source) {
            return refuse("not a source that crosses this link".into());
        }
        let mut subs = self.subs.lock_ok();
        if subs.contains_key(source) {
            return;
        }
        let used = self.reserved(&subs);
        if used + FIRST_RESERVE.min(MIN_VIDEO * 2) > self.limit() {
            drop(subs);
            return refuse(if used == 0 {
                format!(
                    "the link's limit ({:.0} Mbit/s) is below one stream's",
                    self.settings.limit_mbps
                )
            } else {
                format!(
                    "the link is full ({:.0} of {:.0} Mbit/s in use)",
                    used as f64 / 1e6,
                    self.settings.limit_mbps
                )
            });
        }
        let sub = Sub {
            stop: Default::default(),
            reserved: Arc::new(AtomicU64::new(FIRST_RESERVE.min(self.limit() - used))),
            keyframe: Default::default(),
            back: Default::default(),
        };
        let (stop, reserved, keyframe, back) = (
            sub.stop.clone(),
            sub.reserved.clone(),
            sub.keyframe.clone(),
            sub.back.clone(),
        );
        subs.insert(source.to_owned(), sub);
        drop(subs);
        let me = self.clone();
        let name = source.to_owned();
        std::thread::Builder::new()
            .name("idn-site-send".into())
            .spawn(move || {
                if let Err(e) = me.send_source(&name, &stop, &reserved, &keyframe, &back)
                    && !stop.load(Ordering::Relaxed)
                {
                    log(&format!("{name}: {e}"));
                    me.send_ctl(json!({"t": "refused", "source": name, "why": e.to_string()}));
                }
                let mut subs = me.subs.lock_ok();
                if subs.get(&name).is_some_and(|s| Arc::ptr_eq(&s.stop, &stop)) {
                    subs.remove(&name);
                }
            })
            .expect("thread");
    }

    /// Receives `name` here and sends it across until told to stop.
    pub(super) fn send_source(
        &self,
        name: &str,
        stop: &AtomicBool,
        reserved: &AtomicU64,
        keyframe: &AtomicBool,
        back: &Mutex<Vec<Back>>,
    ) -> io::Result<()> {
        let input = Input::open(InputConfig {
            discovery_server: self.settings.discovery_server.clone(),
            secure: Some(self.settings.secure.clone()),
            receiver_name: "site link".into(),
            queue: 4,
            ..InputConfig::new(name)
        })?;
        let header = |kind: &str| json!({"source": name, "kind": kind}).to_string();
        let (mut video, mut audio): (Option<SiteSend>, Option<SiteSend>) = (None, None);
        let open = |slot: &mut Option<SiteSend>, kind: &str| -> io::Result<()> {
            if slot.is_none() {
                let mut s = self.conn.open_uni()?;
                s.send(header(kind).as_bytes())?;
                *slot = Some(s);
            }
            Ok(())
        };
        let mut enc: Option<(Box<dyn VideoEncoder>, (usize, usize))> = None;
        let (mut enc_scale, mut period) = (100u32, Duration::from_millis(33));
        let mut aac: Option<(Vec<u8>, Box<dyn idn_open_protocol::codec::AudioDecoder>)> = None;
        // Sound as Opus once the link is tight (X.12.8); kept from then on, so it doesn't
        // switch back and forth.
        let mut opus: Option<crate::opus::OpusEncoder> = None;
        let mut sound =
            |audio: &mut Option<SiteSend>, rate: u32, ch: &[Vec<f32>]| -> io::Result<()> {
                let fits = rate == 48_000 && ch.len() == 2 && self.far_opus.load(Ordering::Relaxed);
                let tight = match self.settings.sound {
                    SoundCoding::Pcm => false,
                    SoundCoding::Opus => true,
                    SoundCoding::Auto => {
                        self.settings.limit_mbps < TIGHT_MBPS
                            || self.scale.load(Ordering::Relaxed) < 100
                    }
                };
                if fits && (opus.is_some() || tight) {
                    if opus.is_none() {
                        opus = Some(crate::opus::OpusEncoder::open()?);
                        log(&format!("{name}: sound as Opus"));
                    }
                    for p in opus.as_mut().unwrap().push(ch)? {
                        let mut m = Vec::with_capacity(p.len() + 1);
                        m.push(b'O');
                        m.extend_from_slice(&p);
                        audio.as_mut().unwrap().send(&m)?;
                    }
                    return Ok(());
                }
                audio.as_mut().unwrap().send(&audio_message(rate, ch))
            };
        let started = Instant::now();
        while !stop.load(Ordering::Relaxed) && !self.stop.load(Ordering::Relaxed) {
            // What the far receivers send the source (tally lights, PTZ).
            for b in std::mem::take(&mut *back.lock_ok()) {
                match b {
                    Back::Tally(program, preview) => input.set_tally(program, preview),
                    Back::Meta(xml) => input.send_metadata(&xml),
                }
            }
            let Some(ev) = input.recv(Duration::from_millis(200)) else {
                continue;
            };
            match ev {
                InputEvent::Video(v) => {
                    // A picture already late is dropped, not queued behind (X.12.4).
                    if v.received.elapsed() > Duration::from_millis(250) {
                        continue;
                    }
                    let p = &v.picture;
                    let size = (p.width, p.height);
                    let scale = self.scale.load(Ordering::Relaxed);
                    // Opened again for a new size, or a rate 10 points off the link's.
                    if enc
                        .as_ref()
                        .is_none_or(|e| e.1 != size || scale.abs_diff(enc_scale) >= 10)
                    {
                        let fps = (v.header.frame_rate_n.max(1), v.header.frame_rate_d.max(1));
                        let full = self.fit(size.0, size.1, reserved);
                        let floor = (1e6 * (size.0 * size.1) as f64 / (1920.0 * 1080.0))
                            .max(300_000.0) as u64;
                        let cfg = EncoderConfig {
                            codec: self.settings.codec,
                            width: size.0,
                            height: size.1,
                            frame_rate: fps,
                            bitrate: (full * scale as u64 / 100).max(floor),
                            keyframe_interval: (fps.0.saturating_mul(2) / fps.1.max(1)).max(1),
                        };
                        // H.264: the GPU's, else the CPU's (X.12.6); others: the GPU's.
                        let e = if self.settings.codec == Codec::H264 {
                            let (e, b) = self.ff.h264(&cfg)?;
                            if enc.is_none() {
                                log(&format!(
                                    "{name}: H.264 on {b:?} at {:.1} Mbit/s",
                                    cfg.bitrate as f64 / 1e6
                                ));
                            }
                            e
                        } else {
                            EncoderFactory::open(&*self.ff, &cfg).map_err(|e| {
                                io::Error::other(format!(
                                    "no GPU {} encoder here: {e}",
                                    self.settings.codec.name()
                                ))
                            })?
                        };
                        enc = Some((e, size));
                        enc_scale = scale;
                        period = Duration::from_secs_f64(fps.1 as f64 / fps.0 as f64);
                    }
                    let pts = started.elapsed().as_nanos() as i64 / 100;
                    let force = keyframe.swap(false, Ordering::Relaxed);
                    for e in enc
                        .as_mut()
                        .unwrap()
                        .0
                        .encode(&p.planes, p.fourcc, pts, force)?
                    {
                        open(&mut video, "video")?;
                        let fps = (v.header.frame_rate_n, v.header.frame_rate_d);
                        let t = Instant::now();
                        video.as_mut().unwrap().send(&video_message(
                            self.settings.codec,
                            e.keyframe,
                            size,
                            fps,
                            pts,
                            &e.extra,
                            &e.data,
                        ))?;
                        if t.elapsed() > period / 2 {
                            self.blocked.store(true, Ordering::Relaxed);
                        }
                    }
                    // Its memory back to the decoder for the next picture.
                    v.picture.recycle();
                }
                // NDI|HX: as it came.
                InputEvent::Compressed(c) => {
                    open(&mut video, "video")?;
                    let h = &c.header;
                    let size = (h.width as usize, h.height as usize);
                    video.as_mut().unwrap().send(&video_message(
                        c.codec,
                        c.keyframe,
                        size,
                        (h.frame_rate_n, h.frame_rate_d),
                        c.pts,
                        &c.extra,
                        &c.data,
                    ))?;
                }
                InputEvent::Audio(a) => {
                    open(&mut audio, "audio")?;
                    sound(&mut audio, a.header.sample_rate, &a.channels)?;
                }
                InputEvent::CompressedAudio(c) => {
                    if aac.as_ref().is_none_or(|a| a.0 != c.config) {
                        aac = Some((c.config.clone(), self.ff.open_aac(&c.config)?));
                    }
                    let channels = aac.as_mut().unwrap().1.decode(&c.data)?;
                    if !channels.is_empty() {
                        open(&mut audio, "audio")?;
                        sound(&mut audio, c.header.sample_rate, &channels)?;
                    }
                }
                InputEvent::Metadata { kind: 3, xml } => {
                    open(&mut video, "video")?;
                    let mut m = vec![b'M'];
                    m.extend_from_slice(xml.as_bytes());
                    video.as_mut().unwrap().send(&m)?;
                }
                InputEvent::State(ConnectionState::Lost(why)) => log(&format!("{name}: {why}")),
                _ => {}
            }
        }
        for s in [video, audio].iter_mut().flatten() {
            s.finish();
        }
        Ok(())
    }

    /// A stream from the far side: one source's video or sound, offered here.
    pub(super) fn receive(&self, mut r: idn_open_protocol::link::SiteRecv) {
        let Ok(Some(h)) = r.recv() else { return };
        let Ok(h) = serde_json::from_slice::<Value>(&h) else {
            return;
        };
        let name = h["source"].as_str().unwrap_or_default().to_owned();
        let Some((output, receiving)) = self
            .offers
            .lock_ok()
            .get(&name)
            .map(|o| (o.output.clone(), o.receiving.clone()))
        else {
            return;
        };
        receiving.store(true, Ordering::Relaxed);
        let mut dec: Option<(Codec, Box<dyn VideoDecoder>)> = None;
        let mut waiting_key = true;
        let mut coded: VecDeque<(i64, CodedPicture)> = VecDeque::new();
        let mut rate = (0u32, 0u32);
        let mut opus: Option<crate::ffi::AudioDec> = None;
        while let Ok(Some(m)) = r.recv() {
            self.received
                .fetch_add(m.len() as u64 + 4, Ordering::Relaxed);
            match m.first() {
                Some(b'V') => {
                    let Some(v) = parse_video(&m) else { continue };
                    if v.fps != rate && v.fps.0 > 0 && v.fps.1 > 0 {
                        rate = v.fps;
                        output.set_frame_rate(rate);
                    }
                    if dec.as_ref().is_none_or(|d| d.0 != v.codec) {
                        match DecoderFactory::open(&*self.ff, v.codec) {
                            Ok(d) => dec = Some((v.codec, d)),
                            Err(e) => {
                                log(&format!("{name}: no {} decoder: {e}", v.codec.name()));
                                continue;
                            }
                        }
                        waiting_key = true;
                    }
                    if waiting_key && !v.keyframe {
                        continue;
                    }
                    waiting_key = false;
                    coded.push_back((
                        v.pts,
                        CodedPicture {
                            codec: v.codec,
                            keyframe: v.keyframe,
                            data: v.data.to_vec(),
                            extra: v.extra.to_vec(),
                        },
                    ));
                    while coded.len() > 8 {
                        coded.pop_front();
                    }
                    let d = dec.as_mut().unwrap();
                    let mut out = match d.1.decode(v.data, v.keyframe, v.pts) {
                        Ok(p) => p,
                        Err(e) => {
                            log(&format!("{name}: {e}; asking for a keyframe"));
                            waiting_key = true;
                            self.send_ctl(json!({"t": "keyframe", "source": name}));
                            continue;
                        }
                    };
                    while let Some(p) = out {
                        let pts = p.pts.unwrap_or(v.pts);
                        let c = coded
                            .iter()
                            .position(|c| c.0 == pts)
                            .map(|i| coded[i].1.clone());
                        let _ = output.send_planes(PlanarFrame {
                            layout: idn_open_protocol::shq::Fourcc::Shq0,
                            width: p.width,
                            height: p.height,
                            planes: Arc::new(p.planes),
                            timestamp: None,
                            timecode: None,
                            audio: None,
                            coded: c,
                        });
                        out = d.1.poll().ok().flatten();
                    }
                }
                Some(b'A') => {
                    if let Some(block) = parse_audio(&m) {
                        let _ = output.send_audio(block, None);
                    }
                }
                // One Opus packet: 20 ms of stereo 48 kHz sound (X.12.8).
                Some(b'O') => {
                    if opus.is_none() {
                        match crate::opus::decoder() {
                            Ok(d) => opus = Some(d),
                            Err(e) => {
                                log(&format!("{name}: no Opus decoder: {e}"));
                                continue;
                            }
                        }
                    }
                    match opus.as_mut().unwrap().decode(&m[1..]) {
                        Ok(ch) if !ch.is_empty() => {
                            let _ = output.send_audio(
                                AudioBlock {
                                    sample_rate: 48_000,
                                    channels: ch,
                                },
                                None,
                            );
                        }
                        Ok(_) => {}
                        Err(e) => log(&format!("{name}: Opus: {e}")),
                    }
                }
                Some(b'M') => {
                    let _ = output.send_metadata(&String::from_utf8_lossy(&m[1..]));
                }
                _ => {}
            }
        }
        receiving.store(false, Ordering::Relaxed);
    }
}

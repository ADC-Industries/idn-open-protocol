//! An input's worker thread: how it starts, its loop, and what it does with each thing
//! that arrives.

use super::*;

impl Worker {
    pub(super) fn new(
        cfg: InputConfig,
        queue: Arc<Queue>,
        stop: Arc<AtomicBool>,
    ) -> io::Result<Self> {
        let limits = Limits::default();
        let (tx, rx) = mpsc::channel::<Incoming>();
        let tx = Inbox {
            tx,
            video_waiting: Arc::default(),
            audio_waiting: Arc::default(),
            video_lost: Arc::default(),
            queue: queue.clone(),
        };
        let media_epoch = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let sink: Sink = {
            let tx = Mutex::new(tx.clone());
            let epoch = media_epoch.clone();
            Arc::new(move |k, via, f| {
                let e = epoch.load(Ordering::Acquire);
                let _ = tx.lock_ok().send(Incoming::Media(k, via, f, e));
            })
        };
        let want = |t: Transport| cfg.transports.contains(&t);
        let log = |m: String| queue.push(InputEvent::Log(m));
        let sender_ip = trx::Allowed::default();
        let mut endpoints = Vec::new();
        let mut threads = Vec::new();
        let mut udp_session = None;
        let (mut udp_ids, mut quic_ids) = (None, None);
        let udp = if want(Transport::Udp) {
            let u = trx::UdpUnicast::start(0, 0, limits, sink.clone())?;
            log(format!(
                "UDP unicast on ports {} / {} (ids {} / {})",
                u.video.0, u.audio.0, u.video.1, u.audio.1
            ));
            let adv = (u.video, u.audio);
            udp_session = Some(u.generation.clone());
            udp_ids = Some(u.ids.clone());
            endpoints.push(u.endpoint);
            Some(adv)
        } else {
            None
        };
        let tcp = if want(Transport::Tcp) {
            let t = trx::MultiTcp::start(0, 0, limits, sender_ip.clone(), sink.clone())?;
            log(format!(
                "multi-TCP on ports {} / {}",
                t.video_port, t.audio_port
            ));
            let adv = (t.video_port, t.audio_port);
            endpoints.push(t.endpoint);
            Some(adv)
        } else {
            None
        };
        let quic = if want(Transport::Quic) {
            let vid = u32::from_le_bytes(crate::random::secure_random::<4>());
            let aid = u32::from_le_bytes(crate::random::secure_random::<4>());
            let mut server = Server::bind(
                SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), cfg.quic_port),
                vid,
                aid,
                limits,
            )?;
            let port = server.local_addr()?.port();
            log(format!(
                "QUIC media endpoint on UDP port {port} (video id {vid}, audio id {aid})"
            ));
            quic_ids = Some(server.ids());
            let (tx, stop, epoch) = (tx.clone(), stop.clone(), media_epoch.clone());
            let events = queue.clone();
            threads.push(std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    // The endpoint is gone and will never deliver again: end this thread
                    // (the input carries on over its other transports).
                    if server.ended() {
                        events.push(InputEvent::Log(
                            "QUIC media endpoint ended; no more QUIC media".into(),
                        ));
                        return;
                    }
                    for e in server.poll() {
                        // Media marked with the session they came in under (F5-1).
                        let m = match e {
                            QEvent::Message(k, f) => {
                                Incoming::Media(k, "quic", Ok(f), epoch.load(Ordering::Acquire))
                            }
                            e => Incoming::Quic(e),
                        };
                        if tx.send(m).is_err() {
                            return;
                        }
                    }
                }
                server.close_all();
            }));
            Some((port, vid, aid))
        } else {
            None
        };
        // Omitting the shq_* attributes asks for the legacy SpeedHQ format (§1.5).
        let streams = control::EnabledStreams {
            video: cfg.receive_video,
            audio: true,
            text: true,
            shq_new: !cfg.legacy,
            multicast: want(Transport::Multicast),
            udp,
            tcp,
            quic,
            x_idn: if cfg.wants_lossless() {
                vec![("x_idn_audio".into(), "f32".into())]
            } else {
                Vec::new()
            },
        };
        let inline_streams_xml = control::EnabledStreams {
            video: cfg.receive_video,
            audio: true,
            text: true,
            shq_new: !cfg.legacy,
            x_idn: if cfg.wants_lossless() {
                vec![("x_idn_audio".into(), "f32".into())]
            } else {
                Vec::new()
            },
            ..Default::default()
        }
        .to_xml();
        // On a link: no transports, except multicast if this input takes it (§E.11.5);
        // one stream per frame if asked for (§E.13.1).
        let link_streams_xml = control::EnabledStreams {
            video: cfg.receive_video,
            audio: true,
            text: true,
            shq_new: !cfg.legacy,
            multicast: want(Transport::Multicast),
            x_idn: [
                cfg.wants_lossless()
                    .then(|| ("x_idn_audio".to_owned(), "f32".to_owned())),
                cfg.frame_streams
                    .then(|| ("x_idn_frames".to_owned(), "1".to_owned())),
                // Encrypted multicast is taken where offered (§E.17.2).
                want(Transport::Multicast).then(|| ("x_idn_mc".to_owned(), "1".to_owned())),
            ]
            .into_iter()
            .flatten()
            .collect(),
            ..Default::default()
        }
        .to_xml();
        // The test switch's plain NDI receiver has no link (§E.18.8).
        let plain_ndi = cfg.secure.is_none() && crate::ext::test_plain_ndi();
        let link_cfg =
            (want(Transport::Link) && !plain_ndi).then(|| cfg.link.clone().unwrap_or_default());
        let cfg_sync = cfg.sync.clone().map(crate::sync::Receiver::new);
        // Started on the first encrypted session, with that session's credentials.
        let squic: Option<crate::squic::SquicServer> = None;
        let site_seen_enrolled = cfg
            .secure
            .as_ref()
            .and_then(|s| s.site.as_ref())
            .is_some_and(|s| s.ca_id().is_some());
        let full_name = format!("{} ({})", hostname(), cfg.receiver_name);
        // Monitoring only: no `connect` command, so the app keeps control of its input.
        let monitor = match (&cfg.discovery_server, cfg.monitoring) {
            (Some(sv), true) => match crate::discovery::ds::server_addr(sv) {
                Ok(a) => Some(crate::discovery::ds::Advertiser::receiver(
                    a,
                    &full_name,
                    "video",
                    crate::discovery::ds::IDN_RECEIVER_PROPERTIES,
                    false,
                )),
                Err(e) => {
                    log(format!("monitoring off: discovery server {sv}: {e}"));
                    None
                }
            },
            _ => None,
        };
        let quality = if cfg.low_bandwidth { "low" } else { "high" };
        Ok(Worker {
            thin: cfg.picture_rate.and_then(crate::thin::Thin::new),
            cfg,
            queue,
            stop,
            tx,
            rx,
            sink,
            endpoints,
            udp_session,
            sender_ip,
            media_epoch,
            multicast: None,
            tally: (false, false),
            pending_lost: None,
            peer_name: None,
            expect_name: None,
            router_gone: None,
            routed_at: None,
            followed_again: false,
            threads,
            streams,
            stream_ids: [udp_ids, quic_ids],
            inline_streams_xml,
            squic,
            secure_session: false,
            require_checked: Instant::now(),
            squic_offered: false,
            fallback_reported: false,
            monitor,
            monitor_at: Instant::now(),
            monitor_video: 0,
            last_video: None,
            last_audio: None,
            site_seen_enrolled,
            session_started: Instant::now(),
            session_media: false,
            delivered: None,
            quick_drops: 0,
            full_name,
            quality,
            conns: BTreeMap::new(),
            next_conn: 0,
            active: 0,
            quic_peer: None,
            routing: None,
            ever_connected: false,
            last_failure: None,
            hx_need_key: true,
            hx_last_counter: None,
            hx_wait: Some(HX_WAIT_NEW),
            ext: Default::default(),
            peer_instance: None,
            resume_from: None,
            last_addr: None,
            bye: None,
            fast_until: None,
            lost_at: None,
            next_fast: Instant::now(),
            last_report: Instant::now(),
            report_base: [0; 5],
            next_ping: Instant::now(),
            ping_id: 0,
            started: Instant::now(),
            dec_time: 0.0,
            dec_n: 0,
            decode_cpu: Duration::ZERO,
            #[cfg(feature = "gpu")]
            next_gpu: None,
            #[cfg(feature = "gpu")]
            uploads: None,
            codec_meta: VecDeque::new(),
            ahead: Ahead::new(),
            dec_color: None,
            dec_color_logged: None,
            machine: None,
            machine_epoch: 0,
            last_via: "inline",
            color: None,
            hdr_seen: false,
            hdr_match: false,
            resolving: false,
            skip_second_field: false,
            regions_peer: false,
            reference: None,
            refresh_asked: None,
            mc_announcement: None,
            mc_keys: None,
            mc_seq: Default::default(),
            aac: None,
            decoder: None,
            link_cfg,
            link_hint: None,
            link_failed: BTreeMap::new(),
            link_suspect: BTreeMap::new(),
            link: None,
            link_probe: None,
            link_late_seen: 0,
            sync: cfg_sync,
            link_peer_key: None,
            last_upgrade: None,
            retiring: None,
            link_streams_xml,
            channels_requested: None,
            size_requested: None,
        })
    }

    pub(super) fn emit(&self, e: InputEvent) {
        self.queue.push(e);
    }

    /// Hands a decoded picture or audio block over: at once, or with sync at its
    /// presentation time (§E.14.5).
    pub(super) fn present(
        &mut self,
        e: InputEvent,
        video: bool,
        timestamp: i64,
        rate: Option<(u32, u32)>,
        field: u32,
        received: Instant,
    ) {
        match &mut self.sync {
            None => self.emit(e),
            Some(s) => {
                if let Some(e) = s.schedule(e, video, timestamp, rate, field, received) {
                    for h in s.take_flushed() {
                        self.emit(h);
                    }
                    self.emit(e);
                }
            }
        }
    }

    pub(super) fn log(&self, m: impl Into<String>) {
        self.emit(InputEvent::Log(m.into()));
    }

    pub(super) fn stat(&self, f: impl FnOnce(&mut InputStats)) {
        f(&mut self.queue.stats.lock_ok());
    }

    /// Asks the sender for a keyframe (§7.1.4), once per discarded or undecodable frame as
    /// an NDI 6.3 receiver does, logging why the wait began with the first request.
    pub(super) fn ask_keyframe(&mut self) {
        if let Some(why) = self.hx_wait.take() {
            self.log(format!(
                "<- video: waiting for a keyframe ({why}); asking the sender"
            ));
        }
        self.stat(|s| s.hx_key_requests += 1);
        self.send_active(kind::METADATA, r#"<ndi_video request_keyframe="true"/>"#);
    }

    pub(super) fn send_active(&mut self, k: u16, xml: &str) {
        if let Some(s) = self.conns.get_mut(&self.active) {
            let _ = send_text(s, k, xml);
        }
    }

    pub(super) fn run(mut self, cmds: Receiver<Command>) {
        self.emit(InputEvent::State(ConnectionState::Connecting));
        self.start_resolve(Duration::from_secs(5));
        let mut retry_at: Option<Instant> = None;
        while !self.stop.load(Ordering::Relaxed) {
            self.tick(&mut retry_at);
            if retry_at.is_some_and(|t| Instant::now() >= t) && self.conns.is_empty() {
                retry_at = None;
                self.start_resolve(Duration::from_secs(2));
            }
            while let Ok(c) = cmds.try_recv() {
                let is_metadata = matches!(c, Command::Metadata(_));
                let (k, xml) = match c {
                    Command::Tally(p, v) => {
                        self.tally = (p, v);
                        (kind::CONTROL, control::ndi_tally(p, v))
                    }
                    Command::Metadata(x) => (kind::METADATA, x),
                    Command::LowBandwidth(low) => {
                        self.quality = if low { "low" } else { "high" };
                        // On multicast the receiver picks the group itself (R1-37).
                        if self.multicast.is_some()
                            && let Some(a) = self.mc_announcement.clone()
                        {
                            self.join_multicast(&a);
                        }
                        (kind::CONTROL, control::ndi_video(self.quality))
                    }
                    Command::ParallelDecode(on) => {
                        self.cfg.parallel_decode = Some(on);
                        continue;
                    }
                    Command::Coefficients(on) => {
                        self.cfg.coefficients = on;
                        continue;
                    }
                    #[cfg(feature = "gpu")]
                    Command::GpuOff => {
                        if self.cfg.gpu.take().is_some() {
                            self.log(
                                "the application's GPU is gone: pictures in memory from now on",
                            );
                            self.decoder = None;
                            self.end_session();
                        }
                        continue;
                    }
                    Command::AudioChannels(list) => {
                        self.cfg.audio_channels = list;
                        // An extension message: IDN sources only (§E.21.1).
                        if self.ext.names().is_empty() {
                            continue;
                        }
                        let list = self
                            .cfg
                            .audio_channels
                            .as_deref()
                            .map(crate::ext::channel_list)
                            .unwrap_or_default();
                        self.channels_requested = self.cfg.audio_channels.clone();
                        (kind::CONTROL, format!(r#"<x_idn_channels list="{list}"/>"#))
                    }
                    Command::MaxSize(size) => {
                        self.cfg.max_size = size;
                        // An extension message: IDN sources only (§E.3).
                        if self.ext.names().is_empty() {
                            continue;
                        }
                        self.size_requested = size;
                        let (w, h) = size.unwrap_or((0, 0));
                        let xml = format!(r#"<x_idn_size width="{w}" height="{h}"/>"#);
                        (kind::CONTROL, xml)
                    }
                };
                match self.conns.get_mut(&self.active) {
                    Some(s) => {
                        let _ = send_text(s, k, &xml);
                    }
                    // Not queued for later (a stale PTZ move is worse than none), but the
                    // application hears about it (IN-9).
                    None if is_metadata => self.emit(InputEvent::Error(
                        "metadata not sent: not connected to the source".into(),
                    )),
                    None => {}
                }
            }
            self.look_for_router();
            // With sync, wake up when the next held frame is due (§E.14.5); likewise for
            // multicast messages waiting for a re-sent one (§E.17.6).
            self.release_due();
            self.expire_mc();
            let wait = self
                .sync
                .as_ref()
                .and_then(|s| s.next_due())
                .into_iter()
                .chain(self.mc_seq.iter().filter_map(|q| q.next_deadline()))
                .min()
                .map_or(Duration::from_millis(50), |at| {
                    at.saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(50))
                })
                // A pipelined decoder's pictures are collected as they finish: while
                // access units given to it within the last second still wait for their
                // pictures. An idle decoder (or one holding a stream's last pictures until
                // more input comes) is not looked at every 5 ms.
                .min(
                    if self.decoder.is_some()
                        && self
                            .codec_meta
                            .back()
                            .is_some_and(|m| m.3.elapsed() < Duration::from_secs(1))
                    {
                        Duration::from_millis(5)
                    } else {
                        Duration::from_millis(50)
                    },
                );
            let msg = match self.rx.recv_timeout(wait) {
                Ok(Incoming::Resolved(r)) => {
                    if !self.on_resolved(r, &mut retry_at) {
                        break;
                    }
                    continue;
                }
                Ok(Incoming::Upgrade(addr, r)) => {
                    self.on_upgrade(addr, *r);
                    continue;
                }
                Ok(Incoming::Decoded) => {
                    self.collect_ahead(false);
                    continue;
                }
                Ok(Incoming::Machine(epoch, sound, seq)) => {
                    if !sound {
                        self.tx.video_waiting.fetch_sub(1, Ordering::Relaxed);
                    }
                    self.machine_item(epoch, sound, seq);
                    continue;
                }
                Ok(Incoming::RouterLook(None)) => continue,
                Ok(Incoming::RouterLook(Some(_))) => {
                    // Through the routing source again: the normal reconnect, at once.
                    if self.router_gone.is_some() {
                        self.followed_again = true;
                        self.log("the routing source is back; following it again".to_owned());
                        self.end_session();
                        self.emit(InputEvent::State(ConnectionState::Lost(
                            "following the routing source again".into(),
                        )));
                        retry_at = Some(Instant::now());
                    }
                    continue;
                }
                Ok(m) => {
                    if m.is_video() {
                        self.tx.video_waiting.fetch_sub(1, Ordering::Relaxed);
                    }
                    if m.is_audio() {
                        self.tx.audio_waiting.fetch_sub(1, Ordering::Relaxed);
                    }
                    m
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            };
            if let Some(lost) = self.handle(msg) {
                self.emit(InputEvent::State(ConnectionState::Lost(lost)));
                if !self.cfg.reconnect {
                    break;
                }
                self.lost_at = Some(Instant::now());
                retry_at = Some(Instant::now() + Duration::from_secs(1));
                // Closed before any media, several times running: the source is refusing
                // us (it holds a refused connection about 2 s first, §E.8.5). Back off
                // (2 s, doubling to 30 s) instead of retrying at once.
                if !self.session_media && self.session_started.elapsed() < REFUSAL_WINDOW {
                    self.quick_drops += 1;
                } else {
                    self.quick_drops = 0;
                }
                if self.quick_drops >= 3 {
                    let wait = Duration::from_secs(2u64 << (self.quick_drops - 3).min(4))
                        .min(Duration::from_secs(30));
                    self.log(format!(
                        "the source closes the connection at once (refused?); retrying in {} s",
                        wait.as_secs()
                    ));
                    self.fast_until = None;
                    retry_at = Some(Instant::now() + wait);
                } else if self.ext.has("resume") {
                    // resume/1: retry the last address every 250 ms for 10 s, from
                    // `retry_ms` after a planned restart; not after a shutdown (§E.4).
                    let start = match self.bye {
                        Some(crate::ext::Departure::Shutdown) => None,
                        Some(
                            crate::ext::Departure::Restart { retry_ms }
                            | crate::ext::Departure::Config { retry_ms },
                        ) => Some(Duration::from_millis(retry_ms as u64)),
                        None => Some(Duration::ZERO),
                    };
                    if let Some(d) = start {
                        let now = Instant::now();
                        self.next_fast = now + d;
                        self.fast_until = Some(now + d + Duration::from_secs(10));
                    }
                }
            }
        }
        self.finish();
    }

    /// Processes one incoming item. Returns `Some(reason)` when the active source is lost.
    pub(super) fn handle(&mut self, msg: Incoming) -> Option<String> {
        let mut from_conn = None;
        let (f, via) = match msg {
            // A connection already closed (session ended, routing switched) can still
            // deliver frames its reader had read: they belong to no session now.
            Incoming::Tcp(c, _) if !self.conns.contains_key(&c) => return None,
            Incoming::Tcp(c, Ok(Some(f))) => {
                from_conn = Some(c);
                (f, "control")
            }
            Incoming::Tcp(c, r) => {
                self.conns.remove(&c);
                if c == self.active {
                    // Also for a routed session: the routing source re-sends the route
                    // only when it changes, so start again from it (§1.8).
                    // On a link the control stream ends when the connection does, for
                    // whatever reason: say which (a liveness timeout isn't to be
                    // reported as a close).
                    let link_reason = self.link.as_ref().and_then(|l| l.close_reason());
                    self.end_session();
                    return Some(match (r, link_reason) {
                        (_, Some(why)) => why,
                        (Ok(_), None) => "sender closed the connection".into(),
                        (Err(e), None) => format!("control connection error: {e}"),
                    });
                }
                if Some(c) == self.routing {
                    // The routing source left; the target stays, and the source is looked
                    // for again (a routing source that comes back is followed again).
                    self.routing = None;
                    let quick = self.followed_again
                        && self.routed_at.is_some_and(|t| t.elapsed() < ROUTER_STEADY);
                    if quick {
                        self.log(
                            "the routing source closes its connections: staying on its target"
                                .to_owned(),
                        );
                        self.router_gone = None;
                    } else {
                        self.followed_again = false;
                        self.router_gone = Some(Instant::now() + ROUTER_LOOK);
                    }
                }
                return None;
            }
            Incoming::Quic(QEvent::Message(_, f)) => (f, "quic"),
            // Queued for a session that has ended (a source moved, a loss): not this
            // one's, whatever its ids said then (F5-1).
            Incoming::Media(.., e) if e != self.media_epoch.load(Ordering::Relaxed) => {
                return None;
            }
            Incoming::Media(_, via, Ok(f), _) => (f, via),
            Incoming::Media(_, crate::squic::WRONG_TOKEN, Err(_), _) => {
                // Someone with the site's credentials but not this session's media token
                // (§E.8.4): end the session; the reconnect starts a new one.
                self.emit(InputEvent::Error(
                    "secure QUIC: media with a wrong session token (possible attack): session closed".into(),
                ));
                return Some("secure QUIC media with a wrong session token".into());
            }
            Incoming::Media(k, via, Err(e), _) => {
                self.emit(InputEvent::Error(format!("{via}: bad {k:?} message: {e}")));
                return None;
            }
            Incoming::Quic(QEvent::Connected(a)) => {
                self.log(format!("<- quic: sender connected from {a}"));
                self.quic_peer = Some(a);
                return None;
            }
            Incoming::Quic(QEvent::StreamError(k, e)) => {
                self.emit(InputEvent::Error(format!("quic {k:?} stream error: {e}")));
                return None;
            }
            Incoming::Quic(QEvent::Closed(a, r)) => {
                self.log(format!("<- quic: connection from {a} closed: {r}"));
                // The current session's media went silent for the idle timeout: the
                // source is gone even if its control connection hasn't noticed yet.
                // (Older connections closing after a reconnect are not the session's.)
                if self.quic_peer == Some(a) && r == "idle timeout" {
                    // End the control connections too, so the reconnect can start (it
                    // waits for no open connections); their readers' closing events then
                    // no longer belong to the active session.
                    self.end_session();
                    return Some(format!("QUIC media from {a} stopped ({r})"));
                }
                return None;
            }
            // Handled in run().
            Incoming::Resolved(_)
            | Incoming::Upgrade(..)
            | Incoming::Decoded
            | Incoming::RouterLook(_)
            | Incoming::Machine(..) => return None,
        };
        if from_conn.is_some_and(|c| Some(c) == self.routing)
            && matches!(f.kind, kind::VIDEO | kind::AUDIO)
        {
            return None; // the routing source's placeholder (§1.8)
        }
        if f.kind == crate::mcast::MESSAGE_KIND {
            self.on_mc_message(f, via);
            return None;
        }
        // Media transports carry pictures and sound only (§2.3, chapter 6). A control or
        // metadata frame on one (QUIC, UDP, multi-TCP, multicast) came from whoever knows
        // this input's ids, not the session: it could have re-routed or reset the input.
        if !matches!(f.kind, kind::VIDEO | kind::AUDIO)
            && !matches!(via, "control" | "link" | "squic")
        {
            return None;
        }
        let what = match f.kind {
            kind::VIDEO => "video",
            kind::AUDIO => "audio",
            _ => "control/metadata",
        };
        if self.secure_session && matches!(f.kind, kind::VIDEO | kind::AUDIO) {
            // Encrypted sessions take media only from secure QUIC or the TLS control
            // connection; the plain endpoints stay open for plain sessions (§E.8.4). A
            // link session takes it from the link, or from multicast if this input chose
            // it (§E.11.5).
            let expected = if self.link.is_some() {
                via == "link" || via == "multicast"
            } else {
                via == "squic" || via == "control"
            };
            if !expected {
                return None;
            }
            if via == "control" && self.squic_offered && !self.fallback_reported {
                self.fallback_reported = true;
                self.stat(|s| s.secure_fallback = true);
                self.emit(InputEvent::Error(
                    "encrypted media on the control connection: secure QUIC unavailable (UDP blocked?)".into(),
                ));
            }
        }
        if via == "link" && self.retiring.is_some() {
            self.retire_old_session(); // the link carries the media now
        }
        self.stat(|s| *s.via.entry(format!("{what} via {via}")).or_default() += 1);
        let received = Instant::now();
        match f.kind {
            kind::CONTROL | kind::METADATA => {
                self.on_text(&f, from_conn);
                if let Some(r) = self.pending_lost.take() {
                    return Some(r);
                }
            }
            kind::VIDEO => self.on_video(f, via, received),
            kind::AUDIO => self.on_audio(f, via, received),
            k => self.log(format!(
                "<- unknown kind {k} ({} + {} bytes), ignored",
                f.ext.len(),
                f.payload.len()
            )),
        }
        None
    }
}

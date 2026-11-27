//! An input's connection: finding the source, the link and its policy, sessions (opened,
//! retired, timed out, upgraded), the router, and the periodic tick that keeps them.

use super::*;

impl Worker {
    /// Hands over the held frames whose presentation time has come.
    pub(super) fn release_due(&mut self) {
        if let Some(s) = &mut self.sync {
            for e in s.due() {
                self.queue.push(e);
            }
        }
    }

    /// Opens a control connection and performs the receiver's side of §1.4.
    pub(super) fn open_session(&mut self, addr: SocketAddr) -> io::Result<()> {
        self.open_session_timeout(addr, Duration::from_secs(5))
    }

    /// The policy checks of a link session (§E.11.4): sticky secure and the key memory in
    /// mode `open`, sticky records and revocation tracking in modes `s1`/`s2`.
    pub(super) fn link_policy(
        &mut self,
        l: &crate::link::LinkClient,
        sock: &TcpStream,
    ) -> io::Result<()> {
        let source = self.cfg.source.clone();
        match l.mode.creds() {
            None => {
                if self
                    .cfg
                    .secure
                    .as_ref()
                    .is_some_and(|sc| sc.sticky_refuses(&source))
                {
                    l.close(crate::link::code::REFUSED, "sticky");
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "this source used encryption before; refusing an unauthenticated link (reset in settings if it changed)",
                    ));
                }
                // Checked by the name the source announces (`ndi_identify`), not by
                // the address it was reached at.
                self.link_peer_key = Some(l.peer_fingerprint.clone());
                self.stat(|s| s.key_changed = false);
            }
            Some(c) => {
                if let Some(sc) = &self.cfg.secure {
                    crate::secure::remember_peer(sc, c, &source);
                }
                if let crate::secure::Creds::Device(_, e) = c {
                    e.track_session(&l.peer_cert, sock, true)?; // a revocation ends it (§E.10.6)
                }
                self.stat(|s| s.key_changed = false);
            }
        }
        Ok(())
    }

    /// A sink for one link's media, and the flag that ends it. The input's shared sink
    /// stamps what arrives with the session number of that moment; a link's reader that
    /// handed over one more picture just after its session was replaced (a router
    /// switched to another source) got the new session's number, and the picture was
    /// shown after the new source's first: 1 switch in 30, 10 in 30 on a busy machine
    /// (lab 497-C). Here the number is taken first and the flag looked at after: the
    /// flag is set before the number moves on ([`Self::end_link`]), so what a reader
    /// hands over late either has the old number (dropped where it is taken) or finds the
    /// flag set (dropped here).
    pub(super) fn link_sink(&self) -> (Sink, Arc<AtomicBool>) {
        let ended = Arc::new(AtomicBool::new(false));
        let (tx, epoch, flag) = (
            Mutex::new(self.tx.clone()),
            self.media_epoch.clone(),
            ended.clone(),
        );
        let sink: Sink = Arc::new(move |k, via, f| {
            let e = epoch.load(Ordering::Acquire);
            if flag.load(Ordering::Acquire) {
                return;
            }
            let _ = tx.lock_ok().send(Incoming::Media(k, via, f, e));
        });
        (sink, ended)
    }

    /// The session's link let go: its media from here on belongs to no session. Before
    /// the session number moves on (see [`Self::link_sink`]).
    pub(super) fn end_link(&mut self) {
        if let Some(l) = self.link.take() {
            l.ended.store(true, Ordering::Release);
        }
    }

    /// In a TCP session with a sender that offers the link, tries the link in the
    /// background (§E.11.3), at most every [`UPGRADE_INTERVAL`] and not towards a sender
    /// whose UDP looks blocked.
    pub(super) fn try_upgrade(&mut self) {
        let Some(addr) = self.last_addr else { return };
        // A remembered link failure doesn't stop this: it only spares reconnects the
        // QUIC head start. The background try every UPGRADE_INTERVAL is how a source
        // wrongly taken as blocked (a reconnect racing a cable plugged back in) comes
        // back to the link.
        if self.link.is_some()
            || self.routing.is_some()
            || self
                .last_upgrade
                .is_some_and(|t| t.elapsed() < UPGRADE_INTERVAL)
        {
            return;
        }
        let Some(lc) = self.link_cfg.clone() else {
            return;
        };
        self.last_upgrade = Some(Instant::now());
        let (tx, secure) = (self.tx.clone(), self.cfg.secure.clone());
        let (sink, ended) = self.link_sink();
        std::thread::spawn(move || {
            let r = crate::link::connect(
                addr,
                &lc,
                secure.as_ref(),
                Limits::default(),
                sink,
                Duration::from_secs(2),
            )
            .map(|mut l| {
                l.ended = ended;
                l
            });
            let _ = tx.send(Incoming::Upgrade(addr, Box::new(r)));
        });
    }

    /// A background link attempt finished. If the TCP session with that sender is still
    /// the current one, the session moves to the link; the TCP session is closed once
    /// media arrives on the link, or after a second.
    pub(super) fn on_upgrade(&mut self, addr: SocketAddr, r: io::Result<crate::link::LinkClient>) {
        let still = self.link.is_none() && self.active != 0 && self.last_addr == Some(addr);
        match r {
            Ok(l) if still => {
                let old = self.active;
                let control = match l.control.try_clone() {
                    Ok(c) => c,
                    Err(_) => return,
                };
                match self.start_session(addr, control, Some(l)) {
                    Ok(()) => {
                        self.log(format!("moved the session with {addr} to the IDN link"));
                        self.retiring = Some((old, Instant::now() + Duration::from_secs(1)));
                    }
                    // The session was half moved by then (its ids renewed, multicast
                    // left, QUIC stopped): ended, and the reconnect starts a whole one
                    // (bug round F, F5-2; "the TCP session goes on" kept a broken one).
                    Err(e) => {
                        self.active = old;
                        self.log(format!("could not move to the IDN link: {e}; reconnecting"));
                        self.end_session();
                        self.pending_lost = Some(format!("moving to the IDN link failed: {e}"));
                    }
                }
            }
            Ok(_) => {} // the session changed meanwhile: the spare link closes when dropped
            Err(e) => {
                if still {
                    self.link_unreachable(addr, &e);
                } else {
                    self.log(format!("link not used: {e}"));
                }
            }
        }
    }

    /// Closes the TCP session left behind by a move to the link.
    pub(super) fn retire_old_session(&mut self) {
        if let Some((id, _)) = self.retiring.take()
            && let Some(c) = self.conns.remove(&id)
        {
            let _ = c.shutdown(std::net::Shutdown::Both);
        }
    }

    /// Connects to the sender: over the IDN link if it may offer one, else TCP. QUIC gets
    /// a head start, then TCP is tried too; whichever completes first carries the session
    /// (§E.11.3).
    pub(super) fn connect_control(
        &mut self,
        addr: SocketAddr,
        timeout: Duration,
    ) -> io::Result<(TcpStream, Option<crate::link::LinkClient>)> {
        self.check_link_probe();
        let recently_failed = self
            .link_failed
            .get(&addr)
            .is_some_and(|t| t.elapsed() < LINK_FAILURE_MEMORY);
        let Some(lc) = self
            .link_cfg
            .clone()
            .filter(|_| self.link_hint != Some(false) && !recently_failed)
        else {
            return Ok((TcpStream::connect_timeout(&addr, timeout)?, None));
        };
        let (tx, rx) = mpsc::channel();
        let decided = Arc::new(AtomicBool::new(false));
        {
            let (tx, secure) = (tx.clone(), self.cfg.secure.clone());
            let (sink, ended) = self.link_sink();
            std::thread::spawn(move || {
                let r = crate::link::connect(
                    addr,
                    &lc,
                    secure.as_ref(),
                    Limits::default(),
                    sink,
                    timeout,
                )
                .map(|mut l| {
                    l.ended = ended;
                    l
                });
                let _ = tx.send(Attempt::Link(r));
            });
        }
        {
            let decided = decided.clone();
            std::thread::spawn(move || {
                std::thread::sleep(crate::link::QUIC_HEAD_START);
                if !decided.load(Ordering::Relaxed) {
                    let _ = tx.send(Attempt::Tcp(TcpStream::connect_timeout(&addr, timeout)));
                }
            });
        }
        let end =
            Instant::now() + timeout + crate::link::QUIC_HEAD_START + Duration::from_millis(500);
        let (mut link_err, mut tcp_err) = (None::<io::Error>, None);
        while link_err.is_none() || tcp_err.is_none() {
            let left = end.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(Attempt::Link(Ok(l))) => {
                    decided.store(true, Ordering::Relaxed);
                    self.link_failed.remove(&addr);
                    return Ok((l.control.try_clone()?, Some(l)));
                }
                Ok(Attempt::Tcp(Ok(t))) => {
                    decided.store(true, Ordering::Relaxed);
                    match link_err {
                        Some(e) => self.link_unreachable(addr, &e),
                        // Still trying: its outcome is checked later.
                        None => self.link_probe = Some((addr, rx)),
                    }
                    return Ok((t, None));
                }
                Ok(Attempt::Link(Err(e))) => {
                    // Authentication failed in mode s1/s2: a possible attack, no
                    // fallback for this session (§E.11.8).
                    if e.kind() == io::ErrorKind::PermissionDenied {
                        decided.store(true, Ordering::Relaxed);
                        return Err(e);
                    }
                    link_err = Some(e);
                }
                Ok(Attempt::Tcp(Err(e))) => tcp_err = Some(e),
                Err(_) => break,
            }
        }
        decided.store(true, Ordering::Relaxed);
        Err(tcp_err
            .or(link_err)
            .unwrap_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "connecting timed out")))
    }

    /// The outcome of a link attempt that lost the race to TCP: if it failed, UDP is
    /// blocked towards that sender; if it succeeded, the session moves to it (§E.11.3).
    pub(super) fn check_link_probe(&mut self) {
        let Some((addr, rx)) = &self.link_probe else {
            return;
        };
        let addr = *addr;
        match rx.try_recv() {
            Ok(Attempt::Link(r)) => {
                self.link_probe = None;
                self.last_upgrade = Some(Instant::now());
                self.on_upgrade(addr, r);
            }
            Err(mpsc::TryRecvError::Disconnected) => self.link_probe = None,
            Ok(Attempt::Tcp(_)) | Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    /// The link failed where TCP works. Only no answer at all means UDP is blocked on
    /// the way (remembered, §E.11.8); a sender that answered and then turned the link
    /// down (no common mode, a refusal, an authentication failure) is reachable over
    /// UDP, so that is not remembered and the next attempt tries the link again (a wrong
    /// passphrase was read as blocked UDP for 10 min).
    ///
    /// One unanswered attempt is not yet "blocked": a reconnect racing a cable plugged
    /// back in timed out once and pinned the source to TCP for 10 min. The
    /// second unanswered attempt within the memory period is remembered.
    pub(super) fn link_unreachable(&mut self, addr: SocketAddr, e: &io::Error) {
        if e.kind() != io::ErrorKind::TimedOut {
            // TLS alert 120 (no_application_protocol): none of the link modes offered is
            // one the source takes; a source in a site takes no unauthenticated link
            // (§E.11.4), and says why on the TCP session that follows.
            let why = if e.to_string().contains("error 120") {
                " (the source takes none of the link modes this input offers: it may be in a site this input is not a member of)"
            } else {
                ""
            };
            self.log(format!("{e}{why}; using TCP for this session"));
            return;
        }
        let suspect = self
            .link_suspect
            .insert(addr, Instant::now())
            .is_some_and(|t| t.elapsed() < LINK_FAILURE_MEMORY);
        if suspect {
            self.log(format!(
                "{e}; TCP works: UDP to {addr} looks blocked, using TCP for this source for {} min",
                LINK_FAILURE_MEMORY.as_secs() / 60
            ));
            self.link_failed.insert(addr, Instant::now());
        } else {
            self.log(format!(
                "{e}; TCP works: using TCP for this session, trying the link again next time"
            ));
        }
    }

    pub(super) fn open_session_timeout(
        &mut self,
        addr: SocketAddr,
        timeout: Duration,
    ) -> io::Result<()> {
        let (sock, link) = self.connect_control(addr, timeout)?;
        let watch = link.as_ref().map(|l| l.watch());
        self.start_session(addr, sock, link).map_err(|e| {
            // The sender's own reason, rather than the broken pipe it left behind.
            // Its close may still be on the way (the task recording it runs separately).
            let refused = watch.and_then(|w| {
                let end = Instant::now() + Duration::from_millis(500);
                while w.get().is_none() && Instant::now() < end {
                    std::thread::sleep(Duration::from_millis(10));
                }
                crate::link::refused_by_sender(&w)
            });
            match refused {
                Some(why) => io::Error::new(io::ErrorKind::PermissionDenied, why),
                None => e,
            }
        })
    }

    /// Starts the session on a connected control connection: TCP, or a link's control
    /// stream.
    pub(super) fn start_session(
        &mut self,
        addr: SocketAddr,
        sock: TcpStream,
        link: Option<crate::link::LinkClient>,
    ) -> io::Result<()> {
        let r = self.start_session_inner(addr, sock, link);
        if r.is_err() {
            // A start that failed part way leaves no session behind: its link and flags
            // made the next loss read a stale link's reason, and a plain session look
            // like a link's for a second or two.
            self.end_link();
            self.secure_session = false;
            self.squic_offered = false;
        }
        r
    }

    pub(super) fn start_session_inner(
        &mut self,
        addr: SocketAddr,
        sock: TcpStream,
        link: Option<crate::link::LinkClient>,
    ) -> io::Result<()> {
        self.session_started = Instant::now();
        self.session_media = false;
        // (Another sender, perhaps, on another clock: its pictures from now.)
        self.delivered = None;
        sock.set_nodelay(true)?;
        // Notice a source that vanishes without closing (power loss, cable pull, PoE
        // switch reboot) within about 6 s: nothing else is sent on this connection for
        // long periods, so it would otherwise stay open forever and the input would never
        // reconnect. A source that stops reading can't block this input either.
        crate::transport::keepalive(&sock, Duration::from_secs(3));
        sock.set_write_timeout(Some(crate::transport::WRITE_TIMEOUT))?;
        self.quic_peer = None;
        self.multicast = None;
        *self.sender_ip.lock_ok() = Some((addr.ip(), self.media_epoch.load(Ordering::Relaxed)));
        // secure/1 and secure/2 (§E.8.3, §E.10.7): the plain-text start decides whether
        // the session is encrypted, and how.
        let version = match &self.cfg.secure {
            Some(sc) => sc.version(false),
            None if crate::ext::test_plain_ndi() => crate::control::ndi_version(),
            None => crate::ext::ndi_version_with(false, None),
        };
        let version = if self.link_cfg.is_some() {
            crate::link::announce(&version)
        } else {
            version
        };
        // The test switch's plain NDI receiver announces nothing of IDN's.
        let version = if self.sync.is_some() && !crate::ext::test_plain_ndi() {
            crate::link::announce_token(&version, crate::sync::TOKEN)
        } else {
            version
        };
        let version = if self.link_cfg.is_some() && self.cfg.frame_streams {
            crate::link::announce_token(&version, crate::link::FRAMES_TOKEN)
        } else {
            version
        };
        let (mut sock, secure, creds, pre, peer_fp) = match (&link, &self.cfg.secure) {
            // Inside TLS already: no plain-text start (§E.11.5).
            (Some(l), _) => {
                self.link_policy(l, &sock)?;
                let mut sock = sock;
                send_text(&mut sock, kind::CONTROL, &version)?;
                let fp = Some(l.peer_fingerprint.clone()).filter(|f| !f.is_empty());
                (sock, true, l.mode.creds().cloned(), Vec::new(), fp)
            }
            (None, Some(sc)) => {
                let n = crate::secure::negotiate_receiver(sock, sc, &version, &self.cfg.source)?;
                (n.stream, n.secure, n.creds, n.pre, n.peer_fp)
            }
            (None, None) => {
                let mut sock = sock;
                send_text(&mut sock, kind::CONTROL, &version)?;
                (sock, false, None, Vec::new(), None)
            }
        };
        self.log(format!("-> kind 1: {version}"));
        let link_mode = link.as_ref().map(|l| l.mode.name());
        if let Some(m) = link_mode {
            self.log(format!("session over the IDN link (mode {m})"));
        }
        // (The link before it, of a source a router switched away from: ended first; and
        // that source's rings and their watcher, which went on handing over its pictures
        // after the new source's first: lab 497-C on one machine.)
        self.end_link();
        self.machine = None;
        self.link = link;
        self.link_late_seen = 0;
        if let Some(s) = &mut self.sync {
            s.new_session();
        }
        if let Some(c) = creds.as_ref().filter(|_| link_mode.is_none()) {
            self.log(format!("session is encrypted ({})", c.mode()));
            // Secure QUIC with the same credentials (§E.8.4).
            match &self.squic {
                Some(sq) => {
                    if let Err(e) = sq.set_creds(c) {
                        self.log(format!("secure QUIC: {e}"));
                    }
                }
                None => match crate::squic::SquicServer::start(c, 0, Limits::default(), self.sink.clone()) {
                    Ok(sq) => {
                        self.log(format!("secure QUIC media endpoint on UDP port {}", sq.port()));
                        self.squic = Some(sq);
                    }
                    Err(e) => self.log(format!("secure QUIC unavailable ({e}); encrypted media will use the control connection")),
                },
            }
        }
        let site_trust = creds.as_ref().is_some_and(|c| c.mode() == "secure/2");
        let linked = creds.as_ref().is_some_and(|c| c.mode() == "pair/1");
        self.stat(|s| {
            s.encrypted = secure;
            s.site_trust = site_trust;
            s.linked = linked;
            s.link_mode = link_mode;
            s.peer_fp = peer_fp.clone();
        });
        let st = self.queue.stats.lock_ok().clone();
        self.report_base = [
            (st.video + st.late) as u64,
            st.hx_discarded as u64,
            (st.video_dropped + st.late) as u64,
            st.video_errors as u64,
            st.audio as u64,
        ];
        // New identifiers, then the new generation: the UDP loops take the identifiers
        // when they see it.
        if let Some(i) = &self.stream_ids[0] {
            let (v, a) = i.renew();
            if let Some(((vp, _), (ap, _))) = self.streams.udp {
                self.streams.udp = Some(((vp, v), (ap, a)));
            }
        }
        if let Some(i) = &self.stream_ids[1] {
            let (v, a) = i.renew();
            if let Some((port, _, _)) = self.streams.quic {
                self.streams.quic = Some((port, v, a));
            }
        }
        if let Some(g) = &self.udp_session {
            // Release: the identifiers renewed above are seen with it (the UDP loops).
            g.fetch_add(1, Ordering::Release);
        }
        // Media queued until now was the last session's (F5-1).
        self.media_epoch.fetch_add(1, Ordering::AcqRel);
        // A new session: extensions are renegotiated when the sender's ndi_version arrives.
        self.resume_from = self.peer_instance.take().or(self.resume_from.take());
        self.ext = Default::default();
        self.bye = None;
        self.color = None;
        self.last_addr = Some(addr);
        // Encrypted sessions advertise no baseline transports; secure QUIC is offered with
        // a fresh media token (§E.8.4). The offer travels only inside TLS.
        let offer = if secure && link_mode.is_none() {
            self.squic.as_ref().map(|s| s.new_session())
        } else {
            None
        };
        (
            self.secure_session,
            self.squic_offered,
            self.fallback_reported,
        ) = (secure, offer.is_some(), false);
        self.stat(|s| s.secure_fallback = false);
        let streams = match (secure, offer) {
            (true, Some(o)) => control::EnabledStreams {
                video: self.cfg.receive_video,
                audio: true,
                text: true,
                shq_new: !self.cfg.legacy,
                x_idn: if self.cfg.wants_lossless() {
                    vec![("x_idn_audio".into(), "f32".into())]
                } else {
                    Vec::new()
                }
                .into_iter()
                .chain(o.attrs())
                .collect(),
                ..Default::default()
            }
            .to_xml(),
            _ if link_mode.is_some() => self.link_streams_xml.clone(),
            (true, None) => self.inline_streams_xml.clone(),
            (false, _) => self.streams.to_xml(),
        };
        // The size this input needs (`adapt/1`, §E.16.2) and changed regions (§E.15.1);
        // NDI senders ignore them.
        self.channels_requested = self.cfg.audio_channels.clone();
        self.size_requested = self.cfg.max_size;
        let extra = [
            self.cfg
                .max_size
                .map(|(w, h)| format!(r#" x_idn_size="{w}x{h}""#)),
            self.cfg.regions.then(|| r#" x_idn_regions="1""#.to_owned()),
            self.cfg
                .picture_every
                .filter(|n| (2..=600).contains(n))
                .map(|n| format!(r#" x_idn_every="{n}""#)),
            // On Wi-Fi or through a tunnel: sound in packets of 5 ms at least (§E.6; the
            // sender joins small blocks), not a packet every 0.67 ms over the air.
            matches!(
                idn_common::net::path_to(addr.ip()),
                idn_common::net::Path::Wireless | idn_common::net::Path::Tunnel
            )
            .then(|| r#" x_idn_audio_ms="5""#.to_owned()),
            // The audio channels this input wants (§E.21.1).
            self.cfg
                .audio_channels
                .as_deref()
                .map(|l| format!(r#" x_idn_channels="{}""#, crate::ext::channel_list(l))),
            // Compressed video, if this input takes it (§E.18.2), and AAC sound with it
            // (§E.19.2).
            self.cfg
                .codec
                .as_ref()
                .filter(|_| !crate::ext::test_plain_ndi())
                .and_then(|c| {
                    let (wanted, use_) = (c.wanted(), c.use_.name()?);
                    let aac = if c.decoder.decodes_aac() {
                        r#" x_idn_acodec="aac""#
                    } else {
                        ""
                    };
                    (!wanted.is_empty()).then(|| {
                        format!(
                            r#" x_idn_codec="{}" x_idn_codec_use="{use_}"{aac}"#,
                            crate::codec::names(&wanted)
                        )
                    })
                }),
        ]
        .into_iter()
        .flatten()
        .collect::<String>();
        let streams = match streams.strip_suffix("/>") {
            Some(head) if !extra.is_empty() => format!("{head}{extra}/>"),
            _ => streams,
        };
        for xml in [
            control::ndi_identify(&self.full_name),
            control::ndi_video(self.quality),
            streams,
            control::ndi_tally(self.tally.0, self.tally.1),
        ] {
            self.log(format!("-> kind 1: {}", redact_token(&xml)));
            send_text(&mut sock, kind::CONTROL, &xml)?;
        }
        let mut reader = io::BufReader::with_capacity(1 << 20, sock.try_clone()?);
        self.next_conn += 1;
        let id = self.next_conn;
        let tx = self.tx.clone();
        // Frames read during negotiation (the sender's ndi_version) are processed first.
        for f in pre {
            let _ = tx.send(Incoming::Tcp(id, Ok(Some(f))));
        }
        std::thread::spawn(move || {
            loop {
                let r = read_frame(&mut reader, &Limits::default());
                let end = !matches!(r, Ok(Some(_)));
                if tx.send(Incoming::Tcp(id, r)).is_err() || end {
                    return;
                }
            }
        });
        self.conns.insert(id, sock);
        self.active = id;
        Ok(())
    }

    /// While a routing source is gone (§1.8): every [`ROUTER_LOOK`], resolves the source's
    /// name in the background and tries a connection; one that answers comes back as
    /// `Incoming::RouterLook`.
    pub(super) fn look_for_router(&mut self) {
        let Some(at) = self.router_gone else {
            return;
        };
        if Instant::now() < at {
            return;
        }
        // A look takes at most ROUTER_LOOK and a second: the next one starts after it.
        self.router_gone = Some(Instant::now() + ROUTER_LOOK * 2);
        let (src, iface, server, tx) = (
            self.cfg.source.clone(),
            self.cfg.interface,
            self.cfg.discovery_server.clone(),
            self.tx.clone(),
        );
        std::thread::spawn(move || {
            let at = resolve_hinted(&src, iface, server.as_deref(), ROUTER_LOOK)
                .ok()
                .map(|(a, _, _)| a)
                .filter(|a| {
                    std::net::TcpStream::connect_timeout(a, Duration::from_secs(1)).is_ok()
                });
            let _ = tx.send(Incoming::RouterLook(at));
        });
    }

    /// Starts resolving the source name in the background; the result arrives as
    /// `Incoming::Resolved`, so the worker keeps running (fast retries, media) meanwhile.
    pub(super) fn start_resolve(&mut self, timeout: Duration) {
        if self.resolving {
            return;
        }
        self.resolving = true;
        let (src, iface, server, tx) = (
            self.cfg.source.clone(),
            self.cfg.interface,
            self.cfg.discovery_server.clone(),
            self.tx.clone(),
        );
        std::thread::spawn(move || {
            let r =
                resolve_hinted(&src, iface, server.as_deref(), timeout).map_err(|e| e.to_string());
            let _ = tx.send(Incoming::Resolved(r));
        });
    }

    /// Handles a resolution result. Returns `false` if the input should stop (initial
    /// failure without reconnect).
    pub(super) fn on_resolved(
        &mut self,
        r: Result<(SocketAddr, String, Option<bool>), String>,
        retry_at: &mut Option<Instant>,
    ) -> bool {
        self.resolving = false;
        if !self.conns.is_empty() {
            return true; // a fast reconnect already won
        }
        let r = r.and_then(|(a, how, hint)| {
            if !how.is_empty() {
                self.log(how);
            }
            self.link_hint = hint;
            self.open_session_timeout(a, Duration::from_secs(1))
                .map(|_| a)
                .map_err(|e| e.to_string())
        });
        match r {
            Ok(a) => {
                *retry_at = None;
                self.last_failure = None;
                self.connected_ok(a);
                true
            }
            Err(e) => {
                self.stat(|s| s.retries += 1);
                if !self.ever_connected {
                    // In the log too (once a reason): hosts that only log (OBS) showed
                    // nothing while the Node held the program for approval.
                    if self.last_failure.as_deref() != Some(e.as_str()) {
                        self.log(format!("not connected: {e}"));
                        self.last_failure = Some(e.clone());
                    }
                    self.emit(InputEvent::State(ConnectionState::Lost(e)));
                } else {
                    self.log(format!("source not back yet ({e})"));
                }
                if !self.cfg.reconnect {
                    return false;
                }
                // Every second for 30 s after a loss (a restarted sender is back within
                // a second, not two), then every 2 s.
                let soon = self
                    .lost_at
                    .is_some_and(|t| t.elapsed() < Duration::from_secs(30));
                *retry_at = Some(Instant::now() + Duration::from_secs(if soon { 1 } else { 2 }));
                true
            }
        }
    }

    pub(super) fn connected_ok(&mut self, a: SocketAddr) {
        self.lost_at = None;
        self.stat(|s| s.retries = 0);
        if self.ever_connected {
            self.stat(|s| s.reconnects += 1);
        }
        self.ever_connected = true;
        self.hx_need_key = true;
        self.hx_wait = Some(HX_WAIT_NEW);
        self.fast_until = None;
        self.emit(InputEvent::State(ConnectionState::Connected(a)));
    }

    /// Periodic extension work: fast reconnect attempts (§E.4), health reports and pings
    /// (§E.5).
    pub(super) fn tick(&mut self, retry_at: &mut Option<Instant>) {
        self.check_link_probe();
        self.poll_decoder();
        // Per-frame streams (§E.13.2): stragglers dropped because a newer frame came
        // first count as late.
        if let Some(n) = self.link.as_ref().map(|l| l.late_frames())
            && n > self.link_late_seen
        {
            let d = (n - self.link_late_seen) as usize;
            self.link_late_seen = n;
            self.stat(|s| s.late += d);
        }
        if self.retiring.is_some_and(|(_, at)| Instant::now() >= at) {
            self.retire_old_session();
        }
        let now = Instant::now();
        if self.monitor.is_some() && now.duration_since(self.monitor_at) >= Duration::from_secs(1) {
            self.report_monitor(now);
        }
        if let (Some(until), Some(addr)) = (self.fast_until, self.last_addr) {
            if now >= until {
                self.fast_until = None;
            } else if now >= self.next_fast {
                self.next_fast = now + Duration::from_millis(250);
                self.expect_name = self
                    .peer_name
                    .clone()
                    .map(|n| (n, now + Duration::from_secs(5)));
                if self
                    .open_session_timeout(addr, Duration::from_millis(200))
                    .is_ok()
                {
                    self.log(format!("fast reconnect to {addr}"));
                    *retry_at = None;
                    self.connected_ok(addr);
                }
            }
        }
        // Enrolled just now: a session that isn't using site trust is re-opened so that it
        // can (§E.10.7). The fast reconnect brings it back within about a second.
        if let Some(site) = self.cfg.secure.as_ref().and_then(|s| s.site.clone()) {
            let enrolled = site.ca_id().is_some();
            if enrolled
                && !self.site_seen_enrolled
                && !self.conns.is_empty()
                && !self.queue.stats.lock_ok().site_trust
            {
                self.log("site trust is now available: reconnecting to use it");
                if let Some(c) = self.conns.get(&self.active) {
                    let _ = c.shutdown(std::net::Shutdown::Both);
                }
            }
            self.site_seen_enrolled = enrolled;
        }
        // "Require encryption" switched on (locally or by the site): an
        // unencrypted session ends; reconnecting then refuses the source.
        if !self.conns.is_empty()
            && !self.secure_session
            && now.duration_since(self.require_checked) >= Duration::from_secs(1)
        {
            self.require_checked = now;
            if self.cfg.secure.as_ref().is_some_and(|s| s.requires()) {
                self.log("\"require encryption\" is now on: closing the unencrypted session");
                if let Some(c) = self.conns.get(&self.active) {
                    let _ = c.shutdown(std::net::Shutdown::Both);
                }
            }
        }
        if self.conns.is_empty() {
            return;
        }
        if self.ext.has("stats") && now.duration_since(self.last_report) >= Duration::from_secs(1) {
            self.last_report = now;
            let st = self.queue.stats.lock_ok().clone();
            let q_depth = self
                .queue
                .q
                .lock_ok()
                .iter()
                .filter(|e| e.is_media())
                .count() as u64;
            let b = self.report_base;
            let r = crate::ext::ReceiverReport {
                // Pictures received (shown or skipped as late), and those that came too
                // late to show: skipped, or dropped from the application's queue (§E.5).
                v_rx: ((st.video + st.late) as u64).saturating_sub(b[0]),
                v_lost: (st.hx_discarded as u64).saturating_sub(b[1]),
                v_late: ((st.video_dropped + st.late) as u64).saturating_sub(b[2]),
                v_dec_err: (st.video_errors as u64).saturating_sub(b[3]),
                a_rx: (st.audio as u64).saturating_sub(b[4]),
                a_lost: 0,
                q_depth,
                dec_ms: if self.dec_n > 0 {
                    self.dec_time * 1000.0 / self.dec_n as f64
                } else {
                    0.0
                },
                transport: if self.last_via == "control" {
                    "inline".into()
                } else {
                    self.last_via.into()
                },
            };
            (self.dec_time, self.dec_n) = (0.0, 0);
            self.send_active(kind::CONTROL, &r.to_xml());
        }
        if self.ext.has("stats") && now >= self.next_ping {
            // Every second while synced: the clock offset needs samples (§E.14.3).
            let every = if self.sync.is_some() { 1 } else { 2 };
            self.next_ping = now + Duration::from_secs(every);
            self.ping_id += 1;
            let t = self.started.elapsed().as_micros() as u64;
            if let Some(s) = &mut self.sync {
                s.ping_sent(self.ping_id);
            }
            self.send_active(kind::CONTROL, &crate::ext::ping(self.ping_id, t));
        }
        // Sync: the report to the sender (§E.14.4), and the statistics.
        if let Some(s) = &mut self.sync {
            let report = s.report();
            let st = s.stats();
            if let Some(r) = report {
                self.send_active(kind::CONTROL, &r);
            }
            self.stat(|s| s.sync = Some(st));
        }
    }

    /// Ends the current session: closes every control connection (the routing source's
    /// too), so the normal reconnect starts afresh, and leaves its multicast groups.
    pub(super) fn end_session(&mut self) {
        for (_, c) in std::mem::take(&mut self.conns) {
            let _ = c.shutdown(std::net::Shutdown::Both);
        }
        self.active = 0;
        self.routing = None;
        self.router_gone = None;
        self.quic_peer = None;
        self.multicast = None;
        self.mc_announcement = None;
        self.end_link();
        // The source's rings and their watcher go with the session (
        // a source left behind went on adding its pictures to the next one's).
        self.machine = None;
        *self.sender_ip.lock_ok() = None;
        // Its media from now on, and what is queued, belongs to no session (F5-1): the
        // epoch moves on, and its stream ids too (the UDP loops and QUIC drop the old).
        self.media_epoch.fetch_add(1, Ordering::AcqRel);
        if let Some(i) = &self.stream_ids[0] {
            i.renew();
        }
        if let Some(i) = &self.stream_ids[1] {
            i.renew();
        }
        if let Some(g) = &self.udp_session {
            g.fetch_add(1, Ordering::Release);
        }
    }

    pub(super) fn finish(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Close the control connections for real: their reader threads hold clones, so
        // without a shutdown the source would keep a session (and our tally) open.
        self.end_session();
        self.endpoints.clear();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

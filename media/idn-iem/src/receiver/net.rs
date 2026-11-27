//! The receiver's work: its core's handling, the network loop, multicast, the desk's
//! push channel.

use super::*;

impl Core {
    pub(super) fn event(&self, e: Event) {
        let cb = self.callback.lock_ok().clone();
        if let Some(cb) = cb {
            cb(&e);
            return;
        }
        let mut q = self.events.lock_ok();
        if q.len() >= 256 {
            q.pop_front();
        }
        q.push_back(e);
        self.events_cv.notify_all();
    }

    pub(super) fn shut(&self) {
        if self.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        let (flow, to, mcast) = {
            let mut n = self.net.lock_ok();
            (n.flow, n.desk_udp, n.mcast.take())
        };
        if let Some(to) = to {
            let _ = self.socket.send_to(
                &Packet::Bye {
                    flow,
                    client: self.client,
                }
                .to_bytes(),
                to,
            );
        }
        if let Some((stop, t)) = mcast {
            stop.store(true, Ordering::Relaxed);
            let _ = t.join();
        }
        self.jitter.lock_ok().reset();
        self.net_cv.notify_all();
        self.events_cv.notify_all();
    }

    /// The gains for the audio thread, from the mix and channels now.
    pub(super) fn recompile(&self, c: &Ctl) {
        let compiled = mixing::compile(c.mix.as_ref(), &c.channels);
        let mut slot = self.compiled.lock_ok();
        if **slot != compiled {
            *slot = Arc::new(compiled);
            self.compiled_version.fetch_add(1, Ordering::Release);
        }
    }

    /// A change of this phone's: heard at once, sent to the desk.
    /// `f` changes the mix (`ch` given: that channel's entry, by its pair's lead).
    pub(super) fn edit(
        &self,
        ch: Option<u16>,
        field: &str,
        f: impl FnOnce(&mut Mix, Option<u16>),
    ) -> Result<(), Error> {
        if self.stop.load(Ordering::Relaxed) {
            return Err(Error::Closed);
        }
        let mut c = self.ctl.lock_ok();
        let lead = ch.map(|ch| c.lead(ch)).transpose()?;
        let Some(m) = c.mix.as_mut() else {
            return Err(Error::NoMix(
                "not known yet (no state from the desk)".into(),
            ));
        };
        f(m, lead);
        let key = match lead {
            Some(id) => format!("c{id}.{field}"),
            None => field.to_owned(),
        };
        c.pending.entry(key).or_default().dirty = true;
        self.recompile(&c);
        Ok(())
    }

    pub(super) fn send_talk(
        &self,
        data: &[u8],
        samples: u16,
        last: bool,
        seq: u32,
    ) -> Result<(), Error> {
        let (flow, to) = {
            let n = self.net.lock_ok();
            (n.flow, n.desk_udp)
        };
        let to = to.ok_or(Error::NoFlow)?;
        let p = Packet::Talk(wire::Talk {
            flow,
            client: self.client,
            seq,
            frame_samples: samples,
            flags: if last { wire::TALK_LAST } else { 0 },
            data: data.to_vec(),
        });
        self.socket
            .send_to(&p.to_bytes(), to)
            .map(|_| ())
            .map_err(|e| Error::Io(e.to_string()))
    }

    /// An AUDIO packet in (from the desk's address, or the group).
    pub(super) fn audio(&self, a: wire::Audio, mcast: bool) {
        {
            let mut n = self.net.lock_ok();
            if a.flow != n.flow {
                return;
            }
            if a.layout != n.layout {
                self.refetch.store(true, Ordering::Relaxed);
            }
            let now = Instant::now();
            n.audio_at = Some(now);
            if mcast {
                n.mcast_audio_at = Some(now);
            }
        }
        self.jitter.lock_ok().insert(&a);
    }

    pub(super) fn reply(&self, r: wire::Reply) {
        if r.client != self.client {
            return;
        }
        let mut events = Vec::new();
        {
            let mut n = self.net.lock_ok();
            let now = Instant::now();
            n.last_reply = Some(now);
            if r.keepalive_ms > 0 {
                n.keepalive = Duration::from_millis(u64::from(r.keepalive_ms.clamp(100, 10_000)));
            }
            if r.timeout_ms > 0 {
                n.timeout = Duration::from_millis(u64::from(r.timeout_ms.max(500)));
            }
            match r.status {
                Reply::Ok => {
                    if let Some(t) = n.keepalive_sent.take() {
                        let ms = t.elapsed().as_secs_f32() * 1000.0;
                        n.rtt_ms = Some(n.rtt_ms.map_or(ms, |r| r + (ms - r) * 0.2));
                    }
                    if n.session != Session::Ok {
                        n.session = Session::Ok;
                        n.subscribed_at.get_or_insert(now);
                        n.error = None;
                        events.push(Event::Connected {
                            transport: n.transport,
                        });
                    }
                    if r.flow != n.flow && r.flow != 0 {
                        // The desk started again: its new flow, described afresh.
                        n.flow = r.flow;
                        self.jitter.lock_ok().reset();
                        self.refetch.store(true, Ordering::Relaxed);
                    }
                    if r.layout != n.layout {
                        self.refetch.store(true, Ordering::Relaxed);
                    }
                }
                Reply::Unknown => {
                    n.session = Session::Connecting;
                    n.next_subscribe = n.next_subscribe.min(now + Duration::from_millis(100));
                    events.push(Event::Disconnected {
                        reason: "the desk had no session (it started again, or ours timed out)"
                            .into(),
                    });
                }
                Reply::Busy => {
                    n.next_subscribe = now + Duration::from_secs(1);
                    if n.session != Session::Ok {
                        n.session = Session::Refused(Error::Busy(r.message.clone()));
                    }
                }
                Reply::NotAllowed if n.talk_allowed => {
                    // Listening, without talking.
                    n.talk_allowed = false;
                    n.session = Session::Connecting;
                    n.next_subscribe = now;
                    events.push(Event::TalkRefused {
                        reason: r.message.clone(),
                    });
                }
                s => {
                    let e = Error::of_reply(s, &r.message);
                    n.error = Some(e.to_string());
                    if n.session != Session::Refused(e.clone()) {
                        events.push(Event::Error(e.clone()));
                    }
                    n.session = Session::Refused(e);
                    n.next_subscribe = now + Duration::from_secs(3);
                }
            }
        }
        self.net_cv.notify_all();
        for e in events {
            self.event(e);
        }
    }

    /// The session's timers: subscribe (and again), keepalives, multicast's fallback.
    pub(super) fn tick(&self) {
        let now = Instant::now();
        let mut events = Vec::new();
        let mut n = self.net.lock_ok();
        if n.session == Session::Ok
            && n.last_reply
                .is_some_and(|t| now.duration_since(t) > n.timeout)
        {
            n.session = Session::Connecting;
            n.next_subscribe = now;
            events.push(Event::Disconnected {
                reason: "the desk stopped answering".into(),
            });
        }
        // Multicast that doesn't come (or stopped): unicast, when the phone left it to us.
        if n.asked == Transport::Auto
            && n.transport == Transport::Multicast
            && n.session == Session::Ok
        {
            let wait = self.cfg.multicast_wait;
            let since = n.subscribed_at.unwrap_or(now);
            let why = match n.mcast_audio_at {
                None if now.duration_since(since) > wait => {
                    Some(format!("no multicast audio within {} ms", wait.as_millis()))
                }
                Some(t) if now.duration_since(t) > wait.max(Duration::from_secs(1)) => {
                    Some("multicast audio stopped".to_owned())
                }
                _ => None,
            };
            if let Some(reason) = why {
                n.transport = Transport::Unicast;
                n.session = Session::Connecting;
                n.next_subscribe = now;
                events.push(Event::TransportChanged {
                    transport: Transport::Unicast,
                    reason,
                });
            }
        }
        // The group: joined while multicast, left otherwise.
        let want_group = (n.transport == Transport::Multicast)
            .then_some(n.group)
            .flatten();
        if want_group.is_none()
            && let Some((stop, t)) = n.mcast.take()
        {
            stop.store(true, Ordering::Relaxed);
            drop(n);
            let _ = t.join();
            n = self.net.lock_ok();
        }
        let subscribe = n.session != Session::Ok && now >= n.next_subscribe;
        let renew = n.session == Session::Ok && now.duration_since(n.last_keepalive) >= n.keepalive;
        let Some(to) = n.desk_udp else {
            return;
        };
        if subscribe {
            n.next_subscribe = now + Duration::from_millis(500);
            n.subscribes += 1;
            let mut flags = match n.transport {
                Transport::Multicast => wire::WANT_MULTICAST,
                _ => wire::WANT_UNICAST,
            };
            if n.talk_allowed {
                flags |= wire::WANT_TALK;
            }
            if n.transport == Transport::Multicast {
                n.subscribed_at = None;
            }
            let p = Packet::Subscribe(wire::Subscribe {
                flow: n.flow,
                client: self.client,
                flags,
                mix: self.cfg.mix.clone().unwrap_or_default(),
                token: self.token.lock_ok().clone(),
            });
            let _ = self.socket.send_to(&p.to_bytes(), to);
        } else if renew {
            n.last_keepalive = now;
            n.keepalive_sent = Some(now);
            let p = Packet::Keepalive {
                flow: n.flow,
                client: self.client,
            };
            let _ = self.socket.send_to(&p.to_bytes(), to);
        }
        drop(n);
        for e in events {
            self.event(e);
        }
    }

    /// The description fetched again (a new flow or layout): its channels, ports, group.
    pub(super) fn refresh(&self) -> Result<(), Error> {
        let token = self.token.lock_ok().clone();
        let d = fetch_description(&self.endpoint, &token)?;
        {
            let mut n = self.net.lock_ok();
            if d.flow != n.flow {
                n.flow = d.flow;
                self.jitter.lock_ok().reset();
            }
            n.layout = d.layout;
            if let Some(p) = self.cfg.udp_port.or(d.unicast) {
                n.desk_udp = Some(SocketAddr::new(self.endpoint.addr.ip(), p));
            }
            if n.group != d.multicast {
                n.group = d.multicast;
                if let Some((stop, _)) = &n.mcast {
                    // (Joined again, on the new group, at the next tick.)
                    stop.store(true, Ordering::Relaxed);
                }
            }
            if n.group.is_none() && n.transport == Transport::Multicast {
                n.transport = Transport::Unicast;
                n.session = Session::Connecting;
            }
        }
        self.set_channels(d.channels.clone());
        *self.desc.lock_ok() = d;
        Ok(())
    }

    pub(super) fn set_channels(&self, list: Vec<Channel>) {
        let mut c = self.ctl.lock_ok();
        if c.channels == list {
            return;
        }
        c.channels = list;
        self.recompile(&c);
        drop(c);
        self.event(Event::ChannelsChanged);
    }

    /// The desk's state from the push channel: an `iem:` token's (`channels`, `layout`,
    /// `mix`), or the whole desk's (`iem.channels`, `iem.mixes`).
    pub(super) fn apply_state(&self, st: &Value) {
        let (chans, layout, mix_v, err) = if st.get("mix").is_some() {
            (
                &st["channels"],
                &st["layout"],
                st["mix"].clone(),
                st["error"].as_str(),
            )
        } else if st["iem"].is_object() {
            let want = self.ctl.lock_ok().mix_id.clone();
            let m = st["iem"]["mixes"]
                .as_array()
                .and_then(|l| {
                    l.iter()
                        .find(|m| want.as_deref().is_some_and(|w| m["id"] == w))
                        .cloned()
                })
                .unwrap_or(Value::Null);
            (&st["iem"]["channels"], &st["iem"]["layout"], m, None)
        } else {
            return;
        };
        if let Some(l) = layout.as_u64().map(|l| l as u16) {
            if self.net.lock_ok().layout != l {
                self.refetch.store(true, Ordering::Relaxed);
            }
            if chans.is_array() {
                self.set_channels(mixing::channels_of(chans));
            }
        }
        let desk = Mix::from_json(&mix_v);
        let mut c = self.ctl.lock_ok();
        let Some(desk) = desk else {
            if !c.no_mix_said {
                c.no_mix_said = true;
                let why = err.map(str::to_owned).unwrap_or_else(|| match &c.mix_id {
                    Some(m) => format!("no IEM mix {m:?} on this desk"),
                    None => "name a mix (Config::mix): this token is not for one".into(),
                });
                drop(c);
                self.event(Event::Error(Error::NoMix(why)));
                c = self.ctl.lock_ok();
            }
            if c.mix.is_some() {
                c.mix = None;
                c.desk_mix = None;
                self.recompile(&c);
            }
            return;
        };
        c.no_mix_said = false;
        if c.mix_id.is_none() {
            c.mix_id = Some(desk.id.clone());
        }
        c.desk_mix = Some(desk);
        let merged = c.merged();
        if merged != c.mix {
            let first = c.mix.is_none();
            c.mix = merged;
            self.recompile(&c);
            drop(c);
            if !first {
                self.event(Event::MixChanged);
            }
        }
    }

    /// A command's reply: its control confirmed (or refused: the desk's value back).
    pub(super) fn ack(&self, v: &Value) {
        let Some(id) = v["id"].as_u64() else { return };
        let ok = v["ok"].as_bool().unwrap_or(false);
        let mut c = self.ctl.lock_ok();
        let Some(key) = c
            .pending
            .iter()
            .find(|(_, p)| p.inflight == Some(id))
            .map(|(k, _)| k.clone())
        else {
            return;
        };
        let dirty = c.pending.get_mut(&key).is_some_and(|p| {
            p.inflight = None;
            p.dirty
        });
        if !ok || !dirty {
            c.pending.remove(&key);
        }
        if !ok {
            let merged = c.merged();
            if merged != c.mix {
                c.mix = merged;
                self.recompile(&c);
            }
            drop(c);
            let why = v["error"].as_str().unwrap_or("refused").to_owned();
            self.event(Event::Error(Error::Desk(why)));
        }
    }
}

/// The UDP session's thread: answers, unicast audio, the timers.
pub(super) fn net_loop(core: Arc<Core>) {
    let mut buf = vec![0u8; wire::MAX_PACKET + 1];
    while !core.stop.load(Ordering::Relaxed) {
        // (From the desk's address only.)
        if let Ok((len, from)) = core.socket.recv_from(&mut buf)
            && core.net.lock_ok().desk_udp == Some(from)
        {
            match wire::parse(&buf[..len]) {
                Some(Packet::Audio(a)) => core.audio(a, false),
                Some(Packet::Reply(r)) => core.reply(r),
                _ => {}
            }
        }
        core.tick();
        // The group joined when multicast is wanted.
        let join = {
            let n = core.net.lock_ok();
            let finished = n
                .mcast
                .as_ref()
                .is_some_and(|(s, _)| s.load(Ordering::Relaxed));
            (n.transport == Transport::Multicast && (n.mcast.is_none() || finished))
                .then_some(n.group)
                .flatten()
        };
        if let Some(g) = join {
            let old = core.net.lock_ok().mcast.take();
            if let Some((_, t)) = old {
                let _ = t.join();
            }
            match mcast_start(&core, g) {
                Ok(m) => core.net.lock_ok().mcast = Some(m),
                Err(e) => {
                    let mut n = core.net.lock_ok();
                    n.error = Some(e.to_string());
                    if n.asked == Transport::Auto {
                        n.transport = Transport::Unicast;
                        n.session = Session::Connecting;
                        n.next_subscribe = Instant::now();
                        drop(n);
                        core.event(Event::TransportChanged {
                            transport: Transport::Unicast,
                            reason: e.to_string(),
                        });
                    } else {
                        drop(n);
                        core.event(Event::Error(e));
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
            }
        }
    }
}

/// Joins group `g` (port shared with other listeners) and takes its AUDIO.
pub(super) fn mcast_start(
    core: &Arc<Core>,
    (group, port): (Ipv4Addr, u16),
) -> Result<(Arc<AtomicBool>, JoinHandle<()>), Error> {
    use socket2::{Domain, Protocol, Socket, Type};
    let io = |e: std::io::Error| Error::Io(format!("multicast {group}:{port}: {e}"));
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).map_err(io)?;
    s.set_reuse_address(true).map_err(io)?;
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())
        .map_err(io)?;
    let iface = core
        .cfg
        .multicast_interface
        .unwrap_or(Ipv4Addr::UNSPECIFIED);
    s.join_multicast_v4(&group, &iface).map_err(io)?;
    s.set_read_timeout(Some(Duration::from_millis(50)))
        .map_err(io)?;
    let sock: UdpSocket = s.into();
    let stop = Arc::new(AtomicBool::new(false));
    let (c, st) = (core.clone(), stop.clone());
    let t = std::thread::Builder::new()
        .name("idn-iem-mcast".into())
        .spawn(move || {
            let mut buf = vec![0u8; wire::MAX_PACKET + 1];
            while !st.load(Ordering::Relaxed) && !c.stop.load(Ordering::Relaxed) {
                if let Ok((len, from)) = sock.recv_from(&mut buf) {
                    // (From the desk's machine only: another sender on the group is noise.)
                    let desk_ip: Option<IpAddr> = c.net.lock_ok().desk_udp.map(|d| d.ip());
                    let ok = desk_ip.is_none_or(|ip| ip == from.ip() || ip.is_loopback());
                    if ok && let Some(Packet::Audio(a)) = wire::parse(&buf[..len]) {
                        c.audio(a, true);
                    }
                }
            }
            let _ = sock.leave_multicast_v4(&group, &iface);
            st.store(true, Ordering::Relaxed);
        })
        .map_err(|e| Error::Io(e.to_string()))?;
    Ok((stop, t))
}

/// The push channel's thread: the desk's state in, this phone's changes out, the
/// description fetched again when the flow changes.
pub(super) fn push_loop(core: Arc<Core>) {
    let mut ws: Option<tungstenite::WebSocket<http::Stream>> = None;
    let mut backoff = Duration::from_millis(500);
    let mut heard = Instant::now();
    let mut next_try = Instant::now();
    let mut last_fetch: Option<Instant> = None;
    let mut said_down = false;
    let mut last_flush = Instant::now();
    while !core.stop.load(Ordering::Relaxed) {
        if core.refetch.load(Ordering::Relaxed)
            && last_fetch.is_none_or(|t| t.elapsed() > Duration::from_millis(500))
        {
            last_fetch = Some(Instant::now());
            match core.refresh() {
                Ok(()) => core.refetch.store(false, Ordering::Relaxed),
                Err(e) => {
                    core.net.lock_ok().error = Some(e.to_string());
                    core.event(Event::Error(e));
                }
            }
        }
        if ws.is_none() && Instant::now() >= next_try {
            let token = core.token.lock_ok().clone();
            match core.endpoint.websocket("/api/v1/events", &token) {
                Ok(w) => {
                    let _ = w
                        .get_ref()
                        .tcp()
                        .set_read_timeout(Some(Duration::from_millis(15)));
                    ws = Some(w);
                    heard = Instant::now();
                    backoff = Duration::from_millis(500);
                    said_down = false;
                    core.push_up.store(true, Ordering::Relaxed);
                    // What was on its way: sent again.
                    for p in core.ctl.lock_ok().pending.values_mut() {
                        if p.inflight.take().is_some() {
                            p.dirty = true;
                        }
                    }
                    core.event(Event::PushConnected);
                }
                Err(e) => {
                    next_try = Instant::now() + backoff;
                    backoff = (backoff * 2).min(Duration::from_secs(5));
                    if !said_down {
                        said_down = true;
                        core.event(Event::PushDisconnected {
                            reason: e.to_string(),
                        });
                    }
                }
            }
        }
        let Some(w) = ws.as_mut() else {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        };
        let mut down = None;
        let r = w.read();
        // (Anything from the desk, its pings included, is a sign of life: none for 40 s
        // (it pings every 15) and the channel is taken as gone, connected again; a desk
        // that lost power left this side waiting for good; bug run B.)
        if r.is_ok() {
            heard = Instant::now();
        } else if heard.elapsed() > Duration::from_secs(40) {
            down = Some("the desk went silent".to_owned());
        }
        match r {
            Ok(tungstenite::Message::Text(t)) => {
                if let Ok(v) = serde_json::from_str::<Value>(t.as_str()) {
                    match v["type"].as_str() {
                        Some("state") => core.apply_state(&v["state"]),
                        Some("reply") => core.ack(&v),
                        _ => {}
                    }
                }
            }
            Ok(tungstenite::Message::Close(_)) => down = Some("closed by the desk".to_owned()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => down = Some(e.to_string()),
        }
        // This phone's changes: at most every 15 ms, the latest value of each control.
        if down.is_none() && last_flush.elapsed() >= Duration::from_millis(15) {
            last_flush = Instant::now();
            let msgs: Vec<String> = {
                let mut c = core.ctl.lock_ok();
                let keys: Vec<String> = c
                    .pending
                    .iter()
                    .filter(|(_, p)| p.dirty)
                    .map(|(k, _)| k.clone())
                    .collect();
                let mut out = Vec::new();
                for k in keys {
                    match c.command(&k) {
                        Some((id, msg)) => {
                            let p = c.pending.get_mut(&k).expect("listed");
                            p.dirty = false;
                            p.inflight = Some(id);
                            out.push(msg.to_string());
                        }
                        None => {
                            c.pending.remove(&k);
                        }
                    }
                }
                out
            };
            for m in msgs {
                if let Err(e) = w.send(tungstenite::Message::text(m)) {
                    down = Some(e.to_string());
                    break;
                }
            }
        }
        if let Some(reason) = down {
            ws = None;
            core.push_up.store(false, Ordering::Relaxed);
            next_try = Instant::now() + backoff;
            said_down = true;
            core.event(Event::PushDisconnected { reason });
        }
    }
    if let Some(mut w) = ws {
        let _ = w.close(None);
        let _ = w.flush();
    }
}

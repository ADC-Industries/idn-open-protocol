//! The source's control messages, as an input reads them.

use super::*;

impl Worker {
    pub(super) fn on_text(&mut self, f: &Frame, from_conn: Option<u32>) {
        self.stat(|s| s.metadata += 1);
        let xml = match f.text_payload() {
            Ok(x) => x.to_owned(),
            Err(e) => {
                return self.emit(InputEvent::Error(format!(
                    "kind {}: bad text payload ({e})",
                    f.kind
                )));
            }
        };
        if let Some(mut e) = parse_element(&xml) {
            // The test switch's plain NDI receiver doesn't see the sender's IDN extensions.
            if self.cfg.secure.is_none() && crate::ext::test_plain_ndi() {
                e.attrs.remove("x_idn_ext");
            }
            let from_active = from_conn.is_none_or(|c| c == self.active);
            // After a fast reconnect to the last address: the same source, or looked up
            // again by name (F1-1).
            if e.name == "ndi_identify"
                && from_active
                && let Some(n) = e.attr("name")
            {
                let expected = self
                    .expect_name
                    .take()
                    .filter(|(x, until)| !x.is_empty() && Instant::now() < *until);
                if let Some((x, _)) = expected
                    && x != n
                {
                    self.log(format!(
                        "the last address is now \"{n}\", not \"{x}\": looking the source up again"
                    ));
                    self.last_addr = None;
                    self.fast_until = None;
                    self.peer_name = None;
                    self.end_session();
                    self.pending_lost =
                        Some(format!("another source (\"{n}\") at the last address"));
                    return;
                }
                self.peer_name = Some(n.to_owned());
            }
            match e.name.as_str() {
                // The source refused this receiver (§E.10.7) and says why.
                // The source is on this machine and offers its ring (§E.26): taken when it
                // can be opened here (the same account), else the connection carries on
                // as it is.
                "x_idn_machine" if from_active && !self.cfg.machine => {
                    self.send_active(kind::CONTROL, "<x_idn_machine_no/>");
                }
                // Where the source's pictures or sound left the connection for the rings:
                // from these on, through the rings (those the watcher passed on already,
                // still there, taken now).
                "x_idn_machine_from"
                    if from_active
                        && self
                            .machine
                            .as_ref()
                            .is_some_and(|m| Some(m.generation.as_str()) == e.attr("gen")) =>
                {
                    let num = |k: &str| e.attr(k).and_then(|v| v.parse::<u64>().ok());
                    let Some(m) = self.machine.as_mut() else {
                        return;
                    };
                    let (epoch, seen) = (m.epoch, m.seen);
                    let mut again = Vec::new();
                    if let (Some(p), None) = (num("pic"), m.from.0) {
                        m.from.0 = Some(p);
                        let oldest = seen.0.saturating_sub(crate::machine::PIC_SLOTS as u64 - 1);
                        again.extend((p.max(oldest)..=seen.0).map(|q| (false, q)));
                    }
                    if let (Some(a), None) = (num("sound"), m.from.1) {
                        m.from.1 = Some(a);
                        let oldest = seen
                            .1
                            .saturating_sub(crate::machine::AUDIO_SLOTS as u64 - 1);
                        again.extend((a.max(oldest)..=seen.1).map(|q| (true, q)));
                    }
                    for (sound, q) in again {
                        self.machine_item(epoch, sound, q);
                    }
                    return;
                }
                // The same rings again (a repeated offer): taken already.
                "x_idn_machine"
                    if from_active
                        && self.machine.as_ref().is_some_and(|m| {
                            Some(m.generation.as_str()) == e.attr("gen")
                                && Some(m.control.as_str()) == e.attr("control")
                        }) =>
                {
                    #[cfg(feature = "gpu")]
                    let gpu = self.machine.as_ref().is_some_and(|m| m.gpu.is_some());
                    #[cfg(not(feature = "gpu"))]
                    let gpu = false;
                    let ok = format!(
                        r#"<x_idn_machine_ok gen="{}"{}/>"#,
                        e.attr("gen").unwrap_or(""),
                        if gpu { r#" gpu="1""# } else { "" }
                    );
                    self.send_active(kind::CONTROL, &ok);
                }
                "x_idn_machine" if from_active => {
                    let generation = e.attr("gen").unwrap_or("").to_owned();
                    let num =
                        |k: &str| e.attr(k).and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
                    let ring = |name: Option<&str>, bytes: usize, slots: usize| {
                        name.map(|n| crate::machine::Ring::open(n, bytes, slots))
                            .transpose()
                    };
                    let opened = crate::machine::Control::open(e.attr("control").unwrap_or(""))
                        .and_then(|c| {
                            let pic =
                                ring(e.attr("ring"), num("slot_bytes"), crate::machine::PIC_SLOTS)?;
                            let sound = ring(
                                e.attr("audio_ring"),
                                num("audio_slot_bytes"),
                                crate::machine::AUDIO_SLOTS,
                            )?;
                            Ok((c, pic, sound))
                        });
                    match opened {
                        Ok((control, pic, sound)) => {
                            // The GPU ring too, where both are on this GPU (§E.27).
                            #[cfg(feature = "gpu")]
                            let gpu = self.open_gpu_ring(&e, control.name());
                            #[cfg(feature = "gpu")]
                            let gpu_ok = gpu.is_some();
                            #[cfg(not(feature = "gpu"))]
                            let gpu_ok = false;
                            self.log(format!(
                                "<- media: through shared memory on this machine ({}{}{})",
                                control.name(),
                                pic.as_ref()
                                    .map_or(String::new(), |r| format!(", pictures {}", r.name())),
                                sound
                                    .as_ref()
                                    .map_or(String::new(), |r| format!(", sound {}", r.name()))
                            ));
                            let stop = Arc::new(AtomicBool::new(false));
                            let (inbox, stop2) = (self.tx.clone(), stop.clone());
                            self.machine_epoch += 1;
                            let epoch = self.machine_epoch;
                            let name = control.name().to_owned();
                            let seen = control.heads();
                            let _ = std::thread::Builder::new()
                                .name("idn-machine-watch".into())
                                .spawn(move || watch_machine(control, inbox, stop2, epoch, seen));
                            // (The rings before these, if any, and their watcher go.)
                            self.machine = Some(MachineIn {
                                control: name,
                                generation: generation.clone(),
                                epoch,
                                #[cfg(feature = "gpu")]
                                host: None,
                                pic,
                                sound,
                                from: (None, None),
                                seen,
                                stop,
                                #[cfg(feature = "gpu")]
                                gpu,
                            });
                            let gpu = if gpu_ok { r#" gpu="1""# } else { "" };
                            self.send_active(
                                kind::CONTROL,
                                &format!(r#"<x_idn_machine_ok gen="{generation}"{gpu}/>"#),
                            );
                        }
                        Err(err) => {
                            self.machine = None;
                            self.log(format!(
                                "<- media: the source's shared memory: {err}; over the connection"
                            ));
                            self.send_active(kind::CONTROL, "<x_idn_machine_no/>");
                        }
                    }
                }
                "x_idn_refused" => {
                    self.emit(InputEvent::Error(format!(
                        "refused by the source: {}",
                        e.attr("reason").unwrap_or("no reason given")
                    )));
                }
                // The key memory (§E.11.4), by the name the source announces.
                "ndi_identify" if from_active && self.link_peer_key.is_some() => {
                    let (name, fp) = (
                        e.attr("name").unwrap_or("").to_owned(),
                        self.link_peer_key.take().unwrap_or_default(),
                    );
                    let changed = self
                        .link_cfg
                        .as_ref()
                        .is_some_and(|lc| lc.keys.check(&name, &fp));
                    if changed {
                        self.emit(InputEvent::Error(format!(
                            "the link key of \"{name}\" changed since the last connection (reinstalled, or another device using its name?); connecting anyway"
                        )));
                        self.stat(|s| s.key_changed = true);
                    }
                }
                // Group keys, only from the session's own link (§E.17.2).
                "x_idn_mc_key" if from_active && self.link.is_some() => {
                    let rejected = self.queue.mc_rejected.clone();
                    let keys = self
                        .mc_keys
                        .get_or_insert_with(|| crate::mcast::Keyring::new(rejected));
                    if !keys.add(&e) {
                        self.log("malformed x_idn_mc_key; ignored".to_owned());
                    }
                    return;
                }
                "ndi_version" if from_active => {
                    // A new session: keys and message order of the last one are no use.
                    self.mc_keys = None;
                    self.mc_seq = Default::default();
                    if e.attr("x_idn_ext").is_some_and(crate::link::offered) {
                        self.try_upgrade();
                    }
                    if let Some(s) = &mut self.sync {
                        s.set_peer(e.attr("x_idn_ext").is_some_and(|l| {
                            l.split_whitespace().any(|t| t == crate::sync::TOKEN)
                        }));
                    }
                    self.ext = crate::ext::Active::negotiate(e.attr("x_idn_ext"));
                    self.peer_instance = e.attr("x_idn_instance").map(str::to_owned);
                    // A channel list changed while the session started (§E.21.1).
                    if !self.ext.names().is_empty()
                        && self.cfg.audio_channels != self.channels_requested
                    {
                        let list = self
                            .cfg
                            .audio_channels
                            .as_deref()
                            .map(crate::ext::channel_list)
                            .unwrap_or_default();
                        if let Some(s) = self.conns.get_mut(&self.active) {
                            let _ = send_text(
                                s,
                                kind::CONTROL,
                                &format!(r#"<x_idn_channels list="{list}"/>"#),
                            );
                        }
                        self.channels_requested = self.cfg.audio_channels.clone();
                    }
                    // A size changed while the session started.
                    if !self.ext.names().is_empty() && self.cfg.max_size != self.size_requested {
                        let (w, h) = self.cfg.max_size.unwrap_or((0, 0));
                        if let Some(s) = self.conns.get_mut(&self.active) {
                            let _ = send_text(
                                s,
                                kind::CONTROL,
                                &format!(r#"<x_idn_size width="{w}" height="{h}"/>"#),
                            );
                        }
                        self.size_requested = self.cfg.max_size;
                    }
                    let mut names = self.ext.names();
                    let st = self.queue.stats.lock_ok().clone();
                    match st.link_mode {
                        // The link, and the authentication it runs with (§E.11.7).
                        Some(m) => {
                            names.push(crate::link::TOKEN.into());
                            // One stream per frame, when both sides have it on (§E.13.1).
                            if self.cfg.frame_streams
                                && e.attr("x_idn_ext").is_some_and(|l| {
                                    l.split_whitespace().any(|t| t == crate::link::FRAMES_TOKEN)
                                })
                            {
                                names.push(crate::link::FRAMES_TOKEN.into());
                            }
                            // Encrypted multicast, when this input takes multicast.
                            if self.cfg.transports.contains(&Transport::Multicast)
                                && e.attr("x_idn_ext").is_some_and(|l| {
                                    l.split_whitespace().any(|t| t == crate::mcast::TOKEN)
                                })
                            {
                                names.push(crate::mcast::TOKEN.into());
                            }
                            match m {
                                "s1" => names.push("secure/1".into()),
                                "s2" => names.push("secure/2".into()),
                                _ => {}
                            }
                            names.sort();
                        }
                        None if st.encrypted => {
                            names.push(
                                if st.site_trust {
                                    "secure/2"
                                } else {
                                    "secure/1"
                                }
                                .into(),
                            );
                            names.sort();
                        }
                        None => {}
                    }
                    // Changed regions, when both sides have it on (§E.15.1).
                    self.regions_peer = e
                        .attr("x_idn_ext")
                        .is_some_and(|l| l.split_whitespace().any(|t| t == crate::regions::TOKEN));
                    if self.cfg.regions && self.regions_peer {
                        names.push(crate::regions::TOKEN.into());
                        names.sort();
                    }
                    // Compressed video offered, and this input takes it (§E.18.2).
                    if self
                        .cfg
                        .codec
                        .as_ref()
                        .is_some_and(|c| c.use_ != crate::codec::CodecUse::Never)
                        && e.attr("x_idn_ext")
                            .is_some_and(|l| l.split_whitespace().any(|t| t == crate::codec::TOKEN))
                    {
                        names.push(crate::codec::TOKEN.into());
                        names.sort();
                    }
                    self.stat(|s| s.extensions = names);
                    // A reconnect: tell the sender who we were talking to (§E.4).
                    if self.ext.has("resume")
                        && let Some(prev) = self.resume_from.take()
                    {
                        let m = crate::ext::resume(&prev, self.hx_last_counter);
                        self.log(format!("-> kind 1: {m}"));
                        self.send_active(kind::CONTROL, &m);
                    }
                }
                "x_idn_color" if self.ext.has("color") => {
                    self.color
                        .get_or_insert_with(Default::default)
                        .apply_x_idn_color(&e);
                    return;
                }
                "x_idn_bye" if self.ext.has("resume") => {
                    let d = crate::ext::parse_bye(&e);
                    self.log(format!("source is leaving: {d:?}"));
                    self.bye = Some(d);
                    return;
                }
                "x_idn_sender_stats" if self.ext.has("stats") => {
                    let r = crate::ext::SenderReport::parse(&e);
                    self.stat(|s| s.sender_report = Some(r));
                    return;
                }
                "x_idn_pong" if self.ext.has("stats") => {
                    if let Some(t) = e.attr("t").and_then(|t| t.parse::<u64>().ok()) {
                        let rtt_us = (self.started.elapsed().as_micros() as u64).saturating_sub(t);
                        self.stat(|s| s.rtt_ms = Some(rtt_us as f64 / 1000.0));
                        // The sender's clock, for sync (§E.14.3).
                        let num = |k: &str| e.attr(k).and_then(|v| v.parse::<i64>().ok());
                        if let (Some(s), Some(id), Some(rt)) =
                            (&mut self.sync, num("id"), num("rt"))
                        {
                            s.pong(id as u64, rtt_us as i64, rt);
                        }
                    }
                    return;
                }
                // The sync group's delay, as the sender announces it (§E.14.4).
                "x_idn_sync" if from_active => {
                    let delay = e.attr("delay_ms").and_then(|v| v.parse::<f64>().ok());
                    if let (Some(s), Some(g), Some(d)) = (&mut self.sync, e.attr("group"), delay) {
                        s.announced(g, d);
                    }
                    return;
                }
                "x_idn_ping" if self.ext.has("stats") => {
                    let m = crate::ext::pong_for(&e);
                    self.send_active(kind::CONTROL, &m);
                    return;
                }
                // Extension messages are never shown as metadata.
                n if n.starts_with("x_idn_") => return,
                _ => {}
            }
        }
        if let Some(e) = parse_element(&xml).filter(|e| e.name == "ndi_routing") {
            // A routing source names the source to use (§1.8).
            let c = from_conn.unwrap_or(self.active);
            match e.attr("ip").and_then(|a| a.parse::<SocketAddr>().ok()) {
                Some(t)
                    if (c == self.active && self.routing.is_none()) || Some(c) == self.routing =>
                {
                    if self.routing.is_none() {
                        self.routing = Some(c);
                        let off = control::EnabledStreams::default().to_xml();
                        if let Some(rs) = self.conns.get_mut(&c) {
                            let _ = send_text(rs, kind::METADATA, &off);
                        }
                    } else if let Some(old) = self.conns.remove(&self.active) {
                        let _ = old.shutdown(std::net::Shutdown::Both);
                    }
                    let name = e.attr("name").unwrap_or("?").to_owned();
                    match self.open_session(t) {
                        Ok(()) => {
                            self.router_gone = None;
                            self.routed_at = Some(Instant::now());
                            self.emit(InputEvent::State(ConnectionState::Routed { name, addr: t }));
                            // Connected, as to any source: programs that count only that
                            // showed a routed input as down.
                            self.emit(InputEvent::State(ConnectionState::Connected(t)));
                        }
                        Err(e) => {
                            // Retry from the routing source with the normal backoff.
                            self.end_session();
                            self.pending_lost = Some(format!("routed to {name} at {t}: {e}"));
                        }
                    }
                }
                Some(_) => {}
                None => self.log("ndi_routing without a usable ip; ignored"),
            }
        }
        // Only the session's own source may move it to multicast: not a routing source or
        // a connection already replaced.
        if xml.starts_with("<ndi_multicast_v2")
            && self.cfg.transports.contains(&Transport::Multicast)
            && from_conn.is_none_or(|c| c == self.active)
        {
            self.join_multicast(&xml);
        }
        self.emit(InputEvent::Metadata { kind: f.kind, xml });
    }
}

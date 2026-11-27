//! One client's connection: served (with a login, or open), its packets handled until it goes.

use super::*;

impl Broker {
    /// Serves one client connection (TCP, or a WebSocket's bytes) until it ends.
    pub async fn serve<T>(&self, io: T, remote: String)
    where
        T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        if let Err(e) = self.client(io, false, &remote).await {
            idn_open_protocol::log::log(&format!("mqtt: client from {remote}: {e}"));
        }
    }

    /// As [`Broker::serve`], without a login needed (a Node's loopback endpoint: only
    /// this machine reaches it).
    pub async fn serve_open<T>(&self, io: T, remote: String)
    where
        T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        if let Err(e) = self.client(io, true, &remote).await {
            idn_open_protocol::log::log(&format!("mqtt: client from {remote}: {e}"));
        }
    }

    pub(super) async fn client<T>(&self, io: T, open: bool, remote: &str) -> io::Result<()>
    where
        T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (mut rd, mut wr) = tokio::io::split(io);
        let mut buf = BytesMut::with_capacity(4096);
        let (first, v4) =
            tokio::time::timeout(Duration::from_secs(10), read_first(&mut rd, &mut buf))
                .await
                .map_err(|_| err("no CONNECT in time"))??;
        let Packet::Connect(c, will, login) = first else {
            return Err(err("the first packet wasn't CONNECT"));
        };
        let ok = open
            || match &login {
                Some(l) => self.login(&l.username, &l.password, remote).await,
                None => false,
            };
        if !ok {
            let _ = write_packet(
                &mut wr,
                v4,
                &Packet::ConnAck(ConnAck {
                    session_present: false,
                    code: ConnectReturnCode::BadUserNamePassword,
                    properties: None,
                }),
            )
            .await;
            return Err(err("refused: bad user name or password"));
        }
        let mut client_id = c.client_id.clone();
        let assigned = client_id.is_empty();
        // 3.1.1: no id and a session to keep is refused (MQTT-3.1.3-8): the client is never
        // told the id made for it, and each reconnect left another session behind (E8-1).
        if assigned && v4 && !c.clean_start {
            let _ = write_packet(
                &mut wr,
                v4,
                &Packet::ConnAck(ConnAck {
                    session_present: false,
                    code: ConnectReturnCode::ClientIdentifierNotValid,
                    properties: None,
                }),
            )
            .await;
            return Err(err("refused: no client id for a persistent session"));
        }
        if assigned {
            client_id = format!("auto-{}", idn_open_protocol::random::token_hex(8));
        }
        let props = c.properties.clone().unwrap_or_default();
        let asked_expiry = props.session_expiry_interval.unwrap_or(0);
        let expiry_s = asked_expiry.min(MAX_SESSION_EXPIRY);
        let recv_max = props
            .receive_maximum
            .unwrap_or(u16::MAX)
            .clamp(1, IN_FLIGHT);
        let keep_alive = c.keep_alive;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let queued = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let conn = self
            .0
            .next_conn
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let user = login.as_ref().map(|l| l.username.clone()).filter(|_| !open);
        // Another login's session (live or kept): refused, not taken over.
        let owner = self
            .0
            .state
            .lock_ok()
            .sessions
            .get(&client_id)
            .map(|s| s.user.clone());
        if let Some(Some(o)) = owner
            && Some(&o) != user.as_ref()
        {
            let _ = write_packet(
                &mut wr,
                v4,
                &Packet::ConnAck(ConnAck {
                    session_present: false,
                    code: ConnectReturnCode::NotAuthorized,
                    properties: None,
                }),
            )
            .await;
            return Err(err("refused: that client id is another login's"));
        }
        let keep_while_on;
        let session_present = {
            let mut st = self.0.state.lock_ok();
            if let Some(old) = st.live.insert(
                client_id.clone(),
                Live {
                    tx: tx.clone(),
                    conn,
                    queued: queued.clone(),
                },
            ) {
                let _ = old.tx.send(Out::Kick);
            }
            let present = !c.clean_start && st.sessions.contains_key(&client_id);
            if !present {
                st.sessions.insert(client_id.clone(), SessionRec::default());
            }
            let s = st.sessions.get_mut(&client_id).unwrap();
            // A kept session taken up again with no expiry: what was kept for it is
            // delivered on this connection, and forgotten when it ends (it was dropped at
            // once, undelivered; bug round E, E8-7).
            keep_while_on = present && s.expiry_s > 0 && expiry_s == 0;
            if !keep_while_on {
                s.expiry_s = expiry_s;
            }
            s.gone_ms = None;
            if s.user.is_none() {
                s.user = user.clone();
            }
            present
        };
        // Saved and told: an error here (the client gone at once, a full disk) still ends
        // in the clean-up below, not with its live entry and session left for good (bug
        // review 3).
        let setup: io::Result<()> = async {
            if !session_present {
                // A clean start forgets what was kept.
                self.save_session(&client_id, None)?;
            }
            let snapshot = self.0.state.lock_ok().sessions.get(&client_id).cloned();
            self.save_session(&client_id, snapshot.as_ref())?;
            write_packet(
                &mut wr,
                v4,
                &Packet::ConnAck(ConnAck {
                    session_present,
                    code: ConnectReturnCode::Success,
                    properties: Some(ConnAckProperties {
                        // (Told when less than asked for, as MQTT 5 has it.)
                        session_expiry_interval: (expiry_s != asked_expiry).then_some(expiry_s),
                        receive_max: Some(IN_FLIGHT),
                        max_qos: Some(1),
                        retain_available: Some(1),
                        max_packet_size: Some(MAX_PACKET),
                        assigned_client_identifier: assigned.then(|| client_id.clone()),
                        topic_alias_max: Some(0),
                        reason_string: None,
                        user_properties: Vec::new(),
                        wildcard_subscription_available: Some(1),
                        subscription_identifiers_available: Some(0),
                        shared_subscription_available: Some(0),
                        server_keep_alive: None,
                        response_information: None,
                        server_reference: None,
                        authentication_method: None,
                        authentication_data: None,
                    }),
                }),
            )
            .await?;
            Ok(())
        }
        .await;
        // In flight to the client: packet id → the stored message's sequence (0: not
        // stored).
        let mut inflight: BTreeMap<u16, u64> = BTreeMap::new();
        let mut next_pkid: u16 = 1;
        let mut sent_upto = 0u64;
        // QoS 1 messages (not stored) waiting for room in the client's window.
        let mut waiting: std::collections::VecDeque<Msg> = std::collections::VecDeque::new();
        let persistent = expiry_s > 0 || keep_while_on;
        let mut will = will;
        let mut last_heard = tokio::time::Instant::now();
        let limit = if keep_alive > 0 {
            Duration::from_millis(keep_alive as u64 * 1500)
        } else {
            Duration::from_secs(3600 * 24 * 365)
        };
        // Stored messages from before (a returning session) go out first.
        let _ = tx.send(Out::Queued);
        let result: io::Result<bool> = if let Err(e) = setup {
            will = None;
            Err(e)
        } else {
            loop {
                let deadline = last_heard + limit;
                tokio::select! {
                    p = read_packet(&mut rd, &mut buf, v4) => {
                        let p = match p { Ok(p) => p, Err(e) => break Err(e) };
                        last_heard = tokio::time::Instant::now();
                        match p {
                            Packet::Publish(p) => {
                                if p.qos == QoS::ExactlyOnce {
                                    let _ = write_packet(&mut wr, v4, &disconnect(DisconnectReasonCode::QoSNotSupported)).await;
                                    break Ok(false);
                                }
                                let m = match Msg::from_publish(&p) {
                                    Ok(m) => m,
                                    Err(e) => break Err(e),
                                };
                                let allowed = mb::valid_topic(&m.topic)
                                    && !self.0.role.reserves(&m.topic);
                                let resent = allowed
                                    && p.qos == QoS::AtLeastOnce
                                    && self.0.state.lock_ok().seen_before(&client_id, p.pkid, &m, p.dup);
                                let reason = if !allowed {
                                    PubAckReason::NotAuthorized
                                } else if resent {
                                    PubAckReason::Success
                                } else {
                                    let kept = self.hook(&m);
                                    let routed = self.route(&m, Some(&client_id), false).is_ok();
                                    // A command (or a local message going up) acked once it is
                                    // kept; not kept (or not in 10 s): an error, and a resend
                                    // isn't taken as a copy.
                                    let kept = match kept {
                                        Some(rx) => tokio::time::timeout(std::time::Duration::from_secs(10), rx)
                                            .await
                                            .is_ok_and(|r| r.unwrap_or(true)),
                                        None => true,
                                    };
                                    if routed && kept {
                                        if p.qos == QoS::AtLeastOnce {
                                            self.0.state.lock_ok().taken(&client_id, p.pkid, &m);
                                        }
                                        PubAckReason::Success
                                    } else {
                                        PubAckReason::UnspecifiedError
                                    }
                                };
                                if p.qos == QoS::AtLeastOnce
                                    && let Err(e) = write_packet(&mut wr, v4, &Packet::PubAck(PubAck { pkid: p.pkid, reason, properties: None })).await
                                {
                                    break Err(e);
                                }
                            }
                            Packet::Subscribe(s) => {
                                let mut codes = Vec::new();
                                let mut send_retained = Vec::new();
                                {
                                    let mut st = self.0.state.lock_ok();
                                    let sess = st.sessions.entry(client_id.clone()).or_default();
                                    for f in &s.filters {
                                        if !mb::valid_filter(&f.path) {
                                            codes.push(SubscribeReasonCode::TopicFilterInvalid);
                                            continue;
                                        }
                                        if f.path.starts_with("$share/") {
                                            codes.push(SubscribeReasonCode::SharedSubscriptionsNotSupported);
                                            continue;
                                        }
                                        let qos = (f.qos as u8).min(1);
                                        let new = !sess.subs.iter().any(|x| x.filter == f.path);
                                        sess.subs.retain(|x| x.filter != f.path);
                                        sess.subs.push(Sub { filter: f.path.clone(), qos, nolocal: f.nolocal, rap: f.preserve_retain });
                                        codes.push(SubscribeReasonCode::Success(if qos == 1 { QoS::AtLeastOnce } else { QoS::AtMostOnce }));
                                        let rule_sends = match f.retain_forward_rule {
                                            RetainForwardRule::OnEverySubscribe => true,
                                            RetainForwardRule::OnNewSubscribe => new,
                                            RetainForwardRule::Never => false,
                                        };
                                        if rule_sends {
                                            send_retained.push((f.path.clone(), qos));
                                        }
                                    }
                                }
                                let snapshot = self.0.state.lock_ok().sessions.get(&client_id).cloned();
                                let _ = self.save_session(&client_id, snapshot.as_ref());
                                if let Err(e) = write_packet(&mut wr, v4, &Packet::SubAck(SubAck { pkid: s.pkid, return_codes: codes, properties: None })).await {
                                    break Err(e);
                                }
                                // Each filter's retained messages once a packet, counted as
                                // any delivery is: past MAX_BEHIND the client is too far
                                // behind (one SUBSCRIBE naming `#` a thousand times copied the
                                // whole retained store a thousand times; bug review 3).
                                let mut seen = std::collections::BTreeSet::new();
                                let mut behind = false;
                                'filters: for (filter, qos) in send_retained {
                                    if !seen.insert(filter.clone()) {
                                        continue;
                                    }
                                    for mut m in self.retained(&filter) {
                                        m.qos = m.qos.min(qos);
                                        m.retain = true;
                                        let n = m.payload.len();
                                        if queued.fetch_add(n, std::sync::atomic::Ordering::Relaxed) + n > MAX_BEHIND {
                                            behind = true;
                                            break 'filters;
                                        }
                                        let _ = tx.send(Out::Deliver(m));
                                    }
                                }
                                if behind {
                                    let _ = write_packet(&mut wr, v4, &disconnect(DisconnectReasonCode::QuotaExceeded)).await;
                                    break Ok(false);
                                }
                                let _ = self.0.hooks.send(Hook::Subscribed);
                            }
                            Packet::Unsubscribe(u) => {
                                let mut reasons = Vec::new();
                                {
                                    let mut st = self.0.state.lock_ok();
                                    let sess = st.sessions.entry(client_id.clone()).or_default();
                                    for f in &u.filters {
                                        let had = sess.subs.iter().any(|x| &x.filter == f);
                                        sess.subs.retain(|x| &x.filter != f);
                                        reasons.push(if had { UnsubAckReason::Success } else { UnsubAckReason::NoSubscriptionExisted });
                                    }
                                }
                                let snapshot = self.0.state.lock_ok().sessions.get(&client_id).cloned();
                                let _ = self.save_session(&client_id, snapshot.as_ref());
                                if let Err(e) = write_packet(&mut wr, v4, &Packet::UnsubAck(UnsubAck { pkid: u.pkid, reasons, properties: None })).await {
                                    break Err(e);
                                }
                            }
                            Packet::PubAck(a) => {
                                if let Some(seq) = inflight.remove(&a.pkid)
                                    && seq > 0
                                {
                                    let _ = self.unqueue(&client_id, seq);
                                }
                                // The held ones first, in order, as the window allows (sent
                                // here: through the channel, newer messages already in it
                                // would go before them).
                                let mut failed = None;
                                while inflight.len() < recv_max as usize {
                                    let Some(m) = waiting.pop_front() else { break };
                                    queued.fetch_sub(m.payload.len().min(queued.load(std::sync::atomic::Ordering::Relaxed)), std::sync::atomic::Ordering::Relaxed);
                                    if m.expired() { continue; }
                                    let id = take_pkid(&mut next_pkid, &inflight);
                                    inflight.insert(id, 0);
                                    if let Err(e) = write_packet(&mut wr, v4, &Packet::Publish(m.to_publish(id, false))).await {
                                        failed = Some(e);
                                        break;
                                    }
                                }
                                if let Some(e) = failed {
                                    break Err(e);
                                }
                                let _ = tx.send(Out::Queued);
                            }
                            Packet::PingReq(_) => {
                                if let Err(e) = write_packet(&mut wr, v4, &Packet::PingResp(PingResp)).await {
                                    break Err(e);
                                }
                            }
                            Packet::Disconnect(d) => {
                                // A normal end: no will. Any other reason (0x04 "with will",
                                // an error) publishes it (it was skipped for every DISCONNECT;
                                // bug round E, E8-6).
                                let normal = d.reason_code == DisconnectReasonCode::NormalDisconnection;
                                if normal {
                                    will = None;
                                }
                                // (Not for a session that began with expiry 0: MQTT 5 makes
                                // that a protocol error, and a clean session made lasting
                                // here kept up to 10 000 messages for a week, each time;
                                // bug run 2026-11-25.)
                                if let Some(e) = d.properties.and_then(|p| p.session_expiry_interval)
                                    && persistent
                                    && let Some(s) = self.0.state.lock_ok().sessions.get_mut(&client_id)
                                {
                                    s.expiry_s = e.min(MAX_SESSION_EXPIRY);
                                }
                                break Ok(normal);
                            }
                            _ => {
                                let _ = write_packet(&mut wr, v4, &disconnect(DisconnectReasonCode::ProtocolError)).await;
                                break Ok(false);
                            }
                        }
                    }
                    out = rx.recv() => {
                        let Some(out) = out else { break Ok(false) };
                        match out {
                            Out::Kick => {
                                let _ = write_packet(&mut wr, v4, &disconnect(DisconnectReasonCode::SessionTakenOver)).await;
                                will = None;
                                break Ok(true);
                            }
                            Out::Shutdown => {
                                let _ = write_packet(&mut wr, v4, &disconnect(DisconnectReasonCode::ServerShuttingDown)).await;
                                break Ok(false);
                            }
                            Out::Behind => {
                                let _ = write_packet(&mut wr, v4, &disconnect(DisconnectReasonCode::QuotaExceeded)).await;
                                break Ok(false);
                            }
                            Out::LoginGone => {
                                let _ = write_packet(&mut wr, v4, &disconnect(DisconnectReasonCode::NotAuthorized)).await;
                                break Ok(false);
                            }
                            Out::Deliver(m) => {
                                // (Its bytes stay counted while it waits for room in the
                                // client's window: a client that never acknowledged held up
                                // to 10 000 messages of any size past MAX_BEHIND; bug review
                                // 3.)
                                let done = |m: &Msg| {
                                    queued.fetch_sub(m.payload.len().min(queued.load(std::sync::atomic::Ordering::Relaxed)), std::sync::atomic::Ordering::Relaxed);
                                };
                                if m.expired() { done(&m); continue; }
                                let pkid = if m.qos >= 1 {
                                    // Behind any already held: one topic's messages stay in
                                    // order.
                                    if inflight.len() >= recv_max as usize || !waiting.is_empty() {
                                        // Over the client's window: later (a bounded wait;
                                        // a client this far behind loses the oldest).
                                        if waiting.len() >= 10_000 && let Some(old) = waiting.pop_front() { done(&old); }
                                        waiting.push_back(m);
                                        continue;
                                    }
                                    let id = take_pkid(&mut next_pkid, &inflight);
                                    inflight.insert(id, 0);
                                    id
                                } else { 0 };
                                done(&m);
                                if let Err(e) = write_packet(&mut wr, v4, &Packet::Publish(m.to_publish(pkid, false))).await {
                                    break Err(e);
                                }
                            }
                            Out::Queued => {
                                if !persistent { continue; }
                                let room = (recv_max as usize).saturating_sub(inflight.len());
                                if room == 0 { continue; }
                                let batch = match self.queued(&client_id, sent_upto, room) {
                                    Ok(b) => b,
                                    Err(e) => break Err(e),
                                };
                                let mut failed = None;
                                for (seq, m) in batch {
                                    sent_upto = seq;
                                    if m.expired() {
                                        let _ = self.unqueue(&client_id, seq);
                                        continue;
                                    }
                                    let id = take_pkid(&mut next_pkid, &inflight);
                                    inflight.insert(id, seq);
                                    if let Err(e) = write_packet(&mut wr, v4, &Packet::Publish(m.to_publish(id, false))).await {
                                        failed = Some(e);
                                        break;
                                    }
                                }
                                if let Some(e) = failed { break Err(e); }
                            }
                        }
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        let _ = write_packet(&mut wr, v4, &disconnect(DisconnectReasonCode::KeepAliveTimeout)).await;
                        break Ok(false);
                    }
                }
            }
        };
        let _ = wr.shutdown().await;
        // Gone: the will (an end without DISCONNECT), the session kept or forgotten.
        if let Some(w) = will.filter(|_| !matches!(result, Ok(true))) {
            let topic = String::from_utf8_lossy(&w.topic).into_owned();
            if mb::valid_topic(&topic) && !self.0.role.reserves(&topic) {
                let _ = self.route(
                    &Msg::new(&topic, w.message.clone(), (w.qos as u8).min(1), w.retain),
                    Some(&client_id),
                    false,
                );
            }
        }
        let snapshot = {
            let mut st = self.0.state.lock_ok();
            if st.live.get(&client_id).is_some_and(|l| l.conn == conn) {
                st.live.remove(&client_id);
                if keep_while_on && let Some(s) = st.sessions.get_mut(&client_id) {
                    s.expiry_s = expiry_s;
                }
                match st.sessions.get_mut(&client_id) {
                    Some(s) if s.expiry_s > 0 => {
                        s.gone_ms = Some(now_ms());
                        Some(s.clone())
                    }
                    _ => {
                        st.sessions.remove(&client_id);
                        None
                    }
                }
            } else {
                // Taken over: the new connection owns the session.
                return result.map(|_| ());
            }
        };
        self.save_session(&client_id, snapshot.as_ref())?;
        result.map(|_| ())
    }
}

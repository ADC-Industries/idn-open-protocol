//! The stages of a session's frame loop: whether trust moved on, health reports, this
//! machine's rings, sending on the path, the codec, and opening the media path again.

use super::*;

/// Whether the session should end because trust moved on, once a second: site trust
/// became available to a receiver of our site that connected before we were enrolled (it
/// reconnects with it, §E.10.7), or "require encryption" was switched on (locally or by
/// the site) for an unencrypted session (the receiver is refused when it reconnects).
pub(super) fn trust_moved_on(cx: &Ctx, receiver_ca: &Option<String>) -> bool {
    let Ctx {
        cfg,
        peer,
        session_creds,
        secure,
        ..
    } = *cx;
    let using_site = session_creds
        .as_ref()
        .is_some_and(|c| c.mode() == "secure/2");
    let our_ca = cfg
        .secure
        .as_ref()
        .and_then(|s| s.site.as_ref())
        .and_then(|s| s.ca_id());
    if !using_site && our_ca.is_some() && our_ca == *receiver_ca {
        crate::log::log(&format!(
            "receiver {peer}: site trust is now available; closing the session so it reconnects with it"
        ));
        return true;
    }
    if !secure && cfg.secure.as_ref().is_some_and(|s| s.requires()) {
        crate::log::log(&format!(
            "receiver {peer}: \"require encryption\" is now on; closing the unencrypted session"
        ));
        return true;
    }
    false
}

/// Health reporting (§E.5): the pictures sent since the last report, those encoded for
/// this receiver and their encode time, and their bytes; a report about once a second.
pub(super) struct Health {
    pub(super) frames: u64,
    pub(super) enc: f64,
    pub(super) bytes: u64,
    pub(super) tx: u64,
    pub(super) since: Instant,
}

impl Health {
    pub(super) fn new() -> Health {
        Health {
            frames: 0,
            enc: 0.0,
            bytes: 0,
            tx: 0,
            since: Instant::now(),
        }
    }

    /// A frame with a picture sent, of `bytes`; encoded for this receiver from `encoding`.
    pub(super) fn picture(&mut self, encoding: Option<Instant>, bytes: usize) {
        if let Some(t) = encoding {
            self.frames += 1;
            self.enc += t.elapsed().as_secs_f64();
        }
        self.bytes += bytes as u64;
        self.tx += 1;
    }

    /// The report, once a second, to a receiver that takes them (`stats`).
    pub(super) fn report(&mut self, cx: &Ctx, quality: u8) {
        if !cx.client.ext.lock_ok().has("stats")
            || self.since.elapsed() < Duration::from_secs(1)
        {
            return;
        }
        let secs = self.since.elapsed().as_secs_f64();
        let report = crate::ext::SenderReport {
            v_tx: self.tx,
            v_skipped: cx.client.skipped.load(Ordering::Relaxed),
            enc_ms: self.enc * 1000.0 / self.frames.max(1) as f64,
            q: quality,
            mbps: self.bytes as f64 * 8.0 / secs / 1e6,
        };
        let _ = send_text(cx.w, kind::CONTROL, &report.to_xml());
        (self.frames, self.enc, self.bytes, self.since) = (0, 0.0, 0, Instant::now());
    }
}

/// This machine's rings for one receiver (§E.26): the use of them it keeps, the generation
/// offered, and the one taken with whether its pictures and sound left the connection
/// yet (each from the first one written in the rings after the receiver took them, told
/// with `x_idn_machine_from`: before it, the receiver had them over the connection).
pub(super) struct Rings<'a> {
    /// (Before `ring_use`: dropped before it.)
    pub(super) take: Option<Take<'a>>,
    pub(super) ring_use: Option<RingUse<'a>>,
    pub(super) offered: Option<u64>,
    pub(super) switched: (u64, bool, bool),
}

/// A receiver on this machine that took the rings (§E.26): the picture and the sound go
/// through them, each written once for every such receiver, the receiver's watcher woken
/// by the doorbell; nothing for them goes on the connection. Not for what the rings don't
/// carry (HX video or AAC, fields, high precision, a smaller size asked for, multicast).
/// The rings are offered with the first item they take. Gives what is left for the
/// connection, when the rings carry anything.
pub(super) fn through_rings<'a>(
    cx: &Ctx<'a>,
    wn: &Wants,
    f: &Arc<MediaFrame>,
    path: &Option<Path>,
    rings: &mut Rings<'a>,
    regions: &mut Option<Regions>,
) -> io::Result<Option<Wants>> {
    let Ctx {
        client,
        cache,
        w,
        this_machine,
        ..
    } = *cx;
    let mut via_machine: Option<Wants> = None;
    let multicast = matches!(path, Some(Path::Multicast));
    if this_machine
        && !multicast
        && client.ext.lock_ok().has("machine")
        && !client.machine_refused.load(Ordering::Relaxed)
    {
        if rings.ring_use.is_none() {
            rings.ring_use = Some(RingUse::new(cache));
        }
        let pic = if wn.video
            && !wn.low
            && wn.size.is_none()
            && f.hx_video.is_none()
            && !f.planes.is_empty()
            && !f.interlaced
            && f.precise.is_none()
        {
            cache.machine_picture(f)
        } else {
            InRing::No
        };
        let sound = if wn.audio && !f.audio.is_empty() && f.hx_audio.is_none() {
            cache.machine_sound(f)
        } else {
            InRing::No
        };
        let written = matches!(pic, InRing::At(_)) || matches!(sound, InRing::At(_));
        if written
            && let Some((generation, offer)) = cache.machine_offer()
            && rings.offered != Some(generation)
        {
            send_text(w, kind::CONTROL, &offer)?;
            rings.offered = Some(generation);
        }
        if (pic.there() || sound.there())
            && let Some(generation) = rings.offered
        {
            let taken = client.machine_ok.lock_ok().as_deref()
                == Some(generation.to_string().as_str());
            let mode = taken.then(|| client.machine_gpu.load(Ordering::Relaxed));
            if rings.take.as_ref().map(|t| t.gpu) != mode {
                rings.take = mode.map(|gpu| Take::new(cache, gpu));
            }
            if taken {
                if rings.switched.0 != generation {
                    rings.switched = (generation, false, false);
                }
                let mut from = String::new();
                if let (InRing::At(seq), false) = (pic, rings.switched.1) {
                    from += &format!(r#" pic="{seq}""#);
                    rings.switched.1 = true;
                }
                if let (InRing::At(seq), false) = (sound, rings.switched.2) {
                    from += &format!(r#" sound="{seq}""#);
                    rings.switched.2 = true;
                }
                if !from.is_empty() {
                    send_text(
                        w,
                        kind::CONTROL,
                        &format!(r#"<x_idn_machine_from gen="{generation}"{from}/>"#),
                    )?;
                }
                let pic = rings.switched.1 && pic.there();
                let sound = rings.switched.2 && sound.there();
                if !f.planes.is_empty() {
                    client.ring_pictures.store(pic, Ordering::Relaxed);
                }
                if pic {
                    client.totals.video_frames.fetch_add(1, Ordering::Relaxed);
                    if let Some(r) = regions.as_mut() {
                        r.sent = None; // the next picture on the connection is whole
                    }
                }
                via_machine = Some(Wants {
                    video: wn.video && !pic,
                    audio: wn.audio && !sound,
                    ..wn.clone()
                });
            }
        }
    } else if client.machine_refused.load(Ordering::Relaxed) {
        // Refused (another account, or not wanted): the rings aren't kept for it.
        rings.take = None;
        rings.ring_use = None;
    }
    Ok(via_machine)
}

/// What a path keeps between frames: UDP's message sequences, the link's abandoned
/// frames seen, and since when UDP sends have been failing.
#[derive(Default)]
pub(super) struct PathCounters {
    pub(super) vseq: u16,
    pub(super) aseq: u16,
    pub(super) abandoned_seen: u64,
    pub(super) udp_failing_since: Option<Instant>,
}

/// One frame's messages sent on the receiver's path. Gives whether the picture got
/// through (not skipped as the receiver is behind, §E.5 `v_skipped`).
pub(super) fn send_on_path(
    cx: &Ctx,
    path: &mut Path,
    f: &MediaFrame,
    msgs: Vec<(bool, Vec<u8>)>,
    has_video: bool,
    counters: &mut PathCounters,
    hx_started: &mut bool,
) -> io::Result<bool> {
    let Ctx {
        client,
        w,
        sync_member,
        trouble_fp,
        ..
    } = *cx;
    let mut through = true;
    match path {
        Path::Link(m) => {
            if let Some(r) = m.failed() {
                return Err(io::Error::other(format!("link media: {r}")));
            }
            // Per-frame streams (§E.13.2): two frame periods to arrive, or the
            // receiver's sync delay if that is longer.
            let fps = f.frame_rate.0 as f64 / f.frame_rate.1.max(1) as f64;
            if fps > 0.0 {
                let two = Duration::from_secs_f64(2.0 / fps);
                let synced = crate::sync::member_delay(sync_member.0).unwrap_or_default();
                m.set_deadline(two.max(synced));
            }
            // Frames abandoned at their deadline count as skipped (§E.5).
            let gone = m.abandoned();
            if gone > counters.abandoned_seen {
                client
                    .skipped
                    .fetch_add(gone - counters.abandoned_seen, Ordering::Relaxed);
                crate::channel::video_trouble(trouble_fp.as_deref());
                counters.abandoned_seen = gone;
            }
            // A receiver that is behind skips frames (§E.5 `v_skipped`).
            let mut skipped = false;
            for (is_video, m2) in msgs {
                if !m.send(is_video, m2) {
                    if is_video {
                        skipped = true;
                    } else {
                        client.audio_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            through &= !skipped;
            if skipped {
                client.skipped.fetch_add(1, Ordering::Relaxed);
                crate::channel::video_trouble(trouble_fp.as_deref()); // §E.12.5
            }
        }
        Path::Squic(s) => {
            if let Some(r) = s.failed() {
                return Err(io::Error::other(format!("secure QUIC media: {r}")));
            }
            // A receiver that is behind skips frames (§E.5 `v_skipped`).
            let mut skipped = false;
            for (is_video, m) in msgs {
                if !s.send(is_video, m) {
                    if is_video {
                        skipped = true;
                    } else {
                        client.audio_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            through &= !skipped;
            if skipped {
                client.skipped.fetch_add(1, Ordering::Relaxed);
                crate::channel::video_trouble(trouble_fp.as_deref()); // §E.12.5
            }
        }
        Path::Inline => {
            let mut s = w.lock_ok();
            for (_, m) in &msgs {
                s.write_all(m)?;
            }
        }
        Path::Quic { tx, failed, .. } => {
            if let Some(r) = failed.lock_ok().clone() {
                return Err(io::Error::other(r));
            }
            // One item per frame: a receiver that is behind loses whole frames, never
            // parts of one (SE-10), and the loss is counted (§E.5 `v_skipped`).
            let hx_key = f.hx_video.as_ref().map(|h| h.1);
            let frame = QuicFrame { msgs, hx_key };
            if tx.try_send(frame).is_err() && has_video {
                through = false;
                client.skipped.fetch_add(1, Ordering::Relaxed);
                // HX: a lost frame breaks the stream until a keyframe (§7.1.4).
                *hx_started = *hx_started && hx_key.is_none();
            }
        }
        Path::Multicast => {}
        Path::Tcp { video, audio } => {
            for (is_video, m) in &msgs {
                let (conns, seq) = if *is_video { &mut *video } else { &mut *audio };
                // Stripe by the running chunk sequence, so single-chunk messages alternate too.
                let first = *seq as usize;
                for (i, c) in tcp::chunkify(m, seq).iter().enumerate() {
                    conns[(first + i) % 2].write_all(c)?;
                }
                // One tick per media message on each connection (§6.2.2).
                for c in conns.iter_mut() {
                    c.write_all(&tcp::TICK)?;
                }
            }
        }
        Path::Udp { sock, video, audio } => {
            for (is_video, m) in &msgs {
                let ((to, id, _), seq) = if *is_video {
                    (*video, &mut counters.vseq)
                } else {
                    (*audio, &mut counters.aseq)
                };
                let packets = udp::packetize(m, id, seq);
                // A transient error (ENOBUFS under load, a moment without a route)
                // loses packets, which FEC or the next frame covers; only an error
                // lasting UDP_FAILING ends the session.
                match crate::transport::send_batch(sock, to, &packets) {
                    Ok(()) => counters.udp_failing_since = None,
                    Err(e) => {
                        let since = *counters.udp_failing_since.get_or_insert_with(Instant::now);
                        if since.elapsed() >= UDP_FAILING {
                            return Err(e);
                        }
                    }
                }
            }
        }
    }
    Ok(through)
}

/// This receiver's video from the output's shared encoder (§E.18.2) or as SpeedHQ, chosen
/// on frames with a picture; keyframe requests passed on to the encoder (§7.1.4).
pub(super) struct Coding {
    /// The shared encoder its video comes from (§E.18.3).
    pub(super) coded: Option<(Arc<EncSlot>, EncKey)>,
    /// When it moved there (for the pictures still queued from before).
    pub(super) since: Instant,
    pub(super) keyframes_seen: usize,
    /// `q_change_slow` sent (§7.1.2): at identify for an HX source.
    pub(super) told_hx: bool,
}

/// The encoder (or none) for this receiver now: not on multicast nor with per-frame streams
/// (§E.18.3), nor for one on this machine that can take the rings (§E.26).
pub(super) fn choose_codec(
    cx: &Ctx,
    wn: &Wants,
    f: &MediaFrame,
    path: &Option<Path>,
    ring_candidate: bool,
    coding: &mut Coding,
    hx_started: &mut bool,
) {
    let Ctx {
        cfg,
        client,
        cache,
        w,
        peer,
        frames,
        link_conn,
        keyframe_requests,
        ..
    } = *cx;
    // (A candidate drops an encoder it had, e.g. from a smaller size asked for before.)
    if let Some(hub) = cache.codec.get().filter(|_| f.width > 0 && f.height > 0) {
        let per_frame = link_conn.is_some() && cfg.frame_streams && wn.frames;
        let want = (!matches!(path, Some(Path::Multicast)) && !per_frame && !ring_candidate)
            .then(|| {
                // NDI receivers: NDI|HX when the output (else the machine) says so.
                if wn.codecs.is_empty() && client.plain_ndi.load(Ordering::Relaxed) {
                    let ndi = hub.out.ndi.or(crate::node::video::machine().ndi);
                    return crate::codec::choose_ndi(
                        ndi.unwrap_or(crate::codec::NdiVideo::SpeedHq),
                        &hub.offered,
                        client.hevc_ok.load(Ordering::Relaxed),
                    );
                }
                // An encoder already running for another receiver, rather than a
                // second encode or SpeedHQ (§E.18.9); not for an input that only allows
                // compressed video when the output offers it only to those that prefer it.
                let joins = match wn.codec_use.as_deref() {
                    Some("prefer") => true,
                    Some("allow") => hub.out.policy != crate::codec::CodecPolicy::Offer,
                    _ => false,
                };
                // The source's own compressed pictures in a codec it decodes, at their
                // size: passed through, rather than its first choice encoded (a 4K
                // HEVC file was decoded and encoded again as H.264 for receivers on
                // default settings).
                let (w, h) = target_size(f, wn);
                if let Some(c) = hub
                    .passing()
                    .filter(|c| joins && wn.codecs.contains(c) && (w, h) == (f.width, f.height))
                {
                    return Some(c);
                }
                if let Some(c) = joins
                    .then(|| {
                        let current = coding.coded.as_ref().map(|c| c.1.codec);
                        let running = hub.running(wn.size, wn.low);
                        crate::codec::join_running(&wn.codecs, &running, current)
                    })
                    .flatten()
                {
                    return Some(c);
                }
                crate::codec::choose(
                    &hub.out,
                    &hub.offered,
                    &wn.codecs,
                    wn.codec_use.as_deref(),
                    w * h,
                )
            })
            .flatten()
            .map(|codec| EncKey {
                codec,
                size: wn.size,
                low: wn.low,
            });
        if coding.coded.as_ref().map(|c| c.1) != want {
            if let Some((slot, _)) = coding.coded.take() {
                hub.unsubscribe(&slot, &frames.0);
            }
            if let Some(k) = want {
                crate::log::log(&format!("receiver {peer}: {} video", k.codec.name()));
                // An NDI receiver is told it is getting HX (§7.1.2), once.
                if wn.codecs.is_empty() && !std::mem::replace(&mut coding.told_hx, true) {
                    let _ = send_text(
                        w,
                        kind::METADATA,
                        r#"<ndi_capabilities q_change_slow="true"/>"#,
                    );
                }
                coding.coded = Some((hub.subscribe(k, frames.0.clone()), k));
                coding.since = Instant::now();
                *hx_started = false;
            }
            client
                .coded
                .store(coding.coded.is_some(), Ordering::Relaxed);
            *client.slot.lock_ok() = coding.coded.as_ref().map(|c| c.0.clone());
        }
        // Keyframe requests reach the shared encoder (§7.1.4).
        let asked = keyframe_requests.load(Ordering::Relaxed);
        if asked != coding.keyframes_seen {
            coding.keyframes_seen = asked;
            if let Some((slot, _)) = &coding.coded {
                slot.keyframe.store(true, Ordering::Relaxed);
            }
        }
    }
}

/// The receiver's media path opened again, when the transports it asked for changed: the
/// link's streams or multicast (§E.11.5), secure QUIC or media inline (§E.8.4), or QUIC,
/// UDP, TCP or multicast (§6). Gives the frame to go on with: after connecting securely,
/// the newest.
pub(super) fn open_media(
    cx: &Ctx,
    wn: &Wants,
    f: Arc<MediaFrame>,
    path: &mut Option<Path>,
    mc_member: &mut Option<crate::mcast::Member>,
    mc_keys_sent: &mut u64,
    set_mc: &mut dyn FnMut(Option<bool>),
) -> io::Result<Arc<MediaFrame>> {
    let Ctx {
        cfg,
        client,
        w,
        peer,
        frames,
        link_conn,
        multicast_announcement,
        session_creds,
        secure,
        ..
    } = *cx;
    if let Some(lc) = link_conn {
        // The link's media streams, or multicast if the receiver asked for it and
        // this output sends it (§E.11.5).
        // Per-receiver mode keeps IDN receivers on the link; encrypted mode needs a
        // receiver that takes it (§E.17.1).
        let ann = multicast_announcement.as_ref().filter(|a| match a.mode {
            crate::mcast::Mode::Plain => true,
            crate::mcast::Mode::Encrypted => wn.mc_encrypted,
            crate::mcast::Mode::PerReceiver => false,
        });
        *path = Some(match (ann, wn.multicast) {
            (Some(ann), true) => {
                // The keys first, then where to find the stream (§E.17.2).
                if let Some(g) = &ann.group {
                    for m in g.key_messages() {
                        send_text(w, kind::CONTROL, &m)?;
                    }
                    *mc_keys_sent = g.version();
                    *mc_member = Some(g.join());
                }
                send_text(w, kind::CONTROL, &ann.xml)?;
                set_mc(Some(wn.low));
                Path::Multicast
            }
            _ => {
                set_mc(None);
                *mc_member = None;
                // One stream per frame when both sides want it (§E.13.1).
                let per_frame = cfg.frame_streams && wn.frames;
                if per_frame {
                    crate::log::log(&format!("receiver {peer}: one stream per video frame"));
                }
                Path::Link(lc.media(per_frame))
            }
        });
    } else if secure {
        // secure/1 (§E.8.4): secure QUIC if the receiver offers it and it answers in
        // time, else media inline on the control connection, which is inside TLS.
        // Never unencrypted; a QUIC failure other than silence ends the session.
        *path = Some(match &wn.squic {
            None => Path::Inline,
            Some(offer) => {
                let key = session_creds
                    .as_ref()
                    .expect("an encrypted session has credentials");
                // Frames refused while connecting are expected, not "skipped" (§E.5).
                let skipped = client.skipped.load(Ordering::Relaxed);
                let r = crate::squic::connect(key, peer.ip(), offer, crate::squic::CONNECT_TIMEOUT);
                client.skipped.store(skipped, Ordering::Relaxed);
                match r {
                    Ok(s) => {
                        crate::log::log(&format!(
                            "receiver {peer}: media over secure QUIC (UDP {})",
                            offer.port
                        ));
                        Path::Squic(s)
                    }
                    Err(crate::squic::ConnectError::Unreachable(e)) => {
                        crate::log::log(&format!(
                            "receiver {peer}: encrypted media on the control connection: UDP {} blocked? ({e})",
                            offer.port
                        ));
                        Path::Inline
                    }
                    Err(e) => {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            e.to_string(),
                        ));
                    }
                }
            }
        });
    }
    // Connecting may have taken up to 2 s: continue with the newest frame instead of
    // sending the backlog as a burst.
    let f = if secure {
        let mut f = f;
        while let Some(n) = frames.0.recv_timeout(Duration::ZERO) {
            f = n;
        }
        f
    } else {
        f
    };
    if !secure {
        // Encrypted mode sends no plain copy: others get unicast (§E.17.1).
        let ann = multicast_announcement
            .as_ref()
            .filter(|a| a.mode != crate::mcast::Mode::Encrypted);
        *path = Some(match (ann, wn.multicast) {
            // Multicast is preferred when both sides allow it (§6.1).
            (Some(ann), true) => {
                send_text(w, kind::CONTROL, &ann.xml)?;
                set_mc(Some(wn.low));
                Path::Multicast
            }
            _ => {
                set_mc(None);
                open_path(peer.ip(), wn, client)?
            }
        });
    }
    Ok(f)
}

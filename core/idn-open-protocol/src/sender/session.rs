//! One receiver's session: its control connection, the paths its media take (QUIC,
//! UDP, TCP, multicast, the link), and what is sent each frame.

use super::*;

mod reader;
use reader::Reader;

mod paths;
mod stages;
pub(super) use paths::*;
use stages::*;

/// A session in the sync groups' registry (§E.14.4); it leaves when the session ends.
pub(super) struct SyncMember(pub(super) u64);

impl Drop for SyncMember {
    fn drop(&mut self) {
        crate::sync::leave(self.0);
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn serve(
    sock: TcpStream,
    peer: SocketAddr,
    cfg: SenderConfig,
    frames: FrameRx,
    stop: Arc<AtomicBool>,
    metadata: Arc<Mutex<Vec<(String, String, Trust)>>>,
    multicast_announcement: Option<Arc<McOffer>>,
    mc_demand: Arc<Mutex<(usize, usize)>>,
    cache: Arc<EncodeCache>,
    client: Arc<ClientShared>,
    link: Option<LinkSession>,
) -> io::Result<()> {
    // This receiver's share of multicast demand, released when the session ends.
    let mut my_mc: Option<bool> = None; // Some(low)
    let set_mc = |mine: &mut Option<bool>, new: Option<bool>| {
        let mut d = mc_demand.lock_ok();
        match *mine {
            Some(true) => d.1 -= 1,
            Some(false) => d.0 -= 1,
            None => {}
        }
        match new {
            Some(true) => d.1 += 1,
            Some(false) => d.0 += 1,
            None => {}
        }
        *mine = new;
    };
    // The cleanup below must run even if the session panics (a bug): otherwise the
    // receiver would never see the session end.
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        serve_inner(
            sock,
            peer,
            cfg,
            frames,
            stop,
            metadata,
            multicast_announcement,
            &mut |low| set_mc(&mut my_mc, low),
            &cache,
            &client,
            link,
        )
    }))
    .unwrap_or_else(|_| Err(io::Error::other("session ended by an internal error")));
    set_mc(&mut my_mc, None);
    // Close the control connection for real: the reader thread holds a clone of the
    // socket, so without a shutdown the receiver would never see the session end.
    if let Some(w) = client.writer.lock_ok().take() {
        let _ = w.lock_ok().shutdown(std::net::Shutdown::Both);
    }
    // Its tally no longer counts: the others hear the combined tally without it.
    *client.tally.lock_ok() = (false, false);
    client.tally_hub.update();
    r
}

#[allow(clippy::too_many_arguments)]
pub(super) fn serve_inner(
    sock: TcpStream,
    peer: SocketAddr,
    cfg: SenderConfig,
    frames: FrameRx,
    stop: Arc<AtomicBool>,
    metadata: Arc<Mutex<Vec<(String, String, Trust)>>>,
    multicast_announcement: Option<Arc<McOffer>>,
    set_mc: &mut dyn FnMut(Option<bool>),
    cache: &EncodeCache,
    client: &Arc<ClientShared>,
    link: Option<LinkSession>,
) -> io::Result<()> {
    sock.set_nodelay(true)?;
    // The address the receiver reached us at, before encryption replaces `sock` with a
    // local pipe (a link session's comes from the link).
    let reached = sock.local_addr().ok().map(|a| a.ip());
    // A receiver that vanishes or stops reading is noticed, and can't block the threads
    // writing to it (or the locks they hold) for the kernel's ~15 min retry time.
    crate::transport::keepalive(&sock, Duration::from_secs(3));
    sock.set_write_timeout(Some(crate::transport::WRITE_TIMEOUT))?;
    // Both sides send ndi_version immediately (§1.4).
    let version = announcement(&cfg, cache, link.is_some());
    let (sock, secure, session_creds, peer_version, pre) = match (&link, &cfg.secure) {
        // A link session is inside TLS already: no plain-text start (§E.11.5).
        (Some(ls), _) => {
            let mut sock = sock;
            sock.write_all(&Frame::text(kind::CONTROL, 0, &version).encode())?;
            if let Some(crate::secure::Creds::Device(_, e)) = ls.mode.creds() {
                e.track_session(&ls.peer_cert, &sock, true)?; // a revocation ends it (§E.10.6)
            }
            crate::log::log(&format!(
                "receiver {peer}: session over the IDN link (mode {})",
                ls.mode.name()
            ));
            (sock, true, ls.mode.creds().cloned(), None, Vec::new())
        }
        (None, Some(sc)) => {
            let n = crate::secure::negotiate_sender(sock, sc, &version)?;
            (n.stream, n.secure, n.creds, n.peer_version, n.pre)
        }
        (None, None) => {
            let mut sock = sock;
            sock.write_all(&Frame::text(kind::CONTROL, 0, &version).encode())?;
            (sock, false, None, None, Vec::new())
        }
    };
    let link_conn = link.as_ref().map(|l| l.conn.clone());
    // The receiving device, for slowing file transfers with it (§E.12.5): known on a
    // link (an enrolled device's link key is its site-trust identity).
    let trouble_fp = link.as_ref().map(|l| l.peer_fingerprint.clone());
    // This session in the sync groups' registry (§E.14.4).
    let sync_member = Arc::new(SyncMember(crate::sync::member_id()));
    // Mode open: the receiver's key, checked against the one seen before (§E.11.4).
    let link_open = link
        .as_ref()
        .filter(|l| matches!(l.mode, crate::link::Mode::Open))
        .map(|l| (l.keys.clone(), l.peer_fingerprint.clone()));
    // A receiver of our site that connected before we were enrolled: once we are, end
    // the session so that it reconnects with site trust (§E.10.7).
    let receiver_ca = peer_version
        .as_ref()
        .filter(|v| {
            v.attr("x_idn_ext")
                .is_some_and(|l| l.split_whitespace().any(|t| t == "secure/2"))
        })
        .and_then(|v| v.attr("x_idn_ca").map(str::to_owned));
    let mut upgrade_check = Instant::now();
    // (For encrypted sessions `sock` is now the plaintext pipe to the TLS threads.)
    sock.set_write_timeout(Some(crate::transport::WRITE_TIMEOUT))?;
    let w = Arc::new(Mutex::new(sock.try_clone()?));
    client.encrypted.store(secure, Ordering::Relaxed);
    *client.writer.lock_ok() = Some(w.clone());
    // A routing source sends the receiver on and carries nothing itself (§1.8).
    if let Some(route) = &cfg.route {
        let here = link_conn.as_ref().and_then(|l| l.local_ip()).or(reached);
        return serve_routing(&w, sock, route, here, &stop);
    }
    let wants: Arc<Mutex<Option<Wants>>> = Default::default();
    let keyframe_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let low = Arc::new(AtomicBool::new(false));
    // A size the receiver asked for later (`adapt/1`, §E.16.2): Some(None) = full again.
    let size_req: Arc<Mutex<Option<SizeRequest>>> = Arc::default();
    let mut regions = cfg.regions.then(Regions::default);
    let refresh = regions.as_ref().map(|r| r.refresh.clone());
    let closed = Arc::new(AtomicBool::new(false));
    // Reader: receiver's control and metadata messages.
    {
        let (w, wants, low, closed, cfg, metadata) = (
            w.clone(),
            wants.clone(),
            low.clone(),
            closed.clone(),
            cfg.clone(),
            metadata.clone(),
        );
        let keyframe_requests = keyframe_requests.clone();
        let reader_size = size_req.clone();
        let reader_refresh = refresh.clone();
        let client = client.clone();
        let sc = cfg.secure.clone();
        let reader_creds = session_creds.clone();
        let reader_sync = sync_member.clone();
        let reader_trouble_fp = trouble_fp.clone();
        // Re-sending lost encrypted multicast messages over this link (§E.17.6).
        let mc_repair = multicast_announcement
            .as_ref()
            .and_then(|a| a.group.clone())
            .zip(link_conn.clone())
            .map(|(g, lc)| {
                (
                    g,
                    std::cell::OnceCell::<crate::link::LinkRepairs>::new(),
                    lc,
                )
            });
        let pre = pre.into_iter();
        // A peer that connects and says nothing is dropped after FIRST_MESSAGE.
        let waiting_first = pre.len() == 0 && sock.set_read_timeout(Some(FIRST_MESSAGE)).is_ok();
        // The address this receiver reached, for `%IP%` in ndi_capabilities (§1.5).
        let local_ip = link_conn
            .as_ref()
            .and_then(|l| l.local_ip())
            .or_else(|| sock.local_addr().ok().map(|a| a.ip()))
            .map(|ip| ip.to_string())
            .unwrap_or_default();
        let r = io::BufReader::new(sock);
        Reader {
            w,
            wants,
            low,
            closed,
            cfg,
            metadata,
            keyframe_requests,
            reader_size,
            reader_refresh,
            client,
            sc,
            reader_creds,
            reader_sync,
            reader_trouble_fp,
            mc_repair,
            pre,
            waiting_first,
            local_ip,
            r,
            link_open,
            peer,
        }
        .spawn();
    }
    // Wait for ndi_enabled_streams, then set up the media path.
    let mut path: Option<Path> = None;
    let mut current: Option<Wants> = None;
    let mut hx_started = false;
    // Pictures this session was offered, for `every` (a preview's one in N).
    let mut offered: u64 = 0;
    // A receiver on this machine (§E.26): its pictures may go through the ring; the ring
    // last offered to it.
    let this_machine = crate::machine::is_this_machine(peer.ip());
    let mut rings = Rings {
        take: None,
        ring_use: None,
        offered: None,
        switched: (0, false, false),
    };
    let mut adapt = cfg.adapt_floor.map(|floor| Adapt::new(cfg.quality, floor));
    let mut counters = PathCounters::default();
    let mut rc = RateControl {
        q: cfg.quality as f64,
        max: cfg.quality as f64,
    };
    let mut health = Health::new();
    // On encrypted multicast: this receiver's membership, and the key version it has.
    let mut mc_member: Option<crate::mcast::Member> = None;
    let mut mc_keys_sent = 0u64;
    let mut coding = Coding {
        coded: None,
        since: Instant::now(),
        keyframes_seen: 0,
        told_hx: cfg.hx,
    };
    let cx = Ctx {
        cfg: &cfg,
        client,
        cache,
        w: &w,
        peer,
        frames: &frames,
        link_conn: &link_conn,
        multicast_announcement: &multicast_announcement,
        session_creds: &session_creds,
        keyframe_requests: &keyframe_requests,
        secure,
        this_machine,
        sync_member: &sync_member,
        trouble_fp: &trouble_fp,
    };
    // Sound bundled to 5 ms for a receiver this machine reaches over Wi-Fi or a tunnel (or
    // that asks, `x_idn_audio_ms`); each block as it comes otherwise.
    let slow_path = !this_machine
        && matches!(
            idn_common::net::path_to(peer.ip()),
            idn_common::net::Path::Wireless | idn_common::net::Path::Tunnel
        );
    let mut bundle = super::bundle::Bundle::default();
    let mut ready: std::collections::VecDeque<Arc<MediaFrame>> = Default::default();
    while !stop.load(Ordering::Relaxed) && !closed.load(Ordering::Relaxed) {
        // New group keys go to the receiver before they are used (§E.17.5).
        if let Some(g) = multicast_announcement
            .as_ref()
            .and_then(|a| a.group.as_ref())
            && mc_member.is_some()
            && g.version() != mc_keys_sent
        {
            mc_keys_sent = g.version();
            for m in g.key_messages() {
                send_text(&w, kind::CONTROL, &m)?;
            }
        }
        if upgrade_check.elapsed() >= Duration::from_secs(1) {
            upgrade_check = Instant::now();
            if trust_moved_on(&cx, &receiver_ca) {
                return Ok(());
            }
        }
        let f = match ready.pop_front() {
            Some(f) => f,
            None => {
                let Some(f) = frames.0.recv_timeout(Duration::from_millis(200)) else {
                    // Nothing for a while: sound held for joining goes now.
                    if let Some(f) = bundle.flush() {
                        ready.push_back(f);
                        continue;
                    }
                    // Its encoder stopped (closed from that side): ended, so the receiver
                    // connects again to a new one.
                    if frames.0.is_closed() {
                        crate::log::log(&format!(
                            "receiver {peer}: its compressed video stopped; ending the session"
                        ));
                        return Ok(());
                    }
                    continue;
                };
                let asked = wants
                    .lock_ok()
                    .as_ref()
                    .and_then(|w| w.audio_ms)
                    .unwrap_or(0);
                let min_ms = if slow_path {
                    asked.max(super::bundle::SLOW_PATH_MS)
                } else {
                    asked
                };
                ready.extend(bundle.take(f, min_ms));
                match ready.pop_front() {
                    Some(f) => f,
                    None => continue,
                }
            }
        };
        let mut wn = match wants.lock_ok().clone() {
            Some(w) => w,
            None => continue,
        };
        wn.low = low.load(Ordering::Relaxed);
        if let Some(s) = *size_req.lock_ok() {
            wn.size = s;
        }
        if secure {
            // On a link, multicast stays the receiver's choice (§E.11.5).
            let mc = wn.multicast && link_conn.is_some();
            (wn.quic, wn.udp, wn.tcp, wn.multicast) = (None, None, None, mc);
        }
        wn.audio_f32 &= client.ext.lock_ok().has("audio-f32");
        let changed = current.as_ref().is_none_or(|c| {
            c.quic != wn.quic
                || c.udp != wn.udp
                || c.tcp != wn.tcp
                || c.multicast != wn.multicast
                || c.squic != wn.squic
        });
        if matches!(path, Some(Path::Multicast))
            && current.as_ref().is_some_and(|c| c.low != wn.low)
        {
            set_mc(Some(wn.low));
        }
        let f = if changed {
            open_media(
                &cx,
                &wn,
                f,
                &mut path,
                &mut mc_member,
                &mut mc_keys_sent,
                &mut *set_mc,
            )?
        } else {
            f
        };
        current = Some(wn.clone());
        client.video.store(wn.video, Ordering::Relaxed);
        // Compressed video for this receiver (§E.18.2), except on multicast and with
        // per-frame streams (§E.18.3). Chosen on frames with a picture: the sound an
        // encoder passes on while it starts comes in a frame of 0 × 0, under the large
        // picture of the automatic choice, which dropped the encoder, took it again on the
        // next picture, and so on every picture (a 4K file at 3–9 pictures a second,
        // "receiver …: h264 video" logged each time).
        // A receiver on this machine that can take the ring gets its pictures from there,
        // never from an encoder: nothing is encoded for receivers on this machine alone
        // (§E.26). One that refused the ring is served as from afar.
        // (Only one the ring can serve: the full picture, not a smaller size or the proxy.)
        let ring_candidate = this_machine
            && !wn.low
            && wn.size.is_none()
            && client.ext.lock_ok().has("machine")
            && !client.machine_refused.load(Ordering::Relaxed);
        choose_codec(
            &cx,
            &wn,
            &f,
            &path,
            ring_candidate,
            &mut coding,
            &mut hx_started,
        );
        // A picture queued before the receiver moved to the shared encoder: its video comes
        // from there now, not as SpeedHQ in between. Only just after the move: later, a
        // picture comes from the encoder itself, as SpeedHQ while it can't encode (it
        // won't open, or fails), and must go out (they were all dropped: no picture).
        if coding.coded.is_some()
            && f.hx_video.is_none()
            && !f.planes.is_empty()
            && coding.since.elapsed() < Duration::from_millis(100)
        {
            continue;
        }
        // AAC only to a receiver getting compressed video (§E.19.2).
        wn.aac &= coding.coded.is_some();
        // An HX receiver starts on a keyframe (§7.1.3).
        if let Some((_, key, _)) = &f.hx_video {
            if !hx_started && !key {
                continue;
            }
            hx_started = true;
        }
        let t_enc = Instant::now();
        // Quality follows this receiver (`adapt/1`, §E.16.1), with IDN receivers only.
        if let Some(a) = &mut adapt
            && client.ext.lock_ok().has("stats")
        {
            a.update(client);
        }
        let quality = adapt
            .as_ref()
            .map_or(rc.quality(), |a| rc.quality().min(a.quality()));
        client.quality.store(quality, Ordering::Relaxed);
        // Changed regions only on paths that deliver every frame sent, in order, or skip
        // it whole (§E.15.1); elsewhere the receiver starts again from a whole frame.
        let regions_ok = wn.regions
            && match &path {
                Some(Path::Inline | Path::Squic(_) | Path::Quic { .. } | Path::Tcp { .. }) => true,
                Some(Path::Link(_)) => !(cfg.frame_streams && wn.frames),
                _ => false,
            };
        if let Some(r) = regions.as_mut().filter(|_| !regions_ok) {
            r.sent = None;
        }
        let via_machine = through_rings(&cx, &wn, &f, &path, &mut rings, &mut regions)?;
        // (Pictures only: sound comes in frames of its own.)
        if via_machine.is_none() && !f.planes.is_empty() {
            client.ring_pictures.store(false, Ordering::Relaxed);
        }
        // A picture on the GPU only goes through the rings or not at all: nothing to
        // encode here.
        let mut wants = via_machine.clone().unwrap_or_else(|| wn.clone());
        // This receiver's SpeedHQ from the GPU (the whole picture, new format, no regions):
        // then a picture only there is encoded there.
        // (Not one taking its pictures through the picture ring: it reads samples there.
        // Counted so, a switcher recording on the GPU stopped reading its program back
        // and such a receiver got pictures without samples.)
        // (A proxy or a smaller size too, where the GPU shrinks the picture first.)
        #[cfg(feature = "gpu")]
        let size_ok = frames::gpu_size_ok(&f, &wn);
        #[cfg(not(feature = "gpu"))]
        let size_ok = !wn.low && wn.size.is_none();
        let gpu_shq = wn.video
            && crate::node::video::shq_on_gpu_allowed()
            && size_ok
            && wn.new_format
            && !regions_ok
            && coding.coded.is_none()
            && via_machine.as_ref().is_none_or(|w| w.video);
        client.gpu_shq.store(gpu_shq, Ordering::Relaxed);
        #[cfg(feature = "gpu")]
        let gpu_only_ok = gpu_shq && f.gpu.is_some() && !f.interlaced;
        #[cfg(not(feature = "gpu"))]
        let gpu_only_ok = false;
        if f.samples_missing() && !gpu_only_ok {
            wants.video = false;
        }
        // A preview's one picture in N (`x_idn_every`): the others not scaled or encoded
        // for it (one IDN Remote preview of a 1080p30 screen was 13%
        // of a core in the capture, for a picture every 2 s).
        if wants.video && !f.planes.is_empty() {
            offered += 1;
            if let Some(n) = wn.every
                && !(offered - 1).is_multiple_of(u64::from(n))
            {
                wants.video = false;
            }
        }
        let msgs = media_frames_or_skip(
            &f,
            &wants,
            quality,
            cache,
            regions.as_mut().filter(|_| regions_ok),
        );
        let pic_via_machine = via_machine.as_ref().is_some_and(|v| wn.video && !v.video);
        let mut video_sent = pic_via_machine || msgs.iter().any(|(v, _)| *v);
        for (is_video, m) in &msgs {
            let t = &client.totals;
            let (n, b) = if *is_video {
                (&t.video_frames, &t.video_bytes)
            } else {
                (&t.audio_frames, &t.audio_bytes)
            };
            n.fetch_add(1, Ordering::Relaxed);
            b.fetch_add(m.len() as u64, Ordering::Relaxed);
        }
        // Health reporting (§E.5): per-frame accounting, a report about once a second.
        // Audio sent on its own isn't a video frame and doesn't count.
        let video_bytes: usize = msgs.iter().filter(|(v, _)| *v).map(|(_, m)| m.len()).sum();
        // (A picture through the ring counts as sent: §E.5's v_tx.)
        let has_video = pic_via_machine || msgs.iter().any(|(v, _)| *v);
        if has_video {
            // The encode time of pictures encoded for it: one through the ring was only
            // copied there, and isn't counted (the lab read its copy as an encode).
            health.picture((!pic_via_machine).then_some(t_enc), video_bytes);
        }
        health.report(&cx, quality);
        if let (Some(mbps), true) = (cfg.target_mbps, has_video && !pic_via_machine) {
            let fps = f.frame_rate.0 as f64 / f.frame_rate.1.max(1) as f64;
            if f.hx_video.is_none() && fps > 0.0 {
                rc.update(video_bytes, mbps * 1e6 / 8.0 / fps);
            }
        }
        video_sent &= send_on_path(
            &cx,
            path.as_mut().unwrap(),
            &f,
            msgs,
            has_video,
            &mut counters,
            &mut hx_started,
        )?;
        if let Some(r) = &mut regions {
            r.commit(video_sent);
        }
    }
    if let Some(Path::Quic { stop, .. }) = path {
        stop.store(true, Ordering::Relaxed);
    }
    Ok(())
}

/// The `ndi_version` both sides send at once (§1.4), with the IDN announcement (§E.2): with
/// a site key, the plain-text start that decides on encryption first (§E.8.3); the link's
/// announcement on TCP sessions too, so that receivers can move to it (§E.11.3); and the
/// extensions this output takes part in.
fn announcement(cfg: &SenderConfig, cache: &EncodeCache, on_link: bool) -> String {
    let version = match &cfg.secure {
        Some(sc) => sc.version(true),
        None => crate::ext::ndi_version_with(true, None),
    };
    // Announced on TCP sessions too, so that receivers can move to the link (§E.11.3).
    let version = if cfg.link.is_some() {
        crate::link::announce(&version)
    } else {
        version
    };
    // Every sender answers clock pings and agrees sync groups' delays (§E.14).
    let version = crate::link::announce_token(&version, crate::sync::TOKEN);
    let version = if cfg.link.is_some() && cfg.frame_streams {
        crate::link::announce_token(&version, crate::link::FRAMES_TOKEN)
    } else {
        version
    };
    let version = if cfg.regions {
        crate::link::announce_token(&version, crate::regions::TOKEN)
    } else {
        version
    };
    let mc_encrypting = cfg.multicast && cfg.mc.mode == crate::mcast::Mode::Encrypted;
    let version = if mc_encrypting && on_link {
        crate::link::announce_token(&version, crate::mcast::TOKEN)
    } else {
        version
    };
    // The codecs this output encodes (§E.18.2).

    match cache.codec.get() {
        Some(hub) => {
            let v = crate::link::announce_token(&version, crate::codec::TOKEN);
            match v.strip_suffix("/>") {
                Some(head) => format!(
                    r#"{head} x_idn_codecs="{}"/>"#,
                    crate::codec::names(&hub.offered)
                ),
                None => v,
            }
        }
        None => version,
    }
}

/// What every stage of a session's frame loop reads: the output's settings, the receiver's
/// shared state, the output's encodes, the control connection, and the session's facts.
#[derive(Clone, Copy)]
struct Ctx<'a> {
    cfg: &'a SenderConfig,
    client: &'a Arc<ClientShared>,
    cache: &'a EncodeCache,
    w: &'a Arc<Mutex<TcpStream>>,
    peer: SocketAddr,
    frames: &'a FrameRx,
    link_conn: &'a Option<crate::link::LinkConn>,
    multicast_announcement: &'a Option<Arc<McOffer>>,
    session_creds: &'a Option<crate::secure::Creds>,
    /// The receiver's keyframe requests, counted by the reader.
    keyframe_requests: &'a std::sync::atomic::AtomicUsize,
    /// Encrypted (inside TLS, or on the link).
    secure: bool,
    /// The receiver is on this machine (§E.26).
    this_machine: bool,
    /// This session in the sync groups' registry (§E.14.4).
    sync_member: &'a SyncMember,
    /// The receiving device, for slowing file transfers with it (§E.12.5).
    trouble_fp: &'a Option<String>,
}

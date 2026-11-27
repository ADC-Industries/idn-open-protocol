//! Starting a sender: its listener on TCP and the IDN link on UDP at the same port, and
//! the threads that accept and serve receivers.

use super::*;

impl Sender {
    /// Listens on `port` (0: the next free port of IDN's media range whose UDP twin is
    /// free too, for the link; idn_common::ports).
    pub fn start(port: u16, mut cfg: SenderConfig) -> io::Result<Self> {
        let (l, mut twin) = if port == 0 {
            media_listener(cfg.link.is_some())?
        } else {
            (
                TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, port))?,
                None,
            )
        };
        let port = l.local_addr()?.port();
        l.set_nonblocking(true)?;
        // The link listens on UDP at the same port number (§E.11.1). If that port is
        // taken, the sender doesn't announce `link/1` (§E.11.2).
        let (link_tx, link_rx) = std::sync::mpsc::channel::<crate::link::Accepted>();
        let link = match &cfg.link {
            Some(lc) => {
                let tx = Mutex::new(link_tx);
                // (The UDP twin kept from the check, not bound again: another program took
                // it in between, and the sender ran without the link; F5-6.)
                let sock = match twin.take() {
                    Some(s) => Ok(s),
                    None => std::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, port)),
                };
                match sock.and_then(|sock| {
                    crate::link::LinkServer::bind_socket(
                        sock,
                        lc.clone(),
                        cfg.secure.clone(),
                        move |a| {
                            let _ = tx.lock_ok().send(a);
                        },
                    )
                }) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        crate::log::log(&format!(
                            "link: UDP port {port} unavailable ({e}); IDN receivers use TCP"
                        ));
                        None
                    }
                }
            }
            None => None,
        };
        if link.is_none() {
            cfg.link = None;
        }
        let color_info = cfg.color.ndi_color_info();
        let clients: Arc<Mutex<Vec<ClientSlot>>> = Default::default();
        let tally_hub: Arc<TallyHub> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let metadata: Arc<Mutex<Vec<(String, String, Trust)>>> = Default::default();
        let mc_demand: Arc<Mutex<(usize, usize)>> = Default::default();
        let cache: Arc<EncodeCache> = Default::default();
        // Hardware encoders, if the machine has one (§E.18.1: no software fallback).
        let codec_hub = cfg.codec.clone().and_then(|out| {
            let offered = out.encoder.codecs();
            if offered.is_empty() {
                crate::log::log("no hardware video encoder: SpeedHQ only");
                return None;
            }
            Some(Arc::new(CodecHub {
                out,
                offered,
                slots: Mutex::default(),
                coded: Mutex::default(),
                passed: Default::default(),
                encoded: Default::default(),
                #[cfg(feature = "gpu")]
                gpu: Mutex::default(),
                #[cfg(feature = "gpu")]
                gpu_ok: AtomicBool::new(false),
                #[cfg(feature = "gpu")]
                gpu_missed: Mutex::default(),
            }))
        });
        if let Some(h) = &codec_hub {
            let _ = cache.codec.set(h.clone());
        }
        let totals: Arc<SendTotals> = Default::default();
        let (multicast, announcement) = if cfg.multicast {
            let (tx, offer) = start_multicast(
                cfg.quality,
                &cfg.mc,
                stop.clone(),
                mc_demand.clone(),
                cache.clone(),
            )?;
            (Some(tx), Some(Arc::new(offer)))
        } else {
            (None, None)
        };
        let mc_group = announcement.as_ref().and_then(|a| a.group.clone());
        let accepting = {
            let (clients, stop, metadata, mc_demand, cache, totals) = (
                clients.clone(),
                stop.clone(),
                metadata.clone(),
                mc_demand.clone(),
                cache.clone(),
                totals.clone(),
            );
            let by_ip: Arc<Mutex<BTreeMap<IpAddr, usize>>> = Default::default();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    // TCP connections and link sessions (§E.11) alike.
                    let (s, peer, link) = match l.accept() {
                        Ok((s, peer)) => (s, peer, None),
                        Err(_) => match link_rx.try_recv() {
                            Ok(a) => {
                                let keys = cfg.link.as_ref().map(|l| l.keys.clone());
                                let session = keys.map(|keys| LinkSession {
                                    conn: a.conn,
                                    mode: a.mode,
                                    peer_fingerprint: a.peer_fingerprint,
                                    peer_cert: a.peer_cert,
                                    keys,
                                });
                                (a.control, a.peer, session)
                            }
                            Err(_) => {
                                std::thread::sleep(Duration::from_millis(20));
                                continue;
                            }
                        },
                    };
                    // Bounded: sessions in all and per address (each costs two threads
                    // and, once media flows, an encode).
                    {
                        let mut m = by_ip.lock_ok();
                        let total: usize = m.values().sum();
                        let mine = m.entry(peer.ip()).or_default();
                        let local = total < MAX_SESSIONS
                            && *mine >= MAX_SESSIONS_PER_ADDRESS
                            && idn_common::net::path_to(peer.ip()) == idn_common::net::Path::Local;
                        if let Some(why) = session_refused(total, *mine, local) {
                            if *mine == 0 {
                                m.remove(&peer.ip());
                            }
                            crate::log::log(&format!("receiver {peer}: refused, {why}"));
                            // The receiver told why (it said only "the source closes the
                            // connection at once (refused?)": lab 517-B): in the link's
                            // close, or an IDN receiver's x_idn_refused (§E.8.5).
                            match &link {
                                Some(ls) => ls.conn.close(crate::link::code::REFUSED, &why),
                                None => super::tell_refused_later(s, why),
                            }
                            continue;
                        }
                        *mine += 1;
                    }
                    let leave = {
                        let by_ip = by_ip.clone();
                        move || {
                            let mut m = by_ip.lock_ok();
                            if let Some(n) = m.get_mut(&peer.ip()) {
                                *n -= 1;
                                if *n == 0 {
                                    m.remove(&peer.ip());
                                }
                            }
                        }
                    };
                    let _ = s.set_nonblocking(false);
                    let tx = Arc::new(FrameQueue::default());
                    let rx = FrameRx(tx.clone());
                    let shared = Arc::new(ClientShared {
                        totals: totals.clone(),
                        link: Mutex::new(link.as_ref().map(|l| l.conn.clone())),
                        tally_hub: tally_hub.clone(),
                        ..Default::default()
                    });
                    tally_hub.join(&shared);
                    {
                        // Ended sessions' slots go here too: only sending a frame took
                        // them out, so a router or a paused output kept every one a
                        // reconnecting display ever made (bug review 3).
                        let mut held = clients.lock_ok();
                        held.retain(|c| !c.tx.is_closed());
                        held.push(ClientSlot {
                            tx,
                            shared: shared.clone(),
                        });
                    }
                    let (cfg, stop, metadata, ann, demand, cache) = (
                        cfg.clone(),
                        stop.clone(),
                        metadata.clone(),
                        announcement.clone(),
                        mc_demand.clone(),
                        cache.clone(),
                    );
                    let leave2 = leave.clone();
                    let spawned = std::thread::Builder::new().spawn(move || {
                        if let Err(e) = serve(
                            s, peer, cfg, rx, stop, metadata, ann, demand, cache, shared, link,
                        ) {
                            if e.kind() == io::ErrorKind::PermissionDenied {
                                log_refusal(&format!("receiver {peer}: {e}"));
                            } else {
                                crate::log::log(&format!("receiver {peer}: {e}"));
                            }
                        }
                        leave2();
                    });
                    if let Err(e) = spawned {
                        // Out of threads: refuse this one (its slot closes with its queue)
                        // and keep accepting.
                        crate::log::log(&format!("receiver {peer}: refused ({e})"));
                        leave();
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            })
        };
        let departing = Arc::new(AtomicBool::new(false));
        let on_signal = {
            let (clients, stop, departing) = (clients.clone(), stop.clone(), departing.clone());
            crate::shutdown::register(move || {
                depart_all(&clients, &stop, &departing, crate::ext::Departure::Shutdown)
            })
        };
        Ok(Sender {
            port,
            accepting: Mutex::new(Some(accepting)),
            totals,
            multicast,
            mc_group,
            codec_hub,
            cache: cache.clone(),
            taps: Mutex::default(),
            tap_wanted: Mutex::default(),
            #[cfg(feature = "gpu")]
            gpu_copies: Default::default(),
            #[cfg(feature = "gpu")]
            gpu_taps: Mutex::default(),
            needs_why: Mutex::default(),
            clients,
            stop,
            departing,
            color_info,
            metadata,
            metadata_out: std::sync::OnceLock::new(),
            _on_signal: on_signal,
            link,
        })
    }
}

/// A TCP listener on the next free port of IDN's media range, with the same UDP port
/// bound as well when the link wants it (§E.11.1; kept for it); else one the system picks.
fn media_listener(with_udp: bool) -> io::Result<(TcpListener, Option<std::net::UdpSocket>)> {
    let any = std::net::Ipv4Addr::UNSPECIFIED;
    for p in idn_common::ports::media_ports() {
        let Ok(l) = TcpListener::bind((any, p)) else {
            continue;
        };
        if !with_udp {
            return Ok((l, None));
        }
        if let Ok(u) = std::net::UdpSocket::bind((any, p)) {
            return Ok((l, Some(u)));
        }
    }
    Ok((TcpListener::bind((any, 0))?, None))
}

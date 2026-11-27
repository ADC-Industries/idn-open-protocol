//! A session's reader: the receiver's control and metadata messages (§1.5 and the
//! extensions'), on a thread of its own; the session ends when it does.

use super::*;

/// What the reader shares with its session, and its own state.
pub(super) struct Reader {
    pub(super) w: Arc<Mutex<TcpStream>>,
    pub(super) wants: Arc<Mutex<Option<Wants>>>,
    pub(super) low: Arc<AtomicBool>,
    pub(super) closed: Arc<AtomicBool>,
    pub(super) cfg: SenderConfig,
    pub(super) metadata: Arc<Mutex<Vec<(String, String, Trust)>>>,
    pub(super) keyframe_requests: Arc<std::sync::atomic::AtomicUsize>,
    /// A size the receiver asks for (`adapt/1`, §E.16.2).
    pub(super) reader_size: Arc<Mutex<Option<SizeRequest>>>,
    /// Set when the receiver asks for a whole picture (`regions/1`, §E.15.4).
    pub(super) reader_refresh: Option<Arc<AtomicBool>>,
    pub(super) client: Arc<ClientShared>,
    pub(super) sc: Option<crate::secure::SecureConfig>,
    /// The session's credentials, when encrypted.
    pub(super) reader_creds: Option<crate::secure::Creds>,
    pub(super) reader_sync: Arc<SyncMember>,
    /// The receiving device, for slowing file transfers with it (§E.12.5).
    pub(super) reader_trouble_fp: Option<String>,
    /// Re-sending lost encrypted multicast messages over the link (§E.17.6).
    pub(super) mc_repair: Option<(
        Arc<crate::mcast::Group>,
        std::cell::OnceCell<crate::link::LinkRepairs>,
        crate::link::LinkConn,
    )>,
    /// Frames read during negotiation: handled first.
    pub(super) pre: std::vec::IntoIter<crate::frame::Frame>,
    /// A peer that says nothing is dropped after FIRST_MESSAGE.
    pub(super) waiting_first: bool,
    /// The address this receiver reached, for `%IP%` in ndi_capabilities (§1.5).
    pub(super) local_ip: String,
    pub(super) r: io::BufReader<TcpStream>,
    /// Mode open: the receiver's key, checked against the one seen before (§E.11.4).
    pub(super) link_open: Option<(Arc<crate::link::KeyMemory>, String)>,
    pub(super) peer: SocketAddr,
}

impl Reader {
    pub(super) fn spawn(self) {
        std::thread::spawn(move || self.run());
    }

    fn run(self) {
        let Reader {
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
        } = self;
        let (mut pre, mut waiting_first, mut r) = (pre, waiting_first, r);
        // At most 200 re-sent multicast messages a second.
        let mut resend_budget = (Instant::now(), 0u64);
        let mut name = String::new();
        let mut identified = false;
        loop {
            // Frames read during negotiation come first.
            let f = match pre.next() {
                Some(f) => f,
                None => match read_frame(&mut r, &CONTROL_LIMITS) {
                    Ok(Some(f)) => f,
                    _ => break,
                },
            };
            if std::mem::take(&mut waiting_first) {
                let _ = r.get_ref().set_read_timeout(None);
            }
            let Ok(xml) = f.text_payload() else { continue };
            let Some(e) = parse_element(xml) else {
                continue;
            };
            let ext_on = |n: &str| client.ext.lock_ok().has(n);
            match e.name.as_str() {
                "ndi_version" => {
                    let active = crate::ext::Active::negotiate(e.attr("x_idn_ext"));
                    client
                        .plain_ndi
                        .store(e.attr("x_idn_ext").is_none(), Ordering::Relaxed);
                    let hevc = e.attr("sdk").is_some_and(|v| sdk_at_least(v, [4, 2]));
                    client.hevc_ok.store(hevc, Ordering::Relaxed);
                    if active.has("color") {
                        let _ = send_text(&w, kind::CONTROL, &cfg.color.to_xml());
                    }
                    *client.ext.lock_ok() = active;
                }
                // Extension messages: only acted on when the extension is active (§E.3).
                "x_idn_resume" if ext_on("resume") => {
                    // Same instance = a network blip: our next frame goes out at once
                    // anyway (frames are never held back). Pre-encoded HX cannot make
                    // a keyframe on demand (§E.4).
                    let blip = e.attr("instance") == Some(crate::ext::instance_id());
                    crate::log::log(&format!(
                        "receiver {name}: resume ({})",
                        if blip { "same sender" } else { "after restart" }
                    ));
                }
                "x_idn_stats" if ext_on("stats") => {
                    let r = crate::ext::ReceiverReport::parse(&e);
                    let mut last = client.report.lock_ok();
                    // New losses at the receiver: transfers make room (§E.12.5).
                    if last
                        .as_ref()
                        .is_some_and(|l| r.v_lost > l.v_lost || r.a_lost > l.a_lost)
                    {
                        crate::channel::video_trouble(reader_trouble_fp.as_deref());
                    }
                    *last = Some(r);
                }
                "x_idn_ping" if ext_on("stats") => {
                    let _ = send_text(&w, kind::CONTROL, &crate::ext::pong_for(&e));
                }
                // A new channel list (§E.21.1); empty = every channel again.
                "x_idn_channels" => {
                    if let Some(w) = wants.lock_ok().as_mut() {
                        w.channels = e.attr("list").and_then(crate::ext::parse_channel_list);
                    }
                }
                // A new size (§E.16.2); 0 x 0 = the full picture again.
                "x_idn_size" => {
                    let num = |k: &str| e.attr(k).and_then(|v| v.parse::<u32>().ok());
                    let s = num("width")
                        .zip(num("height"))
                        .filter(|&(w, h)| w > 0 && h > 0);
                    *reader_size.lock_ok() = Some(s);
                }
                // Messages a receiver lost on encrypted multicast (§E.17.6).
                "x_idn_mc_resend" => {
                    let num = |k: &str| e.attr(k).and_then(|v| v.parse::<u64>().ok());
                    if let (Some((g, repairs, lc)), Some(st), Some(from), Some(n)) =
                        (&mc_repair, num("stream"), num("from"), num("count"))
                    {
                        // At most 200 messages a second per receiver.
                        if resend_budget.0.elapsed() >= Duration::from_secs(1) {
                            resend_budget = (Instant::now(), 0);
                        }
                        let n = n.min(200u64.saturating_sub(resend_budget.1));
                        let r = repairs.get_or_init(|| lc.repairs());
                        for m in g.resend(st as u8, from, n) {
                            resend_budget.1 += 1;
                            if !r.send(st as u8, &m) {
                                break;
                            }
                        }
                    }
                }
                // A receiver without a usable reference picture (§E.15.4).
                "x_idn_refresh" => {
                    if let Some(r) = &reader_refresh {
                        r.store(true, Ordering::Relaxed);
                    }
                }
                // A synced receiver's report (§E.14.4).
                "x_idn_sync" => {
                    let num = |k: &str| e.attr(k).and_then(|v| v.parse::<f64>().ok());
                    if let (Some(group), Some(delay)) = (e.attr("group"), num("delay_ms")) {
                        crate::sync::report(
                            reader_sync.0,
                            group,
                            num("need_ms"),
                            delay,
                            num("frame_ms").unwrap_or(0.0),
                            w.clone(),
                        );
                    }
                }
                // A receiver on this machine took the ring (§E.26), or couldn't.
                "x_idn_machine_ok" => {
                    *client.machine_ok.lock_ok() = e.attr("gen").map(str::to_owned);
                    client
                        .machine_gpu
                        .store(e.attr("gpu") == Some("1"), Ordering::Relaxed);
                    client.machine_refused.store(false, Ordering::Relaxed);
                }
                "x_idn_machine_no" => {
                    *client.machine_ok.lock_ok() = None;
                    client.machine_refused.store(true, Ordering::Relaxed);
                }
                n if n.starts_with("x_idn_") => {}
                "ndi_identify" => {
                    name = e.attr("name").unwrap_or("").to_owned();
                    *client.name.lock_ok() = name.clone();
                    if let Some((keys, fp)) = &link_open
                        && keys.check(&name, fp)
                    {
                        crate::log::log(&format!(
                            "receiver {name}: its link key changed since it last connected (reinstalled, or another device using this name?); connecting anyway"
                        ));
                    }
                    // Sticky secure (§E.8.5): a receiver that encrypted before must not
                    // be served in plain text now.
                    if let Some(sc) = &sc {
                        if let Some(c) = &reader_creds {
                            crate::secure::remember_peer(sc, c, &name);
                        } else if sc.sticky_refuses(&name) {
                            // Held like every refusal (§E.8.5), so that a receiver that
                            // retries at once doesn't storm (lab X-t7).
                            log_refusal(&format!(
                                "receiver {name}: encrypted before, refusing plain text (names are not identities: is this another device with the same name?)"
                            ));
                            // Not a receiver any more while held (lab X-t7): uncounted.
                            *client.writer.lock_ok() = None;
                            std::thread::sleep(crate::secure::REFUSAL_HOLD);
                            let _ = w.lock_ok().shutdown(std::net::Shutdown::Both);
                            break;
                        }
                    }
                    if !identified {
                        identified = true;
                        let _ = send_text(&w, kind::CONTROL, &control::ndi_identify(&cfg.name));
                        if cfg.hx {
                            let _ = send_text(
                                &w,
                                kind::METADATA,
                                r#"<ndi_capabilities q_change_slow="true"/>"#,
                            );
                        }
                        if !cfg.capabilities.is_empty() {
                            let values: Vec<String> = cfg
                                .capabilities
                                .iter()
                                .map(|(_, v)| v.replace("%IP%", &local_ip))
                                .collect();
                            let a: Vec<(&str, &str)> = cfg
                                .capabilities
                                .iter()
                                .zip(&values)
                                .map(|((k, _), v)| (k.as_str(), v.as_str()))
                                .collect();
                            let _ = send_text(&w, kind::CONTROL, &element("ndi_capabilities", &a));
                        }
                        // The source's combined tally, when something is on.
                        client.tally_hub.greet(&w);
                    }
                }
                "ndi_video" => {
                    // Only a `quality` attribute changes the stream; a keyframe request
                    // carries none (§7.1.4). Pre-encoded HX sources cannot make
                    // keyframes on demand, so requests are only counted.
                    if let Some(q) = e.attr("quality") {
                        low.store(q == "low", Ordering::Relaxed);
                    }
                    if e.attr("request_keyframe") == Some("true") {
                        keyframe_requests.fetch_add(1, Ordering::Relaxed);
                    }
                }
                "ndi_enabled_streams" => {
                    client.active.store(true, Ordering::Relaxed);
                    client
                        .no_text
                        .store(e.attr("text") == Some("false"), Ordering::Relaxed);
                    *wants.lock_ok() = Some(parse_wants(&e, low.load(Ordering::Relaxed)));
                }
                // The receiver refused this output (§E.10.7) and says why.
                "x_idn_refused" => {
                    log_refusal(&format!(
                        "receiver {peer}: refused this output: {}",
                        e.attr("reason").unwrap_or("no reason given")
                    ));
                }
                "ndi_tally" => {
                    let t = (
                        e.attr("on_program") == Some("true"),
                        e.attr("on_preview") == Some("true"),
                    );
                    // The combined tally, echoed to every receiver when it changes
                    // (§1.4).
                    *client.tally.lock_ok() = t;
                    client.tally_hub.update();
                }
                _ if f.kind == kind::METADATA => {
                    // Bounded by count and by bytes: the newest are kept.
                    let trust = match reader_creds.as_ref().map(|c| c.mode()) {
                        Some("secure/2") => Trust::Site,
                        Some("pair/1") => Trust::Linked,
                        _ => Trust::None,
                    };
                    let mut m = metadata.lock_ok();
                    m.push((name.clone(), xml.to_owned(), trust));
                    metadata_arrived();
                    let mut bytes: usize = m.iter().map(|(n, x, _)| n.len() + x.len()).sum();
                    while m.len() > Sender::METADATA_KEPT
                        || (bytes > Sender::METADATA_BYTES_KEPT && m.len() > 1)
                    {
                        let (n, x, _) = m.remove(0);
                        bytes -= n.len() + x.len();
                    }
                }
                _ => {}
            }
        }
        closed.store(true, Ordering::Relaxed);
    }
}

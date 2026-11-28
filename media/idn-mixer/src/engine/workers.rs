//! The console's background work: switchers and their outputs watched, routes and
//! feeds sent, plugins watched and synced, state saved, the engine's run loop.

use super::*;

/// Audio follows video (§M.12): while it is on, a metadata-only connection to the
/// switcher's program source; its tally messages tell the desk what is on air. Not heard
/// from for 6 s (or off): the desk is told nothing is known, and following channels stay
/// up.
pub(super) fn watch_switcher(sh: &Arc<Shared>) {
    use idn_open_protocol::input::{Input, InputConfig, InputEvent};
    let mut current: Option<(String, Input)> = None;
    let mut heard: Option<Instant> = None;
    let forget = |sh: &Shared| {
        sh.desk.lock_ok().set_on_air(None);
        let mut st = sh.afv.lock_ok();
        if st.connected {
            *st = AfvStatus::default();
        }
    };
    while !sh.stop.load(Ordering::Relaxed) {
        let afv = sh.desk.lock_ok().settings().afv.clone();
        let want = (afv.on && !afv.switcher.is_empty()).then_some(afv.switcher);
        if current.as_ref().map(|(n, _)| n) != want.as_ref() {
            current = None;
            heard = None;
            forget(sh);
            if let Some(name) = want {
                let cfg = InputConfig {
                    receiver_name: sh.cfg.name.clone(),
                    secure: sh.cfg.secure.clone(),
                    discovery_server: sh.cfg.discovery_server.clone(),
                    receive_video: false,
                    audio_channels: Some(Vec::new()),
                    ..InputConfig::new(&name)
                };
                match Input::open(cfg) {
                    Ok(i) => current = Some((name, i)),
                    Err(e) => {
                        idn_open_protocol::log::log(&format!(
                            "mixer: audio follows video: {name}: {e}"
                        ));
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                }
            }
        }
        let Some((_, input)) = &current else {
            std::thread::sleep(Duration::from_millis(250));
            continue;
        };
        match input.recv(Duration::from_millis(250)) {
            Some(InputEvent::Metadata { xml, .. }) => {
                if let Some((program, on_air, inputs)) = parse_tally(&xml) {
                    heard = Some(Instant::now());
                    sh.desk
                        .lock_ok()
                        .set_on_air(Some(on_air.iter().cloned().collect()));
                    let now = AfvStatus {
                        connected: true,
                        program,
                        on_air,
                        inputs,
                    };
                    let mut st = sh.afv.lock_ok();
                    let changed =
                        (st.connected, &st.on_air, &st.inputs) != (true, &now.on_air, &now.inputs);
                    *st = now;
                    drop(st);
                    if changed {
                        *sh.version.lock_ok() += 1;
                        sh.changed.notify_all();
                    }
                }
            }
            _ => {
                if heard.is_some_and(|t| t.elapsed() > Duration::from_secs(6)) {
                    heard = None;
                    forget(sh);
                    *sh.version.lock_ok() += 1;
                    sh.changed.notify_all();
                }
            }
        }
    }
}

/// Brings each switcher output on the network onto two desk inputs the first time it is
/// seen (§M.17); gives back the inputs of one gone for an hour that no channel uses; and
/// notices inputs the user took off (they stay off).
pub(super) fn watch_switcher_outputs(sh: &Arc<Shared>) {
    use crate::switchers::{GONE_SECS, Known, ROLE, free_pair, in_place, routes_for};
    let browser =
        idn_open_protocol::discovery::shared(idn_open_protocol::discovery::BrowserConfig {
            interface: None,
            discovery_server: sh.cfg.discovery_server.clone(),
        });
    let browser = match browser {
        Ok(b) => b,
        Err(e) => {
            idn_open_protocol::log::log(&format!("mixer: switchers' outputs: {e}"));
            return;
        }
    };
    let mut next = Instant::now();
    while !sh.stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(200));
        if Instant::now() < next {
            continue;
        }
        next = Instant::now() + Duration::from_secs(2);
        let all = browser.sources();
        // Every source's name, for a lost route's cause (§M.27).
        *sh.listed.lock_ok() = Some(all.iter().map(|s| s.name.clone()).collect());
        let sources: Vec<idn_open_protocol::discovery::Source> = all
            .into_iter()
            .filter(|s| s.offers(ROLE.split('/').next().unwrap_or(ROLE)))
            .collect();
        let here: std::collections::BTreeSet<String> =
            sources.iter().map(|s| s.name.clone()).collect();
        // The feeds (§M.18), to every switcher listening, each time round.
        send_feeds(sh);
        *sh.switchers_here.lock_ok() = here.clone();
        let Some(rx) = sh.receiver.lock_ok().clone() else {
            continue;
        };
        let routes = rx.routes();
        let now = unix_now();
        let mut saved = sh.switchers.lock_ok().clone();
        let before = saved.clone();
        // Inputs a channel uses (its input, or its backup).
        let used: Vec<usize> = {
            let d = sh.desk.lock_ok();
            d.settings()
                .channels
                .iter()
                .enumerate()
                .flat_map(|(i, c)| [c.source.unwrap_or(i + 1), c.input_b])
                .filter(|&k| k > 0)
                .collect()
        };
        for name in &here {
            let k = saved.outputs.entry(name.clone()).or_insert(Known {
                inputs: None,
                seen: 0,
                confirmed: false,
                takes_feed: false,
            });
            let new = k.seen == 0;
            k.seen = now;
            k.takes_feed = sources
                .iter()
                .any(|s| s.name == *name && s.offers(crate::switchers::FEED_ROLE));
            if new && saved.on {
                let taken: Vec<u16> = saved
                    .outputs
                    .values()
                    .filter_map(|k| k.inputs)
                    .flatten()
                    .collect();
                match free_pair(&routes, rx.channels(), &taken) {
                    Some(pair) => match send_routes(sh, &routes_for(rx.name(), name, pair, true)) {
                        Ok(()) => {
                            let k = saved.outputs.get_mut(name).expect("just added");
                            k.inputs = Some(pair);
                        }
                        Err(e) => idn_open_protocol::log::log(&format!("mixer: {name}: {e}")),
                    },
                    None => idn_open_protocol::log::log(&format!(
                        "mixer: {name}: no two desk inputs free"
                    )),
                }
            }
        }
        let mut give_back = Vec::new();
        for (name, k) in saved.outputs.iter_mut() {
            let Some(pair) = k.inputs else {
                continue;
            };
            let placed = in_place(&routes, name, pair);
            if placed {
                k.confirmed = true;
            } else if k.confirmed {
                // The user (or the matrix) took it off: it stays off.
                k.inputs = None;
                k.confirmed = false;
                continue;
            }
            let in_use = pair.iter().any(|&c| used.contains(&(c as usize + 1)));
            if !here.contains(name) && now.saturating_sub(k.seen) > GONE_SECS && !in_use {
                give_back.push((name.clone(), pair));
            }
        }
        for (name, pair) in give_back {
            if send_routes(sh, &routes_for(rx.name(), &name, pair, false)).is_ok() {
                saved.outputs.remove(&name);
            }
        }
        if saved != before {
            // Kept apart from `seen` alone changing (once every 2 s).
            let worth = saved.outputs.len() != before.outputs.len()
                || saved.outputs.iter().any(|(n, k)| {
                    before
                        .outputs
                        .get(n)
                        .is_none_or(|b| (b.inputs, b.confirmed) != (k.inputs, k.confirmed))
                });
            *sh.switchers.lock_ok() = saved;
            if worth {
                save_switchers(sh);
                *sh.version.lock_ok() += 1;
                sh.changed.notify_all();
            }
        }
    }
}

/// The site's transmitters' origins (§A.16) by name, as the Node last said: asked again
/// every 5 s while the desk has routes (none without a Node or outside a site).
fn site_origins(sh: &Shared, want: bool) -> super::SiteOrigins {
    let mut g = sh.site_origins.lock_ok();
    if want
        && g.0.is_none_or(|t| t.elapsed() >= Duration::from_secs(5))
        && let Some(node) = &sh.cfg.node
    {
        g.0 = Some(std::time::Instant::now());
        if let Ok(r) = node.request(idn_open_protocol::xml::Element::new("site_transmitters")) {
            g.1 = r
                .children_named("tx")
                .filter_map(|t| {
                    let o = idn_open_protocol::node::routing::AudioOrigin::from_element(t)?;
                    Some((
                        t.attr("name")?.to_owned(),
                        (t.attr("machine").unwrap_or("").to_owned(), o),
                    ))
                })
                .collect();
        }
    }
    g.1.clone()
}

/// A transmitter's channel `ch` (from 0) as an origin: its label there, `DEVICE/CHANNEL`
/// split where the origin names no device of its own (a pool, a network device's).
fn origin_of(
    o: &idn_open_protocol::node::routing::AudioOrigin,
    ch: usize,
) -> Option<state::InputOrigin> {
    let label = o.channels.get(ch).map(String::as_str).unwrap_or("");
    if o.device.is_empty() {
        let (device, channel) = label.split_once('/')?;
        return Some(state::InputOrigin {
            kind: o.kind.clone(),
            device: device.to_owned(),
            channel: channel.to_owned(),
        });
    }
    Some(state::InputOrigin {
        kind: o.kind.clone(),
        device: o.device.clone(),
        channel: label.to_owned(),
    })
}

/// The network-audio device's receive channels' subscriptions `(device, channel)` (R.33),
/// from the `device.json` idn-aoip keeps beside its rings: read again when it changes.
fn ring_subscriptions(sh: &Shared) -> super::RingSubs {
    let Some(c) = sh.cfg.network_audio.as_ref() else {
        return Vec::new();
    };
    let file = idn_common::paths::program_dir("aoip")
        .join(&c.device)
        .join("device.json");
    let at = std::fs::metadata(&file).and_then(|m| m.modified()).ok();
    let mut g = sh.ring_subs.lock_ok();
    if at != g.0 {
        g.0 = at;
        g.1 = std::fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .map(|v| ring_subs_of(&v))
            .unwrap_or_default();
    }
    g.1.clone()
}

/// `device.json`'s `rx[].from` as `(device, channel)`.
fn ring_subs_of(v: &Value) -> Vec<Option<(String, String)>> {
    v["rx"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            let f = &c["from"];
            Some((
                f["device"].as_str()?.to_owned(),
                f["channel"].as_str()?.to_owned(),
            ))
        })
        .collect()
}

/// Tells the desk which of its inputs something is routed to (the routing matrix's
/// routes to the desk, and a sound card's inputs); channels on the others are parked.
pub(super) fn update_routed(sh: &Shared) {
    let rx = sh.receiver.lock_ok().clone();
    let card = sh.cfg.card_in.as_ref();
    let inputs = sh.desk.lock_ok().io().0;
    // Without a Node or a card every input counts as routed (the desk's own default).
    // (A network-audio device used directly, R.33: its channels count as a card's do,
    // once it is there.)
    let net = sh.cfg.network_audio.as_ref().map(|c| {
        let n = sh.net_audio.lock_ok().map_or(0, |s| s.inputs());
        (c.first, n)
    });
    let have_routing = rx.is_some() || card.is_some() || net.is_some();
    // The routed inputs end where the talkback inputs start (§M.26).
    let routable = sh.iem.first_talkback - 1;
    let routes = rx.map(|rx| rx.routes()).unwrap_or_default();
    let mut names = vec![String::new(); inputs];
    // Each input's source, typed (§M.28.3): filled beside its name.
    let mut sources: Vec<state::InputSource> = (0..inputs)
        .map(|k| state::InputSource {
            input: k + 1,
            ..Default::default()
        })
        .collect();
    let origins = site_origins(sh, !routes.is_empty());
    let subs = net.map(|_| ring_subscriptions(sh)).unwrap_or_default();
    let mut live = vec![false; inputs];
    let switchers = sh.switchers.lock_ok().outputs.clone();
    let routed = have_routing.then(|| {
        let heard = sh.heard.lock_ok();
        let mut routed = vec![false; inputs];
        for r in &routes {
            let k = r.ch as usize;
            if k >= inputs {
                continue;
            }
            routed[k] = true;
            // Sound arrives: its transmitter was heard within the last 200 ms.
            live[k] = heard
                .get(&r.tx)
                .is_some_and(|t| t.elapsed() < Duration::from_millis(200));
            // "HOST (Stage box)" channel 13 → "Stage box 13"; a switcher's output's
            // two sides L and R (§M.17).
            let short = crate::switchers::short_name(&r.tx);
            names[k] = match r.tx_ch {
                0 | 1 if switchers.contains_key(&r.tx) => {
                    format!("{short} {}", ["L", "R"][r.tx_ch as usize])
                }
                n => format!("{short} {}", u32::from(n) + 1),
            };
            let o = origins.get(&r.tx);
            sources[k] = state::InputSource {
                input: k + 1,
                kind: Some("idn".into()),
                label: String::new(),
                via: Some(r.tx.clone()),
                via_channel: Some(u32::from(r.tx_ch) + 1),
                machine: o.map(|(m, _)| m.clone()).filter(|m| !m.is_empty()),
                origin: o.and_then(|(_, o)| origin_of(o, r.tx_ch as usize)),
            };
        }
        if let Some(c) = card {
            let n = c.channels.map_or(inputs, usize::from);
            for k in (c.first..routable.min(inputs)).take(n) {
                routed[k] = true;
                live[k] = true;
                names[k] = format!("Card {}", k - c.first + 1);
                let device = c.device.clone().unwrap_or_else(|| "default".into());
                let n = (k - c.first + 1) as u32;
                sources[k] = state::InputSource {
                    input: k + 1,
                    kind: Some("card".into()),
                    label: String::new(),
                    via: Some(device.clone()),
                    via_channel: Some(n),
                    machine: None,
                    origin: Some(state::InputOrigin {
                        kind: "sound_card".into(),
                        device,
                        channel: format!("In {n}"),
                    }),
                };
            }
        }
        if let Some((first, n)) = net {
            for k in (first..routable.min(inputs)).take(n) {
                routed[k] = true;
                live[k] = true;
                names[k] = format!("Net {}", k - first + 1);
                let j = k - first;
                sources[k] = state::InputSource {
                    input: k + 1,
                    kind: Some("net".into()),
                    label: String::new(),
                    via: sh.cfg.network_audio.as_ref().map(|c| c.device.clone()),
                    via_channel: Some(j as u32 + 1),
                    machine: None,
                    // What its receive channel subscribes to on the network.
                    origin: subs.get(j).cloned().flatten().map(|(device, channel)| {
                        state::InputOrigin {
                            kind: "network_device".into(),
                            device,
                            channel,
                        }
                    }),
                };
            }
        }
        routed
    });
    // Each IEM mix's talkback input (§M.26): there while the mix is, live while its
    // phone talks.
    let routed = {
        let s = sh.desk.lock_ok().settings_arc();
        let talking = sh.iem.talking_slots();
        let mut routed = routed;
        for m in &s.iem_mixes {
            let k = routable + m.talkback.max(1) - 1;
            if k < inputs {
                live[k] = talking[m.talkback.max(1) - 1];
                names[k] = format!("Talkback {}", m.name);
                sources[k] = state::InputSource {
                    input: k + 1,
                    kind: Some("talkback".into()),
                    via: Some(m.name.clone()),
                    ..Default::default()
                };
                if let Some(r) = routed.as_mut() {
                    r[k] = true;
                }
            }
        }
        routed
    };
    for (s, n) in sources.iter_mut().zip(&names) {
        s.label = n.clone();
    }
    *sh.input_sources.lock_ok() = sources;
    *sh.input_names.lock_ok() = names;
    let mut d = sh.desk.lock_ok();
    let n = d.settings().channels.len();
    let before: Vec<bool> = (0..n).map(|i| d.active(i)).collect();
    let shown_before: Vec<Option<bool>> = d.settings().channels.iter().map(|c| c.shown).collect();
    if let Some(routed) = routed {
        d.set_routed(routed);
        d.set_live(live);
    }
    // Manual parking (the default): a channel not yet placed stays
    // parked until the first time its input is routed (or it is added by hand), then goes
    // on the desk for good: a route taken away later never parks it, only the user does.
    // (Placed once at the desk's start, every channel without a route then parked for
    // good, so routing one later never brought it on.)
    let active_now: Vec<bool> = (0..n).map(|i| d.active(i)).collect();
    let place = to_place(d.settings().auto_park, &shown_before, &active_now);
    if let Some(now) = place {
        // As every change: the desk let go while the settings are made.
        drop(d);
        let _ = change_settings(sh, false, |s| {
            for (c, on) in s.channels.iter_mut().zip(now) {
                if on {
                    c.shown.get_or_insert(true);
                }
            }
            Ok(())
        });
        d = sh.desk.lock_ok();
    }
    let after: Vec<bool> = (0..n).map(|i| d.active(i)).collect();
    let shown_after: Vec<Option<bool>> = d.settings().channels.iter().map(|c| c.shown).collect();
    drop(d);
    if before != after || shown_before != shown_after {
        *sh.version.lock_ok() += 1;
        sh.changed.notify_all();
    }
}

/// Manual parking: the channels to put on the desk now: not yet placed (`shown` unset)
/// and routed now. `None`: none (or automatic parking, where routing decides each time).
fn to_place(auto_park: bool, shown: &[Option<bool>], active: &[bool]) -> Option<Vec<bool>> {
    if auto_park {
        return None;
    }
    let now: Vec<bool> = shown
        .iter()
        .zip(active)
        .map(|(s, &a)| s.is_none() && a)
        .collect();
    now.iter().any(|&b| b).then_some(now)
}

/// Watches the plugin host: started again when it stops (the strips pass on dry
/// meanwhile, and their plugins come back with their last state); plugins' states read
/// about once a second while they change.
pub(super) fn watch_plugins(sh: &Arc<Shared>) {
    let mut tick = 0u64;
    let started = Instant::now();
    let mut card_restart_said = false;
    let mut last_restart: Option<Instant> = None;
    // The wait before the next start of the plugin host, growing while it keeps stopping
    // (a plugin that crashes as it loads); back to the first once it runs for a minute.
    let mut restarts =
        idn_common::backoff::Backoff::new(Duration::from_secs(5), Duration::from_secs(60));
    let mut restart_wait = Duration::ZERO;
    // A rate to run at already said in the log (run by hand: said once).
    let mut rate_said = 0u32;
    let mut saved = SavedDesk {
        bytes: sh
            .cfg
            .state_dir
            .as_ref()
            .and_then(|d| std::fs::read(d.join(DESK_FILE)).ok())
            .unwrap_or_default(),
        from: None,
    };
    while !sh.stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(250));
        tick += 1;
        // A chain stuck for 10 s: set aside, the host started again without it (E2-3).
        if let Some(chain) = sh.plugins.stuck_too_long() {
            sh.desk.lock_ok().set_plugins(None, BTreeMap::new());
            idn_open_protocol::log::log(&format!(
                "mixer: {} stopped answering for 10 s: set aside, the plugin host started again",
                crate::plugins::strip_name(chain)
            ));
            sh.plugins.set_aside(chain);
            sync_plugins(sh, false);
            continue;
        }
        if sh.plugins.stopped() {
            sh.desk.lock_ok().set_plugins(None, BTreeMap::new());
            // Not more than once in 5 s, then 10, 20 … up to a minute (a plugin that
            // crashes as it loads).
            if last_restart.is_some_and(|t| t.elapsed() < restart_wait) {
                continue;
            }
            idn_open_protocol::log::log("mixer: the plugin host stopped; starting it again");
            last_restart = Some(Instant::now());
            restart_wait = restarts.next();
            sync_plugins(sh, false);
            continue;
        }
        if restarts.current().is_some()
            && last_restart.is_some_and(|t| t.elapsed() >= Duration::from_secs(60))
        {
            restarts.reset();
        }
        if tick.is_multiple_of(4) {
            // The failure watch (§M.27), each second.
            super::problems::watch(sh);
            store_states(sh, false);
            save_desk(sh, &mut saved);
            rate_moved(sh, &mut saved, &mut rate_said);
            // The output card ended for good (its driver invalidated the stream, or it took
            // no sound for 10 s): the desk started again (its clock and outputs are the
            // card's), when the Node runs it, after its first minute, and not again within
            // 10 minutes of the last such start (kept across starts: a card that ends at
            // every start was a restart a minute; bug run 2026-11-25). Run by hand: said.
            let card_ended = sh
                .cards_status
                .lock_ok()
                .as_ref()
                .is_some_and(|c| c.lock_ok()["output"]["ended"] == true);
            if card_ended && started.elapsed() >= Duration::from_secs(60) && !card_restart_said {
                let mark = sh.cfg.state_dir.as_ref().map(|d| d.join("card-restart"));
                let recent = mark
                    .as_ref()
                    .and_then(|m| std::fs::metadata(m).ok()?.modified().ok())
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|e| e < Duration::from_secs(600));
                if !idn_common::switches::on("IDN_SUPERVISED") {
                    idn_open_protocol::log::log(
                        "mixer: the output sound card's stream ended: start the desk again to open it anew",
                    );
                    card_restart_said = true;
                } else if recent {
                    idn_open_protocol::log::log(
                        "mixer: the output sound card's stream ended again within 10 minutes: not starting again (restart the desk to open it anew)",
                    );
                    card_restart_said = true;
                } else if !sh.restart_wanted.swap(true, Ordering::SeqCst) {
                    if let Some(m) = &mark {
                        let _ = std::fs::write(m, b"");
                    }
                    idn_open_protocol::log::log(
                        "mixer: the output sound card's stream ended: starting again to open it anew",
                    );
                }
            }
            // `restart` (§M.28): saved, then started again at once by the supervisor (a
            // recording started since: once it ends).
            if sh.restart_wanted.load(Ordering::SeqCst) {
                let _no_start = super::recording::STARTING.lock_ok();
                if sh.recorder.lock_ok().is_none() && sh.recording_busy.load(Ordering::SeqCst) == 0
                {
                    idn_open_protocol::log::log("mixer: restart asked for: starting again");
                    store_states(sh, true);
                    save_desk(sh, &mut saved);
                    std::process::exit(idn_common::supervisor::AGAIN_NOW);
                }
            }
        }
        // Plugins whose latency became known (or changed) once running: the strips line
        // up again.
        if sh.plugins.poll_latency() {
            let chains = sh.plugins.chains();
            idn_open_protocol::log::log(&format!("mixer: plugin latency now {chains:?}"));
            sh.desk.lock_ok().set_chains(chains);
        }
        update_routed(sh);
    }
    // Stopping: the last changes kept.
    store_states(sh, true);
    save_desk(sh, &mut saved);
}

/// Sends route changes for the desk's receiver to this machine's Node.
pub(super) fn send_routes(
    sh: &Shared,
    changes: &[idn_open_protocol::node::routing::RouteChange],
) -> Result<(), String> {
    let node = sh.cfg.node.as_ref().ok_or("no IDN Node")?;
    for c in changes {
        let reply = node.request(c.to_element()).map_err(|e| e.to_string())?;
        if reply.name == "error" {
            return Err(reply
                .attr("text")
                .or(reply.attr("code"))
                .unwrap_or("refused")
                .to_owned());
        }
    }
    Ok(())
}

/// Sends the desk's feeds on its output (§M.18): each switcher output with a bus, the
/// desk channels that carry it.
pub(super) fn send_feeds(sh: &Shared) {
    let feeds = sh.switchers.lock_ok().feeds.clone();
    let s = sh.desk.lock_ok().settings_arc();
    let list: Vec<(String, [usize; 2])> = feeds
        .iter()
        .filter_map(|(name, bus)| Some((name.clone(), feed_outputs(&s, bus)?)))
        .collect();
    let _ = sh.out.send_metadata(&crate::switchers::feeds_xml(&list));
}

/// The desk outputs (from 0) carrying a feed's bus, left and right, if the output patch
/// has them.
pub(super) fn feed_outputs(s: &Settings, bus: &str) -> Option<[usize; 2]> {
    let j = crate::switchers::bus_pair(bus)?;
    let at = |k: usize| s.outputs.iter().position(|o| *o == format!("bus:{k}"));
    Some([at(j)?, at(j + 1)?])
}

/// The rate the desk is to run at now (§N.40): asked for at its start, else its own,
/// else the machine's (from its start's command, else as the Node says now).
fn rate_wanted(sh: &Shared) -> u32 {
    let valid = |r: &u32| idn_open_protocol::node::rate::valid(*r);
    sh.cfg
        .rate
        .filter(valid)
        .or(sh.desk.lock_ok().settings().rate.filter(valid))
        .or(sh.cfg.machine_rate.filter(valid))
        .unwrap_or_else(idn_open_protocol::node::rate::machine)
}

/// A rate other than the one it runs at: under the Node's supervisor (`IDN_SUPERVISED`)
/// the desk is saved and the program ends with `AGAIN_NOW`, which the supervisor starts
/// again at once (a second of silence); run by hand, it says so once. Not while it records
/// (bug run J2-6: the recording's last two seconds were cut): it waits.
fn rate_moved(sh: &Shared, saved: &mut SavedDesk, said: &mut u32) {
    let want = rate_wanted(sh);
    if want == crate::desk::rate_hz() || *said == want {
        return;
    }
    // Not while it records: it starts again once the recording ends. No recording starts
    // from the check to the exit (N2-1).
    let _no_start = super::recording::STARTING.lock_ok();
    if sh.recorder.lock_ok().is_some()
        || sh.recording_busy.load(std::sync::atomic::Ordering::SeqCst) > 0
    {
        return;
    }
    if idn_common::switches::on("IDN_SUPERVISED") {
        idn_open_protocol::log::log(&format!(
            "mixer: the desk's rate is now {}: starting again",
            idn_open_protocol::node::rate::label(want)
        ));
        store_states(sh, true);
        save_desk(sh, saved);
        std::process::exit(idn_common::supervisor::AGAIN_NOW);
    }
    *said = want;
    idn_open_protocol::log::log(&format!(
        "mixer: the desk's rate is set to {}: start the desk again to use it",
        idn_open_protocol::node::rate::label(want)
    ));
}

/// The desk's settings as last written: their bytes, and the settings they were made
/// from (settings are only ever swapped whole, never changed in place, so the same `Arc`
/// means the same settings).
#[derive(Default)]
pub(super) struct SavedDesk {
    pub(super) bytes: Vec<u8>,
    pub(super) from: Option<Arc<Settings>>,
}

/// Writes the desk's settings (with the plugins' states) if they changed since `last`.
pub(super) fn save_desk(sh: &Shared, last: &mut SavedDesk) {
    let Some(dir) = &sh.cfg.state_dir else {
        return;
    };
    let s = sh.desk.lock_ok().settings_arc();
    // The same settings as last written: nothing to serialise (this runs each second).
    if last.from.as_ref().is_some_and(|f| Arc::ptr_eq(f, &s)) {
        return;
    }
    // Serialised with the desk let go (it took the clock's lock with it).
    let Ok(b) = serde_json::to_vec(&*s) else {
        return;
    };
    if b == last.bytes {
        last.from = Some(s);
        return;
    }
    // Whole or not at all (one writer at a time: the watcher and the stop hook).
    static SAVING: Mutex<()> = Mutex::new(());
    let _one = SAVING.lock_ok();
    let ok = idn_common::fsutil::write_atomic(&dir.join(DESK_FILE), &b);
    match ok {
        Ok(()) => {
            last.bytes = b;
            last.from = Some(s);
        }
        Err(e) => idn_open_protocol::log::log(&format!("mixer: can't save the desk: {e}")),
    }
}

pub(super) fn save_switchers(sh: &Shared) {
    let Some(dir) = &sh.cfg.state_dir else {
        return;
    };
    let body = serde_json::to_vec_pretty(&*sh.switchers.lock_ok()).unwrap_or_default();
    // (Whole or not at all: a cut-off file lost every switcher.)
    let _ = std::fs::create_dir_all(dir)
        .and_then(|()| idn_common::fsutil::write_atomic(&dir.join(SWITCHERS_FILE), &body));
}

/// Plugins' states read into the settings (those changed, or all), without a new version
/// (nothing the panel shows changed).
pub(super) fn store_states(sh: &Shared, all: bool) {
    let states = sh.plugins.refresh(all);
    if !states.is_empty() {
        let mut stored = Vec::new();
        let _ = change_settings(sh, false, |s| {
            stored = plugins::store_states(s, &states);
            Ok(())
        });
        sh.plugins.committed(&stored);
    }
}

/// Brings the plugin host's chains in line with the settings, and tells the desk.
/// `recall`: after a whole-desk load (its plugin states loaded over ones just read).
pub(super) fn sync_plugins(sh: &Shared, recall: bool) {
    let _turn = sh.syncing.lock_ok();
    let s = sh.desk.lock_ok().settings_arc();
    let (io, chains) = sh.plugins.apply(&s, recall);
    sh.desk.lock_ok().set_plugins(io, chains);
}

/// Changes the desk's settings: `f` on a copy, normalised, then swapped in. Only the swap
/// holds the desk, which the clock thread takes every block; the clone, the change, the
/// normalising and freeing the old settings (plugin states in them: hundreds of kB) are
/// done with it let go. Changes take turns on `editing`, so none is
/// lost to another made at the same time.
pub(super) fn change_settings(
    sh: &Shared,
    slow: bool,
    f: impl FnOnce(&mut Settings) -> Result<(), String>,
) -> Result<(), String> {
    let _turn = sh.editing.lock_ok();
    let (now, (n, outputs)) = {
        let d = sh.desk.lock_ok();
        (d.settings_arc(), d.shape())
    };
    let mut s = (*now).clone();
    f(&mut s)?;
    let s = s.normalised(n, outputs);
    // The suppressors it turns on made first, the desk let go (K6-4).
    let need = sh.desk.lock_ok().denoisers_needed(&s);
    let rate = crate::desk::rate_hz();
    let made = need
        .into_iter()
        .map(|i| (i, idn_dsp::denoise::RateDenoiser::new(rate)))
        .collect();
    let old = sh.desk.lock_ok().set_normalised_with(s, slow, made);
    drop(old);
    drop(now);
    Ok(())
}

/// The 2 ms clock: take a block from the inputs, run the desk, send the buses.
pub(super) fn run(sh: &Arc<Shared>, input: &Mixer) {
    idn_common::rt::flush_denormals();
    let n = input.channels() as usize;
    let outputs = sh.desk.lock_ok().io().1;
    // Samples a block (32 to 256), as the desk was made.
    let bs = sh.desk.lock_ok().block();
    let period = Duration::from_micros(bs as u64 * 1_000_000 / u64::from(rate_hz()));
    // When each block is due: its card, else site time, else its own timer (R.26).
    let mut pacer = idn_open_protocol::audio::pace::Pacer::new(
        "mixer: the desk's",
        "card",
        bs as u32,
        rate_hz(),
        sh.card_time.lock_ok().clone(),
    );
    let (base, base_mono) = (Instant::now(), idn_open_protocol::ptp::mono_ns());
    let mut k: u64 = 0;
    // (Read once: a lookup in the environment every block or frame otherwise.)
    let crash_test = idn_common::switches::on("IDN_CLOCK_CRASH_TEST");
    // A network-audio device as the desk's sound card (R.33): its rings, block by block.
    let mut net = sh
        .cfg
        .network_audio
        .clone()
        .map(|c| crate::netaudio::NetAudio::new(c, bs, outputs));
    let mut net_scratch = vec![0f32; bs];
    let mut interleaved = vec![0f32; bs * n];
    // The routed inputs, then the IEM mixes' talkback inputs (§M.26).
    let talk_n = crate::iem::TALKBACK_INPUTS;
    let mut talk_interleaved = vec![0f32; bs * talk_n];
    let mut talk_silent = true;
    let mut planar = vec![vec![0f32; bs]; n + talk_n];
    let mut out = vec![vec![0f32; bs]; outputs];
    // The outputs leave the clock thread: a block handed to the output thread, which feeds
    // the sound card and sends to the network (allocating there), and hands back a block's
    // buffers for the next. Nothing is allocated here per block (
    // 67 allocations a block, and the card's resampler, on this thread).
    // Blocks in hand: 10 ms or more of them at the desk's rate (4 of 128 and 15 of 32 at 48 kHz;
    // bug run N4-4: 480 was 5 ms at 96 kHz), whatever the block.
    let depth = ((crate::desk::rate_hz() as usize / 100) / bs).max(4);
    let (to_out, from_clock) = std::sync::mpsc::sync_channel::<Vec<Vec<f32>>>(2 * depth);
    let (to_clock, spare) = std::sync::mpsc::sync_channel::<Vec<Vec<f32>>>(2 * depth);
    for _ in 0..depth {
        let _ = to_clock.try_send(vec![vec![0f32; bs]; outputs]);
    }
    {
        let sh = sh.clone();
        let _ = std::thread::Builder::new()
            .name("idn-mixer-out".into())
            .spawn(move || {
                while let Ok(block) = from_clock.recv() {
                    if let Some(card) = sh.card_out.lock_ok().as_ref() {
                        card.push_all(rate_hz(), &block);
                    }
                    let _ = to_clock.try_send(vec![vec![0f32; bs]; block.len()]);
                    sh.endpoints.send(&block, rate_hz());
                    let _ = sh.out.send_audio(
                        AudioBlock {
                            sample_rate: rate_hz(),
                            channels: block,
                        },
                        None,
                    );
                }
            });
    }
    // Blocks in a row that started a period or more late, and how many make 100 ms.
    let mut behind = 0u32;
    let behind_limit = (0.1 * rate()) as u32 / bs as u32;
    while !sh.stop.load(Ordering::Relaxed) {
        // In 64 bits: `period * k as u32` wrapped after 2³² blocks (99 days), and the
        // clock then skipped ahead for ever, the desk silent.
        let due_mono = pacer.due(k);
        let due = base + Duration::from_nanos(due_mono.saturating_sub(base_mono).max(0) as u64);
        if k.is_multiple_of(500) {
            use idn_open_protocol::audio::pace::Source;
            let prefer = match sh.desk.lock_ok().settings().clock.as_str() {
                "card" => Some(Source::Reference),
                "site" => Some(Source::Site),
                "own" => Some(Source::Own),
                _ => None,
            };
            pacer.set_prefer(prefer);
            *sh.clock_source.lock_ok() = (pacer.source_name(), pacer.ppm());
            if let Some(n) = &net {
                *sh.net_audio.lock_ok() = Some(n.snapshot());
            }
        }
        let now = Instant::now();
        if let Some(d) = due.checked_duration_since(now) {
            if !idn_open_protocol::audio::pace::sleep_unless_stopped(d, &sh.stop) {
                break;
            }
        } else if sh.realtime.load(Ordering::Relaxed) {
            // Late: still blocking for a moment each block. Real-time through RealtimeKit,
            // the process may run 200 ms without blocking (RLIMIT_RTTIME), then the kernel
            // stops it.
            std::thread::sleep(Duration::from_micros(1));
        }
        if now
            .checked_duration_since(due)
            .is_some_and(|l| l > Duration::from_millis(50))
        {
            // Far behind (a stall): skip ahead rather than rush.
            let skip = ((now - due).as_nanos() / period.as_nanos()) as u64;
            sh.late.fetch_add(skip, Ordering::Relaxed);
            k += skip;
            continue;
        }
        let t0 = Instant::now();
        sh.clock_beat.beat();
        // For trying the restart (lab): one panic about 5 s in, with `IDN_CLOCK_CRASH_TEST`.
        if crash_test && k >= 5 * u64::from(rate_hz()) / bs as u64 && sh.clock_beat.restarts() == 0
        {
            panic!("IDN_CLOCK_CRASH_TEST: the desk's clock stops, as asked");
        }
        // Real-time and its blocks' own work more than their period for 100 ms (the desk
        // can't keep up): back to normal priority, so the threads feeding it get their
        // turn. (Counted on the work, not on starting late: catching up after one stall
        // under 50 ms started every block late and dropped real-time for good; bug run
        // 2026-11-25.)
        if behind >= behind_limit && sh.realtime.swap(false, Ordering::Relaxed) {
            idn_common::rt::normal_this_thread();
            idn_open_protocol::log::log(
                "mixer: the desk can't keep up with its blocks; its clock back to normal priority",
            );
        }
        {
            let late = t0.saturating_duration_since(due).as_micros() as u64;
            let mut l = sh.start_late.lock_ok();
            if l.2.elapsed() >= Duration::from_secs(10) {
                *l = (l.1, 0, t0);
            }
            l.1 = l.1.max(late);
        }
        input.fill(&mut interleaved);
        for (i, frame) in interleaved.chunks_exact(n).enumerate() {
            for (c, &v) in frame.iter().enumerate() {
                planar[c][i] = v;
            }
        }
        // Phones talking (§M.26): their talkback inputs; else silence, once.
        if sh.iem.talking.load(Ordering::Relaxed) > 0 {
            sh.iem.talk.fill(&mut talk_interleaved);
            for (i, frame) in talk_interleaved.chunks_exact(talk_n).enumerate() {
                for (c, &v) in frame.iter().enumerate() {
                    planar[n + c][i] = v;
                }
            }
            talk_silent = false;
        } else if !talk_silent {
            planar[n..].iter_mut().for_each(|p| p.fill(0.0));
            talk_silent = true;
        }
        // The network's block from the device's rings, onto its channels (and where this
        // block's outputs go, below).
        let net_tick = net.as_mut().and_then(|na| {
            let t = na.tick(idn_open_protocol::node::clock::site_now(), rate_hz())?;
            // (The desk's inputs only: past them are the talkback inputs, which the
            // device's extra channels were added into, block after block.)
            na.read(&t, &mut planar[..n], &mut net_scratch);
            Some(t)
        });
        // Multitrack recording: the inputs as they came (§M.13).
        if let Some(r) = sh.recorder.lock_ok().as_ref() {
            r.push(&planar);
        }
        // Virtual soundcheck: the recording in place of its inputs (silent while paused),
        // but for the ones kept live.
        if let Some(sc) = sh.soundcheck.lock_ok().as_ref() {
            let live = sh.live.lock_ok();
            let kept = |i: usize| live.get(i).copied().unwrap_or(false);
            match sc.next() {
                Some(block) => {
                    for (i, v) in block {
                        if let Some(p) = planar.get_mut(i).filter(|_| !kept(i)) {
                            let k = p.len().min(v.len());
                            p[..k].copy_from_slice(&v[..k]);
                        }
                    }
                }
                None => {
                    for &i in &sc.inputs {
                        if let Some(p) = planar.get_mut(i).filter(|_| !kept(i)) {
                            p.fill(0.0);
                        }
                    }
                }
            }
        }
        let mut d = sh.desk.lock_ok();
        d.process(&planar, &mut out);
        // The IEM flow (§M.26): the marked channels before their faders, to its thread
        // (with a buffer it handed back: nothing allocated here once running).
        if d.iem_len() > 0 {
            let mut buf = sh.iem.spare.lock_ok().try_recv().unwrap_or_default();
            let ids = d.iem_tap(&mut buf);
            drop(d);
            if sh
                .iem
                .to_flow
                .try_send(iem_link::Block { ids, audio: buf })
                .is_err()
            {
                sh.iem.dropped.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            drop(d);
        }
        if let (Some(n), Some(t)) = (&mut net, &net_tick) {
            n.write(
                t,
                &out,
                idn_open_protocol::node::clock::site_now(),
                rate_hz(),
            );
        }
        // (A spare block's buffers, else, rarely, new ones: the output thread behind.)
        let next = spare
            .try_recv()
            .unwrap_or_else(|_| vec![vec![0f32; bs]; out.len()]);
        let block = std::mem::replace(&mut out, next);
        if to_out.try_send(block).is_err() {
            sh.out_dropped.fetch_add(1, Ordering::Relaxed);
        }
        let took = t0.elapsed();
        sh.block_us
            .store(took.as_micros() as u64, Ordering::Relaxed);
        // Over its time: the DSP over its budget (§M.27's `overloaded`).
        if took > period {
            sh.slow_blocks.fetch_add(1, Ordering::Relaxed);
        }
        behind = if took > period { behind + 1 } else { 0 };
        k += 1;
    }
}

#[cfg(test)]
mod parking_tests {
    use super::to_place;

    /// Parked until first routed (or added by hand); a route taken away
    /// never parks it again; the user's own parking stays.
    #[test]
    fn placed_the_first_time_routed_and_never_parked_by_routing() {
        // A fresh desk, nothing routed: nothing placed (all wait, parked).
        assert_eq!(to_place(false, &[None, None], &[false, false]), None);
        // Channel 2 routed: it goes on.
        assert_eq!(
            to_place(false, &[None, None], &[false, true]),
            Some(vec![false, true])
        );
        // Its route gone again: nothing to do (it stays on: shown is set).
        assert_eq!(to_place(false, &[None, Some(true)], &[false, false]), None);
        // Parked by the user, then routed: stays parked.
        assert_eq!(
            to_place(false, &[Some(false), Some(true)], &[true, true]),
            None
        );
        // Automatic parking: routing decides every time; nothing placed.
        assert_eq!(to_place(true, &[None, None], &[true, true]), None);
    }
}

#[cfg(test)]
mod source_tests {
    use super::*;
    use idn_open_protocol::node::routing::AudioOrigin;

    /// A transmitter's channel as a desk input's origin (§M.28.3): a device's own labels,
    /// or a pool's `DEVICE/CHANNEL`; nothing where a pool's channel is unsubscribed.
    #[test]
    fn origins_from_a_transmitters_channels() {
        let card = AudioOrigin {
            kind: "sound_card".into(),
            device: "Focusrite 18i20".into(),
            channels: vec!["In 1".into(), "In 2".into()],
        };
        let o = origin_of(&card, 1).unwrap();
        assert_eq!(
            (o.kind.as_str(), o.device.as_str(), o.channel.as_str()),
            ("sound_card", "Focusrite 18i20", "In 2")
        );
        let pool = AudioOrigin {
            kind: "network_device".into(),
            device: String::new(),
            channels: vec!["Stage Box Left/Mic 5".into(), String::new()],
        };
        let o = origin_of(&pool, 0).unwrap();
        assert_eq!(
            (o.device.as_str(), o.channel.as_str()),
            ("Stage Box Left", "Mic 5")
        );
        assert!(origin_of(&pool, 1).is_none());
        assert!(origin_of(&pool, 9).is_none());
        // A network-audio device's `device.json`: each receive channel's subscription.
        let v = serde_json::json!({"rx": [
            {"name": "RX 1", "from": {"device": "Stage Box Left", "channel": "Mic 5"}},
            {"name": "RX 2", "from": null},
        ]});
        assert_eq!(
            ring_subs_of(&v),
            vec![Some(("Stage Box Left".into(), "Mic 5".into())), None]
        );
    }
}

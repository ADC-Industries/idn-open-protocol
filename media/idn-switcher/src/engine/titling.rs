//! Titles and the data they show: feeds, desks' values, drawing.

use super::*;

/// Titles' data (§S.20): each source read when due (every `every` seconds; a slow one
/// holds up only this thread), rows moved on by themselves when set to. A title whose
/// texts change is drawn again by the titles' thread.
pub(super) fn data_tick(sh: &Arc<Shared>) {
    use idn_open_protocol::input::InputConfig;
    use idn_open_protocol::text::TextReceiver;
    // IDN Data feeds, by source name: received, not read (R.22).
    let mut receivers: BTreeMap<String, TextReceiver> = BTreeMap::new();
    while !sh.stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(250));
        let mut changed = false;
        let feeds: Vec<(u32, String)> = sh
            .title_data
            .lock_ok()
            .iter()
            .filter(|(_, d)| !d.source.feed.is_empty())
            .map(|(id, d)| (*id, d.source.feed.clone()))
            .collect();
        // The closed captions' source (G2) is received here too.
        let from = sh.captions.lock_ok().from.clone();
        receivers.retain(|n, _| feeds.iter().any(|(_, f)| f == n) || from.as_ref() == Some(n));
        if let Some(f) = &from {
            if !receivers.contains_key(f) {
                let cfg = InputConfig {
                    receiver_name: sh.cfg.name.clone(),
                    secure: sh.cfg.secure.clone(),
                    discovery_server: sh.cfg.discovery_server.clone(),
                    ..InputConfig::new(f)
                };
                if let Ok(r) = TextReceiver::open(f, cfg) {
                    receivers.insert(f.clone(), r);
                }
            }
            if let Some(r) = receivers.get(f) {
                let _ = r.captions(Duration::ZERO);
                for line in r.new_finals() {
                    let mut c = sh.captions.lock_ok();
                    if c.on {
                        c.captioner.say(&line.text);
                        c.recent.push_back(line.text.chars().take(200).collect());
                        if c.recent.len() > 8 {
                            c.recent.pop_front();
                        }
                        changed = true;
                    }
                }
            }
        }
        for (id, name) in feeds {
            if !receivers.contains_key(&name) {
                let cfg = InputConfig {
                    receiver_name: sh.cfg.name.clone(),
                    secure: sh.cfg.secure.clone(),
                    discovery_server: sh.cfg.discovery_server.clone(),
                    ..InputConfig::new(&name)
                };
                match TextReceiver::open(&name, cfg) {
                    Ok(r) => {
                        receivers.insert(name.clone(), r);
                    }
                    Err(e) => {
                        idn_open_protocol::log::log(&format!("switcher: data feed {name}: {e}"));
                        continue;
                    }
                }
            }
            // A data feed's rows, or a captions source's text as one row: `{caption}` (its
            // last two lines, the draft too), `{line}` (the one being said), `{final}`.
            let Some(m) = receivers[&name].data(Duration::ZERO).or_else(|| {
                let c = receivers[&name].captions(Duration::ZERO);
                let last = c.last()?;
                let mut row = crate::data::Row::new();
                let two: Vec<&str> = c
                    .iter()
                    .rev()
                    .take(2)
                    .rev()
                    .map(|l| l.text.as_str())
                    .collect();
                row.insert("caption".into(), two.join("\n"));
                row.insert("line".into(), last.text.clone());
                row.insert(
                    "final".into(),
                    c.iter()
                        .rev()
                        .find(|l| l.is_final)
                        .map(|l| l.text.clone())
                        .unwrap_or_default(),
                );
                Some(idn_open_protocol::text::DataMessage::rows(vec![row]))
            }) else {
                continue;
            };
            let mut data = sh.title_data.lock_ok();
            if let Some(d) = data.get_mut(&id).filter(|d| d.source.feed == name)
                && (d.rows != m.rows || d.error != m.error)
            {
                d.rows = m.rows;
                d.row = d.row.min(d.rows.len().saturating_sub(1));
                d.error = m.error;
                d.read_at = Some(Instant::now());
                changed = true;
            }
        }
        let due: Vec<(u32, crate::data::Source)> = sh
            .title_data
            .lock_ok()
            .iter()
            .filter(|(_, d)| {
                d.source.feed.is_empty()
                    && d.read_at
                        .is_none_or(|t| t.elapsed() >= Duration::from_secs_f32(d.source.every))
            })
            .map(|(id, d)| (*id, d.source.clone()))
            .collect();
        for (id, src) in due {
            if sh.stop.load(Ordering::Relaxed) {
                return;
            }
            let read = src.read();
            let mut data = sh.title_data.lock_ok();
            // Unless the title's source changed while it was being read.
            let Some(d) = data.get_mut(&id).filter(|d| d.source == src) else {
                continue;
            };
            d.read_at = Some(Instant::now());
            match read {
                Ok(rows) => {
                    changed |= d.rows != rows || d.error.is_some();
                    d.rows = rows;
                    d.row = d.row.min(d.rows.len().saturating_sub(1));
                    d.error = None;
                }
                Err(e) => {
                    changed |= d.error.as_ref() != Some(&e);
                    d.error = Some(e);
                }
            }
        }
        for d in sh.title_data.lock_ok().values_mut() {
            let every = Duration::from_secs_f32(d.source.advance);
            if d.advancing
                && d.source.advance > 0.0
                && d.rows.len() > 1
                && d.moved_at.elapsed() >= every
            {
                d.row = (d.row + 1) % d.rows.len();
                d.moved_at = Instant::now();
                changed = true;
            }
        }
        if changed {
            let mut s = sh.state.lock_ok();
            bump(&mut s, &sh.changed);
        }
    }
}

/// A placeholder's value besides a title's fields (§S.19): the clock, the date, a timer.
pub(super) fn title_value(sh: &Shared, now: &crate::clock::LocalTime, key: &str) -> Option<String> {
    const DAYS: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let (h, m, s) = (now.hour, now.minute, now.second);
    Some(match key {
        "clock" => format!("{h:02}:{m:02}"),
        "clock:s" => format!("{h:02}:{m:02}:{s:02}"),
        "clock:12" => format!(
            "{}:{m:02} {}",
            (h + 11) % 12 + 1,
            if h < 12 { "am" } else { "pm" }
        ),
        "date" => format!("{}-{:02}-{:02}", now.year, now.month, now.day),
        "date:long" => format!(
            "{} {} {} {}",
            DAYS[now.weekday as usize % 7],
            now.day,
            MONTHS[(now.month as usize + 11) % 12],
            now.year
        ),
        k => {
            let name = k.strip_prefix("timer:")?;
            let timers = sh.timers.lock_ok();
            crate::clock::duration_text(timers.get(name)?.value(now).0)
        }
    })
}

/// Draws a title on its input: its pictures read first (once, with the picture loader),
/// its placeholders filled in now.
pub(super) fn draw_title(
    sh: &Shared,
    slot: &Slot,
    title: &crate::title::Title,
) -> Result<(), String> {
    for file in title.pictures() {
        if sh.title_pictures.lock_ok().contains_key(file) {
            continue;
        }
        let loader = sh
            .loader
            .lock_ok()
            .clone()
            .ok_or("no picture loader for the title's pictures")?;
        let f = loader(file).map_err(|e| format!("{file}: {e}"))?;
        sh.title_pictures
            .lock_ok()
            .insert(file.to_owned(), Arc::new(f.pixels()));
    }
    let now = crate::clock::local_now();
    let row = data_row(sh, slot.id);
    let value = |k: &str| row_value(&row, k).or_else(|| title_value(sh, &now, k));
    let pictures = sh.title_pictures.lock_ok().clone();
    let picture = |p: &str| pictures.get(p).cloned();
    let texts = title.texts(&|k| {
        row_value(&row, k)
            .or_else(|| title.fields.get(k).cloned())
            .or_else(|| title_value(sh, &now, k))
    });
    let frame = {
        let mut fonts = sh.fonts.lock_ok();
        let fonts = fonts.get_or_insert_with(crate::title::Fonts::new);
        crate::title::render_with(
            title,
            sh.cfg.width,
            sh.cfg.height,
            fonts,
            &crate::title::Lookups {
                value: &value,
                picture: &picture,
            },
        )
    };
    store(slot, Arc::new(frame), None);
    sh.title_texts.lock_ok().insert(slot.id, texts);
    Ok(())
}

/// Timers reaching their end (their trigger events), and titles showing a clock or a
/// timer drawn again when what they show changes. Its own thread, four times a second.
pub(super) fn titles_tick(sh: &Arc<Shared>) {
    let mut shown: BTreeMap<String, u64> = BTreeMap::new();
    while !sh.stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(250));
        let now = crate::clock::local_now();
        let mut events = Vec::new();
        let mut changed = false;
        {
            let mut timers = sh.timers.lock_ok();
            shown.retain(|n, _| timers.contains_key(n));
            for (name, t) in timers.iter_mut() {
                let (secs, at_end) = t.value(&now);
                if shown.insert(name.clone(), secs) != Some(secs) {
                    changed = true;
                }
                let counting = t.since.is_some() || matches!(t.mode, TimerMode::Until { .. });
                if at_end && counting && !t.ended {
                    t.ended = true;
                    if let TimerMode::Down(total) = t.mode {
                        t.banked = total;
                        t.since = None;
                    }
                    events.push(TriggerEvent::TimerEnd(name.clone()));
                }
                // An Until timer's day ends: it counts again tomorrow.
                if !at_end && matches!(t.mode, TimerMode::Until { .. }) {
                    t.ended = false;
                }
            }
        }
        for ev in &events {
            run_event(sh, ev);
        }
        // Titles whose texts now read differently (a clock, a timer, their data).
        let moving: Vec<(u32, crate::title::Title)> = sh
            .titles
            .lock_ok()
            .iter()
            .filter(|(_, t)| t.is_moving() || t.data.is_some())
            .map(|(id, t)| (*id, t.clone()))
            .collect();
        for (id, title) in moving {
            let row = data_row(sh, id);
            let texts = title.texts(&|k| {
                row_value(&row, k)
                    .or_else(|| title.fields.get(k).cloned())
                    .or_else(|| title_value(sh, &now, k))
            });
            if sh.title_texts.lock_ok().get(&id) == Some(&texts) {
                continue;
            }
            let slot = sh.inputs.lock_ok().get(&id).cloned();
            if let Some(slot) = slot {
                let _ = draw_title(sh, &slot, &title);
            }
        }
        if !events.is_empty() {
            let mut s = sh.state.lock_ok();
            bump(&mut s, &sh.changed);
        } else if changed {
            // Only the timers' seconds moved: counted on their own (`timer_ticks`), which
            // moves the public version but not the state version of real changes.
            let _s = sh.state.lock_ok();
            sh.timer_ticks.fetch_add(1, Ordering::Relaxed);
            sh.changed.notify_all();
        }
    }
}

/// What a desk says it feeds: `<idn_feeds feeds="HOST (Switcher)=12,13&#10;…"/>`, each
/// line a switcher output's full name and the desk's two channels (from 0).
pub fn parse_feeds(xml: &str) -> Option<BTreeMap<String, [u16; 2]>> {
    let e = idn_open_protocol::control::parse_element(xml)?;
    if e.name != "idn_feeds" {
        return None;
    }
    Some(
        e.attr("feeds")
            .unwrap_or("")
            .split('\n')
            .filter_map(|l| {
                let (name, ch) = l.rsplit_once('=')?;
                let (a, b) = ch.split_once(',')?;
                Some((
                    name.trim().to_owned(),
                    [a.trim().parse().ok()?, b.trim().parse().ok()?],
                ))
            })
            .collect(),
    )
}

/// Listens to the desks on the network (sources announcing `desk/1`) and routes this
/// switcher's receiver from the channels each desk feeds its outputs with (§S.26): the
/// program's pair, each mix slot's. Only this machine's receiver changes. A feed a desk
/// stops naming is taken off; a desk that goes away leaves its routes (it comes back).
pub(super) fn watch_desks(sh: &Arc<Shared>) {
    use idn_open_protocol::input::{Input, InputConfig, InputEvent};
    use idn_open_protocol::node::routing::RouteChange;
    let node = sh.cfg.node.clone();
    let browser =
        match idn_open_protocol::discovery::shared(idn_open_protocol::discovery::BrowserConfig {
            interface: None,
            discovery_server: sh.cfg.discovery_server.clone(),
        }) {
            Ok(b) => b,
            Err(e) => {
                idn_open_protocol::log::log(&format!("switcher: desks' feeds: {e}"));
                return;
            }
        };
    idn_open_protocol::log::log(&match &sh.cfg.discovery_server {
        Some(sv) => format!("switcher: desks and data feeds from the Discovery Server {sv}"),
        None => "switcher: desks and data feeds from mDNS".into(),
    });
    // What each desk last said, by desk.
    let mut said: BTreeMap<String, BTreeMap<String, [u16; 2]>> = BTreeMap::new();
    let mut look = Instant::now();
    while !sh.stop.load(Ordering::Relaxed) {
        if Instant::now() >= look {
            look = Instant::now() + Duration::from_secs(2);
            let all = browser.sources();
            *sh.listed.lock_ok() = Some(
                all.iter()
                    .map(|s| {
                        let at = s.socket_addr().map(|a| a.to_string()).unwrap_or_default();
                        (s.name.clone(), at)
                    })
                    .collect(),
            );
            *sh.text_sources.lock_ok() = all
                .iter()
                .filter(|s| s.offers("text"))
                .map(|s| s.name.clone())
                .collect();
            *sh.other_sources.lock_ok() = all
                .iter()
                .filter(|s| !s.offers("text"))
                .map(|s| s.name.clone())
                .collect();
            let here: Vec<String> = all
                .into_iter()
                .filter(|s| s.offers("desk"))
                .map(|s| s.name)
                .collect();
            let mut desks = sh.desks.lock_ok();
            desks.retain(|n, _| here.contains(n));
            for name in here {
                if desks.contains_key(&name) {
                    continue;
                }
                let cfg = InputConfig {
                    receiver_name: sh.cfg.name.clone(),
                    secure: sh.cfg.secure.clone(),
                    discovery_server: sh.cfg.discovery_server.clone(),
                    receive_video: false,
                    audio_channels: Some(Vec::new()),
                    ..InputConfig::new(&name)
                };
                match Input::open(cfg) {
                    Ok(i) => {
                        desks.insert(name, (Arc::new(i), None));
                    }
                    Err(e) => idn_open_protocol::log::log(&format!("switcher: desk {name}: {e}")),
                }
            }
        }
        let desks: Vec<(String, Arc<Input>)> = sh
            .desks
            .lock_ok()
            .iter()
            .map(|(n, (i, _))| (n.clone(), i.clone()))
            .collect();
        if desks.is_empty() {
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }
        let mut heard = false;
        for (name, input) in &desks {
            let wait = Duration::from_millis(250 / desks.len() as u64);
            if let Some(InputEvent::Metadata { xml, .. }) = input.recv(wait)
                && let Some(f) = parse_feeds(&xml)
            {
                if let Some(d) = sh.desks.lock_ok().get_mut(name) {
                    d.1 = Some(Instant::now());
                }
                if said.get(name) != Some(&f) {
                    said.insert(name.clone(), f);
                    heard = true;
                }
            }
        }
        // The feeds are routed by the Node (none: the desks' scenes still work).
        let Some(node) = node.as_ref().filter(|_| heard) else {
            continue;
        };
        // This switcher's outputs that take a feed, and where on the receiver.
        let mut outputs = vec![(sh.program_out.name().to_owned(), 0usize)];
        {
            let s = sh.state.lock_ok();
            let outs = sh.mix_outs.lock_ok();
            for (id, m) in &s.mixes {
                if let Some(o) = outs.get(id) {
                    outputs.push((o.name().to_owned(), 2 + 2 * m.slot));
                }
            }
        }
        let mut applied = sh.feeds.lock_ok().clone();
        for (out, first) in outputs {
            let want = said.iter().find_map(|(desk, f)| {
                f.get(&out).map(|&ch| Feed {
                    desk: desk.clone(),
                    ch,
                })
            });
            if applied.get(&out) == want.as_ref() {
                continue;
            }
            // Taken off only when the desk that fed it says so (not when it goes away).
            if want.is_none()
                && applied
                    .get(&out)
                    .is_some_and(|f| !said.contains_key(&f.desk))
            {
                continue;
            }
            let changes: Vec<RouteChange> = (0..2)
                .map(|k| RouteChange {
                    receiver: sh.cfg.name.clone(),
                    ch: (first + k) as u16,
                    to: want.as_ref().map(|f| (f.desk.clone(), f.ch[k])),
                })
                .collect();
            let ok = changes.iter().all(|c| {
                node.request(c.to_element())
                    .is_ok_and(|r| r.name != "error")
            });
            if ok {
                idn_open_protocol::log::log(&format!(
                    "switcher: {out}: sound {}",
                    want.as_ref().map_or("as the stream's".into(), |f| format!(
                        "from {} {}–{}",
                        f.desk,
                        f.ch[0] + 1,
                        f.ch[1] + 1
                    ))
                ));
                match want {
                    Some(f) => applied.insert(out, f),
                    None => applied.remove(&out),
                };
            }
        }
        let changed = *sh.feeds.lock_ok() != applied;
        if changed {
            *sh.feeds.lock_ok() = applied;
            let mut s = sh.state.lock_ok();
            bump(&mut s, &sh.changed);
        }
    }
}

//! Reading the devices (idn-aoip-control.md §7, §1.1) one at a time, and writing the list as it changes.

use super::*;

/// Reads devices as they come due, one at a time, and writes the list when it changes.
pub(crate) fn reader(
    ip: Ipv4Addr,
    own: (&str, Option<SocketAddr>),
    dir: &std::path::Path,
    devices: &Mutex<BTreeMap<String, Entry>>,
    dropped: &std::sync::atomic::AtomicBool,
) {
    let file = dir.join("devices.xml");
    let socket = match UdpSocket::bind((ip, 0)) {
        Ok(s) => {
            // (No "connection reset" reports on Windows: M8-4.)
            crate::engine::no_udp_resets(&s);
            s
        }
        Err(e) => {
            idn_common::log::log_or_stderr(&format!(
                "idn-aoip: network devices: no socket to ask them ({e})"
            ));
            return;
        }
    };
    let mut ids = Ids(0);
    let mut written = String::new();
    // Written at least every minute, so a reader can tell a running device's list from a
    // stopped one's (bug run I4-5).
    let mut written_at = Instant::now();
    let mut said: Option<(String, Instant)> = None;
    loop {
        // A device whose setting the Node just changed: read again at once (R.32 stage 2).
        wanted_by_file(&dir.join(REREAD_DIR), devices);
        let now = Instant::now();
        let due = {
            let mut d = devices.lock_ok();
            // Kept while mDNS says it is there or it answers reads (a device gone without a
            // goodbye: its reads fail, dropped a minute on).
            let before = d.len();
            d.retain(|_, e| {
                e.last_seen.is_some_and(|t| now.duration_since(t) < UNSEEN)
                    || e.answered_at
                        .is_some_and(|t| now.duration_since(t) < ANSWERED_KEEPS)
            });
            if d.len() < before {
                dropped.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            // The due one read longest ago (never read first, then changed ones): not
            // the first in name order (bug run I3-6: devices late in the order were
            // never read behind a few silent ones).
            let due = d
                .iter()
                .filter(|(_, e)| e.seen.is_some())
                .filter(|(_, e)| {
                    e.read_at.is_none_or(|t| {
                        let since = now.duration_since(t);
                        since >= REREAD || (e.wanted && since >= Duration::from_secs(1))
                    })
                })
                .min_by_key(|(_, e)| (e.read_at.is_some(), !e.wanted, e.read_at))
                .and_then(|(n, e)| {
                    let ask_settings = e
                        .settings_failed
                        .is_none_or(|t| now.duration_since(t) >= SETTINGS_AGAIN);
                    Some((n.clone(), e.seen.clone()?, ask_settings))
                });
            // The change flag cleared as the read starts: a change heard during it reads
            // the device again (bug run I3-8: it was cleared after, and lost).
            if let Some(e) = due.as_ref().and_then(|(n, _, _)| d.get_mut(n)) {
                e.wanted = false;
            }
            due
        };
        if let Some((name, seen, ask_settings)) = due {
            let protocol = inv::arc_protocol(seen.arc_version.as_deref());
            // A device whose last read failed gets one try a request, not three (bug run
            // M3-1: 3 s each, the reader too slow for the ones that answer).
            let failing = {
                let d = devices.lock_ok();
                d.get(&name).is_some_and(|e| {
                    e.read_at.is_some()
                        && e.answered_at
                            .is_none_or(|a| e.read_at.is_some_and(|r| a < r))
                })
            };
            TRIES_NOW.with(|t| t.set(if failing { 1 } else { TRIES }));
            let result = read(&socket, seen.arc, protocol, &mut ids);
            // Present by a read only when the device read is the one listed (bug run L3-4:
            // a renamed device's old name, or another device on its old address, kept for
            // good).
            let answered = match &result {
                Ok(r) | Err((Some(r), _)) => r.name.eq_ignore_ascii_case(&name),
                Err((None, _)) => false,
            };
            // Another name read there: the device was renamed; its new name looked for
            // at once (M3-2).
            if matches!(&result, Ok(r) | Err((Some(r), _)) if !r.name.eq_ignore_ascii_case(&name)) {
                dropped.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            // Its settings (stage 2: shown, not changed yet), from one that answers.
            let settings = (answered && ask_settings).then(|| {
                ask(&socket, seen.arc, &inv::settings_request(ids.next()))
                    .ok()
                    .and_then(|d| inv::parse_settings(&d).ok())
            });
            let mut d = devices.lock_ok();
            if let Some(e) = d.get_mut(&name) {
                e.read_at = Some(Instant::now());
                if answered {
                    e.answered_at = e.read_at;
                }
                match settings {
                    Some(Some(s)) => {
                        e.settings = Some(s);
                        e.settings_failed = None;
                    }
                    Some(None) => e.settings_failed = e.read_at,
                    None => {}
                }
                match result {
                    Ok(r) => {
                        e.reading = Some(r);
                        e.error = None;
                    }
                    // What was read before stays shown, with why it is now stale; a
                    // device never read whole is shown by its name and counts.
                    Err((bare, why)) => {
                        e.error = Some(why);
                        if e.reading.is_none() {
                            e.reading = bare;
                        }
                    }
                }
            }
        }
        let doc = document(own, &devices.lock_ok());
        if doc != written || written_at.elapsed() >= Duration::from_secs(60) {
            // Kept as written only when it was (bug run I3-10: a failed write wasn't tried
            // again until the list changed).
            // Unchanged: its time set to now, so a reader still sees a running device's list
            // (lab Alpha84 538-A: the write of an unchanged file is skipped, the file aged,
            // and past 3 min the Node reported no list at all).
            // (Written again when the file is gone: M3-7.)
            let done = if doc == written {
                touch(&file).or_else(|_| idn_common::fsutil::write_atomic(&file, doc.as_bytes()))
            } else {
                idn_common::fsutil::write_atomic(&file, doc.as_bytes())
            };
            match done {
                Ok(()) => {
                    written = doc;
                    written_at = Instant::now();
                }
                // Said once, and again when it changes or a minute on (bug run N3-9:
                // twice a second, the Node's log filled with it).
                Err(e) => {
                    let e = e.to_string();
                    if said
                        .as_ref()
                        .is_none_or(|(w, at)| *w != e || at.elapsed() >= Duration::from_secs(60))
                    {
                        idn_common::log::log_or_stderr(&format!(
                            "idn-aoip: {}: {e}",
                            file.display()
                        ));
                        said = Some((e, Instant::now()));
                    }
                }
            }
        }
        if due_none(devices) {
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

/// The folder in the state folder where the Node names devices to read again: one empty
/// file a device, named by its instance in hex (a request never lost to another's write,
/// bug run 2026-11-26: one shared file read, then removed, dropped a line added between).
pub const REREAD_DIR: &str = "reread.d";

/// A device's file name in [`REREAD_DIR`]: its instance's bytes in hex (any instance name
/// is a safe file name so).
pub fn reread_name(instance: &str) -> String {
    idn_common::hex::hex(instance.as_bytes())
}

/// Asks the device list in `state_dir` to read `instance` again at once (the Node, after
/// it changed one of the device's settings; R.32 stage 2).
pub fn ask_reread(state_dir: &std::path::Path, instance: &str) -> std::io::Result<()> {
    let dir = state_dir.join(REREAD_DIR);
    std::fs::create_dir_all(&dir)?;
    std::fs::File::create(dir.join(reread_name(instance))).map(|_| ())
}

/// The devices named in `dir` marked to read again, their settings too. Each file is
/// removed before its device is marked: one made again meanwhile stays for the next round.
pub(crate) fn wanted_by_file(dir: &std::path::Path, devices: &Mutex<BTreeMap<String, Entry>>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut names = Vec::new();
    for e in rd.flatten() {
        let file = e.file_name();
        let Some(hex) = file.to_str() else { continue };
        let bytes = idn_common::hex::unhex(hex);
        if std::fs::remove_file(e.path()).is_ok()
            && let Some(name) = bytes.and_then(|b| String::from_utf8(b).ok())
        {
            names.push(name);
        }
    }
    if names.is_empty() {
        return;
    }
    let mut d = devices.lock_ok();
    for name in names {
        if let Some(e) = d.get_mut(&name) {
            e.wanted = true;
            e.settings_failed = None;
        }
    }
}

/// A file's modification time set to now (its content as it is).
pub(crate) fn touch(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::File::options()
        .write(true)
        .open(path)?
        .set_modified(std::time::SystemTime::now())
}

pub(crate) fn due_none(devices: &Mutex<BTreeMap<String, Entry>>) -> bool {
    let now = Instant::now();
    devices
        .lock_ok()
        .values()
        .all(|e| e.seen.is_none() || e.read_at.is_some_and(|t| now.duration_since(t) < REREAD))
}

/// Message IDs: from 1, never 0 (§0.2).
pub(crate) struct Ids(pub(crate) u16);

impl Ids {
    pub(crate) fn next(&mut self) -> u16 {
        self.0 = self.0.wrapping_add(1).max(1);
        self.0
    }
}

/// [`control::exchange`](crate::control::exchange) with this thread's tries and the
/// 1 s wait.
pub(crate) fn ask(socket: &UdpSocket, to: SocketAddr, req: &[u8]) -> Result<Vec<u8>, String> {
    crate::control::exchange(socket, to, req, TRIES_NOW.with(|t| t.get()), WAIT)
}

/// Reads one device: its name and counts, then (legacy path) its channels.
pub(crate) fn read(
    socket: &UdpSocket,
    arc: SocketAddr,
    protocol: u16,
    ids: &mut Ids,
) -> Result<Reading, (Option<Reading>, String)> {
    let m = |e: Malformed| e.to_string();
    let name = ask(socket, arc, &inv::name_request(ids.next()))
        .and_then(|d| inv::parse_name(&d).map_err(m))
        .map_err(|e| (None, e))?;
    let counts = ask(socket, arc, &inv::counts_request(ids.next()))
        .and_then(|d| inv::parse_counts(&d).map_err(m))
        .map_err(|e| (None, e))?;
    let bare = Reading {
        name,
        counts,
        rx: None,
        tx: None,
    };
    if inv::is_modern(protocol) {
        return Ok(bare);
    }
    // Its name and counts kept when its channels can't be read (lab 529(e)).
    match channels(socket, arc, counts, ids) {
        Ok((rx, tx)) => Ok(Reading {
            rx: Some(rx),
            tx: Some(tx),
            ..bare
        }),
        Err(why) => Err((Some(bare), why)),
    }
}

/// A device's receive and transmit channels (§7.2–§7.4).
pub(crate) fn channels(
    socket: &UdpSocket,
    arc: SocketAddr,
    counts: inv::Counts,
    ids: &mut Ids,
) -> Result<(Vec<RxChannel>, Vec<TxChannel>), String> {
    let m = |e: Malformed| e.to_string();
    if counts.rx > MOST_CHANNELS || counts.tx > MOST_CHANNELS {
        return Err(format!(
            "{} in, {} out: more channels than are read ({MOST_CHANNELS} each way; outputs only as \
             announced, {MOST_ANNOUNCED} at most)",
            counts.rx, counts.tx
        ));
    }
    let began = std::time::Instant::now();
    let in_time = || {
        if began.elapsed() > READ_WITHIN {
            Err("too slow listing its channels".to_owned())
        } else {
            Ok(())
        }
    };
    // Receive channels, page by page (§7.2).
    let mut rx: Vec<RxChannel> = Vec::new();
    let mut start = 1u16;
    for _ in 0..MOST_PAGES {
        if counts.rx == 0 {
            break;
        }
        in_time()?;
        let page = inv::parse_rx_page(
            &ask(socket, arc, &inv::rx_page_request(ids.next(), start))?,
            start,
        )
        .map_err(m)?;
        rx.extend(page.items.iter().cloned());
        match inv::next_rx_start(&page, rx.len(), usize::from(counts.rx)) {
            Some(s) => start = s,
            None => break,
        }
    }
    if rx.len() != usize::from(counts.rx) {
        return Err(format!(
            "receiver channel count disagrees with inventory ({} listed, {} said)",
            rx.len(),
            counts.rx
        ));
    }
    // Transmit channels (§7.4), their friendly names (§7.3) over the page's names.
    let mut tx: Vec<TxChannel> = Vec::new();
    let mut start = 1u16;
    for _ in 0..MOST_PAGES {
        if counts.tx == 0 {
            break;
        }
        in_time()?;
        let d = ask(socket, arc, &inv::tx_page_request(ids.next(), start))?;
        let said = d.get(11).copied().map_or(0, usize::from);
        let page = inv::parse_tx_page(&d, start).map_err(m)?;
        if page.items.is_empty() {
            break;
        }
        tx.extend(page.items.iter().cloned());
        if tx.len() > usize::from(counts.tx) {
            return Err("transmitter channel count disagrees with inventory".into());
        }
        match inv::next_tx_start(&page, said) {
            Some(s) => start = s,
            None => break,
        }
    }
    if counts.tx > 0 && tx.is_empty() {
        return Err("the device advertises transmitter channels but listed none".into());
    }
    if counts.tx > 0 {
        let names = inv::parse_tx_names(&ask(
            socket,
            arc,
            &inv::tx_names_request(ids.next(), counts.tx),
        )?)
        .map_err(m)?;
        let mut named = std::collections::BTreeSet::new();
        for (n, label) in names {
            if let Some(c) = tx.iter_mut().find(|c| c.number == n) {
                c.name = Some(label);
                named.insert(n);
            }
        }
        // Past 32 outputs the first answer holds 32 names at most (D6 §7.3): the others
        // asked page by page from 33, for the ones not named yet (lab Alpha85 542-B: an
        // output past 32 not renamed showed its factory name, "39", not "TX 39"). A device
        // that doesn't answer them keeps its page names.
        let mut start = 33u16;
        while start <= counts.tx {
            in_time()?;
            let Some(names) = ask(
                socket,
                arc,
                &inv::tx_names_request_from(ids.next(), start, counts.tx),
            )
            .ok()
            .and_then(|d| inv::parse_tx_names(&d).ok()) else {
                break;
            };
            if names.is_empty() {
                break;
            }
            for (n, label) in names {
                if !named.contains(&n)
                    && let Some(c) = tx.iter_mut().find(|c| c.number == n)
                {
                    c.name = Some(label);
                }
            }
            start = start.saturating_add(32);
        }
    }
    Ok((rx, tx))
}

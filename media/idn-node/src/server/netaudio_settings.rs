//! Another device's settings changed from the Site Manager (roadmap R.32 stage 2,
//! idn-aoip-controller.md K.6, K.14): receive latency, sample rate, encoding, AES67,
//! preferred leader, word clock, identify, reboot, factory reset and clear configuration.
//! Each is offered only once checked on real equipment (`writes::CHECKED`; the lab switch
//! IDN_NETAUDIO_UNCHECKED offers the others for testing); reboot and the resets only with
//! the device's name as `confirm`. The Node sends them itself with IDN's own builders
//! (`idn_aoip_device::writes`, spec idn-aoip-control.md), reads the device back, and has
//! this machine's network-audio device read it again at once.

use super::netaudio_do::{Target, ask, find_device, next_id};
use super::*;
use idn_aoip_device::writes::{self as w, Command, kind};
use idn_aoip_device::{control, inventory as inv};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};

/// A device's settings read, kept 5 s per device (its mDNS instance), so a page asking
/// again doesn't hold one of the Node's places for seconds each time; a change sent to the
/// device drops it.
/// A device's settings as read: (name, value) pairs, and when.
type Read = (Instant, Vec<(String, String)>);
static READS: Mutex<BTreeMap<String, Read>> = Mutex::new(BTreeMap::new());
const READ_KEPT: Duration = Duration::from_secs(5);

fn forget_read(instance: &str) {
    READS.lock_ok().remove(instance);
}

/// A result sent but not confirmed by the device: said so, plainly.
fn unconfirmed(name: &str, what: &str, why: &str) -> String {
    format!("{name} {what}: sent; the device doesn't confirm it yet ({why})")
}

/// Whether the lab switch offers the commands not checked on real equipment yet.
pub(super) fn unchecked() -> bool {
    idn_common::switches::on(w::UNCHECKED_SWITCH)
}

/// The ops this machine offers (the lab gate; read only is said apart), for its report.
pub(super) fn offered_now() -> String {
    w::offered_ops(unchecked()).join(" ")
}

/// Whether this machine may make `c` (K.5, K.14): its network audio on, not read only,
/// and the command checked in the lab or the lab switch on.
pub(super) fn may(on: bool, read_only: bool, c: Command, unchecked: bool) -> Result<(), String> {
    if !on {
        return Err("the network audio device isn't on here".into());
    }
    if read_only {
        return Err("this machine's network audio is read only (its setting)".into());
    }
    w::gate(c, unchecked)
}

/// A yes/no value as the API gives it.
fn yes_no(v: &str) -> Result<bool, String> {
    idn_common::text::parse_on_off(v).ok_or_else(|| format!("{v:?}: give true or false"))
}

/// Nanoseconds as milliseconds, said plainly (0, 0.25, 1, 2).
fn ms(ns: u32) -> String {
    let s = format!("{:.3}", f64::from(ns) / 1e6);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() { "0" } else { s }.to_owned()
}

fn list(v: &[u32]) -> String {
    v.iter().map(u32::to_string).collect::<Vec<_>>().join(" ")
}

/// Asks this machine's network-audio device to read `instance` again at once (a file in
/// its state folder's `reread.d`), so the list shows what the device now has; else within
/// 30 s.
fn reread(s: &Settings, instance: &str) {
    let host = idn_open_protocol::discovery::hostname();
    let state = idn_common::paths::program_dir("aoip").join(s.netaudio.device_name(&host));
    let _ = idn_aoip_device::devices::ask_reread(&state, instance);
}

/// A listed device: its element, where it is, its name and mDNS instance; not this
/// machine's own (set in the Node's settings).
fn listed<'a>(doc: &'a Element, device: &str) -> Result<(&'a Element, Target, String), String> {
    let (d, t) = find_device(doc, device)?;
    if d.attr("this") == Some("true") {
        return Err(format!(
            "{device} is this machine's own network audio device: set it in the Node's settings"
        ));
    }
    let name = d.attr("name").unwrap_or(device).to_owned();
    Ok((d, t, name))
}

fn ipv4(t: &Target) -> Result<Ipv4Addr, String> {
    match t.arc {
        SocketAddr::V4(a) => Ok(*a.ip()),
        SocketAddr::V6(_) => Err("not an IPv4 device".into()),
    }
}

impl NodeCore {
    /// `<netaudio_do op="set_latency|set_rate|…|clear_config" device value confirm/>`: done
    /// (with what the device reads back), or why not.
    pub(super) fn netaudio_setting(&self, c: Command, d: &Element) -> Result<String, String> {
        let s = self.settings();
        may(s.netaudio.on, s.netaudio.read_only, c, unchecked())?;
        let a = |k: &str| d.attr(k).unwrap_or("").trim().to_owned();
        let doc = self.netaudio_devices()?;
        let (dev, t, name) = listed(&doc, &a("device"))?;
        let instance = dev.attr("instance").unwrap_or(&name).to_owned();
        // Reboot and the resets: the device's current name typed (K.5).
        let confirm = a("confirm");
        w::confirmed(c, &name, &confirm)?;
        let named = !name.is_empty() && confirm == name;
        let value = a("value");
        // Whether anything reached the device: then it is read again however this ends
        // (bug run 2026-11-26: only a full success had it read again).
        let mut sent = false;
        let result = if c == Command::Latency {
            self.set_latency(dev, &t, &name, &value, &mut sent)
        } else {
            match ipv4(&t).and_then(|ip| self.talk(ip)) {
                Err(e) => Err(e),
                Ok(talk) => {
                    let r = (|| -> Result<String, String> {
                        Ok(match c {
                            Command::Rate | Command::Encoding => {
                                set_choice(&talk, &name, c, &value)?
                            }
                            Command::Aes67 => set_aes67(&talk, &name, yes_no(&value)?)?,
                            Command::PreferredLeader | Command::WordClock => {
                                set_clock(&talk, &name, c, yes_no(&value)?, named)?
                            }
                            Command::Identify => {
                                if caps_of(&talk, None).is_some_and(|k| k.lacks(w::cap::IDENTIFY)) {
                                    return Err(format!("{name} says it can't identify itself"));
                                }
                                talk.send(&w::identify(next_id()))?;
                                format!(
                                    "{name} asked to identify itself (it sends no answer: look at it)"
                                )
                            }
                            Command::Reboot => {
                                talk.send(&w::reboot(next_id(), talk.mac))?;
                                format!(
                                    "{name}: restart sent (it leaves the list and comes back when up)"
                                )
                            }
                            Command::FactoryReset => {
                                talk.send(&w::factory_reset(next_id(), talk.mac))?;
                                format!("{name}: factory reset sent (it restarts)")
                            }
                            Command::ClearConfig => clear(&talk, &name, &value)?,
                            Command::Latency => unreachable!("handled above"),
                        })
                    })();
                    sent = talk.sent();
                    r
                }
            }
        };
        if sent {
            reread(&s, &instance);
            forget_read(&instance);
        }
        match &result {
            Ok(said) => self.log(&format!("network audio: {said} (from the Site Manager)")),
            Err(e) if sent => self.log(&format!(
                "network audio: {name} {}: {e} (from the Site Manager)",
                c.what()
            )),
            Err(_) => {}
        }
        result
    }

    /// `<netaudio_do op="read_settings" device/>`: what the device has and offers, as
    /// its statuses say (§2.2, §4.1, §4.2, §5.1, §6.5.4), and the commands this machine
    /// offers for it. Reading: allowed on a read-only site too.
    pub(super) fn netaudio_read_settings(
        &self,
        d: &Element,
    ) -> Result<Vec<(String, String)>, String> {
        let s = self.settings();
        if !s.netaudio.on {
            return Err("the network audio device isn't on here".into());
        }
        let doc = self.netaudio_devices()?;
        let (dev, t, name) = listed(&doc, d.attr("device").unwrap_or("").trim())?;
        let instance = dev.attr("instance").unwrap_or(&name).to_owned();
        if let Some((at, kept)) = READS.lock_ok().get(&instance)
            && at.elapsed() < READ_KEPT
        {
            return Ok(kept.clone());
        }
        let mut out: Vec<(String, String)> = Vec::new();
        let mut put = |k: &str, v: String| out.push((k.to_owned(), v));
        let un = unchecked();
        put("name", name);
        put(
            "commands",
            if s.netaudio.read_only {
                String::new()
            } else {
                w::offered_ops(un).join(" ")
            },
        );
        put("unchecked", un.to_string());
        put("read_only", s.netaudio.read_only.to_string());
        // The latency as the list read it (§1.1), and the choices in its range.
        let ns = |k: &str| {
            dev.attr(k)
                .and_then(|v| v.parse::<f64>().ok())
                .and_then(w::ms_to_ns)
        };
        if let Some(l) = ns("latency_ms") {
            put("latency_ms", ms(l));
        }
        if let (Some(lo), Some(hi)) = (ns("latency_min_ms"), ns("latency_max_ms")) {
            let c: Vec<String> = w::latency_choices(lo, hi).into_iter().map(ms).collect();
            put("latency_choices_ms", c.join(" "));
        }
        let talk = self.talk(ipv4(&t)?)?;
        for req in [
            w::rate_probe(next_id(), talk.mac),
            w::encoding_probe(next_id(), talk.mac),
            w::aes67_probe(next_id(), talk.mac),
            w::clock_query(next_id(), talk.mac),
        ] {
            talk.send(&req)?;
        }
        let got = talk.statuses(
            &[
                kind::RATE_STATUS,
                kind::ENCODING_STATUS,
                kind::AES67_STATUS,
                kind::CLOCK_STATUS,
            ],
            Duration::from_secs(2),
        );
        let mut heard = Vec::new();
        let mut device_id = None;
        for st in &got {
            let Some(h) = w::status(st) else { continue };
            device_id.get_or_insert(h.device);
            match h.kind {
                kind::RATE_STATUS | kind::ENCODING_STATUS => {
                    let Ok(c) = w::parse_choice(st, h.kind) else {
                        continue;
                    };
                    let k = if h.kind == kind::RATE_STATUS {
                        "rate"
                    } else {
                        "encoding"
                    };
                    heard.push(k);
                    put(k, c.current.to_string());
                    put(&format!("{k}s"), list(&c.offered));
                    put(&format!("{k}_fixed"), (!c.writable()).to_string());
                }
                kind::AES67_STATUS => {
                    let Ok(x) = w::parse_aes67(st) else { continue };
                    heard.push("aes67");
                    put("aes67", x.configured.to_string());
                    put("aes67_now", x.current.to_string());
                    put("aes67_reboot_needed", x.reboot_needed().to_string());
                }
                kind::CLOCK_STATUS => {
                    let Ok(k) = w::parse_clock(st) else { continue };
                    heard.push("clock");
                    put("clock_source", w::source_name(k.source));
                    put("preferred_leader", k.leader.to_string());
                    put("leader_fixed", (!k.leader_writable()).to_string());
                    put("clock_fixed", (!k.writable()).to_string());
                    if let Some(v) = k.word_clock {
                        put("word_clock_state", w::word_clock_state(v).to_owned());
                    }
                }
                _ => {}
            }
        }
        put("heard", heard.join(" "));
        if let Some(caps) = caps_of(&talk, device_id) {
            let has = |bit| (!caps.lacks(bit)).to_string();
            if caps.primary.is_some() {
                put("can_identify", has(w::cap::IDENTIFY));
                put("can_rate", has(w::cap::RATE));
                put("can_encoding", has(w::cap::ENCODING));
                put("can_aes67", has(w::cap::AES67));
            }
            if let Some(ro) = caps.read_only {
                put(
                    "word_clock_read_only",
                    (ro & w::cap::RO_WORD_CLOCK != 0).to_string(),
                );
            }
        }
        let mut reads = READS.lock_ok();
        reads.retain(|_, (at, _)| at.elapsed() < READ_KEPT);
        reads.insert(instance, (Instant::now(), out.clone()));
        Ok(out)
    }

    /// A settings-port conversation with the device at `ip`, from this machine's
    /// network-audio interface (its MAC in the messages; §0.3's fallback when unknown).
    fn talk(&self, ip: Ipv4Addr) -> Result<w::Talk, String> {
        let iface = self.interface_toward(ip);
        let mac = idn_common::net::mac_of(iface).unwrap_or(w::FALLBACK_MAC);
        w::Talk::open(iface, ip, mac, next_id())
    }

    /// The interface to reach `ip` from: the setting; else the address the system sends
    /// from toward it; else the one idn-aoip takes with no setting (HostInfo::local). Its
    /// MAC goes in the messages and the notification group is joined on it (bug run
    /// 2026-11-26: unset, the fallback MAC was sent and the group joined on 0.0.0.0).
    fn interface_toward(&self, ip: Ipv4Addr) -> Ipv4Addr {
        let set = self.netaudio_interface();
        if !set.is_unspecified() {
            return set;
        }
        UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .and_then(|s| {
                s.connect((ip, w::SETTINGS_PORT))?;
                s.local_addr()
            })
            .ok()
            .and_then(|a| match a.ip() {
                IpAddr::V4(v) if !v.is_unspecified() => Some(v),
                _ => None,
            })
            .or_else(|| {
                idn_open_protocol::discovery::responder::HostInfo::local(None)
                    .ok()
                    .map(|h| h.addr)
            })
            .unwrap_or(Ipv4Addr::UNSPECIFIED)
    }

    /// The receive latency (§1.1): one of the choices in the device's range, written over
    /// ARC, its reply checked, the configured latency read back.
    fn set_latency(
        &self,
        dev: &Element,
        t: &Target,
        name: &str,
        value: &str,
        sent: &mut bool,
    ) -> Result<String, String> {
        let ns = value
            .parse::<f64>()
            .ok()
            .and_then(w::ms_to_ns)
            .ok_or_else(|| format!("{value:?}: a latency in ms"))?;
        let range = |k: &str| {
            dev.attr(k)
                .and_then(|v| v.parse::<f64>().ok())
                .and_then(w::ms_to_ns)
        };
        let (Some(lo), Some(hi)) = (range("latency_min_ms"), range("latency_max_ms")) else {
            return Err(format!(
                "{name}'s latency range isn't read (it didn't answer the settings read)"
            ));
        };
        let choices = w::latency_choices(lo, hi);
        if !choices.contains(&ns) {
            let c: Vec<String> = choices.into_iter().map(ms).collect();
            return Err(format!(
                "{} ms: not a latency {name} offers ({} ms)",
                ms(ns),
                c.join(", ")
            ));
        }
        let iface = self.netaudio_interface();
        let id = next_id();
        let req = w::set_latency(inv::LEGACY, id, ns)?;
        *sent = true;
        let reply = ask(iface, t.arc, &req)?;
        control::reply_ok(&reply, id, w::SET_LATENCY)?;
        let back = ask(iface, t.arc, &inv::settings_request(next_id()))
            .ok()
            .and_then(|d| inv::parse_settings(&d).ok())
            .and_then(|s| s.latency_ns);
        match back {
            Some(b) if b == ns => Ok(format!("{name} receive latency {} ms (read back)", ms(ns))),
            Some(b) => Err(format!(
                "{name} took receive latency {} ms but reads back {} ms",
                ms(ns),
                ms(b)
            )),
            None => Ok(unconfirmed(
                name,
                &format!("receive latency {} ms", ms(ns)),
                "it took the write; no answer to the read back",
            )),
        }
    }
}

/// The device's capabilities (§6.5.4), asked with its device ID (`id`, from a status it
/// sent; else one asked for with a clock query); `None` when unknown.
fn caps_of(talk: &w::Talk, id: Option<[u8; 8]>) -> Option<w::Caps> {
    let id = match id {
        Some(id) => id,
        None => {
            talk.send(&w::clock_query(next_id(), talk.mac)).ok()?;
            let d = talk.status(&[kind::CLOCK_STATUS], Duration::from_secs(2), |_| true)?;
            w::status(&d)?.device
        }
    };
    talk.caps(id)
}

/// The sample rate (§2.1, §2.2) or encoding (§4.1): only a value the device's own status
/// lists, and only while it lets it change; then asked again until it says the new value
/// (20 s at most).
fn set_choice(talk: &w::Talk, name: &str, c: Command, value: &str) -> Result<String, String> {
    let (status_kind, what, unit, cap) = if c == Command::Rate {
        (kind::RATE_STATUS, "sample rate", "Hz", w::cap::RATE)
    } else {
        (kind::ENCODING_STATUS, "encoding", "bits", w::cap::ENCODING)
    };
    let v: u32 = value
        .parse()
        .ok()
        .filter(|&v| v > 0)
        .ok_or_else(|| format!("{value:?}: a {what} as a number"))?;
    let probe = || {
        if c == Command::Rate {
            w::rate_probe(next_id(), talk.mac)
        } else {
            w::encoding_probe(next_id(), talk.mac)
        }
    };
    talk.send(&probe())?;
    let st = talk
        .status(&[status_kind], Duration::from_secs(3), |_| true)
        .ok_or_else(|| format!("{name} didn't say its {what} (no status within 3 s)"))?;
    let now = w::parse_choice(&st, status_kind).map_err(|e| e.to_string())?;
    if caps_of(talk, w::status(&st).map(|h| h.device)).is_some_and(|k| k.lacks(cap)) {
        return Err(format!("{name} says it can't change its {what}"));
    }
    now.allows(v, what)?;
    if now.current == v {
        return Ok(format!("{name} {what} is {v} {unit} already"));
    }
    let req = if c == Command::Rate {
        w::rate_write(next_id(), v)?
    } else {
        w::encoding_write(next_id(), v)?
    };
    talk.send(&req)?;
    let mut last = now.current;
    let until = Instant::now() + Duration::from_secs(20);
    while Instant::now() < until {
        talk.send(&probe())?;
        let done = talk.status(&[status_kind], Duration::from_secs(2), |s| {
            w::parse_choice(s.d, status_kind).is_ok_and(|x| {
                last = x.current;
                x.current == v
            })
        });
        if done.is_some() {
            return Ok(format!("{name} {what} {v} {unit} (it says so)"));
        }
    }
    Ok(unconfirmed(
        name,
        &format!("{what} {v} {unit}"),
        &format!("it still says {last} {unit} after 20 s"),
    ))
}

/// AES67 on or off (§4.2): only for a device that says its AES67 state; sent three
/// times, then "configured" read back (2 s).
fn set_aes67(talk: &w::Talk, name: &str, on: bool) -> Result<String, String> {
    talk.send(&w::aes67_probe(next_id(), talk.mac))?;
    let st = talk
        .status(&[kind::AES67_STATUS], Duration::from_secs(3), |_| true)
        .ok_or_else(|| format!("{name} didn't say whether it has AES67 (no status within 3 s)"))?;
    let now = w::parse_aes67(&st).map_err(|e| e.to_string())?;
    if caps_of(talk, w::status(&st).map(|h| h.device)).is_some_and(|k| k.lacks(w::cap::AES67)) {
        return Err(format!("{name} says it can't change AES67"));
    }
    let word = if on { "on" } else { "off" };
    if now.configured == on {
        return Ok(format!("{name} AES67 is {word} already"));
    }
    talk.send_thrice(&w::aes67_write(next_id(), talk.mac, on))?;
    talk.send(&w::aes67_probe(next_id(), talk.mac))?;
    let mut got = None;
    talk.status(&[kind::AES67_STATUS], Duration::from_secs(2), |s| {
        w::parse_aes67(s.d).is_ok_and(|x| {
            got = Some(x);
            x.configured == on
        })
    });
    match got {
        Some(x) if x.configured == on && x.reboot_needed() => Ok(format!(
            "{name} AES67 {word} (configured; it takes effect when the device restarts)"
        )),
        Some(x) if x.configured == on => Ok(format!("{name} AES67 {word} (it says so)")),
        Some(x) => Ok(unconfirmed(
            name,
            &format!("AES67 {word}"),
            &format!(
                "it still says configured {} after 2 s",
                if x.configured { "on" } else { "off" }
            ),
        )),
        None => Ok(unconfirmed(
            name,
            &format!("AES67 {word}"),
            "no status within 2 s",
        )),
    }
}

/// Preferred leader or word clock (§5.1): only where the clock status lets it change;
/// sent three times, the status read back (3 s). Word clock on: never with the input's
/// state "none" or "missing"; with any state but "valid" only when `named` (the device's
/// name given as `confirm`; K.5).
fn set_clock(
    talk: &w::Talk,
    name: &str,
    c: Command,
    on: bool,
    named: bool,
) -> Result<String, String> {
    talk.send(&w::clock_query(next_id(), talk.mac))?;
    let st = talk
        .status(&[kind::CLOCK_STATUS], Duration::from_secs(3), |_| true)
        .ok_or_else(|| format!("{name} didn't say its clock (no status within 3 s)"))?;
    let now = w::parse_clock(&st).map_err(|e| e.to_string())?;
    let word = if on { "on" } else { "off" };
    let (mask, source, leader, what) = if c == Command::PreferredLeader {
        if !now.leader_writable() {
            return Err(format!(
                "{name}'s preferred leader can't be changed (its clock status says so)"
            ));
        }
        if now.leader == on {
            return Ok(format!("{name} preferred leader is {word} already"));
        }
        (w::clock_mask::LEADER, now.source, on, "preferred leader")
    } else {
        if !now.writable() {
            return Err(format!(
                "{name}'s clock can't be changed (its clock status says so)"
            ));
        }
        let ro = caps_of(talk, w::status(&st).map(|h| h.device)).and_then(|k| k.read_only);
        if ro.is_some_and(|r| r & w::cap::RO_WORD_CLOCK != 0) {
            return Err(format!("{name}'s word clock is read only"));
        }
        let want = if on {
            w::source::WORD_CLOCK
        } else {
            w::source::INTERNAL
        };
        if now.source == want {
            return Ok(format!("{name} word clock is {word} already"));
        }
        if on {
            word_clock_may(name, now.word_clock, named)?;
        }
        if !on && now.source != w::source::WORD_CLOCK {
            return Ok(format!(
                "{name} isn't on word clock (its clock: {})",
                w::source_name(now.source)
            ));
        }
        (w::clock_mask::SOURCE, want, now.leader, "word clock")
    };
    talk.send_thrice(&w::clock_message(next_id(), talk.mac, mask, source, leader))?;
    talk.send(&w::clock_query(next_id(), talk.mac))?;
    let mut got = None;
    talk.status(&[kind::CLOCK_STATUS], Duration::from_secs(3), |s| {
        w::parse_clock(s.d).is_ok_and(|k| {
            got = Some(k);
            if mask == w::clock_mask::LEADER {
                k.leader == on
            } else {
                k.source == source
            }
        })
    });
    let done = got.is_some_and(|k| {
        if mask == w::clock_mask::LEADER {
            k.leader == on
        } else {
            k.source == source
        }
    });
    if !done {
        return Ok(unconfirmed(
            name,
            &format!("{what} {word}"),
            if got.is_some() {
                "its clock status still says otherwise after 3 s"
            } else {
                "no status within 3 s"
            },
        ));
    }
    let input = got
        .and_then(|k| k.word_clock)
        .filter(|_| c == Command::WordClock)
        .map(|v| format!("; its word clock input: {}", w::word_clock_state(v)))
        .unwrap_or_default();
    Ok(format!("{name} {what} {word} (it says so{input})"))
}

/// Whether word clock may be turned on with the input in `state` (§5.1's extension,
/// e+30): never "none" (1) or "missing" (4); "valid" (3) always; any other (unknown,
/// invalid, or a device not saying) only with the device's name as `confirm`.
fn word_clock_may(name: &str, state: Option<u16>, named: bool) -> Result<(), String> {
    let said = state.map_or("not said by the device", w::word_clock_state);
    match state {
        Some(1 | 4) => Err(format!(
            "{name}: no word clock to follow (its word clock input: {said})"
        )),
        Some(3) => Ok(()),
        _ if named => Ok(()),
        _ => Err(format!(
            "{name}'s word clock input is {said}: give the device's name, {name}, as confirm to switch to it anyway"
        )),
    }
}

/// Clear configuration (§6): `all`, or `keep_ip` (all but its IP address); the device's
/// status must say it did that mode (3 s).
fn clear(talk: &w::Talk, name: &str, value: &str) -> Result<String, String> {
    let (action, how) = match value {
        "all" => (w::clear::ALL, "all of it"),
        "keep_ip" => (w::clear::KEEP_IP, "all but its IP address"),
        _ => return Err(format!("{value:?}: give all or keep_ip")),
    };
    talk.send(&w::clear_config(next_id(), talk.mac, action))?;
    let mut got = None;
    talk.status(&[kind::CLEAR_STATUS], Duration::from_secs(3), |s| {
        w::parse_cleared(s.d).is_ok_and(|x| {
            got = Some(x);
            x.executed == action
        })
    });
    match got {
        Some(x) if x.executed == action => Ok(format!("{name}: configuration cleared ({how})")),
        Some(x) => Err(format!(
            "{name}: asked to clear {how}, it says it did mode {}",
            x.executed
        )),
        None => Ok(unconfirmed(
            name,
            &format!("clear configuration ({how})"),
            "no status within 3 s",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lab gate (K.14): a command not checked yet refused without the switch, offered
    /// with it; read only and the device off refuse whatever the switch.
    #[test]
    fn the_gate_and_read_only() {
        for c in Command::ALL {
            if !w::CHECKED.contains(&c) {
                let e = may(true, false, c, false).unwrap_err();
                assert!(e.contains("not checked on real equipment yet (lab "), "{e}");
            }
            assert!(may(true, false, c, true).is_ok());
            assert!(may(true, true, c, true).unwrap_err().contains("read only"));
            assert!(may(false, false, c, true).is_err());
        }
    }

    #[test]
    fn values_said_plainly() {
        assert_eq!(ms(250_000), "0.25");
        assert_eq!(ms(1_000_000), "1");
        assert_eq!(ms(150_000), "0.15");
        assert_eq!(ms(0), "0");
        assert!(yes_no("On").unwrap() && !yes_no("false").unwrap());
        assert!(yes_no("maybe").is_err());
    }

    /// Word clock on (bug run 2026-11-26): never with no input or one missing; "valid"
    /// always; unknown or invalid only with the device's name.
    #[test]
    fn word_clock_only_with_an_input() {
        for (state, named, ok) in [
            (Some(1), true, false),
            (Some(4), true, false),
            (Some(3), false, true),
            (Some(2), false, false),
            (Some(2), true, true),
            (Some(0), false, false),
            (None, false, false),
            (None, true, true),
        ] {
            let r = word_clock_may("Box", state, named);
            assert_eq!(r.is_ok(), ok, "{state:?} {named}: {r:?}");
        }
        let e = word_clock_may("Box", Some(4), true).unwrap_err();
        assert!(e.contains("missing"), "{e}");
        let e = word_clock_may("Box", Some(2), false).unwrap_err();
        assert!(e.contains("invalid") && e.contains("confirm"), "{e}");
    }

    /// Reboot and the resets need the device's name as listed; the others don't.
    #[test]
    fn dangerous_ones_need_the_name() {
        let doc = idn_open_protocol::xml::parse(
            r#"<network_devices><device instance="Box-1" name="Stage-Box" address="10.0.0.9" arc_port="4440"/><device instance="Me" name="Me" address="10.0.0.2" arc_port="4440" this="true"/></network_devices>"#,
        )
        .unwrap();
        let (_, _, name) = listed(&doc, "Box-1").unwrap();
        assert_eq!(name, "Stage-Box");
        assert!(w::confirmed(Command::Reboot, &name, "Box-1").is_err());
        assert!(w::confirmed(Command::FactoryReset, &name, "Stage-Box").is_ok());
        // This machine's own device: never here.
        assert!(listed(&doc, "Me").is_err());
    }
}

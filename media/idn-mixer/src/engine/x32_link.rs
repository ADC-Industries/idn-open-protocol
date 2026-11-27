//! The X32 the desk controls over Ethernet (§M.23.1): connected as the desk's `x32`
//! setting says, its changes turned into new versions of the state, and each desk
//! channel's preamp where one of the X32's USB channels feeds it (the X-USB card on the
//! desk, R.27, §M.23).

use super::*;
use crate::x32::{self, X32};
use std::sync::mpsc::{Receiver, RecvTimeoutError};

/// How long `"auto"` looks for an X32 on the LAN, and how often it (or a connection that
/// failed) is tried again.
const FIND_FOR: Duration = Duration::from_millis(500);
const TRY_AGAIN: Duration = Duration::from_secs(10);
/// Changes arriving together make one new version.
const COALESCE: Duration = Duration::from_millis(50);

/// The X32 client and why there is none.
#[derive(Default)]
pub(super) struct Slot {
    pub(super) client: Option<Arc<X32>>,
    /// Why the setting's X32 isn't connected (not found, a bad address).
    error: Option<String>,
}

impl Slot {
    /// Why the setting's X32 isn't connected, if it isn't.
    pub(super) fn error(&self) -> Option<String> {
        self.error.clone()
    }
}

fn bump(sh: &Shared) {
    *sh.version.lock_ok() += 1;
    sh.changed.notify_all();
}

/// Where the setting says the X32 is: `None` while `"auto"` finds none.
fn address(setting: &str) -> Result<Option<std::net::SocketAddr>, String> {
    if setting.eq_ignore_ascii_case("auto") {
        let found = x32::discover(FIND_FOR).map_err(|e| format!("looking for an X32: {e}"))?;
        return match found.first() {
            Some(f) => Ok(Some(f.addr)),
            None => Err("no X32 answered on the LAN".into()),
        };
    }
    x32::parse_address(setting).map(Some)
}

/// The X32 watcher's thread: (re)connects as the setting changes, and wakes the state's
/// readers on each change the X32 reports (coalesced).
pub(super) fn watch(sh: &Shared) {
    let mut have = String::new();
    let mut rx: Option<Receiver<x32::Change>> = None;
    let mut tried: Option<Instant> = None;
    while !sh.stop.load(Ordering::Relaxed) {
        let want = sh
            .desk
            .lock_ok()
            .settings_arc()
            .x32
            .address
            .trim()
            .to_owned();
        if want != have {
            have = want;
            rx = None;
            tried = None;
            let mut slot = sh.x32.lock_ok();
            let had = slot.client.take().is_some() || slot.error.take().is_some();
            drop(slot);
            if had {
                bump(sh);
            }
        }
        let idle = sh.x32.lock_ok().client.is_none();
        if !have.is_empty() && idle && tried.is_none_or(|t| t.elapsed() >= TRY_AGAIN) {
            tried = Some(Instant::now());
            let got = address(&have).and_then(|a| match a {
                Some(a) => X32::connect(a).map_err(|e| format!("X32 at {a}: {e}")),
                None => Err("no X32 found".into()),
            });
            let mut slot = sh.x32.lock_ok();
            match got {
                Ok(x) => {
                    idn_open_protocol::log::log(&format!(
                        "mixer: X32 control at {} (§M.23.1)",
                        x.address()
                    ));
                    rx = Some(x.changes());
                    slot.client = Some(Arc::new(x));
                    slot.error = None;
                }
                Err(e) => {
                    if slot.error.as_deref() != Some(e.as_str()) {
                        idn_open_protocol::log::log(&format!("mixer: X32: {e}"));
                    }
                    slot.error = Some(e);
                }
            }
            drop(slot);
            bump(sh);
        }
        match &rx {
            Some(r) => match r.recv_timeout(Duration::from_millis(100)) {
                Ok(_) => {
                    std::thread::sleep(COALESCE);
                    while r.try_recv().is_ok() {}
                    bump(sh);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => rx = None,
            },
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// The X32 USB channel (1–32) desk channel `n` (from 1) takes: its input (`source`, else
/// its own number; a backup input has none) among the card's inputs, the card on the desk
/// being an X32's X-USB card (R.27).
pub(super) fn usb_of(cfg: &ConsoleConfig, s: &Settings, n: usize) -> Option<u8> {
    let card = cfg.card_in.as_ref()?;
    if !card.device.as_deref().is_some_and(idn_audio::is_x32) {
        return None;
    }
    let input = s.channels.get(n.checked_sub(1)?)?.source.unwrap_or(n);
    let j = input.checked_sub(1)?.checked_sub(card.first)?;
    let limit = card.channels.map_or(x32::proto::USB_CHANNELS, usize::from);
    (j < limit.min(x32::proto::USB_CHANNELS)).then_some(j as u8 + 1)
}

/// A preamp as the state shows it.
fn preamp_json(usb: &x32::UsbChannel, p: &x32::Preamp) -> Value {
    json!({
        "usb": usb.usb,
        "input": usb.source.to_string(),
        "headamp": p.headamp,
        "gain_db": p.gain_db,
        "phantom": p.phantom,
        // The X32's own channel strip's polarity (not what the desk receives).
        "invert": p.invert,
        "x32_channel": p.channel,
    })
}

impl Console {
    /// The X32 the desk controls (§M.23.1), once its setting names one and it was found
    /// (connected or not: [`X32::connected`]).
    pub fn x32(&self) -> Option<Arc<X32>> {
        self.shared.x32.lock_ok().client.clone()
    }

    /// The X32 USB channel (1–32) desk channel `n` (from 1) takes, with the X32's X-USB
    /// card as the desk's card (`--card-in x32`, R.27).
    pub fn x32_usb(&self, n: usize) -> Option<u8> {
        usb_of(&self.shared.cfg, &self.settings(), n)
    }

    /// Sets the X32 preamp behind desk channel `n` (from 1; §M.23.1): gain (dB, −12…+60 in
    /// 0.5 dB steps), 48 V phantom power, and the X32 polarity — the X32 channel strip's,
    /// which does not change what the desk receives over USB (the desk's own `polarity`
    /// does). Refused while the desk is locked, when the channel has no X32 preamp, or the
    /// X32 isn't answering; nothing is sent unless all of it can be.
    pub fn preamp(
        &self,
        n: usize,
        gain_db: Option<f32>,
        phantom: Option<bool>,
        invert: Option<bool>,
    ) -> Result<(), String> {
        if self.locked() {
            return Err("the desk is locked".into());
        }
        if gain_db.is_none() && phantom.is_none() && invert.is_none() {
            return Err("preamp: gain_db, phantom and/or invert".into());
        }
        if gain_db.is_some_and(|g| !g.is_finite()) {
            return Err("gain_db: dB, -12 to 60".into());
        }
        let s = self.settings();
        if n == 0 || n > s.channels.len() {
            return Err(format!("channels are 1 to {}", s.channels.len()));
        }
        let x = self
            .x32()
            .ok_or("no X32: the desk's x32 setting names none (or it wasn't found)")?;
        let usb = usb_of(&self.shared.cfg, &s, n).ok_or(format!(
            "channel {n} has no X32 preamp: its input is not one of the X32's USB channels"
        ))?;
        if !x.connected() {
            return Err("the X32 is not answering".into());
        }
        let p = x.preamp(usb).ok_or_else(|| {
            let what = x
                .usb(usb)
                .map_or("nothing read yet".into(), |u| u.source.to_string());
            format!("channel {n} has no X32 preamp: X32 USB channel {usb} carries {what}")
        })?;
        if invert.is_some() && p.channel.is_none() {
            return Err(format!(
                "channel {n}: no X32 channel takes its input, so it has no X32 polarity"
            ));
        }
        let e = |e: x32::Error| e.to_string();
        if let Some(g) = gain_db {
            x.set_gain(usb, g).map_err(e)?;
        }
        if let Some(on) = phantom {
            x.set_phantom(usb, on).map_err(e)?;
        }
        if let Some(on) = invert {
            x.set_invert(usb, on).map_err(e)?;
        }
        self.changed();
        Ok(())
    }

    /// The state's `preamp` (per channel: its X32 preamp, or null) and `x32` (null without
    /// the setting).
    pub(super) fn x32_state(&self, s: &Settings) -> (Value, Value) {
        let setting = s.x32.address.trim();
        let slot = self.shared.x32.lock_ok();
        let client = slot.client.clone();
        let error = slot.error.clone();
        drop(slot);
        let preamps: Vec<Value> = (1..=s.channels.len())
            .map(|n| {
                let (Some(x), Some(usb)) = (&client, usb_of(&self.shared.cfg, s, n)) else {
                    return Value::Null;
                };
                match (x.usb(usb), x.preamp(usb)) {
                    (Some(u), Some(p)) => preamp_json(&u, &p),
                    _ => Value::Null,
                }
            })
            .collect();
        let status = if setting.is_empty() {
            Value::Null
        } else {
            let st = client.as_ref().map(|x| x.status());
            json!({
                "address": setting,
                "endpoint": st.as_ref().map(|s| s.address.to_string()),
                "connected": st.as_ref().is_some_and(|s| s.connected),
                "error": error,
                "info": st.as_ref().and_then(|s| s.info.clone()),
                "usb": st.as_ref().map_or(Vec::new(), |s| s.usb.iter().map(|(u, p)| json!({
                    "usb": u.usb,
                    "source": u.source,
                    "label": u.source.to_string(),
                    "via": u.via,
                    "headamp": u.headamp,
                    "x32_channel": u.channel,
                    "preamp": p,
                })).collect()),
            })
        };
        (Value::Array(preamps), status)
    }

    /// The preamps as OSC values (§M.16): `/ch/N/preamp/gain`, `…/phantom`, `…/invert`.
    pub fn preamp_osc(&self) -> Vec<(String, Value)> {
        let Some(x) = self.x32() else {
            return Vec::new();
        };
        let s = self.settings();
        let mut out = Vec::new();
        for n in 1..=s.channels.len() {
            let Some(p) = usb_of(&self.shared.cfg, &s, n).and_then(|u| x.preamp(u)) else {
                continue;
            };
            out.push((format!("/ch/{n}/preamp/gain"), json!(p.gain_db)));
            out.push((format!("/ch/{n}/preamp/phantom"), json!(p.phantom)));
            if let Some(i) = p.invert {
                out.push((format!("/ch/{n}/preamp/invert"), json!(i)));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::CardIn;
    use crate::engine::typed::Command;
    use crate::x32::fake::Fake;
    use idn_common::testing::wait_until;
    use idn_open_protocol::osc::{Arg, Message};

    /// A desk on the X32's X-USB card (32 channels from desk input 1), controlling the
    /// fake X32's preamps.
    fn console(fake: &Fake) -> Console {
        Console::start(ConsoleConfig {
            name: "X32 desk test".into(),
            channels: 40,
            mdns: Some(false),
            card_in: Some(CardIn {
                device: Some("x32".into()),
                channels: Some(32),
                first: 0,
            }),
            x32: Some(fake.addr.to_string()),
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn x32_preamps_on_the_desk() {
        let fake = Fake::start("X32", Duration::from_secs(10));
        let c = console(&fake);
        // The state: channel 1's preamp (local 1 at +6 dB), the X32 itself.
        assert!(wait_until(
            Duration::from_secs(5),
            || c.state()["preamp"][0]["gain_db"] == json!(6.0)
                && !c.state()["preamp"][24].is_null()
                // (Channel 27's too, used below: its USB route read; bug run Q.)
                && !c.state()["preamp"][26].is_null()
        ));
        let st = c.state();
        let p = &st["preamp"][0];
        assert_eq!(
            (&p["usb"], &p["headamp"], &p["input"], &p["phantom"]),
            (&json!(1), &json!(0), &json!("Local 1"), &json!(false))
        );
        assert_eq!(
            (&p["invert"], &p["x32_channel"]),
            (&json!(false), &json!(1))
        );
        assert_eq!(st["strips"][0]["preamp"]["usb"], 1);
        assert_eq!(st["x32"]["connected"], true);
        assert_eq!(st["x32"]["info"]["model"], "X32");
        assert_eq!(st["x32"]["usb"].as_array().unwrap().len(), 32);
        // No preamp: the card's own input (USB 29), a channel past the card (input 33).
        assert!(st["preamp"][28].is_null());
        assert!(st["preamp"][32].is_null());
        assert_eq!(c.x32_usb(33), None);
        assert_eq!(c.x32_usb(9), Some(9));

        // The command reaches the X32 (gain and 48 V; the X32 polarity by the typed form).
        c.command("preamp", &json!({"n": 1, "gain_db": 24.0, "phantom": true}))
            .unwrap();
        assert!(wait_until(Duration::from_secs(2), || fake
            .value("/headamp/000/gain")
            == Some(Arg::Float(0.5))
            && fake.value("/headamp/000/phantom") == Some(Arg::Int(1))));
        assert_eq!(c.state()["preamp"][0]["gain_db"], 24.0);
        c.run(Command::Preamp {
            n: 27,
            gain_db: None,
            phantom: None,
            invert: Some(true),
        })
        .unwrap();
        assert!(wait_until(Duration::from_secs(2), || fake
            .value("/ch/05/preamp/invert")
            == Some(Arg::Int(1))));
        // Refused: no preamp behind it, nothing asked, not a channel.
        let e = c
            .command("preamp", &json!({"n": 29, "gain_db": 0.0}))
            .unwrap_err();
        assert!(
            e.contains("no X32 preamp") && e.contains("Card in 2"),
            "{e}"
        );
        assert!(
            c.command("preamp", &json!({"n": 33, "gain_db": 0.0}))
                .is_err()
        );
        assert!(c.command("preamp", &json!({"n": 1})).is_err());
        assert!(
            c.command("preamp", &json!({"n": 1, "phantom": "on"}))
                .is_err()
        );
        assert!(
            c.command("preamp", &json!({"n": 41, "gain_db": 0.0}))
                .is_err()
        );

        // A change on the X32 reaches the state, as a new version.
        let v = c.version();
        fake.push("/headamp/001/gain", Arg::Float(0.0));
        assert!(wait_until(Duration::from_secs(2), || c.state()["preamp"]
            [1]["gain_db"]
            == json!(-12.0)));
        // (The watcher wakes the readers, coalescing for 50 ms.)
        assert!(wait_until(Duration::from_secs(2), || c.version() > v));

        // Locked: refused, by the dispatcher and the typed form alike.
        c.lock(true);
        assert_eq!(
            c.command("preamp", &json!({"n": 1, "gain_db": 0.0})),
            Err("the desk is locked".into())
        );
        assert!(
            c.run(Command::Preamp {
                n: 1,
                gain_db: None,
                phantom: Some(false),
                invert: None
            })
            .is_err()
        );
        c.lock(false);
        assert_eq!(fake.value("/headamp/000/phantom"), Some(Arg::Int(1)));

        // Not a setting: not in the settings or a snapshot; the X32 setting is the desk's.
        let addr = fake.addr.to_string();
        let set = serde_json::to_value(c.settings()).unwrap();
        assert!(set["channels"][0].get("preamp").is_none());
        assert_eq!(set["x32"]["address"], json!(addr));
        c.snapshot_save("Before").unwrap();
        let snap = c.shared.snapshots.lock_ok()["Before"].clone();
        assert_eq!(snap.x32.address, "");
        assert!(
            serde_json::to_value(&snap.channels[0])
                .unwrap()
                .get("preamp")
                .is_none()
        );
        c.snapshot_recall("Before").unwrap();
        assert_eq!(c.settings().x32.address, addr);
        assert_eq!(c.state()["snapshot"]["changed"], false);
        // A recall sent nothing to the X32 (its sets: only the three asked for above).
        let sets = fake
            .log()
            .into_iter()
            .filter(|m| !m.args.is_empty())
            .count();
        assert_eq!(sets, 3);

        // OSC: the values, and a message becoming the command.
        assert!(
            c.preamp_osc()
                .contains(&("/ch/1/preamp/gain".into(), json!(24.0)))
        );
        let settings = serde_json::to_value(c.settings()).unwrap();
        assert_eq!(
            crate::osc::command_for(
                &Message::new("/ch/3/preamp/gain", vec![Arg::Float(12.0)]),
                &settings
            ),
            Ok(Some(("preamp".into(), json!({"n": 3, "gain_db": 12.0}))))
        );
        assert_eq!(
            crate::osc::command_for(
                &Message::new("/ch/3/preamp/phantom", vec![Arg::Int(0)]),
                &settings
            ),
            Ok(Some(("preamp".into(), json!({"n": 3, "phantom": false}))))
        );

        // The setting cleared: no X32, no preamps.
        c.desk(&json!({"x32": {"address": ""}})).unwrap();
        assert!(wait_until(Duration::from_secs(2), || c.x32().is_none()));
        let st = c.state();
        assert!(st["x32"].is_null());
        assert!(st["preamp"].as_array().unwrap().iter().all(Value::is_null));
        assert!(
            c.desk(&json!({"x32": {"address": "not an address"}}))
                .is_err()
        );
    }
}

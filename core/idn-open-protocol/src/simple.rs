//! Simple mode (G6, idn-node.md §N.20) for a host's UI (since IDN Media has no
//! panels of its own): a program's controls cut down to what a volunteer needs, for
//! everyone until someone leaves it. Entering is a button; leaving is a button too, or
//! needs a PIN if one is set.
//!
//! The PIN is the machine's, kept by its IDN Node (so the Site Manager can set it, change
//! it, wipe it, and keep it from the panels); without a Node, the program keeps its own.
//! Simple mode guards against mistakes, not against someone set on getting in: the API's
//! password does that.

use crate::node::Node;
use idn_common::sync::LockOk as _;
use serde_json::{Value, json};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A program's simple mode.
pub struct SimpleMode {
    file: PathBuf,
    node: Option<Arc<Node>>,
    local: Mutex<Local>,
    /// The Node's last answer, and when it was asked.
    asked: Mutex<Option<(Instant, Option<NodeLock>)>>,
    /// Wrong local PINs in a row.
    fails: crate::web::PinLock,
}

/// The Node's PIN: (one is set, only the site changes it).
type NodeLock = (bool, bool);

/// What `simple.json` keeps: on or off, and the PIN's hash when there is no Node.
#[derive(Default)]
struct Local {
    on: bool,
    pin: Option<String>,
}

impl SimpleMode {
    /// Simple mode of the program whose state is in `dir` (`simple.json`), its PIN through
    /// `node` if the program has one.
    pub fn open(dir: &Path, node: Option<Arc<Node>>) -> SimpleMode {
        let file = dir.join("simple.json");
        // (Unreadable: set aside and said, and simple mode ON — a volunteer's station isn't
        // opened up by a file it can't read, bug round 2026-11-09.)
        let v: Value = match idn_common::fsutil::load_saved(&file, |b| {
            serde_json::from_slice::<Value>(b).ok()
        }) {
            Ok(v) => v.unwrap_or_default(),
            Err(why) => {
                crate::log::log(&format!("simple mode: {why}; on until set again"));
                serde_json::json!({"on": true})
            }
        };
        SimpleMode {
            file,
            node,
            local: Mutex::new(Local {
                on: v["on"].as_bool().unwrap_or(false),
                pin: v["pin"]
                    .as_str()
                    .filter(|h| h.starts_with("$argon2"))
                    .map(str::to_owned),
            }),
            asked: Mutex::default(),
            fails: crate::web::PinLock::default(),
        }
    }

    /// In simple mode.
    pub fn is_on(&self) -> bool {
        self.local.lock_ok().on
    }

    /// The Node's PIN: (one is set, only the site changes it); `None` without a Node, or
    /// an older one (the program's own PIN then). Asked at most every 2 s.
    fn node_lock(&self) -> Option<NodeLock> {
        let node = self.node.as_ref()?;
        let mut a = self.asked.lock_ok();
        if let Some((t, v)) = *a
            && t.elapsed() < Duration::from_secs(2)
        {
            return v;
        }
        let v = node.panel_lock().ok();
        *a = Some((Instant::now(), v));
        v
    }

    /// For the panel: `{"on", "pin": a PIN is needed to leave, "site": the Site Manager
    /// holds it, "where": "node" or "here"}`.
    pub fn state(&self) -> Value {
        let on = self.is_on();
        match self.node_lock() {
            Some((pin, site)) => json!({"on": on, "pin": pin, "site": site, "where": "node"}),
            None => json!({"on": on, "pin": self.local.lock_ok().pin.is_some(),
                           "site": false, "where": "here"}),
        }
    }

    /// Into simple mode (a button; no PIN).
    pub fn enter(&self) -> io::Result<()> {
        self.local.lock_ok().on = true;
        self.save()
    }

    /// Out of simple mode, with the PIN if one is set.
    pub fn leave(&self, pin: &str) -> Result<(), String> {
        if self.node_lock().is_some() {
            self.node
                .as_ref()
                .expect("a Node")
                .panel_unlock(pin)
                .map_err(|e| e.to_string())?;
        } else {
            self.check_local(pin)?;
        }
        self.local.lock_ok().on = false;
        self.save().map_err(|e| e.to_string())
    }

    fn check_local(&self, pin: &str) -> Result<(), String> {
        let Some(hash) = self.local.lock_ok().pin.clone() else {
            return Ok(());
        };
        // Five wrong in a row: a minute's wait (as the Node's PIN).
        self.fails
            .check(|| crate::web::verify_password(&hash, pin.trim()))
    }

    /// Sets the PIN (`None` or empty wipes it), from the full panel only; refused while the
    /// site holds it. 4 to 32 characters.
    pub fn set_pin(&self, pin: Option<&str>) -> Result<(), String> {
        if self.is_on() {
            return Err("leave simple mode first".into());
        }
        let pin = pin.map(str::trim).filter(|p| !p.is_empty());
        if let Some(p) = pin
            && !(4..=32).contains(&p.chars().count())
        {
            return Err("the PIN: 4 to 32 characters".into());
        }
        if self.node_lock().is_some() {
            *self.asked.lock_ok() = None;
            return self
                .node
                .as_ref()
                .expect("a Node")
                .set_panel_pin(pin)
                .map_err(|e| e.to_string());
        }
        let hash = match pin {
            Some(p) => Some(crate::web::hash_password(p).map_err(|e| e.to_string())?),
            None => None,
        };
        let wiped = hash.is_none();
        self.local.lock_ok().pin = hash;
        self.save().map_err(|e| e.to_string())?;
        // Wiped: its hash gone from any copy of the file set aside as damaged too.
        if wiped {
            idn_common::fsutil::remove_set_asides(&self.file);
        }
        Ok(())
    }

    fn save(&self) -> io::Result<()> {
        // Written whole (a cut-off file read as simple mode on with no PIN: anyone could
        // leave it), with the state held across: two changes at once could land in the
        // other order and the file say the opposite of what holds (bug review 3).
        let l = self.local.lock_ok();
        let v = json!({"on": l.on, "pin": l.pin});
        if let Some(d) = self.file.parent() {
            std::fs::create_dir_all(d)?;
        }
        idn_common::fsutil::write_private(&self.file, v.to_string().as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_button_without_a_pin_and_a_pin_when_set() {
        let dir = idn_common::testing::TempDir::new("simple");
        let s = SimpleMode::open(&dir, None);
        assert!(!s.is_on());
        // No PIN: in and out with the buttons.
        s.enter().unwrap();
        assert!(s.is_on());
        assert!(s.set_pin(Some("2468")).is_err(), "not from simple mode");
        s.leave("").unwrap();
        assert!(!s.is_on());
        // A PIN: out only with it; kept over a restart.
        assert!(s.set_pin(Some("12")).is_err(), "too short");
        s.set_pin(Some("2468")).unwrap();
        s.enter().unwrap();
        let s = SimpleMode::open(&dir, None);
        assert!(s.is_on());
        assert_eq!(s.state()["pin"], true);
        assert_eq!(s.leave("1111"), Err("wrong PIN".into()));
        assert!(s.is_on());
        s.leave("2468").unwrap();
        // Wiped: the button again.
        s.set_pin(None).unwrap();
        s.enter().unwrap();
        s.leave("").unwrap();
        // Five wrong in a row: a minute's wait, even for the right one.
        s.set_pin(Some("2468")).unwrap();
        s.enter().unwrap();
        for _ in 0..5 {
            assert!(s.leave("0000").is_err());
        }
        assert!(s.leave("2468").unwrap_err().contains("wait"));
    }
}

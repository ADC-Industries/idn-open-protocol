//! Handsets and drivers (§V.13): devices with a credential of their own (a radio app in a
//! truck), enrolled once with a code; the driver the site's program (its dispatch system) puts on one,
//! with the channels they are in; a handset's session kept through a dropped link (its
//! sound queued) and resumed with its token; the SOS button.

use super::{
    FrameQueue, HandsetTag, Hub, Out, SLOTS, Sid, State, Who, is_dispatch, may, now_ms, send,
};
use crate::model::{Access, Driver, Handset, valid_name};
use idn_common::sync::LockOk as _;
use idn_open_protocol::hash::sha256_hex;
use idn_open_protocol::random;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

// (Every timing and limit here is a site setting: model::Settings.)
/// What a handset's credential starts with: `idnv1.<handset id>.<secret>`.
pub const CREDENTIAL_PREFIX: &str = "idnv1.";
/// Handsets a site keeps at most.
const MAX_HANDSETS: usize = 1000;

/// Wrong enrolment codes in the last minute (§V.13.1): by address (IPv6 by its /64: one
/// host has a whole one) and site-wide. Behind Industrial's tunnel every app shares the
/// tunnel's address: the site-wide count is what holds there.
#[derive(Default)]
pub(crate) struct EnrolTries {
    by: HashMap<IpAddr, VecDeque<Instant>>,
    all: VecDeque<Instant>,
    pruned: Option<Instant>,
}

const MINUTE: Duration = Duration::from_secs(60);

/// The address a try is counted under.
fn bucket(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(a) => match a.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let mut seg = a.segments();
                seg[4..].fill(0);
                IpAddr::V6(std::net::Ipv6Addr::from(seg))
            }
        },
    }
}

fn forget_old(q: &mut VecDeque<Instant>, now: Instant) {
    while q.front().is_some_and(|t| now.duration_since(*t) > MINUTE) {
        q.pop_front();
    }
}

impl EnrolTries {
    /// Whether `ip` may try a code now (fewer than `per_ip` wrong from its address and
    /// `all` site-wide this minute), counted as wrong until [`Self::succeeded`].
    pub fn attempt(&mut self, ip: IpAddr, per_ip: u32, all: u32) -> bool {
        let now = Instant::now();
        forget_old(&mut self.all, now);
        let q = self.by.entry(bucket(ip)).or_default();
        forget_old(q, now);
        if q.len() >= per_ip as usize || self.all.len() >= all as usize {
            return false;
        }
        q.push_back(now);
        self.all.push_back(now);
        true
    }

    /// The try was right: not counted, and the address's count cleared.
    pub fn succeeded(&mut self, ip: IpAddr) {
        self.by.remove(&bucket(ip));
        self.all.pop_back();
    }

    /// Addresses with nothing in the last minute forgotten (from the tick, every 10 s).
    pub fn prune(&mut self) {
        let now = Instant::now();
        if self
            .pruned
            .is_some_and(|t| now.duration_since(t) < Duration::from_secs(10))
        {
            return;
        }
        self.pruned = Some(now);
        self.by.retain(|_, q| {
            forget_old(q, now);
            !q.is_empty()
        });
        forget_old(&mut self.all, now);
    }
}

/// An SOS alert not yet acknowledged.
pub(crate) struct Alert {
    id: u64,
    handset_id: String,
    handset: String,
    from: String,
    driver: Option<String>,
    first_ms: i64,
    time_ms: i64,
    position: Option<Value>,
    text: String,
    count: u32,
    /// The last press (merged within `sos_merge_s`).
    last: Instant,
    /// When dispatch was last sent it (`sos_realert_s`).
    sent: Instant,
}

impl Alert {
    pub fn json(&self) -> Value {
        json!({
            "id": self.id,
            "handset": {"id": self.handset_id, "name": self.handset},
            "from": self.from,
            "driver": self.driver,
            "first_ms": self.first_ms,
            "time_ms": self.time_ms,
            "position": self.position,
            "text": self.text,
            "count": self.count,
        })
    }
}

/// Constant-time equality of two hashes.
fn same(a: &str, b: &str) -> bool {
    idn_open_protocol::trust::proto::same(a.as_bytes(), b.as_bytes())
}

/// A code as typed: its letters and digits, upper case (dashes, spaces dropped).
fn bare_code(s: &str) -> String {
    s.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// `len` characters (10 to 16 of [`random::CODE_ALPHABET`], 5 bits each: 10 are 50
/// bits; no 0/O, 1/I, typed from a screen or paper), shown in two halves `ABCDE-FGHJK`.
fn new_code(len: usize) -> String {
    let c = random::code_base32(len.clamp(10, 16));
    let half = c.len().div_ceil(2);
    format!("{}-{}", &c[..half], &c[half..])
}

/// A session's resume token (§V.13).
pub(crate) fn resume_token() -> String {
    let b = random::secure_random::<24>();
    data_encoding::BASE32_NOPAD.encode(&b).to_lowercase()
}

fn new_id() -> String {
    let b = random::secure_random::<5>();
    format!("h{}", data_encoding::BASE32_NOPAD.encode(&b).to_lowercase())
}

/// A credential: the handset's id (to find it) and 256 secret bits.
fn new_credential(id: &str) -> String {
    let b = random::secure_random::<32>();
    format!(
        "{CREDENTIAL_PREFIX}{id}.{}",
        data_encoding::BASE32_NOPAD.encode(&b).to_lowercase()
    )
}

impl Hub {
    // ---- the records

    /// Every handset as administrators see it, with its sessions on now.
    pub fn handset_list(&self) -> Vec<Value> {
        let list: Vec<Handset> = self.hs.lock_ok().values().cloned().collect();
        let mut on: HashMap<String, usize> = HashMap::new();
        for s in self.st.lock_ok().sessions.values() {
            if let Some(h) = &s.who.handset {
                *on.entry(h.id.clone()).or_default() += 1;
            }
        }
        let now = now_ms();
        let mut out: Vec<Value> = list
            .iter()
            .map(|h| {
                let mut v = h.public(now);
                v["connected"] = json!(on.get(&h.id).copied().unwrap_or(0));
                v
            })
            .collect();
        out.sort_by_key(|v| v["name"].as_str().unwrap_or_default().to_lowercase());
        out
    }

    /// A handset by its id, else its name (any case).
    pub fn handset_find(&self, key: &str) -> Option<Handset> {
        let key = key.trim();
        let hs = self.hs.lock_ok();
        hs.get(key)
            .or_else(|| hs.values().find(|h| h.name.eq_ignore_ascii_case(key)))
            .cloned()
    }

    /// Changes handset `key` with `f`, stored before it is kept (the record's lock held
    /// throughout, so two changes don't undo each other).
    fn handset_update(
        &self,
        key: &str,
        f: impl FnOnce(&mut Handset) -> Result<(), String>,
    ) -> Result<Handset, String> {
        let mut hs = self.hs.lock_ok();
        let key = key.trim();
        let id = match hs.get(key) {
            Some(h) => h.id.clone(),
            None => hs
                .values()
                .find(|h| h.name.eq_ignore_ascii_case(key))
                .map(|h| h.id.clone())
                .ok_or_else(|| format!("no handset {key}"))?,
        };
        let mut h = hs[&id].clone();
        f(&mut h)?;
        self.store.put_handset(&h).map_err(|e| e.to_string())?;
        hs.insert(id, h.clone());
        Ok(h)
    }

    /// A new handset, not enrolled yet.
    pub fn handset_add(&self, name: &str) -> Result<Handset, String> {
        let name = name.trim();
        if !valid_name(name) {
            return Err("a handset's name: 1 to 64 characters".into());
        }
        let mut hs = self.hs.lock_ok();
        if hs.values().any(|h| h.name.eq_ignore_ascii_case(name)) {
            return Err("there is a handset of that name".into());
        }
        if hs.len() >= MAX_HANDSETS {
            return Err(format!("{MAX_HANDSETS} handsets at most"));
        }
        let h = Handset {
            id: new_id(),
            name: name.to_owned(),
            device: String::new(),
            created_ms: now_ms(),
            enrolled_ms: 0,
            last_seen_ms: 0,
            revoked: false,
            secret_sha256: String::new(),
            code_sha256: String::new(),
            code_until_ms: 0,
            driver: None,
        };
        self.store.put_handset(&h).map_err(|e| e.to_string())?;
        hs.insert(h.id.clone(), h.clone());
        Ok(h)
    }

    /// A one-time enrolment code for handset `key`, working for `valid` (the API: the
    /// site's `code_hours`), `code_length` characters; one made before stops working. Only
    /// its hash is kept.
    pub fn handset_code(&self, key: &str, valid: Duration) -> Result<(Handset, String), String> {
        let code = new_code(self.settings().code_length as usize);
        let hash = sha256_hex(bare_code(&code).as_bytes());
        let until = now_ms() + valid.as_millis() as i64;
        let h = self.handset_update(key, |h| {
            h.code_sha256 = hash;
            h.code_until_ms = until;
            Ok(())
        })?;
        Ok((h, code))
    }

    /// A new credential for handset `key` at once (a program provisioning it, no code):
    /// the one before stops working and its sessions end.
    pub fn handset_issue(&self, key: &str) -> Result<(Handset, String), String> {
        let id = self.handset_find(key).ok_or("no such handset")?.id;
        let cred = new_credential(&id);
        let hash = sha256_hex(cred.as_bytes());
        let h = self.handset_update(&id, |h| {
            h.secret_sha256 = hash;
            h.enrolled_ms = now_ms();
            h.revoked = false;
            h.code_sha256.clear();
            h.code_until_ms = 0;
            Ok(())
        })?;
        self.handset_end(&h.id, "this handset was given a new credential");
        Ok((h, cred))
    }

    /// Enrolment with a code (once, within its time): the handset and its new credential.
    /// An earlier credential of that handset stops working, its sessions end.
    pub fn handset_enrol(&self, code: &str, device: &str) -> Result<(Handset, String), String> {
        let given = sha256_hex(bare_code(code).as_bytes());
        let now = now_ms();
        let id = {
            let hs = self.hs.lock_ok();
            // Every pending code compared in full: no early way out that tells which.
            let mut hit = None;
            for h in hs.values() {
                if !h.code_sha256.is_empty()
                    && same(&h.code_sha256, &given)
                    && h.code_until_ms > now
                {
                    hit = Some(h.id.clone());
                }
            }
            hit
        }
        .ok_or("wrong or expired code")?;
        let cred = new_credential(&id);
        let hash = sha256_hex(cred.as_bytes());
        let device: String = device
            .trim()
            .chars()
            .filter(|c| !c.is_control())
            .take(200)
            .collect();
        let h = self.handset_update(&id, |h| {
            // (Used meanwhile by another try with the same code: once only.)
            if h.code_sha256.is_empty() || !same(&h.code_sha256, &given) {
                return Err("wrong or expired code".into());
            }
            h.code_sha256.clear();
            h.code_until_ms = 0;
            h.secret_sha256 = hash;
            h.enrolled_ms = now;
            h.revoked = false;
            h.device = device;
            Ok(())
        })?;
        self.handset_end(&h.id, "this handset was enrolled again");
        Ok((h, cred))
    }

    pub fn handset_rename(&self, key: &str, name: &str) -> Result<Handset, String> {
        let name = name.trim().to_owned();
        if !valid_name(&name) {
            return Err("a handset's name: 1 to 64 characters".into());
        }
        let me = self.handset_find(key).ok_or("no such handset")?;
        if self
            .hs
            .lock_ok()
            .values()
            .any(|h| h.id != me.id && h.name.eq_ignore_ascii_case(&name))
        {
            return Err("there is a handset of that name".into());
        }
        let h = self.handset_update(&me.id, |h| {
            h.name = name;
            Ok(())
        })?;
        self.handset_follow(&h.id);
        Ok(h)
    }

    /// Revoked: its credential and any code stop working, its sessions end at once.
    pub fn handset_revoke(&self, key: &str) -> Result<Handset, String> {
        let h = self.handset_update(key, |h| {
            h.revoked = true;
            h.code_sha256.clear();
            h.code_until_ms = 0;
            Ok(())
        })?;
        self.handset_end(&h.id, "this handset was revoked");
        Ok(h)
    }

    pub fn handset_remove(&self, key: &str) -> Result<Handset, String> {
        let h = self.handset_find(key).ok_or("no such handset")?;
        {
            let mut hs = self.hs.lock_ok();
            self.store
                .remove_handset(&h.id)
                .map_err(|e| e.to_string())?;
            hs.remove(&h.id);
        }
        self.handset_end(&h.id, "this handset was removed");
        Ok(h)
    }

    /// When it was last on.
    pub fn handset_seen(&self, id: &str) {
        let _ = self.handset_update(id, |h| {
            h.last_seen_ms = now_ms();
            Ok(())
        });
    }

    // ---- signing in

    /// The handset a credential (`Authorization: Bearer …`) is, if it is a live one's.
    pub fn handset_by_credential(&self, token: &str) -> Option<Handset> {
        let rest = token.strip_prefix(CREDENTIAL_PREFIX)?;
        let (id, _) = rest.split_once('.')?;
        let h = self.hs.lock_ok().get(id).cloned()?;
        let ok = !h.revoked
            && !h.secret_sha256.is_empty()
            && same(&h.secret_sha256, &sha256_hex(token.as_bytes()));
        ok.then_some(h)
    }

    /// Who a handset's session is: its driver (their name shown, `driver:<id>` its key,
    /// their roles and assigned channels), else "Handset NAME" (`handset:<id>`) with the
    /// role `handset`. Never an administrator, never a site user's identity.
    pub fn handset_who(&self, h: &Handset) -> Who {
        let tag = HandsetTag {
            id: h.id.clone(),
            name: h.name.clone(),
            driver: h.driver.as_ref().map(|d| d.id.clone()),
            secret: h.secret_sha256.clone(),
        };
        match &h.driver {
            Some(d) => Who {
                name: d.name.clone(),
                ident: Some(format!("driver:{}", d.id)),
                user: true,
                roles: d.roles.clone(),
                handset: Some(tag),
                granted: self.granted(d),
                ..Default::default()
            },
            None => Who {
                name: format!("Handset {}", h.name),
                ident: Some(format!("handset:{}", h.id)),
                user: true,
                roles: vec!["handset".into()],
                handset: Some(tag),
                ..Default::default()
            },
        }
    }

    /// The channels a driver's assignment names, as they are now (by name, any case).
    fn granted(&self, d: &Driver) -> Vec<u32> {
        let st = self.st.lock_ok();
        let mut ids: Vec<u32> = d
            .channels
            .iter()
            .filter_map(|n| {
                st.channels
                    .values()
                    .find(|c| c.name.eq_ignore_ascii_case(n.trim()))
                    .map(|c| c.id)
            })
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    fn handset_sessions(st: &State, id: &str) -> Vec<Sid> {
        st.sessions
            .iter()
            .filter(|(_, s)| s.who.handset.as_ref().is_some_and(|h| h.id == id))
            .map(|(k, _)| *k)
            .collect()
    }

    /// Whether handset `id`'s record still has the credential `secret` and isn't revoked
    /// (under `hs`, held by the caller).
    fn handset_live(hs: &BTreeMap<String, Handset>, id: &str, secret: &str) -> bool {
        hs.get(id)
            .is_some_and(|r| !r.revoked && !r.secret_sha256.is_empty() && r.secret_sha256 == secret)
    }

    /// Joins `sid` to its driver's channels it isn't in, noting them as the assignment's.
    fn join_granted(&self, st: &mut State, sid: Sid, granted: &[u32]) {
        for &c in granted {
            let inside = st
                .sessions
                .get(&sid)
                .is_some_and(|s| s.channels.contains_key(&c));
            if !inside
                && self.join_locked(st, sid, c, false).is_ok()
                && let Some(s) = st.sessions.get_mut(&sid)
            {
                s.by_driver.insert(c);
            }
        }
    }

    /// A new resume token for `sid`, sent in its next `hello`: kept with the one it last
    /// proved it has (its last resume's, or its first) and any sent since (§V.13.6).
    fn new_token(st: &mut State, sid: Sid) -> Option<String> {
        let s = st.sessions.get_mut(&sid)?;
        let t = resume_token();
        s.resume.push(t.clone());
        // (The first is the proved one; at most eight in all.)
        while s.resume.len() > 8 {
            s.resume.remove(1);
        }
        Some(t)
    }

    /// A handset's session just connected afresh (its `hello` sent; not a resume): an
    /// earlier session of the same handset — waiting for its resume, or on a link not yet
    /// known lost — is closed and taken over, its channels joined again (`rejoined`), its
    /// driver's channels joined.
    pub fn handset_connected(&self, sid: Sid) {
        let mut st = self.st.lock_ok();
        let Some(who) = st.sessions.get(&sid).map(|s| s.who.clone()) else {
            return;
        };
        let Some(tag) = who.handset.clone() else {
            return;
        };
        let older: Vec<Sid> = Self::handset_sessions(&st, &tag.id)
            .into_iter()
            .filter(|s| *s != sid)
            .collect();
        let mut carry: Vec<(u32, bool, bool)> = Vec::new();
        for &o in &older {
            if let Some(s) = st.sessions.get(&o) {
                // The same person (driver) gets password channels back too.
                let same_person = s.who.key() == who.key();
                carry.extend(
                    s.channels
                        .keys()
                        .map(|c| (*c, same_person, s.by_driver.contains(c) && same_person)),
                );
                let _ = s.tx.send(Out::Close("this handset connected again".into()));
            }
            // (Its talk ends and its floors go now, not when its link times out.)
            self.drop_session_locked(&mut st, o);
        }
        let mut rejoined = Vec::new();
        for (c, same_person, assigned) in carry {
            if self.join_locked(&mut st, sid, c, same_person).is_ok() {
                rejoined.push(c);
                if assigned && let Some(s) = st.sessions.get_mut(&sid) {
                    s.by_driver.insert(c);
                }
            }
        }
        self.join_granted(&mut st, sid, &who.granted);
        if !rejoined.is_empty()
            && let Some(s) = st.sessions.get(&sid)
        {
            send(s, json!({"t": "rejoined", "channels": rejoined}));
        }
        drop(st);
        if let Some(r) = self.rtc() {
            for o in older {
                r.forget(o);
            }
        }
    }

    /// A handset's connection back with a resume token (§V.13.6): its session — identity,
    /// channels, its place in them — on the new connection at once, a fresh `hello`
    /// (`resumed`: true, `backlog`: the sound frames to send, sent next), no re-join.
    /// `have` (the client's last `seq` from each talker): what it didn't get is sent again,
    /// from what the session kept. `None` (a fresh connect then): no such session (its
    /// window over, a wrong token), or a credential no longer the handset's. Its session,
    /// its sound queue and connection number.
    pub fn handset_resume(
        &self,
        handset: &HandsetTag,
        token: &str,
        tx: tokio::sync::mpsc::UnboundedSender<Out>,
        have: Option<&HashMap<Sid, u32>>,
    ) -> Option<(Sid, Arc<FrameQueue>, u64)> {
        let find = |st: &State| {
            st.sessions
                .iter()
                .find(|(_, s)| {
                    s.who.handset.as_ref().is_some_and(|h| h.id == handset.id)
                        && s.resume.iter().any(|t| same(t, token))
                })
                .map(|(k, s)| (*k, s.conn, s.who.clone()))
        };
        let (sid, conn, who) = find(&self.st.lock_ok())?;
        let settings = self.store.user_settings(who.key());
        // The record and the session checked again under the locks the change is made
        // under: a revoke, or another resume with the same token, in between loses or
        // wins whole.
        let hs = self.hs.lock_ok();
        if !Self::handset_live(&hs, &handset.id, &handset.secret) {
            return None;
        }
        let mut st = self.st.lock_ok();
        drop(hs);
        match find(&st) {
            Some((s2, c2, _)) if s2 == sid && c2 == conn => {}
            _ => return None,
        }
        // (A talk on the connection it left ends cleanly: its floor goes.)
        self.stop_talk_locked(&mut st, sid, "link lost");
        let dispatch = is_dispatch(&who, &st.settings);
        let s = st.sessions.get_mut(&sid)?;
        if s.away.is_none() {
            // The connection it left isn't known lost yet: told, and let go.
            let _ =
                s.tx.send(Out::Close("resumed on another connection".into()));
        }
        s.tx = tx;
        s.conn += 1;
        s.away = None;
        s.rtc = false;
        s.slots = [None; SLOTS];
        let frames = Arc::new(FrameQueue::take_from(&s.frames, have));
        s.frames = frames.clone();
        // The token it came with is the one it is known to have, with the new one.
        let used = s
            .resume
            .iter()
            .find(|t| same(t, token))
            .cloned()
            .unwrap_or_default();
        s.resume = vec![used];
        let generation = s.conn;
        let token = Self::new_token(&mut st, sid).unwrap_or_default();
        let mut hello = Self::hello(&st, sid, &who, &settings, dispatch);
        hello["resume"] = json!(token);
        hello["resumed"] = json!(true);
        hello["backlog"] = json!(frames.pending());
        let chans: Vec<u32> = st.sessions[&sid].channels.keys().copied().collect();
        let s = &st.sessions[&sid];
        send(s, hello);
        for c in chans {
            send(s, Self::members_message(&st, c));
            send(
                s,
                json!({"t": "talkers", "channel": c, "talkers": Self::talkers(&st, c)}),
            );
        }
        if dispatch {
            for a in st.alerts.values() {
                send(s, json!({"t": "sos", "sos": a.json()}));
            }
        }
        self.broadcast_presence(&st);
        drop(st);
        if let Some(r) = self.rtc() {
            r.forget(sid);
        }
        Some((sid, frames, generation))
    }

    /// A session's connection ended (`generation` the one it was on). A handset's waits
    /// the site's `resume_s` for its resume — its talk and calls ended, still in its
    /// channels and shown "reconnecting", its sound kept — unless it said `bye`; any
    /// other's ends. A connection a handset has left (resumed elsewhere) ends nothing.
    pub fn detach(&self, sid: Sid, generation: u64, bye: bool) {
        let mut st = self.st.lock_ok();
        let Some(s) = st.sessions.get(&sid) else {
            return;
        };
        if s.conn != generation {
            return;
        }
        let resume = st.settings.resume_s > 0;
        if s.who.handset.is_none() || bye || !resume {
            self.drop_session_locked(&mut st, sid);
        } else {
            self.stop_talk_locked(&mut st, sid, "link lost");
            let calls: Vec<u64> = st
                .calls
                .iter()
                .filter(|(_, c)| c.a == sid || c.b == Some(sid))
                .map(|(k, _)| *k)
                .collect();
            for c in calls {
                self.end_call_locked(&mut st, c, "link lost");
            }
            if let Some(s) = st.sessions.get_mut(&sid) {
                s.away = Some(Instant::now());
                s.rtc = false;
                s.slots = [None; SLOTS];
            }
            self.broadcast_presence(&st);
        }
        drop(st);
        if let Some(r) = self.rtc() {
            r.forget(sid);
        }
    }

    /// Ends every session of handset `id` at once (revoked, removed, a new credential).
    fn handset_end(&self, id: &str, why: &str) {
        let mut st = self.st.lock_ok();
        let ids = Self::handset_sessions(&st, id);
        for &s in &ids {
            if let Some(x) = st.sessions.get(&s) {
                let _ = x.tx.send(Out::Close(why.into()));
            }
            self.drop_session_locked(&mut st, s);
        }
        drop(st);
        if let Some(r) = self.rtc() {
            for s in ids {
                r.forget(s);
            }
        }
    }

    /// Every connected driver's assigned channels found again (a channel renamed, made,
    /// removed): each session follows its record.
    pub(super) fn regrant(&self) {
        let ids: BTreeSet<String> = self
            .st
            .lock_ok()
            .sessions
            .values()
            .filter_map(|s| s.who.handset.as_ref().filter(|h| h.driver.is_some()))
            .map(|h| h.id.clone())
            .collect();
        for id in ids {
            self.handset_follow(&id);
        }
    }

    /// Handset `id`'s record changed (its driver, its name, its channels' names): its
    /// live sessions become who it is now at once. Another person (a driver changed or
    /// cleared): talk and calls ended, every channel the old assignment put it in left,
    /// those it may no longer be in left, a fresh `hello`, the new driver's channels
    /// joined. The same person: the assignment's channels followed (left where no longer
    /// assigned and not otherwise allowed, joined where newly assigned).
    pub(crate) fn handset_follow(&self, id: &str) {
        let Some(h) = self.hs.lock_ok().get(id).cloned() else {
            return self.handset_end(id, "this handset was removed");
        };
        if h.revoked {
            return self.handset_end(id, "this handset was revoked");
        }
        let who = self.handset_who(&h);
        let settings = self.store.user_settings(who.key());
        let dispatch = self.dispatch(&who);
        let mut st = self.st.lock_ok();
        for sid in Self::handset_sessions(&st, id) {
            let old = st.sessions[&sid].who.clone();
            let same_person = old.key() == who.key();
            if same_person && old.roles == who.roles && old.name == who.name {
                // (Renamed handset, or its channels found again.)
                if let Some(s) = st.sessions.get_mut(&sid) {
                    s.who = who.clone();
                }
                if old.granted != who.granted {
                    let gone: Vec<u32> = st.sessions[&sid]
                        .by_driver
                        .iter()
                        .copied()
                        .filter(|c| !who.granted.contains(c))
                        .collect();
                    for c in gone {
                        if let Some(s) = st.sessions.get_mut(&sid) {
                            s.by_driver.remove(&c);
                        }
                        if st
                            .channels
                            .get(&c)
                            .and_then(|ch| may(&who, ch, true))
                            .is_none()
                        {
                            self.leave_locked(&mut st, sid, c, "driver changed");
                        }
                    }
                    self.join_granted(&mut st, sid, &who.granted);
                }
                continue;
            }
            self.stop_talk_locked(&mut st, sid, "driver changed");
            let calls: Vec<u64> = st
                .calls
                .iter()
                .filter(|(_, c)| c.a == sid || c.b == Some(sid))
                .map(|(k, _)| *k)
                .collect();
            for c in calls {
                self.end_call_locked(&mut st, c, "driver changed");
            }
            // Every channel the old assignment put it in goes with it, Open and Guest
            // ones too.
            let assigned: Vec<u32> = if same_person {
                Vec::new()
            } else {
                st.sessions[&sid].by_driver.iter().copied().collect()
            };
            for c in assigned {
                self.leave_locked(&mut st, sid, c, "driver changed");
            }
            if let Some(s) = st.sessions.get_mut(&sid) {
                s.who = who.clone();
                if !same_person {
                    s.by_driver.clear();
                }
            }
            let mine: Vec<u32> = st.sessions[&sid].channels.keys().copied().collect();
            for c in mine {
                // A password channel stays only for the same person (they gave it).
                match st
                    .channels
                    .get(&c)
                    .and_then(|ch| may(&who, ch, same_person))
                {
                    None => self.leave_locked(&mut st, sid, c, "driver changed"),
                    Some(talk) => {
                        if let Some(s) = st.sessions.get_mut(&sid) {
                            s.channels.insert(c, talk);
                        }
                    }
                }
            }
            let token = Self::new_token(&mut st, sid);
            if let Some(s) = st.sessions.get(&sid) {
                let mut hello = Self::hello(&st, sid, &who, &settings, dispatch);
                if let Some(t) = token {
                    hello["resume"] = json!(t);
                    hello["resumed"] = json!(false);
                }
                send(s, hello);
                if dispatch {
                    for a in st.alerts.values() {
                        send(s, json!({"t": "sos", "sos": a.json()}));
                    }
                }
            }
            self.join_granted(&mut st, sid, &who.granted);
        }
        self.broadcast_presence(&st);
    }

    // ---- drivers

    /// The site's program assigns `driver` to handset `key` (its id or name), or clears it
    /// (`None`: off shift). A driver is on one handset at a time: assigned here, taken off
    /// any other, in one step under the records' lock. Audited, with the channels whose
    /// Password or Auth gate it passes.
    pub fn assign_driver(
        &self,
        key: &str,
        driver: Option<Driver>,
        by: &str,
    ) -> Result<Value, String> {
        let mut driver = driver;
        if let Some(d) = &mut driver {
            d.check()?;
            d.id = d.id.trim().to_owned();
            d.name = d.name.trim().to_owned();
            d.roles = d.roles.iter().map(|r| r.trim().to_owned()).collect();
            let mut seen = BTreeSet::new();
            d.channels = d
                .channels
                .iter()
                .map(|c| c.trim().to_owned())
                .filter(|c| seen.insert(c.to_lowercase()))
                .collect();
            d.since_ms = now_ms();
        }
        let (h, before, moved) = {
            let mut hs = self.hs.lock_ok();
            let k = key.trim();
            let id = hs
                .get(k)
                .map(|h| h.id.clone())
                .or_else(|| {
                    hs.values()
                        .find(|h| h.name.eq_ignore_ascii_case(k))
                        .map(|h| h.id.clone())
                })
                .ok_or_else(|| format!("no handset {k}"))?;
            let mut moved: Vec<Handset> = Vec::new();
            if let Some(d) = &driver {
                for h in hs.values() {
                    if h.id != id && h.driver.as_ref().is_some_and(|x| x.id == d.id) {
                        let mut o = h.clone();
                        o.driver = None;
                        moved.push(o);
                    }
                }
            }
            let mut h = hs[&id].clone();
            let before = h.driver.clone();
            h.driver = driver.clone();
            for o in &moved {
                self.store.put_handset(o).map_err(|e| e.to_string())?;
            }
            self.store.put_handset(&h).map_err(|e| e.to_string())?;
            for o in &moved {
                hs.insert(o.id.clone(), o.clone());
            }
            hs.insert(id, h.clone());
            (h, before, moved)
        };
        for o in &moved {
            self.host.audit(
                by,
                "voice.driver.clear",
                json!({"handset": o.name, "driver": driver.as_ref().map(|d| d.id.clone()), "why": format!("moved to {}", h.name)}),
            );
            self.handset_follow(&o.id);
        }
        let chans = self.channels();
        let (mut found, mut unknown, mut gated) = (Vec::new(), Vec::new(), Vec::new());
        for n in driver.iter().flat_map(|d| d.channels.iter()) {
            match chans.iter().find(|c| c.name.eq_ignore_ascii_case(n)) {
                Some(c) => {
                    found.push(c.name.clone());
                    if matches!(c.access, Access::Password { .. } | Access::Auth { .. }) {
                        gated.push(c.name.clone());
                    }
                }
                None => unknown.push(n.clone()),
            }
        }
        match &driver {
            Some(d) => self.host.audit(
                by,
                "voice.driver.assign",
                json!({
                    "handset": h.name, "driver": d.id, "name": d.name, "roles": d.roles,
                    "channels": found, "past_gates": gated, "unknown_channels": unknown,
                }),
            ),
            None => self.host.audit(
                by,
                "voice.driver.clear",
                json!({"handset": h.name, "driver": before.as_ref().map(|d| d.id.clone())}),
            ),
        }
        self.handset_follow(&h.id);
        Ok(json!({
            "handset": h.public(now_ms()),
            "channels": found,
            "past_gates": gated,
            "unknown_channels": unknown,
        }))
    }

    // ---- SOS

    /// A handset's SOS (its radio's button): to every session with the dispatch
    /// permission (and those that connect later) until one acknowledges it, the audit, the
    /// log and MQTT `events`. Presses within the site's `sos_merge_s` of the last are the
    /// same alert; later ones, still unacknowledged, send it again (`count`). Its number.
    pub fn sos(&self, sid: Sid, position: Option<Value>, text: &str) -> Result<u64, String> {
        let position = match position {
            None => None,
            Some(p) => match (p["lat"].as_f64(), p["lon"].as_f64()) {
                (Some(lat), Some(lon))
                    if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) =>
                {
                    Some(json!({"lat": lat, "lon": lon}))
                }
                _ => return Err("position: {\"lat\", \"lon\"} in degrees".into()),
            },
        };
        let text: String = text
            .trim()
            .chars()
            .filter(|c| !c.is_control())
            .take(500)
            .collect();
        let mut st = self.st.lock_ok();
        let Some(s) = st.sessions.get(&sid) else {
            return Err("gone".into());
        };
        let Some(tag) = s.who.handset.clone() else {
            return Err("SOS is for handsets".into());
        };
        let from = s.who.name.clone();
        let now = now_ms();
        let merge = Duration::from_secs(st.settings.sos_merge_s as u64);
        let id = if let Some(a) = st.alerts.values_mut().find(|a| a.handset_id == tag.id) {
            let quiet = a.last.elapsed() < merge;
            a.last = Instant::now();
            if position.is_some() {
                a.position = position;
            }
            if !text.is_empty() {
                a.text = text;
            }
            if quiet {
                return Ok(a.id);
            }
            a.count += 1;
            a.time_ms = now;
            a.from = from.clone();
            a.sent = Instant::now();
            a.id
        } else {
            let id = st.next_alert;
            st.next_alert += 1;
            st.alerts.insert(
                id,
                Alert {
                    id,
                    handset_id: tag.id.clone(),
                    handset: tag.name.clone(),
                    from: from.clone(),
                    driver: tag.driver.clone(),
                    first_ms: now,
                    time_ms: now,
                    position,
                    text,
                    count: 1,
                    last: Instant::now(),
                    sent: Instant::now(),
                },
            );
            id
        };
        let a = st.alerts[&id].json();
        for s in st
            .sessions
            .values()
            .filter(|s| is_dispatch(&s.who, &st.settings))
        {
            send(s, json!({"t": "sos", "sos": a}));
        }
        drop(st);
        let mut ev = a.clone();
        ev["event"] = json!("sos");
        self.host.publish("events", &ev, false);
        self.host.audit(&from, "voice.sos", a);
        idn_open_protocol::log::log(&format!(
            "voice: SOS from {from} (handset {}), alert {id}",
            tag.name
        ));
        Ok(id)
    }

    /// SOS alert `id` acknowledged by `by` (a dispatcher's session `from`, else the API
    /// or MQTT): taken off every dispatcher's list, the handset told, audited.
    pub fn sos_ack(&self, id: u64, by: &str, from: Option<Sid>) -> Result<(), String> {
        let mut st = self.st.lock_ok();
        if let Some(sid) = from {
            let ok = st
                .sessions
                .get(&sid)
                .is_some_and(|s| is_dispatch(&s.who, &st.settings));
            if !ok {
                return Err("acknowledging an SOS is for dispatch".into());
            }
        }
        let a = st
            .alerts
            .remove(&id)
            .ok_or("no such SOS (acknowledged already?)")?;
        let now = now_ms();
        let m = json!({"t": "sos_ack", "id": id, "by": by, "time_ms": now});
        for s in st.sessions.values().filter(|s| {
            is_dispatch(&s.who, &st.settings)
                || s.who.handset.as_ref().is_some_and(|h| h.id == a.handset_id)
        }) {
            send(s, m.clone());
        }
        drop(st);
        let detail = json!({
            "id": id, "handset": {"id": a.handset_id, "name": a.handset}, "from": a.from,
            "by": by, "time_ms": now,
        });
        let mut ev = detail.clone();
        ev["event"] = json!("sos_ack");
        self.host.publish("events", &ev, false);
        self.host.audit(by, "voice.sos.ack", detail);
        Ok(())
    }

    /// Each SOS not acknowledged for the site's `sos_realert_s` (0: never) sent to
    /// dispatch again (`"realert": true`; MQTT `sos_realert`). From the tick.
    pub(super) fn sos_realert_locked(&self, st: &mut State) {
        let every = st.settings.sos_realert_s;
        if every == 0 {
            return;
        }
        let every = Duration::from_secs(every as u64);
        let due: Vec<Value> = st
            .alerts
            .values_mut()
            .filter(|a| a.sent.elapsed() >= every)
            .map(|a| {
                a.sent = Instant::now();
                a.json()
            })
            .collect();
        for a in due {
            for s in st
                .sessions
                .values()
                .filter(|s| is_dispatch(&s.who, &st.settings))
            {
                send(s, json!({"t": "sos", "sos": a, "realert": true}));
            }
            let mut ev = a;
            ev["event"] = json!("sos_realert");
            self.host.publish("events", &ev, false);
        }
    }

    /// The SOS alerts not yet acknowledged.
    pub fn sos_list(&self) -> Vec<Value> {
        self.st.lock_ok().alerts.values().map(Alert::json).collect()
    }
}

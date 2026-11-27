//! What an administrator sets up and users choose: channels (§V.4), their ways in (§V.3),
//! a user's own settings (§V.5) and the voice settings of the site.

use serde::{Deserialize, Serialize};

/// The kind of a channel (§V.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Kind {
    /// A fed source, listen-only: `source` an IDN source of the site.
    Iem { source: String },
    /// A talk channel: full duplex, or one talker at a time (`half_duplex`), a talk ending
    /// after `limit_s` seconds (0: no limit) in half duplex.
    Walkie {
        #[serde(default)]
        half_duplex: bool,
        #[serde(default = "default_limit")]
        limit_s: u32,
    },
}

fn default_limit() -> u32 {
    60
}

/// A channel's way in (§V.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "access", rename_all = "snake_case")]
pub enum Access {
    /// Anyone with a display name, no account.
    Guest,
    /// Any signed-in site user.
    Open,
    /// Anyone with the password (its hash, `idn_open_protocol::web` form).
    Password { hash: String },
    /// Only these users and roles; `talk` false: listen only.
    Auth { allow: Vec<Allowed> },
}

/// One entry of an Auth channel's list: a user (`user`) or a role (`role`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Allowed {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default = "yes")]
    pub talk: bool,
}

fn yes() -> bool {
    true
}

/// A channel (§V.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Channel {
    /// Its number on the wire (the WebSocket's frames, slots), never reused.
    pub id: u32,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(flatten)]
    pub kind: Kind,
    #[serde(flatten)]
    pub access: Access,
    /// Transmissions recorded (IEM: not by default).
    #[serde(default = "yes")]
    pub record: bool,
    /// A Walkie-Talkie channel talked into from an IDN source while it has sound (the
    /// desk's talkback, §V.10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub talk_source: Option<String>,
    /// The channel made an IDN source ("HOST (Voice NAME)") for a desk input (§V.10).
    #[serde(default)]
    pub as_source: bool,
    /// Its own store-and-forward window for handsets whose link is down, seconds (0: none
    /// kept); unset: the site's ([`Settings::store_forward_s`], §V.13).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_forward_s: Option<u32>,
    /// Its own jitter buffer advice for clients, ms; unset: the site's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jitter_start_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jitter_max_ms: Option<u32>,
}

impl Channel {
    pub fn is_iem(&self) -> bool {
        matches!(self.kind, Kind::Iem { .. })
    }

    pub fn half_duplex(&self) -> Option<u32> {
        match self.kind {
            Kind::Walkie {
                half_duplex: true,
                limit_s,
            } => Some(limit_s),
            _ => None,
        }
    }

    /// The channel as clients see it (no password hash).
    pub fn public(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        if let Some(o) = v.as_object_mut() {
            o.remove("hash");
        }
        v
    }
}

/// Who may talk to a user straight away (instant talk, §V.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum InstantFrom {
    #[default]
    Anyone,
    Dispatch,
    Never,
}

/// A user's own Voice settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UserSettings {
    #[serde(default)]
    pub instant_from: InstantFrom,
}

/// The site's Voice settings (administrators; IDN Industrial's API key). Every timing and
/// limit of the links and handsets (§V.13) is one, with its default and range
/// ([`Settings::check`]); a field left out of a change keeps its value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Days recordings and messages are kept (0: until deleted).
    #[serde(default = "thirty")]
    pub keep_days: u32,
    /// Users and roles with the dispatch permission (§V.2): priority, all-call.
    #[serde(default)]
    pub dispatch: Vec<Allowed>,
    /// A handset whose link dropped keeps its session this long for its resume, seconds
    /// (0: none, it ends at once). 0–3600, default 120.
    #[serde(default = "d_resume")]
    pub resume_s: u32,
    /// Sound for a handset whose link is down kept and sent on its resume. Default on.
    #[serde(default = "yes")]
    pub store_forward: bool,
    /// ... for this long at most, seconds (a channel may set its own). 0–300, default 30.
    #[serde(default = "thirty")]
    pub store_forward_s: u32,
    /// Sound kept for a WebSocket client whose link is slow, seconds; older goes first.
    /// 1–60, default 10.
    #[serde(default = "ten")]
    pub ws_queue_s: u32,
    /// An enrolment code works for this long, hours. 1–720, default 24.
    #[serde(default = "d_code_hours")]
    pub code_hours: u32,
    /// An enrolment code's characters (5 bits each). 10–16, default 10.
    #[serde(default = "ten")]
    pub code_length: u32,
    /// Enrolment tries an address (IPv6: its /64) may get wrong a minute. 1–100, default
    /// 5.
    #[serde(default = "d_enrol")]
    pub enrol_per_minute: u32,
    /// ... and the whole site (behind a tunnel every app has the tunnel's address).
    /// 1–10000, default 60.
    #[serde(default = "d_enrol_all")]
    pub enrol_all_per_minute: u32,
    /// Sound queued for every session together (slow links, handsets away, sent sound
    /// kept for a resume), MB; past it each queue's oldest go first, counted. 8–4096,
    /// default 64.
    #[serde(default = "d_budget")]
    pub frame_budget_mb: u32,
    /// Handset credentials, enrolment and provisioning taken over plain HTTP from a
    /// connection that isn't a trusted proxy's (IDN Industrial's plant network without
    /// its TLS port). Default off.
    #[serde(default)]
    pub plain_handsets: bool,
    /// SOS presses from one handset this close together are one alert, seconds. 0–60,
    /// default 5.
    #[serde(default = "d_enrol")]
    pub sos_merge_s: u32,
    /// An SOS not yet acknowledged is sent to dispatch again this often, seconds (0:
    /// never). 0–3600, default 30.
    #[serde(default = "thirty")]
    pub sos_realert_s: u32,
    /// A talk that brings no sound for this long ends (a talker whose link dropped),
    /// seconds. 1–60, default 3.
    #[serde(default = "d_silence")]
    pub talk_silence_s: u32,
    /// ... before its first packet (a microphone opening). 1–120, default 10.
    #[serde(default = "ten")]
    pub talk_first_s: u32,
    /// The jitter buffer clients are told to start with, ms (a channel may set its own).
    /// 20–5000, default 200.
    #[serde(default = "d_jitter_start")]
    pub jitter_start_ms: u32,
    /// ... and to grow to at most, after stalls. start–20000, default 2000.
    #[serde(default = "d_jitter_max")]
    pub jitter_max_ms: u32,
    /// A session that sends nothing for this long ends, seconds. 20–600, default 30.
    #[serde(default = "thirty")]
    pub idle_timeout_s: u32,
    /// Clients are told to ping this often, seconds; at most half the idle timeout.
    /// 2–300, default 10.
    #[serde(default = "ten")]
    pub ping_s: u32,
    /// Voice over IDN's link (§V.14: QUIC, for handsets moving between networks) offered
    /// where the Site Manager runs its listener. Default on.
    #[serde(default = "yes")]
    pub link: bool,
    /// Clients give QUIC this long to connect before they use the WebSocket, ms.
    /// 500–30000, default 3000.
    #[serde(default = "d_link_connect")]
    pub link_connect_ms: u32,
    /// ... and on the WebSocket try QUIC again this often, seconds. 5–3600, default 60.
    #[serde(default = "d_link_retry")]
    pub link_retry_s: u32,
    /// QUIC keep-alive, ms (the idle timeout is `idle_timeout_s`). 1000–60000 and below
    /// the idle timeout, default 5000.
    #[serde(default = "d_link_keepalive")]
    pub link_keepalive_ms: u32,
}

fn thirty() -> u32 {
    30
}
fn ten() -> u32 {
    10
}
fn d_resume() -> u32 {
    120
}
fn d_code_hours() -> u32 {
    24
}
fn d_enrol() -> u32 {
    5
}
fn d_enrol_all() -> u32 {
    60
}
fn d_budget() -> u32 {
    64
}
fn d_silence() -> u32 {
    3
}
fn d_jitter_start() -> u32 {
    200
}
fn d_jitter_max() -> u32 {
    2000
}
fn d_link_connect() -> u32 {
    3000
}
fn d_link_retry() -> u32 {
    60
}
fn d_link_keepalive() -> u32 {
    5000
}

impl Default for Settings {
    fn default() -> Self {
        // (Each default once, where serde has it.)
        serde_json::from_value(serde_json::json!({})).expect("defaults")
    }
}

impl Settings {
    /// Each value within its range.
    pub fn check(&self) -> Result<(), String> {
        let within = |name: &str, v: u32, lo: u32, hi: u32| {
            if (lo..=hi).contains(&v) {
                Ok(())
            } else {
                Err(format!("{name}: {lo} to {hi}"))
            }
        };
        within("resume_s", self.resume_s, 0, 3600)?;
        within("store_forward_s", self.store_forward_s, 0, 300)?;
        within("ws_queue_s", self.ws_queue_s, 1, 60)?;
        within("code_hours", self.code_hours, 1, 720)?;
        within("code_length", self.code_length, 10, 16)?;
        within("enrol_per_minute", self.enrol_per_minute, 1, 100)?;
        within("enrol_all_per_minute", self.enrol_all_per_minute, 1, 10_000)?;
        within("frame_budget_mb", self.frame_budget_mb, 8, 4096)?;
        within("sos_merge_s", self.sos_merge_s, 0, 60)?;
        within("sos_realert_s", self.sos_realert_s, 0, 3600)?;
        within("talk_silence_s", self.talk_silence_s, 1, 60)?;
        within("talk_first_s", self.talk_first_s, 1, 120)?;
        within("jitter_start_ms", self.jitter_start_ms, 20, 5000)?;
        within(
            "jitter_max_ms",
            self.jitter_max_ms,
            self.jitter_start_ms,
            20_000,
        )?;
        within("idle_timeout_s", self.idle_timeout_s, 20, 600)?;
        within("ping_s", self.ping_s, 2, (self.idle_timeout_s / 2).min(300))?;
        within("link_connect_ms", self.link_connect_ms, 500, 30_000)?;
        within("link_retry_s", self.link_retry_s, 5, 3600)?;
        within(
            "link_keepalive_ms",
            self.link_keepalive_ms,
            1000,
            (self.idle_timeout_s * 1000 - 1).min(60_000),
        )?;
        Ok(())
    }

    /// What clients are told in `hello` to keep their link and play sound with.
    pub fn link(&self) -> serde_json::Value {
        serde_json::json!({
            "ping_s": self.ping_s,
            "idle_timeout_s": self.idle_timeout_s,
            "resume_s": self.resume_s,
            "jitter_start_ms": self.jitter_start_ms,
            "jitter_max_ms": self.jitter_max_ms,
            "link_connect_ms": self.link_connect_ms,
            "link_retry_s": self.link_retry_s,
            "link_keepalive_ms": self.link_keepalive_ms,
        })
    }
}

impl Channel {
    /// Its own overrides checked: store-and-forward 0–300 s, jitter 20–5000 ms to start
    /// and at most 20000 ms, the start not above the maximum.
    pub fn check_overrides(&self) -> Result<(), String> {
        if self.store_forward_s.is_some_and(|v| v > 300) {
            return Err("store_forward_s: 0 to 300".into());
        }
        if self
            .jitter_start_ms
            .is_some_and(|v| !(20..=5000).contains(&v))
        {
            return Err("jitter_start_ms: 20 to 5000".into());
        }
        if self
            .jitter_max_ms
            .is_some_and(|v| !(20..=20_000).contains(&v))
        {
            return Err("jitter_max_ms: 20 to 20000".into());
        }
        if let (Some(a), Some(b)) = (self.jitter_start_ms, self.jitter_max_ms)
            && a > b
        {
            return Err("jitter_start_ms above jitter_max_ms".into());
        }
        Ok(())
    }
}

/// A handset (§V.13): a radio or phone app with a credential of its own, enrolled once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handset {
    pub id: String,
    /// Shown ("Truck 12"); unique on the site.
    pub name: String,
    /// What the app said it runs on at enrolment (model, OS).
    #[serde(default)]
    pub device: String,
    pub created_ms: i64,
    /// When it last enrolled or was given a credential (0: not yet).
    #[serde(default)]
    pub enrolled_ms: i64,
    #[serde(default)]
    pub last_seen_ms: i64,
    #[serde(default)]
    pub revoked: bool,
    /// Its credential's SHA-256 (hex); empty until enrolled. The credential itself is kept
    /// by the handset alone.
    #[serde(default)]
    pub secret_sha256: String,
    /// A pending enrolment code's SHA-256 (hex) and when it expires (0: none).
    #[serde(default)]
    pub code_sha256: String,
    #[serde(default)]
    pub code_until_ms: i64,
    /// The driver the site's program (its dispatch system) assigned it.
    #[serde(default)]
    pub driver: Option<Driver>,
}

impl Handset {
    /// As administrators see it (no hashes).
    pub fn public(&self, now_ms: i64) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "name": self.name,
            "device": self.device,
            "created_ms": self.created_ms,
            "enrolled_ms": self.enrolled_ms,
            "last_seen_ms": self.last_seen_ms,
            "revoked": self.revoked,
            "enrolled": !self.secret_sha256.is_empty(),
            "code_expires_ms": (!self.code_sha256.is_empty() && self.code_until_ms > now_ms)
                .then_some(self.code_until_ms),
            "driver": self.driver,
        })
    }
}

/// A driver as the site's program gives one (§V.13): not a site account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Driver {
    /// The program's own id for them.
    pub id: String,
    pub name: String,
    /// Matched against Auth lists and the dispatch list as a user's roles are.
    #[serde(default)]
    pub roles: Vec<String>,
    /// Voice channels (by name) they are put in, past their Password or Auth gates.
    #[serde(default)]
    pub channels: Vec<String>,
    /// When assigned.
    #[serde(default)]
    pub since_ms: i64,
}

impl Driver {
    /// Checked as given: an id, a name, at most 32 roles and 100 channels.
    pub fn check(&self) -> Result<(), String> {
        let id = self.id.trim();
        if id.is_empty() || id.chars().count() > 128 || id.chars().any(char::is_control) {
            return Err("driver id: 1 to 128 characters".into());
        }
        if !valid_name(&self.name) {
            return Err("driver name: 1 to 64 characters".into());
        }
        if self.roles.len() > 32 || !self.roles.iter().all(|r| valid_name(r)) {
            return Err("driver roles: at most 32, each 1 to 64 characters".into());
        }
        if self.channels.len() > 100 || !self.channels.iter().all(|c| valid_name(c)) {
            return Err("driver channels: at most 100 names".into());
        }
        Ok(())
    }
}

/// A certificate's SHA-256 as `AA:BB:…` (64 hex digits, colons or not, either case);
/// `None` if it isn't one.
pub fn fingerprint_text(s: &str) -> Option<String> {
    let hex: String = s
        .chars()
        .filter(|c| *c != ':' && !c.is_whitespace())
        .collect();
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let up = hex.to_ascii_uppercase();
    Some(
        up.as_bytes()
            .chunks(2)
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect::<Vec<_>>()
            .join(":"),
    )
}

/// Whether a valid channel name: 1–64 characters, no control characters.
pub fn valid_name(n: &str) -> bool {
    let n = n.trim();
    !n.is_empty() && n.chars().count() <= 64 && !n.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_round_trip_and_hide_their_password() {
        let c = Channel {
            id: 3,
            name: "Stage".into(),
            description: String::new(),
            kind: Kind::Walkie {
                half_duplex: true,
                limit_s: 30,
            },
            access: Access::Password {
                hash: "secret-hash".into(),
            },
            record: true,
            talk_source: None,
            as_source: false,
            store_forward_s: Some(0),
            jitter_start_ms: None,
            jitter_max_ms: None,
        };
        let j = serde_json::to_string(&c).unwrap();
        let back: Channel = serde_json::from_str(&j).unwrap();
        assert_eq!(back, c);
        assert!(!c.public().to_string().contains("secret-hash"));
        assert_eq!(c.half_duplex(), Some(30));
        let iem: Channel = serde_json::from_value(serde_json::json!({
            "id": 4, "name": "Band", "kind": "iem", "source": "PC (Aux 1)", "access": "open"
        }))
        .unwrap();
        assert!(iem.is_iem() && iem.record);
        assert_eq!(iem.store_forward_s, None);
    }

    /// Settings stored before §V.13 load with the new defaults; each range is checked.
    #[test]
    fn settings_defaults_and_ranges() {
        let s: Settings = serde_json::from_str(r#"{"keep_days": 7, "dispatch": []}"#).unwrap();
        assert_eq!(s.keep_days, 7);
        assert_eq!(s.resume_s, 120);
        assert!(s.store_forward);
        assert_eq!(s.store_forward_s, 30);
        assert_eq!((s.jitter_start_ms, s.jitter_max_ms), (200, 2000));
        assert_eq!((s.idle_timeout_s, s.ping_s), (30, 10));
        assert!(s.link);
        assert_eq!(
            (s.link_connect_ms, s.link_retry_s, s.link_keepalive_ms),
            (3000, 60, 5000)
        );
        assert!(s.check().is_ok());
        assert_eq!(Settings::default(), serde_json::from_str("{}").unwrap());
        let bad = Settings {
            store_forward_s: 301,
            ..Settings::default()
        };
        assert!(bad.check().is_err());
        let bad = Settings {
            ping_s: 20,
            idle_timeout_s: 30,
            ..Settings::default()
        };
        assert!(bad.check().is_err());
        // The keep-alive comes within the idle timeout.
        let bad = Settings {
            link_keepalive_ms: 30_000,
            idle_timeout_s: 30,
            ..Settings::default()
        };
        assert!(bad.check().is_err());
    }

    #[test]
    fn fingerprints_are_written_one_way() {
        let hex = "ab".repeat(32);
        let fp = fingerprint_text(&hex).unwrap();
        assert!(fp.starts_with("AB:AB:") && fp.len() == 95);
        assert_eq!(fingerprint_text(&fp).as_deref(), Some(fp.as_str()));
        assert!(fingerprint_text("abc").is_none());
    }
}

//! The Site Manager's changes as typed commands (IDN Media as a server and libraries; the
//! UI is the client's): each `POST /api/v1/<path>` the API takes, as a [`SiteCommand`] a host
//! runs with [`crate::host::Handle::run`] for a user (that user's role checked and the
//! change audited, as over HTTP). Its JSON schema: [`schema`] (`GET /api/v1/schema`).
//! Not here: the backup, the restore and a release's upload (files, not commands).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One change to the site.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "cmd", content = "body", rename_all = "snake_case")]
pub enum SiteCommand {
    /// Opens the site to new machines for `seconds` (10 to 3600; default 600).
    Pairing {
        #[serde(skip_serializing_if = "Option::is_none")]
        seconds: Option<u64>,
    },
    /// Closes the site to new machines.
    Close {},
    /// Approves the join request `id`; `name`: the machine's name in the site, instead of
    /// the one it asked with: its certificate's (the authority's devices, health, and
    /// `Enrollment::name_in_site` on the machine), and its Node's, set once it is in.
    Approve {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Denies the join request `id`.
    Deny { id: u64 },
    /// A join code for one machine (extensions §E.10.10): six digits, single use, 10
    /// minutes, 5 wrong tries. Answered `{code, expires, id}` (`expires` Unix seconds); the
    /// code is shown this once. `cancel`: takes the waiting code `id` back instead.
    JoinCode {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cancel: Option<u64>,
    },
    /// Takes the machine `fingerprint` out of the site.
    Revoke { fingerprint: String },
    /// Allows a denied or revoked `fingerprint` to ask again.
    Clear { fingerprint: String },
    /// Marks machine `fingerprint` as a pairing relay (`on`), or not:
    /// only a relay shows the site's pairing codes, so only a client pairing through one
    /// is PIN-verified (`KeyRequest::code_ok`); through any other machine a request waits
    /// for a person. A box whose host program relays a client app's pairings is one.
    Relay { fingerprint: String, on: bool },
    /// Restarts the service `service` on the machine `fingerprint` (off, then on).
    Restart {
        fingerprint: String,
        service: String,
    },
    /// Shows the machine `fingerprint`'s name on its screens (its display service) for
    /// `seconds` (default 10).
    Identify {
        fingerprint: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seconds: Option<u64>,
    },
    /// Asks the machine `fingerprint` for its log files now.
    Logs { fingerprint: String },
    /// Changes the machine `fingerprint`'s settings: `name`, `interface`, `require`,
    /// `site_lock`, `video_<key>`, `clock`, `clock_domain`, `clock_protocol`,
    /// `clock_priority`, `pin`/`pin_off`/`pin_site_only`, `lan_password`/`lan_off` (only
    /// those given change; `pin` "none": no PIN, empty: unchanged; the yes/no ones a
    /// boolean or "true"/"on"/"false"/"off").
    Node {
        fingerprint: String,
        #[serde(flatten)]
        settings: Map<String, Value>,
    },
    /// Settings applied to several machines at once: `{"to": [fingerprints], "settings":
    /// {key: value}}`.
    Apply {
        to: Vec<String>,
        settings: Map<String, Value>,
    },
    /// The receiver `receiver` of the machine `fingerprint`: its latency (`None`: automatic).
    ReceiverLatency {
        fingerprint: String,
        receiver: String,
        ms: Option<u32>,
    },
    /// One cell of the routing matrix (`GET /api/v1/matrix`'s ids): the receiver
    /// `receiver_id`'s channel `ch` from the transmitter `tx_id`'s channel `tx_ch` (both
    /// from 0), `on` the state wanted (none: the other of now, as a click). The Site
    /// Manager makes the change it means: a route, a device's subscription, or a route
    /// through a machine's network audio pool.
    RouteCell {
        receiver_id: String,
        ch: u16,
        tx_id: String,
        tx_ch: u16,
        #[serde(skip_serializing_if = "Option::is_none")]
        on: Option<bool>,
    },
    /// The machine `from`'s settings now, of the groups `groups` (`GET
    /// /api/v1/settings_schema`'s ids; none: general, video, clock, panel, audio, listen),
    /// onto the machines `to`; never a machine's own keys nor its PIN (`pin`, with the
    /// panel group, sets theirs).
    CopySettings {
        from: String,
        to: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        groups: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pin: Option<String>,
    },
    /// The template `name` applied to the machines `to` (its machine-own keys left out).
    ApplyTemplate { name: String, to: Vec<String> },
    /// Routes channels of the receiver `receiver` of the machine `fingerprint`.
    Route {
        fingerprint: String,
        receiver: String,
        changes: Vec<RouteChange>,
    },
    /// A change on the network's devices (stage 1b): `op` subscribe, unsubscribe,
    /// rename_device, rename_rx or rename_tx, with `device`, `channel`, `tx_device`,
    /// `tx_channel`, `name`, `machine` as the op takes. Stage 2 (each offered once checked
    /// in the lab): set_latency, set_rate, set_encoding, set_aes67, set_preferred_leader,
    /// set_word_clock, identify, reboot, factory_reset, clear_config, with `value`; reboot
    /// and the resets with `confirm`, the device's current name.
    #[serde(rename = "network-devices/do")]
    NetworkDevicesDo(Map<String, Value>),
    /// A route between a network device and IDN through a machine's pool (stage 1c):
    /// `{dir: "in", device, tx_channel, fingerprint, receiver, ch}` or `{dir: "out", tx,
    /// tx_ch, device, channel}`.
    #[serde(rename = "network-devices/pool")]
    NetworkDevicesPool(Map<String, Value>),
    /// A site option: `scope` site, device, output or input (`target` for the last three).
    Option {
        scope: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        key: String,
        value: String,
    },
    /// A site option unset.
    Unset {
        scope: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        key: String,
    },
    /// The site password (every session ends).
    Password { password: String },
    /// A release sent to machines: `version`, to which (`to`: fingerprints; none: all),
    /// `when` "now" or "restart", `staged` (a rollout in steps). Machines whose IDN Node
    /// another product updates (their `update.managed_by`, idn-node.md §N.23) are left out
    /// of an IDN Media release, each refused in the job's `sent` with the reason.
    Update {
        version: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        to: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        when: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        staged: Option<bool>,
    },
    /// Stops a staged rollout.
    #[serde(rename = "update/stop")]
    UpdateStop {},
    /// A release deleted.
    #[serde(rename = "release/delete")]
    ReleaseDelete { version: String },
    /// The machine `fingerprint` back to its previous release.
    Rollback { fingerprint: String },
    /// The site's MQTT broker: on or off, and the address it listens on (either left out:
    /// as it is now).
    Mqtt {
        #[serde(skip_serializing_if = "Option::is_none")]
        on: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        listen: Option<String>,
    },
    /// A broker login added (with `password`) or removed (`remove`).
    MqttUser {
        username: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        password: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        remove: Option<bool>,
    },
    /// The metrics token: `action` "new" (answered once) or "off".
    MetricsToken { action: String },
    /// A routing preset: `action` save, recall or delete, by `name`.
    RoutingPreset { action: String, name: String },
    /// A template of settings saved (`settings`) or deleted (`delete`).
    Template {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        settings: Option<Map<String, Value>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        delete: Option<bool>,
    },
    /// A token for an engine (idn-node.md §N.31.5), for the user running it, as their role
    /// allows: `aud` (the engine, and the machine's fingerprint; none: any of the site's),
    /// `scope` read, control, `bus:<bus>` or `iem:<mix>`, `ttl_s` (at most 12 hours, the
    /// default). Answered `{token, scope, aud, iat, exp, jti}`.
    Token {
        aud: TokenAud,
        scope: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        ttl_s: Option<u64>,
    },
    /// A named, long-lived API key (admins): answered once with its `key`. `days`: its
    /// life (1 to 3650; none: 10 years): a key a client re-pairs for (a browser) can be
    /// shorter, and its revocation is forgotten once it would have ended anyway.
    ApiKey {
        name: String,
        aud: TokenAud,
        scope: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        days: Option<u32>,
    },
    /// Revokes a named API key: it stops on every machine at once.
    ApiKeyRevoke { id: String },
    /// The scheduled site backups (idn-node.md §N.42): the fields given. `passphrase` (at
    /// least 8 characters) is needed before `on`; `folder` a full path (empty: `backups`
    /// in the Site Manager's folder).
    BackupSchedule {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        every_hours: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        keep: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        folder: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        passphrase: Option<String>,
    },
    /// A scheduled-style backup now, into the schedule's folder.
    BackupNow {},
    /// A user: `action` "set" (with `role` admin, operator or viewer — left out: the
    /// user's own kept, a new user a viewer — and `password`, needed for a new one) or
    /// "remove".
    User {
        action: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        role: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        password: Option<String>,
    },
}

/// What a token is for: an engine (`desk`, `switcher`, `display`, `data`, `node`) and a
/// machine's fingerprint (none: any of the site's machines).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TokenAud {
    pub engine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
}

/// One channel's route: the receiver's channel `ch` (from 0) from the transmitter `tx`'s
/// channel `tx_ch` (`tx` empty: cleared).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RouteChange {
    pub ch: u16,
    pub tx: String,
    pub tx_ch: u16,
}

impl SiteCommand {
    /// Its path under `/api/v1/` (`route`, `network-devices/do`, …).
    pub fn path(&self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| v["cmd"].as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    /// Its JSON body (`{}` when none).
    pub fn body(&self) -> Value {
        serde_json::to_value(self)
            .ok()
            .map(|v| v["body"].clone())
            .filter(|b| !b.is_null())
            .unwrap_or_else(|| Value::Object(Map::new()))
    }
}

/// The JSON schema of [`SiteCommand`].
pub fn schema() -> Value {
    serde_json::to_value(schemars::schema_for!(SiteCommand)).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Every command is a path the API answers (the generic changes' and the explicit
    /// POST routes'), and back to the same JSON.
    #[test]
    fn site_commands_are_the_apis_posts() {
        // (Each a command's name in commands.rs, matched as `"name"`.)
        let src = include_str!("commands.rs");
        for (c, body) in [
            (
                SiteCommand::Pairing { seconds: Some(60) },
                json!({"seconds": 60}),
            ),
            (SiteCommand::Close {}, json!({})),
            (SiteCommand::Approve { id: 3, name: None }, json!({"id": 3})),
            (SiteCommand::JoinCode { cancel: None }, json!({})),
            (
                SiteCommand::JoinCode { cancel: Some(2) },
                json!({"cancel": 2}),
            ),
            (
                SiteCommand::Route {
                    fingerprint: "ab".into(),
                    receiver: "Desk".into(),
                    changes: vec![RouteChange {
                        ch: 0,
                        tx: "B (Mic)".into(),
                        tx_ch: 1,
                    }],
                },
                json!({"fingerprint": "ab", "receiver": "Desk", "changes": [{"ch": 0, "tx": "B (Mic)", "tx_ch": 1}]}),
            ),
            (
                SiteCommand::NetworkDevicesDo(
                    json!({"op": "unsubscribe", "device": "Box", "channel": 2})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
                json!({"op": "unsubscribe", "device": "Box", "channel": 2}),
            ),
            (SiteCommand::UpdateStop {}, json!({})),
            (
                SiteCommand::RouteCell {
                    receiver_id: "idn/ab/Desk".into(),
                    ch: 0,
                    tx_id: "Box (network audio)".into(),
                    tx_ch: 2,
                    on: None,
                },
                json!({"receiver_id": "idn/ab/Desk", "ch": 0, "tx_id": "Box (network audio)", "tx_ch": 2}),
            ),
            (
                SiteCommand::CopySettings {
                    from: "ab".into(),
                    to: vec!["cd".into()],
                    groups: vec!["video".into()],
                    pin: None,
                },
                json!({"from": "ab", "to": ["cd"], "groups": ["video"]}),
            ),
            (
                SiteCommand::ApplyTemplate {
                    name: "Booth".into(),
                    to: vec!["cd".into()],
                },
                json!({"name": "Booth", "to": ["cd"]}),
            ),
            (
                SiteCommand::Token {
                    aud: TokenAud {
                        engine: "desk".into(),
                        machine: None,
                    },
                    scope: "read".into(),
                    ttl_s: Some(600),
                },
                json!({"aud": {"engine": "desk"}, "scope": "read", "ttl_s": 600}),
            ),
            (
                SiteCommand::Option {
                    scope: "site".into(),
                    target: None,
                    key: "audio.rate".into(),
                    value: "96000".into(),
                },
                json!({"scope": "site", "key": "audio.rate", "value": "96000"}),
            ),
        ] {
            assert_eq!(c.body(), body, "{c:?}");
            let p = format!("\"{}\"", c.path());
            assert!(src.contains(&p), "{p} is not a command");
            let back: SiteCommand =
                serde_json::from_value(json!({"cmd": c.path(), "body": body})).unwrap();
            assert_eq!(back, c);
        }
        let s = schema().to_string();
        for name in [
            "pairing",
            "join_code",
            "route",
            "network-devices/pool",
            "user",
            "update/stop",
            "route_cell",
            "copy_settings",
            "apply_template",
        ] {
            assert!(s.contains(name), "{name} not in the schema");
        }
        // Every command's path is one a role's capabilities list (crate::users::POST_PATHS).
        fn cmds(v: &Value, out: &mut Vec<String>) {
            match v {
                Value::Object(o) => {
                    if let Some(c) = o.get("properties").and_then(|p| p.get("cmd")) {
                        for x in c["enum"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .chain(c.get("const"))
                        {
                            out.extend(x.as_str().map(str::to_owned));
                        }
                    }
                    o.values().for_each(|x| cmds(x, out));
                }
                Value::Array(a) => a.iter().for_each(|x| cmds(x, out)),
                _ => {}
            }
        }
        let mut paths = Vec::new();
        cmds(&schema(), &mut paths);
        assert!(paths.len() >= 25, "{paths:?}");
        for p in &paths {
            assert!(
                crate::users::POST_PATHS.contains(&p.as_str()),
                "{p} not in POST_PATHS"
            );
        }
    }
}

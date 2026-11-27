//! The host's typed view of the Site Manager ([`crate::host::Handle`]): the site's events
//! as they happen ([`SiteEvent`], from [`Handle::events`](crate::host::Handle::events)), who
//! a change is made for ([`Caller`]), and each machine's health ([`DeviceHealth`]).

use idn_common::sync::LockOk as _;
use idn_open_protocol::trust::{Authority, NodeReport};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// Who a change made through the host is for, as the audit writes it (its `peer` and
/// `client`): the address the host's user came from and the client they used (the host's
/// browser page, its app, a surface).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Caller {
    /// Their address (`192.168.1.20`), as the host saw it; empty: none.
    pub address: String,
    /// The client's name for people (`Host web`, `Host iPad`); empty: none.
    pub client: String,
}

/// Something that happened in the site, typed (idn-node.md §N.41): what the Site Manager's
/// push channel sends as `{"type": "event", "event": NAME, …}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SiteEvent {
    /// A device asks to join: show its code, offer approve and deny (`id` for them).
    JoinRequested {
        id: u64,
        name: String,
        kind: String,
        fingerprint: String,
        /// The code it shows, to compare.
        code: String,
        address: Option<String>,
        /// It typed a join code and waits for approval as well.
        by_code: bool,
    },
    /// A request withdrawn (the device went away, the window closed) or answered.
    JoinCancelled {
        id: u64,
    },
    /// A device joined the site (approved, or by code).
    DeviceJoined {
        fingerprint: String,
        name: String,
        kind: String,
    },
    /// A request denied.
    JoinDenied {
        fingerprint: String,
        name: String,
    },
    /// A denied or removed device asked again and was refused.
    JoinRefused {
        fingerprint: String,
        name: String,
    },
    /// A device approved by code withdrew (its person didn't confirm the code on it).
    JoinWithdrawn {
        fingerprint: String,
        name: String,
    },
    /// A device removed from the site.
    DeviceRemoved {
        fingerprint: String,
        name: String,
    },
    /// A device connected to the site, or lost its connection.
    DeviceOnline {
        fingerprint: String,
        name: String,
    },
    DeviceOffline {
        fingerprint: String,
        name: String,
    },
    /// A join code typed on a device: used up.
    CodeUsed {
        id: u64,
        fingerprint: String,
        name: String,
    },
    /// A join code that ran out of time unused.
    CodeExpired {
        id: u64,
    },
    /// A join code that can't be used any more: its tries used up, or a newer one took its
    /// place (`replaced`).
    CodeBurned {
        id: u64,
        replaced: bool,
    },
    /// A wrong join code typed.
    CodeWrong {
        name: String,
        address: Option<String>,
        tries_left: u32,
    },
    /// A client pairing at another machine of the site (a client app): its request
    /// relayed here, waiting for the host's answer
    /// ([`Handle::key_requests`](crate::host::Handle::key_requests),
    /// [`approve_key_request`](crate::host::Handle::approve_key_request)). `pin`: it typed
    /// one (the PIN itself is in `key_requests`, not on the push channel).
    KeyRequested {
        id: u64,
        /// The client typed the pairing code this site gave the relay (checked here).
        code_ok: bool,
        client: String,
        via: String,
        via_fingerprint: String,
        pin: bool,
    },
    /// A relayed pairing gone unanswered: its machine disconnected, or 10 minutes passed.
    KeyRequestCancelled {
        id: u64,
    },
    /// A machine left the site by itself (it may join again).
    DeviceLeft {
        fingerprint: String,
        name: String,
    },
    /// Any other typed event: a machine needing attention (and `ok` when that's over), a
    /// version mismatch, a machine's own events passed on by its Node (§N.41). Its name and
    /// fields as the push channel sends them.
    Other {
        name: String,
        fields: Value,
    },
}

/// One event: its place (ask from here next), when (seconds since 1970), its level
/// (`error`, `warning`, `info`, `ok`), its text for people, and what it is.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SiteEventEntry {
    pub seq: u64,
    pub at: u64,
    pub level: String,
    pub text: String,
    pub event: SiteEvent,
}

/// What [`Handle::events`](crate::host::Handle::events) gives: the events after the place
/// asked from, the place to ask from next, and whether some were missed (only the newest
/// 500 are kept: read the whole state again, then go on from `next`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SiteEvents {
    pub events: Vec<SiteEventEntry>,
    pub next: u64,
    pub missed: bool,
}

impl SiteEvents {
    pub(crate) fn of((list, next, missed): (Vec<(u64, Value)>, u64, bool)) -> SiteEvents {
        SiteEvents {
            events: list.into_iter().map(|(seq, v)| entry(seq, &v)).collect(),
            next,
            missed,
        }
    }
}

/// A push-channel event (`{"type": "event", "event", "level", "text", "at", …}`) typed.
pub(crate) fn entry(seq: u64, v: &Value) -> SiteEventEntry {
    let s = |k: &str| v[k].as_str().unwrap_or("").to_owned();
    let n = |k: &str| v[k].as_u64().unwrap_or(0);
    let opt = |k: &str| v[k].as_str().filter(|x| !x.is_empty()).map(str::to_owned);
    let (fp, name) = (s("fingerprint"), s("machine"));
    let event = match v["event"].as_str().unwrap_or("") {
        "join_requested" => SiteEvent::JoinRequested {
            id: n("id"),
            name,
            kind: s("kind"),
            fingerprint: fp,
            code: s("code"),
            address: opt("address"),
            by_code: v["by_code"] == true,
        },
        "join_cancelled" => SiteEvent::JoinCancelled { id: n("id") },
        "device_joined" => SiteEvent::DeviceJoined {
            fingerprint: fp,
            name,
            kind: s("kind"),
        },
        "join_denied" => SiteEvent::JoinDenied {
            fingerprint: fp,
            name,
        },
        "join_refused" => SiteEvent::JoinRefused {
            fingerprint: fp,
            name,
        },
        "join_withdrawn" => SiteEvent::JoinWithdrawn {
            fingerprint: fp,
            name,
        },
        "device_revoked" => SiteEvent::DeviceRemoved {
            fingerprint: fp,
            name,
        },
        "device_online" => SiteEvent::DeviceOnline {
            fingerprint: fp,
            name,
        },
        "device_offline" => SiteEvent::DeviceOffline {
            fingerprint: fp,
            name,
        },
        "join_code_used" => SiteEvent::CodeUsed {
            id: n("id"),
            fingerprint: fp,
            name,
        },
        "join_code_expired" => SiteEvent::CodeExpired { id: n("id") },
        "join_code_burned" => SiteEvent::CodeBurned {
            id: n("id"),
            replaced: v["why"] == "replaced",
        },
        "key_requested" => SiteEvent::KeyRequested {
            id: n("id"),
            code_ok: v["code_ok"] == true,
            client: s("client"),
            via: name,
            via_fingerprint: fp,
            pin: v["pin"] == true,
        },
        "key_request_cancelled" => SiteEvent::KeyRequestCancelled { id: n("id") },
        "device_left" => SiteEvent::DeviceLeft {
            fingerprint: fp,
            name,
        },
        "join_code_wrong" => SiteEvent::CodeWrong {
            name,
            address: opt("address"),
            tries_left: n("tries_left") as u32,
        },
        other => {
            let mut fields = v.clone();
            if let Some(o) = fields.as_object_mut() {
                for k in ["type", "event", "level", "text", "at"] {
                    o.remove(k);
                }
            }
            SiteEvent::Other {
                name: other.to_owned(),
                fields,
            }
        }
    };
    SiteEventEntry {
        seq,
        at: n("at"),
        level: s("level"),
        text: s("text"),
        event,
    }
}

/// A named API key, as the Site Manager lists it ([`Handle::api_keys`](crate::host::Handle::api_keys)).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct ApiKeyInfo {
    pub id: String,
    pub name: String,
    /// Its audience: the engine (`app:example`, `desk`, …) and machine (`*`: any).
    pub engine: String,
    pub machine: String,
    pub scope: String,
    /// Made (Unix seconds), by whom (the user, or `user (via machine)` for a relayed
    /// pairing), and when it ends.
    pub created: u64,
    pub by: String,
    pub expires: u64,
    pub revoked: bool,
    pub revoked_at: Option<u64>,
    /// When and on which machine it was last used, where told.
    pub last_used: Option<u64>,
    pub last_machine: Option<String>,
}

impl ApiKeyInfo {
    pub(crate) fn of(k: &Value) -> ApiKeyInfo {
        let s = |x: &Value| x.as_str().unwrap_or("").to_owned();
        ApiKeyInfo {
            id: s(&k["id"]),
            name: s(&k["name"]),
            engine: s(&k["aud"]["engine"]),
            machine: s(&k["aud"]["machine"]),
            scope: s(&k["scope"]),
            created: k["created"].as_u64().unwrap_or(0),
            by: s(&k["by"]),
            expires: crate::tokens::key_expires(k),
            revoked: k["revoked"] == true,
            revoked_at: k["revoked_at"].as_u64(),
            last_used: k["last_used"].as_u64(),
            last_machine: k["last_machine"].as_str().map(str::to_owned),
        }
    }
}

/// A problem a machine has now (§N.41.4): its name (`offline`, `clock_unlocked`,
/// `version_mismatch`, `service_failed`, …), level and text; its fields as the state
/// gives them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeviceProblem {
    pub problem: String,
    pub level: String,
    pub text: String,
    pub fields: Value,
}

/// One of a machine's services, as its Node last reported it: what
/// [`SiteCommand::Restart`](crate::typed::SiteCommand::Restart) takes as `service`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct DeviceService {
    /// Its id (`Restart`'s `service`).
    pub id: String,
    /// Its name for people (`"Display"`, `"Switcher"`, …).
    pub name: String,
    /// `running`, `starting`, `restarting`, `failed`, `off`, `unset` (not set up yet),
    /// `unsupported` (not on this machine) or `no_session` (runs in a log-in session, and
    /// none is there).
    pub state: String,
}

impl DeviceService {
    /// The services in a Node's report (its `service` elements).
    pub(crate) fn from_report(r: &idn_open_protocol::xml::Element) -> Vec<DeviceService> {
        r.children_named("service")
            .filter_map(|x| {
                let id = x.attr("id").filter(|v| !v.is_empty())?;
                Some(DeviceService {
                    id: id.to_owned(),
                    name: x
                        .attr("title")
                        .filter(|v| !v.is_empty())
                        .unwrap_or(id)
                        .to_owned(),
                    state: x.attr("state").unwrap_or("").to_owned(),
                })
            })
            .collect()
    }
}

/// What machine it is, as its Node last reported it ([`DeviceHealth::machine`]): what
/// the machine's system doesn't say is empty or `None`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[non_exhaustive]
pub struct DeviceMachine {
    /// The CPU's model name, e.g. `AMD Ryzen 7 7840HS w/ Radeon 780M Graphics`.
    pub cpu: String,
    /// Logical CPUs (hardware threads).
    pub cpus: Option<u32>,
    /// Its GPUs' adapter names, e.g. `["NVIDIA GeForce RTX 3080"]`.
    pub gpus: Vec<String>,
    /// The operating system, e.g. `Ubuntu`, `Windows 11 Pro`, `macOS`.
    pub os: String,
    /// Its version, e.g. `24.04`, `24H2`, `15.1`.
    pub os_version: String,
    /// The kernel's release (Linux, macOS) or the build (Windows, `26100.2314`).
    pub kernel: String,
    /// When the machine started (seconds since 1970).
    pub boot_time: Option<u64>,
    /// Its memory, in bytes.
    pub memory_total: Option<u64>,
    /// Its memory available when its Node last reported, in bytes (not on macOS).
    pub memory_free: Option<u64>,
}

impl DeviceMachine {
    /// From a Node's report (its `machine` element; `None` from a Node older than this).
    pub(crate) fn from_report(r: &idn_open_protocol::xml::Element) -> Option<DeviceMachine> {
        let m = r.child("machine")?;
        let s = |k: &str| m.attr(k).unwrap_or("").to_owned();
        let n = |k: &str| m.attr(k).and_then(|v| v.parse::<u64>().ok());
        Some(DeviceMachine {
            cpu: s("cpu"),
            cpus: m.attr("cpus").and_then(|v| v.parse().ok()),
            gpus: m
                .children_named("gpu")
                .filter_map(|g| g.attr("name").filter(|v| !v.is_empty()).map(str::to_owned))
                .collect(),
            os: s("os"),
            os_version: s("os_version"),
            kernel: s("kernel"),
            boot_time: n("boot_time"),
            memory_total: n("mem_total"),
            memory_free: n("mem_free"),
        })
    }
}

/// A machine's health, typed ([`Handle::device_health`](crate::host::Handle::device_health)).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct DeviceHealth {
    pub fingerprint: String,
    /// Its name now (its Node's, else as enrolled).
    pub name: String,
    /// `output`, `input` or `app`.
    pub kind: String,
    /// Connected to the site now.
    pub online: bool,
    /// A pairing relay ([`SiteCommand::Relay`](crate::typed::SiteCommand::Relay)).
    pub relay: bool,
    /// When its Node last reported (seconds since 1970): kept after it goes offline;
    /// `None` when no Node has reported since the Site Manager started (an app enrolled
    /// by itself has none).
    pub last_seen: Option<u64>,
    /// The build its Node runs (`0.2.0-Alpha92 …`), as last reported.
    pub version: Option<String>,
    /// When its Node started (seconds since 1970), as last reported; `None` when no Node
    /// has reported. With [`DeviceMachine::boot_time`]: restarted, and when.
    pub node_started: Option<u64>,
    /// What machine it is (CPU, GPUs, system, start, memory), as its Node last reported;
    /// `None` when no Node has reported, or only a Node older than this field.
    pub machine: Option<DeviceMachine>,
    /// Its services stopped and starting again now (ids), as its Node last reported.
    pub restarting: Vec<String>,
    /// Its services, as its Node last reported them (empty: no Node has reported).
    pub services: Vec<DeviceService>,
    /// An update running on it: its state (`downloading`, `staged`, `installing`,
    /// `checking`), else `None`.
    pub updating: Option<String>,
    /// The product that installs and updates its IDN Node itself (`Example Player`), as its Node
    /// last reported (idn-node.md §N.23): the Site Manager's IDN Media updates aren't sent
    /// to it. `None`: the Site Manager's updates.
    pub update_managed_by: Option<String>,
    /// What needs attention on it (empty: nothing): the same roll-up as the state's
    /// `attention`.
    pub attention: Vec<DeviceProblem>,
}

/// Each machine's last report kept (by fingerprint): when it came (seconds since 1970) and
/// the report, so a machine gone offline still has its last-seen time and build.
#[derive(Default)]
pub(crate) struct LastSeen(Mutex<BTreeMap<String, (u64, idn_open_protocol::xml::Element)>>);

impl LastSeen {
    /// The reports now taken in.
    pub(crate) fn note(&self, reports: &[NodeReport]) {
        let now = idn_common::epoch::now();
        let mut g = self.0.lock_ok();
        for r in reports {
            // (Its arrival, rounded once: the same at every look; bug run 2026-11-24.)
            let at = now.saturating_sub(r.age).as_secs();
            g.insert(r.fingerprint.clone(), (at, r.report.clone()));
        }
    }

    /// Every enrolled machine's health: from its report now, or the last one kept.
    pub(crate) fn health(&self, a: &Authority, job: &Value) -> Vec<DeviceHealth> {
        let reports = a.node_reports();
        self.note(&reports);
        let devices = a.devices();
        let roll =
            crate::attention::rollup(&devices, &reports, &crate::attention::own_version(), job);
        let kept = self.0.lock_ok();
        devices
            .into_iter()
            .filter(|d| !d.revoked)
            .map(|d| {
                let last = kept.get(&d.fingerprint);
                let rep = last.map(|(_, r)| r);
                let attr = |k: &str| {
                    rep.and_then(|r| r.attr(k))
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned)
                };
                let services = rep.map_or_else(Vec::new, DeviceService::from_report);
                let restarting = services
                    .iter()
                    .filter(|s| s.state == "restarting")
                    .map(|s| s.id.clone())
                    .collect();
                let updating = rep
                    .and_then(|r| r.child("update"))
                    .and_then(|u| u.attr("state"))
                    .filter(|s| matches!(*s, "downloading" | "staged" | "installing" | "checking"))
                    .map(str::to_owned);
                let update_managed_by = rep.and_then(crate::updates::managed_by_of);
                let attention = roll["attention"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|m| m["fingerprint"] == d.fingerprint.as_str())
                    .flat_map(|m| m["problems"].as_array().cloned().unwrap_or_default())
                    .map(|p| {
                        let mut fields = p.clone();
                        if let Some(o) = fields.as_object_mut() {
                            for k in ["problem", "level", "text"] {
                                o.remove(k);
                            }
                        }
                        DeviceProblem {
                            problem: p["problem"].as_str().unwrap_or("").to_owned(),
                            level: p["level"].as_str().unwrap_or("").to_owned(),
                            text: p["text"].as_str().unwrap_or("").to_owned(),
                            fields,
                        }
                    })
                    .collect();
                // The Node's start: the report's time less its uptime then.
                let node_started = last.and_then(|(at, r)| {
                    let up = r.attr("uptime_s")?.parse::<u64>().ok()?;
                    Some(at.saturating_sub(up))
                });
                DeviceHealth {
                    name: attr("name").unwrap_or_else(|| d.name.clone()),
                    version: attr("build"),
                    node_started,
                    machine: rep.and_then(DeviceMachine::from_report),
                    last_seen: last.map(|(at, _)| *at),
                    fingerprint: d.fingerprint,
                    kind: d.kind,
                    online: d.online,
                    relay: d.relay,
                    restarting,
                    services,
                    updating,
                    update_managed_by,
                    attention,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_nodes_services_from_its_report() {
        use idn_open_protocol::xml::Element;
        let svc = |id: &str, title: &str, state: &str| {
            Element::new("service")
                .with_attr("id", id)
                .with_attr("title", title)
                .with_attr("state", state)
        };
        let r = Element::new("idn_node_report")
            .with_attr("name", "Booth")
            .with_child(svc("display", "Display", "running"))
            .with_child(svc("switcher", "", "restarting"))
            .with_child(Element::new("audio"))
            .with_child(svc("", "No id", "running"));
        let s = DeviceService::from_report(&r);
        assert_eq!(
            s,
            vec![
                DeviceService {
                    id: "display".into(),
                    name: "Display".into(),
                    state: "running".into()
                },
                // No title: its id for people too. No id: left out (nothing to restart).
                DeviceService {
                    id: "switcher".into(),
                    name: "switcher".into(),
                    state: "restarting".into()
                },
            ]
        );
    }

    #[test]
    fn a_nodes_machine_from_its_report() {
        use idn_open_protocol::xml::{Element, parse};
        let r = parse(
            r#"<idn_node_report v="1" uptime_s="60"><machine cpu="AMD Ryzen 7 7840HS" cpus="16"
                os="Ubuntu" os_version="24.04" kernel="6.8.0" boot_time="1700000000"
                mem_total="32000000000" mem_free="20000000000"><gpu name="AMD Phoenix1"/>
                <gpu name=""/></machine></idn_node_report>"#,
        )
        .unwrap();
        let m = DeviceMachine::from_report(&r).expect("its machine");
        assert_eq!(
            m,
            DeviceMachine {
                cpu: "AMD Ryzen 7 7840HS".into(),
                cpus: Some(16),
                gpus: vec!["AMD Phoenix1".into()],
                os: "Ubuntu".into(),
                os_version: "24.04".into(),
                kernel: "6.8.0".into(),
                boot_time: Some(1_700_000_000),
                memory_total: Some(32_000_000_000),
                memory_free: Some(20_000_000_000),
            }
        );
        // A report without `<machine>`: none; one that says little: what it says.
        assert_eq!(
            DeviceMachine::from_report(&Element::new("idn_node_report")),
            None
        );
        let bare = Element::new("idn_node_report").with_child(Element::new("machine"));
        assert_eq!(
            DeviceMachine::from_report(&bare),
            Some(DeviceMachine::default())
        );
    }

    #[test]
    fn push_channel_events_typed() {
        let e = entry(
            7,
            &json!({"type": "event", "event": "join_requested", "level": "info",
                "text": "x asks", "at": 5, "id": 3, "machine": "Stage PC", "kind": "output",
                "fingerprint": "ab", "code": "12-34", "address": "10.0.0.2", "by_code": false}),
        );
        assert_eq!((e.seq, e.at, e.level.as_str()), (7, 5, "info"));
        assert_eq!(
            e.event,
            SiteEvent::JoinRequested {
                id: 3,
                name: "Stage PC".into(),
                kind: "output".into(),
                fingerprint: "ab".into(),
                code: "12-34".into(),
                address: Some("10.0.0.2".into()),
                by_code: false,
            }
        );
        let e = entry(
            8,
            &json!({"event": "join_code_burned", "id": 4, "why": "replaced"}),
        );
        assert_eq!(
            e.event,
            SiteEvent::CodeBurned {
                id: 4,
                replaced: true
            }
        );
        let e = entry(
            9,
            &json!({"type": "event", "event": "clock_unlocked", "level": "warning",
                "text": "t", "at": 1, "fingerprint": "cd"}),
        );
        assert_eq!(
            e.event,
            SiteEvent::Other {
                name: "clock_unlocked".into(),
                fields: json!({"fingerprint": "cd"})
            }
        );
    }
}

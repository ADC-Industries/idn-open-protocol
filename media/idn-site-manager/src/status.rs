//! What the panel shows: the site's status, and the routing matrix's data
//! (idn-routing.md §A.6) with the Discovery Server's sources.

use super::*;

/// The site's status (`GET /api/v1/status`), typed: its schema is
/// `GET /api/v1/status_schema` (refactor 4).
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct SiteStatus {
    /// Moves when anything in the status changes (`GET /api/v1/state?after=VERSION`).
    pub version: u64,
    /// Who asked, and what their role lets them change (a UI offers only those).
    pub me: Me,
    /// The site's MQTT broker's settings (§N.34; null: none).
    pub mqtt: serde_json::Value,
    pub site: SiteInfo,
    pub machines: Vec<MachineStatus>,
    /// Join requests waiting.
    pub pending: Vec<Pending>,
    pub options: OptionsView,
    /// The routing matrix's data (idn-routing.md §A.6).
    pub audio: serde_json::Value,
    pub templates: serde_json::Value,
    pub routing_presets: Vec<PresetSummary>,
    /// Site monitoring's streams (N5).
    pub streams: serde_json::Value,
    pub metrics: Metrics,
    /// Releases and their rollout (§N.23).
    pub updates: serde_json::Value,
    /// What needs attention, the versions, the rollout (§N.41.4).
    pub attention: serde_json::Value,
    pub versions: serde_json::Value,
    pub rollout: serde_json::Value,
    /// Scheduled backups (§N.42; never the passphrase).
    pub backups: serde_json::Value,
    /// The panel's log, newest last.
    pub log: Vec<String>,
}

/// The caller.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct Me {
    /// Their user name ("": not logged in).
    pub user: String,
    /// `viewer`, `operator` or `admin` ("": none).
    pub role: String,
    /// The commands their role may run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<&'static str>>,
    /// The commands they may run only in part (by the body), and which parts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities_limited: Option<serde_json::Value>,
}

/// The site itself.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct SiteInfo {
    /// This machine's host name.
    pub host: String,
    /// The site CA's fingerprint.
    pub ca: String,
    /// The Discovery Server's port.
    pub port: u16,
    pub pairing_open: bool,
    /// `approval`, `code_or_approval` or `code_and_approval` (§E.10.10).
    pub join_mode: String,
    /// The join codes waiting: their expiry and tries, never the codes.
    pub join_codes: Vec<JoinCodeView>,
    pub build: String,
}

/// A join code waiting (never the code itself).
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct JoinCodeView {
    pub id: u64,
    pub expires: u64,
    pub tries_left: u32,
    pub issuer: String,
}

/// A machine of the site.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct MachineStatus {
    pub fingerprint: String,
    /// Its name now (its Node's, else as enrolled).
    pub name: String,
    pub enrolled_as: String,
    pub kind: String,
    pub admin: bool,
    pub revoked: bool,
    pub online: bool,
    /// Its Node's last report; null without one.
    pub node: Option<NodeView>,
}

/// A machine's Node as its last report says.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct NodeView {
    pub build: String,
    pub uptime_s: Option<u64>,
    pub cpu_percent: Option<f64>,
    /// The machine's GPU busy, % (where its driver says).
    pub gpu_percent: Option<f64>,
    /// Seconds since its last report.
    pub last_seen_s: u64,
    pub name: String,
    pub interface: String,
    pub require: bool,
    pub lan_panel: bool,
    pub lan_port: u16,
    pub state: String,
    pub address: String,
    pub video: VideoView,
    pub clock: ClockView,
    /// Its network links (N5, §N.30).
    pub links: Vec<LinkView>,
    /// The machine's own rate (empty: the site's), and the one in use (§N.40).
    pub audio_rate: String,
    pub rate: RateView,
    pub settings: serde_json::Value,
    pub programs: Vec<ProgramView>,
    /// Its update (§N.23, §N.42): `state`, `version`, `progress`, `error` while one runs,
    /// the site's newest release for it when it asked (`available`, `checked_at`), and the
    /// product that updates it instead of the Site Manager (`managed_by`, with
    /// `managed_by_source` `package` or `setting`; absent: none).
    pub update: serde_json::Value,
}

/// A Node's video settings, as reported (text).
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct VideoView {
    pub threads: String,
    pub parallel_decode: String,
    pub encoding: String,
    pub decoding: String,
    pub ndi: String,
    pub precision: String,
    pub processor: String,
    pub format: String,
}

/// A Node's site clock, as reported (text).
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct ClockView {
    pub state: String,
    pub locked: String,
    pub gm: String,
    pub offset_ns: String,
    pub delay_ns: String,
    pub jitter_ns: String,
    pub domain: String,
    pub protocol: String,
    pub enabled: String,
    pub ports: String,
    pub error: String,
}

/// A network link of a machine.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct LinkView {
    pub name: String,
    pub addr: String,
    pub up: bool,
    pub physical: bool,
    pub speed_mbps: Option<u64>,
}

/// The audio rate in use and where it comes from.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct RateView {
    pub hz: Option<u32>,
    pub from: String,
}

/// A program on a machine and its registrations.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct ProgramView {
    pub app: String,
    pub registrations: Vec<RegView>,
}

/// One of a program's registrations.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct RegView {
    pub kind: String,
    pub name: String,
    pub status: String,
}

/// A join request waiting.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct Pending {
    pub id: u64,
    pub name: String,
    pub kind: String,
    pub code: String,
    pub fingerprint: String,
    pub address: String,
    pub by_code: bool,
    /// When it asked (seconds since 1970).
    pub asked_at: u64,
}

/// The site's options.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct OptionsView {
    pub version: u64,
    pub entries: Vec<OptionEntry>,
}

/// One option: its scope (`site`, `device`, `output`, `input`), target, key and value.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct OptionEntry {
    pub scope: String,
    pub target: String,
    pub key: String,
    pub value: String,
}

/// A saved routing preset, summed up.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct PresetSummary {
    pub name: serde_json::Value,
    pub saved: serde_json::Value,
    pub receivers: usize,
}

/// Site metrics' endpoint.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct Metrics {
    pub on: bool,
}

/// How long a status made is given again ([`State::site_part`]).
const STATUS_FRESH: Duration = Duration::from_millis(250);

/// [`SiteStatus`]'s JSON schema.
pub fn status_schema() -> serde_json::Value {
    schemars::schema_for!(SiteStatus).to_value()
}

impl State {
    /// The status for `user` (with `role`), as JSON: the site's part
    /// ([`State::site_part`]) with who asks.
    pub(super) fn status_for(&self, user: &str, role: Option<users::Role>) -> serde_json::Value {
        let (mut v, _) = self.site_part();
        let (all, limited) = role.map(users::capabilities).unzip();
        v["me"] = serde_json::to_value(Me {
            user: user.to_owned(),
            role: role.map_or("", users::Role::as_str).to_owned(),
            capabilities: all,
            capabilities_limited: limited,
        })
        .unwrap_or_default();
        v
    }

    /// The status's version now.
    pub(super) fn status_version_now(&self) -> u64 {
        self.site_part().1
    }

    /// The site's part of the status (`me` empty) and its version (moved on when anything
    /// in it changed since the last look): made again at most every [`STATUS_FRESH`] (every
    /// push channel and long poll asks; building it reads every machine's report), and
    /// after any change ([`State::status_changed`]).
    fn site_part(&self) -> (serde_json::Value, u64) {
        let mut g = self.status_cache.lock_ok();
        if let Some((at, v, ver)) = g.as_ref()
            && at.elapsed() < STATUS_FRESH
        {
            return (v.clone(), *ver);
        }
        let s = self.site_status();
        let text = serde_json::to_string(&s).unwrap_or_default();
        let ver = self.status_version(&text);
        let mut v = serde_json::to_value(&s).unwrap_or_default();
        v["version"] = ver.into();
        *g = Some((std::time::Instant::now(), v.clone(), ver));
        (v, ver)
    }

    /// Something changed: the next status is made afresh.
    pub(super) fn status_changed(&self) {
        *self.status_cache.lock_ok() = None;
    }

    /// The status's version: moved on when `text` (the status without it) differs from
    /// the last.
    fn status_version(&self, text: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut h);
        let h = h.finish();
        let mut g = self.status_seen.lock_ok();
        if g.1 != h {
            *g = (g.0 + 1, h);
        }
        g.0
    }

    /// The status, typed (its `version` 0 and `me` empty: [`State::status_for`] sets them).
    pub(super) fn site_status(&self) -> SiteStatus {
        let a = &self.authority;
        let reports = a.node_reports();
        let machines = a
            .devices()
            .into_iter()
            .map(|d| {
                let node = reports.iter().find(|r| r.fingerprint == d.fingerprint);
                MachineStatus {
                    fingerprint: d.fingerprint.clone(),
                    // The Node's current name when it reports one (a rename from the panel
                    // doesn't change the enrolment record).
                    name: node
                        .and_then(|n| n.report.attr("name"))
                        .filter(|n| !n.is_empty())
                        .unwrap_or(&d.name)
                        .to_owned(),
                    enrolled_as: d.name.clone(),
                    kind: d.kind.clone(),
                    admin: d.admin,
                    revoked: d.revoked,
                    online: d.online,
                    node: node.map(node_view),
                }
            })
            .collect();
        let pending = a
            .pending()
            .into_iter()
            .map(|p| Pending {
                id: p.id,
                name: p.name.clone(),
                kind: p.kind.clone(),
                code: p.code.clone(),
                fingerprint: p.fingerprint.clone(),
                address: p.address.map(|x| x.to_string()).unwrap_or_default(),
                by_code: p.by_code,
                asked_at: p.asked_at,
            })
            .collect();
        let o = a.options();
        let entries = o
            .entries()
            .into_iter()
            .map(|(scope, key, value)| {
                let (sc, target) = match scope {
                    OptionScope::Site => ("site", String::new()),
                    OptionScope::Device(fp) => ("device", fp),
                    OptionScope::Output(n) => ("output", n),
                    OptionScope::Input(n) => ("input", n),
                };
                OptionEntry {
                    scope: sc.into(),
                    target,
                    key,
                    value,
                }
            })
            .collect();
        // What needs attention (§N.41.4): the machines' problems, the versions, the rollout.
        let attention = crate::attention::rollup(
            &a.devices(),
            &reports,
            &crate::attention::own_version(),
            &self.updates.state()["job"],
        );
        SiteStatus {
            site: SiteInfo {
                host: idn_open_protocol::discovery::hostname(),
                ca: a.ca_fingerprint(),
                port: self.ds_port,
                pairing_open: a.pairing_open(),
                join_mode: a.join_mode().as_str().to_owned(),
                join_codes: a
                    .join_codes()
                    .iter()
                    .map(|c| JoinCodeView {
                        id: c.id,
                        expires: c.expires,
                        tries_left: c.tries_left,
                        issuer: c.issuer.clone(),
                    })
                    .collect(),
                build: idn_open_protocol::build_line(),
            },
            machines,
            pending,
            options: OptionsView {
                version: o.version,
                entries,
            },
            audio: self.audio_value(&reports),
            // (A template's PIN as "pin_set", never its hash: the status is any role's,
            // and a short PIN's hash falls to an offline search; bug run 2026-11-25.)
            templates: serde_json::Value::Array(
                self.templates()
                    .into_iter()
                    .map(|mut t| {
                        if let Some(o) = t["settings"].as_object_mut()
                            && let Some(h) = o.remove("panel_pin")
                        {
                            o.insert(
                                "pin_set".into(),
                                serde_json::Value::Bool(h.as_str().is_some_and(|h| !h.is_empty())),
                            );
                        }
                        t
                    })
                    .collect(),
            ),
            routing_presets: self
                .presets()
                .iter()
                .map(|p| PresetSummary {
                    name: p["name"].clone(),
                    saved: p["saved"].clone(),
                    receivers: p["receivers"].as_array().map_or(0, Vec::len),
                })
                .collect(),
            streams: self.watch.streams_json(),
            metrics: Metrics {
                on: self.dir.join(METRICS_TOKEN).exists(),
            },
            updates: self.updates.state(),
            attention: attention["attention"].clone(),
            versions: attention["versions"].clone(),
            rollout: attention["rollout"].clone(),
            backups: self.backups_state(),
            log: self.log.lock_ok().iter().cloned().collect(),
            version: 0,
            me: Me {
                user: String::new(),
                role: String::new(),
                capabilities: None,
                capabilities_limited: None,
            },
            mqtt: self
                .mqtt
                .as_ref()
                .map_or(serde_json::Value::Null, |m| m.settings()),
        }
    }

    /// The routing matrix's data (idn-routing.md §A.6): [`audio_json`] with the Discovery
    /// Server's sources.
    pub(super) fn audio_value(
        &self,
        reports: &[idn_open_protocol::trust::NodeReport],
    ) -> serde_json::Value {
        audio_json(reports, &self.ds_sources())
    }

    /// The Discovery Server's sources, asked for at most every 5 s.
    pub(super) fn ds_sources(&self) -> Vec<String> {
        let mut c = self.sources.lock_ok();
        if let Some((at, v)) = c.as_ref()
            && at.elapsed() < Duration::from_secs(5)
        {
            return v.clone();
        }
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], self.ds_port));
        let v: Vec<String> =
            idn_open_protocol::discovery::ds::query_sources(addr, Duration::from_millis(500))
                .map(|l| l.into_iter().map(|s| s.name).collect())
                .unwrap_or_default();
        *c = Some((std::time::Instant::now(), v.clone()));
        v
    }
}

/// The routing matrix's data (idn-routing.md §A.6): transmitters (from the Nodes'
/// reports, then the Discovery Server's other `sources` as 2-channel guesses) and
/// receivers with their routes.
pub(super) fn audio_json(
    reports: &[idn_open_protocol::trust::NodeReport],
    sources: &[String],
) -> serde_json::Value {
    use serde_json::{Value, json};
    let labels = |e: &idn_open_protocol::xml::Element| -> Vec<String> {
        idn_open_protocol::node::routing::parse_labels(e.attr("labels"))
    };
    let machine =
        |r: &idn_open_protocol::trust::NodeReport| r.report.attr("name").unwrap_or("").to_owned();
    let mut txs: Vec<Value> = Vec::new();
    let mut known: std::collections::BTreeSet<String> = Default::default();
    let mut rxs: Vec<Value> = Vec::new();
    for r in reports {
        for t in r.report.children_named("audio_tx") {
            let name = t.attr("name").unwrap_or("").to_owned();
            known.insert(name.clone());
            txs.push(json!({
                "name": name,
                "channels": t.attr("channels").and_then(|c| c.parse::<u16>().ok()).unwrap_or(2),
                "labels": labels(t),
                "machine": machine(r),
                "known": true,
                // Where its sound comes from, as its program said (§A.16).
                "origin": idn_open_protocol::node::routing::AudioOrigin::from_element(t)
                    .map(|o| o.to_json()),
            }));
        }
        for x in r.report.children_named("audio_rx") {
            // With how each route's sound arrives (`via`, §A.16).
            let routes: Vec<Value> = x
                .children_named("route")
                .map(idn_open_protocol::node::routing::route_json)
                .collect();
            rxs.push(json!({
                "fingerprint": r.fingerprint,
                "machine": machine(r),
                "name": x.attr("name").unwrap_or(""),
                "channels": x.attr("channels").and_then(|c| c.parse::<u16>().ok()).unwrap_or(0),
                "labels": labels(x),
                "app": x.attr("app").unwrap_or(""),
                // What it is (§A.16): program, sound_card, … ("other" when not said).
                "rx_kind": idn_open_protocol::node::routing::ReceiverKind::parse(x.attr("kind").unwrap_or("")).name(),
                "latency_ms": idn_open_protocol::node::routing::parse_latency(x.attr("latency_ms")),
                "routes": routes,
            }));
        }
    }
    for name in sources {
        if known.insert(name.clone()) {
            txs.push(
                json!({ "name": name, "channels": 2, "labels": [], "machine": "", "known": false }),
            );
        }
    }
    txs.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    rxs.sort_by(|a, b| {
        (a["machine"].as_str(), a["name"].as_str())
            .cmp(&(b["machine"].as_str(), b["name"].as_str()))
    });
    json!({ "transmitters": txs, "receivers": rxs })
}

/// A machine's Node as its report says.
fn node_view(n: &idn_open_protocol::trust::NodeReport) -> NodeView {
    let e = &n.report;
    let at = |k: &str| e.attr(k).unwrap_or("").to_owned();
    let child = |c: &str, k: &str| e.child(c).and_then(|v| v.attr(k)).unwrap_or("").to_owned();
    let video = |k: &str| child("video", k);
    let clock = |k: &str| child("clock", k);
    NodeView {
        build: at("build"),
        uptime_s: e.attr("uptime_s").and_then(|v| v.parse().ok()),
        cpu_percent: e.attr("cpu_percent").and_then(|v| v.parse().ok()),
        gpu_percent: e.attr("gpu_percent").and_then(|v| v.parse().ok()),
        last_seen_s: n.age.as_secs(),
        name: at("name"),
        interface: at("interface"),
        require: e.attr("require") == Some("true"),
        lan_panel: e.attr("lan_panel") == Some("true"),
        lan_port: e
            .attr("lan_port")
            .and_then(|p| p.parse().ok())
            .unwrap_or(29201),
        state: at("state"),
        address: n.address.map(|x| x.to_string()).unwrap_or_default(),
        video: VideoView {
            threads: video("threads"),
            parallel_decode: video("parallel_decode"),
            encoding: video("encoding"),
            decoding: video("decoding"),
            ndi: video("ndi"),
            precision: video("precision"),
            processor: video("processor"),
            format: video("format"),
        },
        clock: ClockView {
            state: clock("state"),
            locked: clock("locked"),
            gm: clock("gm"),
            offset_ns: clock("offset_ns"),
            delay_ns: clock("delay_ns"),
            jitter_ns: clock("jitter_ns"),
            domain: clock("domain"),
            protocol: clock("protocol"),
            enabled: clock("enabled"),
            ports: clock("ports"),
            error: clock("error"),
        },
        links: e
            .child("links")
            .into_iter()
            .flat_map(|l| l.children_named("link"))
            .map(|l| LinkView {
                name: l.attr("name").unwrap_or("").to_owned(),
                addr: l.attr("addr").unwrap_or("").to_owned(),
                up: l.attr("up") == Some("true"),
                physical: l.attr("physical") != Some("false"),
                speed_mbps: l.attr("speed_mbps").and_then(|v| v.parse().ok()),
            })
            .collect(),
        audio_rate: at("audio_rate"),
        rate: RateView {
            hz: e
                .child("audio_rate")
                .and_then(|r| r.attr("hz"))
                .and_then(|v| v.parse().ok()),
            from: child("audio_rate", "from"),
        },
        settings: settings_json(e),
        update: e.child("update").map_or(serde_json::Value::Null, |u| {
            serde_json::Value::Object(
                u.attrs
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                    .collect(),
            )
        }),
        programs: e
            .children_named("program")
            .map(|p| ProgramView {
                app: p.attr("app").unwrap_or("").to_owned(),
                registrations: p
                    .children_named("reg")
                    .map(|r| RegView {
                        kind: r.attr("kind").unwrap_or("").to_owned(),
                        name: r.attr("name").unwrap_or("").to_owned(),
                        status: r.attr("status").unwrap_or("").to_owned(),
                    })
                    .collect(),
            })
            .collect(),
    }
}

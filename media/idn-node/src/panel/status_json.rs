//! The panel's status (`GET /api/v1/status`): the Node's status as the pages read it, made
//! as its type ([`crate::panel_state::NodeStatus`]).

use super::*;
use crate::panel_state as ps;

impl State {
    pub(super) fn status(&self) -> Value {
        let s = self.core.status();
        let node_settings = self.core.settings();
        // The machine's rate in use, read once (bug run J1-6).
        let rate_now = self.core.audio_rate();
        let a = |k: &str| s.attr(k).unwrap_or("").to_owned();
        // Linked computers (§N.32): on unless in a site, the links, a request waiting here,
        // this computer's own request.
        let pairs = s.child("pairs").map(|p| {
            json!({
                "on": p.attr("on") == Some("true"),
                "in_site": p.attr("in_site") == Some("true"),
                "list": p.children_named("pair").map(|l| json!({
                    "fp": l.attr("fp"),
                    "name": l.attr("name"),
                    "since": l.attr("since").and_then(|v| v.parse::<u64>().ok()),
                    "once": l.attr("once") == Some("true"),
                })).collect::<Vec<_>>(),
                "incoming": p.child("incoming").map(|w| json!({
                    "by": w.attr("by"),
                    "code": w.attr("code"),
                    "left_s": w.attr("left_s").and_then(|v| v.parse::<u64>().ok()),
                })),
                "outgoing": p.child("outgoing").map(|w| json!({
                    "to": w.attr("to"),
                    "code": w.attr("code"),
                    "state": w.attr("state"),
                    "reason": w.attr("reason"),
                })),
                "last": p.child("last").map(|l| json!({
                    "at": l.attr("at").and_then(|v| v.parse::<u64>().ok()),
                    "ok": l.attr("ok") == Some("true"),
                    "text": l.attr("text"),
                })),
            })
        });
        // The machine's video settings (§N.13); "" where unset.
        let video = |k: &str| {
            s.child("video")
                .and_then(|v| v.attr(k))
                .unwrap_or("")
                .to_owned()
        };
        // This computer's sound bridges (§N.14), run by the session agent.
        let audio = |k: &str| {
            s.child("audio")
                .and_then(|v| v.attr(k))
                .unwrap_or("")
                .to_owned()
        };
        let capture = |k: &str| {
            s.child("capture")
                .and_then(|v| v.attr(k))
                .unwrap_or("")
                .to_owned()
        };
        let sitelink = |k: &str| {
            s.child("sitelink")
                .and_then(|v| v.attr(k))
                .unwrap_or("")
                .to_owned()
        };
        let aes67 = |k: &str| {
            s.child("aes67")
                .and_then(|v| v.attr(k))
                .unwrap_or("")
                .to_owned()
        };
        let listen = |k: &str| {
            s.child("listen")
                .and_then(|v| v.attr(k))
                .unwrap_or("")
                .to_owned()
        };
        // The listening page's address as phones reach it: the machine's interface if
        // set, else the address it goes out by.
        let listen_url = {
            let ip = s
                .attr("interface")
                .filter(|i| !i.is_empty())
                .map(str::to_owned)
                .or_else(|| idn_common::net::outgoing_ip().map(|ip| ip.to_string()))
                .unwrap_or_else(idn_open_protocol::discovery::hostname);
            let port = listen("port");
            match port.as_str() {
                "" => format!("http://{ip}:29216/"),
                "80" => format!("http://{ip}/"),
                p => format!("http://{ip}:{p}/"),
            }
        };
        let found = self.aes67_found.lock_ok();
        let aes67_found = json!({ "running": found.running, "at": found.at, "streams": found.streams, "error": found.error });
        drop(found);
        let opt = |e: &idn_open_protocol::xml::Element, k: &str| e.attr(k).map(str::to_owned);
        let programs: Vec<ps::Program> = s
            .children_named("client")
            .map(|c| ps::Program {
                app: opt(c, "app"),
                pid: opt(c, "pid"),
                user: opt(c, "user"),
                exe: opt(c, "exe"),
                approval: opt(c, "approval"),
                since: c.attr("since").and_then(|v| v.parse::<u64>().ok()),
                registrations: c
                    .children_named("reg")
                    .map(|r| ps::Registration {
                        kind: opt(r, "kind"),
                        name: opt(r, "name"),
                        port: r.attr("port").and_then(|p| p.parse::<u16>().ok()),
                    })
                    .collect(),
            })
            .collect();
        let options = self
            .core
            .enrollment()
            .map(|e| {
                let o = e.options();
                let view = o.for_device(&self.core.fingerprint());
                ps::NodeOptions {
                    version: o.version,
                    values: view
                        .entries()
                        .into_iter()
                        .map(|(k, v)| json!({"key": k, "value": v}))
                        .collect(),
                }
            })
            .unwrap_or_default();
        let child = |n: &str, k: &str| s.child(n).and_then(|c| c.attr(k)).unwrap_or("").to_owned();
        let on = |n: &str, k: &str| s.child(n).and_then(|c| c.attr(k)) == Some("true");
        let at_list =
            |d: &idn_open_protocol::xml::Element| d.attr("at").and_then(|v| v.parse::<u64>().ok());
        let (vsound, vscreens) = self.devices();
        let status = ps::NodeStatus {
            version: 0,
            node_version: a("node"),
            build: a("build"),
            name: a("name"),
            fingerprint: a("fp"),
            server: a("server"),
            interface: a("interface"),
            require: a("require") == "true",
            passphrase_set: a("passphrase_set") == "true",
            key_id: a("key_id"),
            state: a("state"),
            connected: a("connected") == "true",
            code: a("code"),
            // A typed join code's answer (§N.11.8): tries left, and how the site takes
            // machines (`approval`, `code_or_approval`, `code_and_approval`; empty: not known).
            code_left: a("code_left").parse::<u32>().ok(),
            join_mode: a("join_mode"),
            // The site's devices, from its directory (§N.42); null until known.
            site_devices: a("site_devices").parse::<u32>().ok(),
            welcomed: a("welcomed") != "false",
            // The simple-mode PIN (§N.20), the site's lock (N4, §N.31).
            panel: ps::PanelLock {
                pin: on("panel", "pin"),
                site_only: on("panel", "site_only"),
                locked: on("panel", "locked"),
            },
            error: a("error"),
            // (Set per caller: `GET status` and the state.)
            lan: false,
            lan_panel: a("lan_panel") == "true",
            lan_port: a("lan_port").parse::<u16>().unwrap_or(LAN_PANEL_PORT),
            auto_site: a("auto_site") == "true",
            // (Not "audio": that is the routing matrix's.)
            bridges: ps::Bridges {
                mic: audio("mic") == "true",
                mic_name: audio("mic_name"),
                mic_device: audio("mic_device"),
                apps: audio("apps"),
                speaker: audio("speaker") == "true",
                speaker_name: audio("speaker_name"),
                x32: audio("x32") == "true",
                x32_name: audio("x32_name"),
            },
            capture: ps::Capture {
                screens: capture("screens"),
                webcams: capture("webcams"),
            },
            sitelink: ps::SiteLink {
                mode: sitelink("mode"),
                address: sitelink("address"),
                port: sitelink("port"),
                limit: sitelink("limit"),
                groups: sitelink("groups"),
                sound: sitelink("sound"),
            },
            listen: ps::Listen {
                channels: listen("channels"),
                port: listen("port"),
                title: listen("title"),
                kbps: listen("kbps"),
                url: listen_url,
            },
            data: ps::DataService {
                on: on("data", "on"),
                library: on("data", "library"),
                port: child("data", "port"),
            },
            captions: ps::CaptionsService {
                on: on("captions", "on"),
                source: child("captions", "source"),
                model: child("captions", "model"),
                words: child("captions", "words"),
                name: child("captions", "name"),
                lang: child("captions", "lang"),
            },
            netaudio: ps::NetAudio {
                on: on("netaudio", "on"),
                device: child("netaudio", "device"),
                channels: child("netaudio", "channels"),
                latency: child("netaudio", "latency"),
                receivers_latency: child("netaudio", "receivers_latency"),
                interface: child("netaudio", "interface"),
                desk: on("netaudio", "desk"),
                desk_name: child("netaudio", "desk_name"),
                desk_block: child("netaudio", "desk_block"),
                desk_outputs: child("netaudio", "desk_outputs"),
                read_only: on("netaudio", "read_only"),
            },
            apps: ps::Apps {
                switcher: on("apps", "switcher"),
                mixer: on("apps", "mixer"),
                display: on("apps", "display"),
            },
            routers: child("routers", "list"),
            omt: ps::Omt {
                sends: child("omt", "sends"),
                recvs: child("omt", "recvs"),
            },
            media: s.child("media").map_or(Value::Null, |m| {
                json!({
                    "dir": m.attr("dir"),
                    "transfers": m.children_named("transfer").map(|x| json!({
                        "way": x.attr("way"), "peer": x.attr("peer"), "file": x.attr("file"),
                        "size": x.attr("size").and_then(|v| v.parse::<u64>().ok()),
                        "done": x.attr("done").and_then(|v| v.parse::<u64>().ok()),
                        "state": x.attr("state"),
                    })).collect::<Vec<_>>(),
                })
            }),
            aes67: ps::Aes67 {
                sends: aes67("sends"),
                recvs: aes67("recvs"),
                found: aes67_found,
            },
            services: s
                .children_named("service")
                .map(|x| {
                    let xa = |k: &str| x.attr(k).unwrap_or("").to_owned();
                    ps::ServiceState {
                        id: xa("id"),
                        kind: xa("kind"),
                        title: xa("title"),
                        runs: xa("runs"),
                        on: xa("on") == "true",
                        auto: x.attr("auto") != Some("false"),
                        state: xa("state"),
                        restarts: x.attr("restarts").and_then(|v| v.parse::<u32>().ok()),
                        for_s: x.attr("for_s").and_then(|v| v.parse::<u64>().ok()),
                        line: xa("line"),
                        by: xa("by"),
                    }
                })
                .collect(),
            os: std::env::consts::OS.to_owned(),
            problems: self.core.problems(),
            video: ps::VideoSettings {
                threads: video("threads"),
                parallel_decode: video("parallel_decode"),
                encoding: video("encoding"),
                decoding: video("decoding"),
                ndi: video("ndi"),
                precision: video("precision"),
                processor: video("processor"),
                format: video("format"),
            },
            clock: s.child("clock").map(|c| {
                let ca = |k: &str| c.attr(k).unwrap_or("").to_owned();
                ps::ClockState {
                    state: ca("state"),
                    locked: c.attr("locked") == Some("true"),
                    gm: ca("gm"),
                    offset_ns: c.attr("offset_ns").and_then(|v| v.parse::<f64>().ok()),
                    delay_ns: c.attr("delay_ns").and_then(|v| v.parse::<f64>().ok()),
                    domain: ca("domain"),
                    ports: ca("ports"),
                    error: ca("error"),
                }
            }),
            audio_rate: ps::AudioRate {
                setting: node_settings.audio_rate,
                hz: rate_now.0,
                from: rate_now.1.name().to_owned(),
            },
            clock_settings: ps::ClockSettings {
                on: node_settings.clock.on,
                domain: node_settings.clock.domain,
                protocol: node_settings.clock.protocol.name().to_owned(),
                priority: node_settings.clock.priority1,
            },
            sites: s
                .children_named("site")
                .map(|x| ps::SiteFound {
                    name: opt(x, "name"),
                    server: opt(x, "server"),
                    ca: opt(x, "ca"),
                })
                .collect(),
            audio: idn_open_protocol::node::routing::matrix_json(&s, &a("name")),
            uptime_s: a("uptime_s").parse::<u64>().unwrap_or(0),
            programs,
            // Per-program approval (§N.8.1): on or off, and the rules.
            approve_programs: a("approve_programs") == "true",
            program_rules: s
                .children_named("program_rule")
                .map(|r| ps::ProgramRule {
                    path: opt(r, "path"),
                    allow: r.attr("allow") == Some("true"),
                    app: opt(r, "app"),
                })
                .collect(),
            link_requests: a("link_requests") != "false",
            network: self.sources(),
            pairs: pairs.unwrap_or(Value::Null),
            options,
            log: self.core.log_lines(),
            keep_awake: a("keep_awake") != "false",
            // A site found on the network, asked to join and not kept yet (kept once it
            // approves this machine).
            trial_server: a("trial_server"),
            trial_name: a("trial_name"),
            // On an IDN Appliance: the welcome's site step joins a site or makes one here
            // (§N.11.7).
            appliance: idn_common::switches::on("IDN_APPLIANCE"),
            // The site's name, where the joined server is among the sites found.
            site_name: s
                .children_named("site")
                .find(|x| x.attr("server") == s.attr("server"))
                .and_then(|x| x.attr("name"))
                .or_else(|| s.attr("trial_name").filter(|n| !n.is_empty()))
                .map(str::to_owned),
            interfaces: idn_common::net::links()
                .into_iter()
                .map(|l| ps::Interface {
                    name: l.name,
                    addr: l.addr.to_string(),
                    up: l.up,
                    physical: l.physical,
                    speed_mbps: l.speed_mbps,
                })
                .collect(),
            // Behringer's X-USB driver (Windows: its ASIO driver); null where none is needed.
            x32_driver: x32_driver(),
            // VB-Audio's VB-CABLE (the IDN Microphone's cable on Windows): installed, its
            // package beside the programs, an install asked for; null elsewhere.
            vbcable: if cfg!(windows) {
                json!({
                    "installed": vbcable_installed(),
                    "package": idn_common::supervisor::programs_dir().map(|d| d.join("vbcable").join("VBCABLE_Setup_x64.exe")).is_some_and(|p| p.is_file()),
                    "requested": !a("vbcable_request").is_empty(),
                })
            } else {
                Value::Null
            },
            // IDN's virtual sound card (Windows 11, §N.37) and virtual screens (§N.36:
            // Windows' driver and its screens, or Linux's connectors); null elsewhere.
            // Read at most every 30 s, and after a change to them here.
            vsound,
            vscreens,
            // The encoders that work in the session (the session agent's list), for the Video page.
            encoders: s.child("encoders").map_or(Value::Null, |d| {
                json!({
                    "at": at_list(d),
                    "list": d.children_named("encoder").map(|x| json!({
                        "processor": x.attr("processor"), "codec": x.attr("codec"), "by": x.attr("by"),
                    })).collect::<Vec<_>>(),
                })
            }),
            // This session's displays by their identity (the session agent's list), for the screens.
            displays: s.child("displays").map_or(Value::Null, |d| {
                json!({
                    "at": at_list(d),
                    "list": d.children_named("display").map(|x| {
                        let n = |k: &str| x.attr(k).and_then(|v| v.parse::<u32>().ok());
                        json!({
                            "id": x.attr("id"), "name": x.attr("name"), "width": n("width"),
                            "height": n("height"), "hz": n("hz").filter(|h| *h > 0),
                            "primary": x.attr("primary") == Some("true"), "position": n("position"),
                            "connector": x.attr("connector"),
                        })
                    }).collect::<Vec<_>>(),
                })
            }),
            // The programs with sound open in the session (the session agent's list), for "Add".
            audio_apps: s.child("audio_apps").map_or(Value::Null, |a| {
                json!({
                    "at": at_list(a),
                    "wanted": a.attr("wanted") == Some("true"),
                    "list": a.children_named("app").map(|x| json!({
                        "exe": x.attr("exe"), "name": x.attr("name"), "playing": x.attr("playing") == Some("true"),
                    })).collect::<Vec<_>>(),
                })
            }),
        };
        serde_json::to_value(status).unwrap_or_default()
    }
}

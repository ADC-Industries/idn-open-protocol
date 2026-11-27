//! The desk's API front: the shared engine front (`web::engine`, as the switcher's and
//! the display service's) with what is the desk's own: a bus's or an IEM mix's token
//! sees and changes its part only (§M.25, §M.26), a locked desk refuses changes (423,
//! §M.11), `plugins_scan` answers with the list, and the desk's own pages.

use super::*;
use idn_open_protocol::web::engine::{Engine, Refusal};
use idn_open_protocol::web::{Request, Response, Who};

impl Engine for Console {
    fn change_header(&self) -> &'static str {
        "X-IDN-Mixer"
    }

    fn audit(&self) -> &idn_open_protocol::web::audit::Audit {
        &self.audit
    }

    fn http_sequencer(&self) -> &Mutex<idn_open_protocol::web::events::Sequencer> {
        &self.http_seq
    }

    fn version(&self) -> u64 {
        Console::version(self)
    }

    fn wait_change(&self, after: u64, timeout: Duration) {
        Console::wait_change(self, after, timeout);
    }

    /// A bus token's: its bus alone (§M.25); an IEM mix's: its mix (§M.26).
    fn state_for(&self, who: &Who) -> Value {
        Console::state_for(self, who)
    }

    fn schema(&self) -> Value {
        typed::schema()
    }

    fn state_schema(&self) -> Option<Value> {
        Some(state::state_schema())
    }

    /// Read-only, or outside a bus's or an IEM mix's token.
    fn permits(&self, who: &Who, cmd: &str, body: &Value) -> Result<(), String> {
        Console::permits(self, who, cmd, body)
    }

    fn run_for(&self, who: &Who, cmd: &str, body: &Value) -> Result<Value, Refusal> {
        // Plugins looked for again: the list answered (§M.6).
        if cmd == "plugins_scan" {
            return self
                .plugin_list(true)
                .map(|l| json!({ "plugins": l }))
                .map_err(|e| Refusal::new("503 Service Unavailable", e));
        }
        // Locked (§M.11): nothing changes but the lock (and a musician's own IEM mix,
        // §M.26).
        if self.locked() && !works_locked(cmd) {
            self.audit
                .record(who, cmd, body, Some("the desk is locked"));
            return Err(Refusal::new("423 Locked", "the desk is locked"));
        }
        // (`iem_mix_add`, `endpoint_add` answer with the new one's id; `latency_test`
        // with its measurement.)
        self.command_answer_as(who, cmd, body).map_err(Into::into)
    }

    /// The typed failure events (§M.27): not to a bus's or an IEM mix's token (they are
    /// the whole desk's).
    fn pushed_events(&self, who: &Who, seen: &mut u64) -> Vec<String> {
        if scoped::limited(who) {
            *seen = self.shared.events.seq();
            return Vec::new();
        }
        self.shared.events.pushed(seen)
    }

    fn event_seq(&self) -> u64 {
        self.shared.events.seq()
    }

    /// `meters`: every 40 ms (a scoped token's: its part).
    fn topic_every(&self, name: &str) -> Option<Duration> {
        (name == "meters").then_some(Duration::from_millis(40))
    }

    fn topic(&self, _name: &str, who: &Who) -> Value {
        self.meters_for(who)
    }

    /// Not a bus's or an IEM mix's token: the audit is the whole desk's.
    fn audit_for(&self, who: &Who) -> Result<(), String> {
        if scoped::limited(who) {
            Err(scoped::outside_bus("the audit"))
        } else {
            Ok(())
        }
    }

    fn route(this: &Arc<Self>, r: &Request, who: &Who) -> Option<Response> {
        if let Some(a) = idn_open_protocol::web::ui::asset(&r.path).filter(|_| r.method == "GET") {
            return Some(a);
        }
        // A bus token (§N.31.5, §M.25) reads its bus's state, meters and controls (and
        // the schema); an IEM mix's token (§M.26) those and the IEM flow's description;
        // nothing else of the desk.
        if r.method == "GET"
            && scoped::limited(who)
            && !matches!(
                r.path.as_str(),
                "/api/v1/state"
                    | "/api/v1/events"
                    | "/api/v1/meters"
                    | "/api/v1/controls"
                    | "/api/v1/schema"
            )
            && !(r.path == "/api/v1/iem" && scoped::limited_to_mix(who))
        {
            let why = match &who.scope {
                idn_open_protocol::trust::token::Scope::Iem(m) => scoped::outside_mix(m, &r.path),
                _ => scoped::outside_bus(&r.path),
            };
            return Some(Response::json_err("403 Forbidden", &why));
        }
        Some(match (r.method.as_str(), r.path.as_str()) {
            // Typed failure events (§M.27), the newest first (`?n=`, 100 by default, 500
            // at most). Not a bus's or an IEM mix's token (the whole desk's).
            ("GET", "/api/v1/events/recent") => {
                let n = r
                    .param("n")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(100usize)
                    .min(500);
                Response::json(Value::Array(this.failure_events().newest(n)).to_string())
                    .with("Cache-Control", "no-store")
            }
            // The IEM flow as a phone needs it (§M.26, idn-iem.md §I.2).
            ("GET", "/api/v1/iem") => {
                Response::json(this.iem_description().to_string()).with("Cache-Control", "no-store")
            }
            // The control limits: ranges, steps, defaults, units, choices (spec §M.5).
            ("GET", "/api/v1/controls") => Response::json(this.controls_for(who).to_string()),
            ("GET", "/api/v1/meters") => {
                Response::json(this.meters_for(who).to_string()).with("Cache-Control", "no-store")
            }
            // Plugins (§M.6): the list on this machine, a plugin's parameters. Looking again
            // is a change (POST plugins_scan): any page open on this machine could start
            // one with a GET.
            ("GET", "/api/v1/plugins") => match this.plugin_list(false) {
                Ok(l) => Response::json(json!({"plugins": l}).to_string()),
                Err(e) => Response::json_err("503 Service Unavailable", &e),
            },
            // The EQ analyzer: a channel's spectrum (§M.11).
            ("GET", "/api/v1/spectrum") => {
                match r.param("channel").and_then(|n| n.parse::<usize>().ok()) {
                    Some(n) => match this.spectrum(n) {
                        Ok(s) => Response::json(s.to_string()).with("Cache-Control", "no-store"),
                        Err(e) => Response::json_err("400 Bad Request", &e),
                    },
                    None => Response::json_err("400 Bad Request", "channel: from 1"),
                }
            }
            // Each plugin's output level since last asked: the rack's meters.
            ("GET", "/api/v1/plugin_levels") => {
                let mut v = json!({});
                if let Some(n) = r.param("channel").and_then(|n| n.parse::<u64>().ok()) {
                    v["channel"] = json!(n);
                } else {
                    for k in ["bus", "group", "matrix", "fx"] {
                        if let Some(b) = r.param(k) {
                            v[k] = json!(b);
                        }
                    }
                }
                match this.plugin_levels(&v) {
                    Ok(l) => Response::json(l.to_string()).with("Cache-Control", "no-store"),
                    Err(e) => Response::json_err("400 Bad Request", &e),
                }
            }
            // The Routing view's Network tab: the routing matrix through the IDN Node.
            // Its meters: this machine's transmitters' levels (idn-routing.md §A.15).
            ("GET", "/api/v1/routing/levels") => match this.routing_levels() {
                Ok(v) => {
                    Response::json(json!({ "tx": v }).to_string()).with("Cache-Control", "no-store")
                }
                Err(e) => Response::json_err("503 Service Unavailable", &e),
            },
            ("GET", "/api/v1/routing") => match this.routing() {
                Ok(v) => Response::json(v.to_string()).with("Cache-Control", "no-store"),
                Err(e) => Response::json_err("503 Service Unavailable", &e),
            },
            // The recordings, newest first (§M.13).
            ("GET", "/api/v1/recordings") => {
                Response::json(json!({"recordings": this.recordings()}).to_string())
                    .with("Cache-Control", "no-store")
            }
            ("GET", "/api/v1/plugin_params") => {
                let mut v = json!({"index": r.param("index").and_then(|i| i.parse::<u64>().ok())});
                if let Some(n) = r.param("channel").and_then(|n| n.parse::<u64>().ok()) {
                    v["channel"] = json!(n);
                } else {
                    for k in ["bus", "group", "matrix", "fx"] {
                        if let Some(b) = r.param(k) {
                            v[k] = json!(b);
                        }
                    }
                }
                match this.plugin_params(&v) {
                    Ok(p) => Response::json(p.to_string()).with("Cache-Control", "no-store"),
                    Err(e) => Response::json_err("400 Bad Request", &e),
                }
            }
            _ => return None,
        })
    }
}

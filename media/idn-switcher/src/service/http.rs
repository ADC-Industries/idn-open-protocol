//! The switcher's HTTP API (`/api/v1/…`): a request answered from the service. The
//! `idn-switcher` program serves it on 127.0.0.1 (and, with a password, on the network);
//! an embedder may serve it from its own server.

use super::*;
use idn_open_protocol::web::{Request, Response, Who, audit};

impl Service {
    /// Answers one API request (`GET /api/v1/state`, `POST /api/v1/<command>`,
    /// `GET /api/v1/events`, …) from this machine: everything allowed, audited as
    /// `local`.
    pub fn handle(self: &Arc<Self>, r: Request) -> Response {
        let who = Who::local(&r);
        self.handle_as(r, who)
    }

    /// [`handle`](Self::handle) for `who` (a network front's caller: a token's holder or
    /// a password's session, idn-node.md §N.31.5): the shared engine front
    /// (`web::engine`) with the switcher's own pages; a read-only caller's changes refused
    /// (403), every change written in the audit as theirs.
    pub fn handle_as(self: &Arc<Self>, r: Request, who: Who) -> Response {
        idn_open_protocol::web::engine::handle_as(self, r, who)
    }

    /// A command run for `who`, written in the audit as theirs (HTTP, the push channel,
    /// MQTT; §S.35). Not checked against their scope: their front did that.
    pub fn command_as(&self, who: &Who, cmd: &str, v: &Value) -> Result<Value, String> {
        let done = self.command(cmd, v);
        self.audit
            .record(who, cmd, v, done.as_ref().err().map(String::as_str));
        done
    }

    /// A typed command run for `who` (a host's own user), audited as theirs.
    pub fn run_as(&self, who: &Who, c: Command) -> Result<Value, String> {
        self.command_as(who, c.name(), &c.body())
    }

    /// One client's push channel on this machine (`web::engine::events_as`): the state
    /// as it changes, typed failure events (§S.37), `meters` ten times a second once
    /// subscribed, the audit, commands answered in order.
    pub fn events(self: &Arc<Self>, ws: idn_open_protocol::web::WebSocket) {
        idn_open_protocol::web::engine::events_as(self, ws, Who::host(""));
    }

    /// The version a client holds: the engine's, the show's and undo's, the streams'.
    fn quick_version(&self) -> u64 {
        self.sw.version()
            + self.show_version.load(std::sync::atomic::Ordering::Relaxed)
            + *self.streams.version.lock_ok()
    }
}

impl idn_open_protocol::web::engine::Engine for Service {
    fn change_header(&self) -> &'static str {
        "X-IDN-Switcher"
    }

    fn audit(&self) -> &audit::Audit {
        &self.audit
    }

    fn http_sequencer(&self) -> &Mutex<idn_open_protocol::web::events::Sequencer> {
        &self.seq
    }

    fn version(&self) -> u64 {
        self.quick_version()
    }

    /// Long poll: answered when the state moves past `after`. Streams and media players
    /// change without the engine knowing: looked at each second (a playing file's
    /// position).
    fn wait_change(&self, after: u64, timeout: Duration) {
        let streams_v = *self.streams.version.lock_ok();
        let (streams0, media0) = (self.streams.state(), self.media.state());
        for _ in 0..timeout.as_secs().max(1) {
            let show_v = self.show_version.load(std::sync::atomic::Ordering::Relaxed);
            if self
                .sw
                .wait_change(after.saturating_sub(show_v), Duration::from_secs(1))
                + show_v
                > after
                || self.show_version.load(std::sync::atomic::Ordering::Relaxed) != show_v
                || *self.streams.version.lock_ok() != streams_v
                || self.streams.state() != streams0
                || self.media.state() != media0
            {
                break;
            }
        }
    }

    fn state_for(&self, _who: &Who) -> Value {
        self.state()
    }

    fn schema(&self) -> Value {
        typed::schema()
    }

    fn state_schema(&self) -> Option<Value> {
        Some(super::state_schema::state_schema())
    }

    fn run_for(
        &self,
        who: &Who,
        cmd: &str,
        body: &Value,
    ) -> Result<Value, idn_open_protocol::web::engine::Refusal> {
        self.command_as(who, cmd, body).map_err(Into::into)
    }

    /// Typed failure events (§S.37).
    fn pushed_events(&self, _who: &Who, seen: &mut u64) -> Vec<String> {
        self.events.pushed(seen)
    }

    fn event_seq(&self) -> u64 {
        self.events.seq()
    }

    /// `meters`: ten times a second, as `GET /api/v1/meters`.
    fn topic_every(&self, name: &str) -> Option<Duration> {
        (name == "meters").then_some(Duration::from_millis(100))
    }

    fn topic(&self, _name: &str, _who: &Who) -> Value {
        self.sw.meters()
    }

    /// The switcher's own pages: recent events, addresses, cards, pictures (scenes,
    /// inputs, the multiview), title templates, the filters' schema, fonts, screens,
    /// meters, the monitor's sound.
    fn route(this: &Arc<Self>, r: &Request, _who: &Who) -> Option<Response> {
        Some(match (r.method.as_str(), r.path.as_str()) {
            // Typed events (§S.37), the newest first (`?n=`, 100 by default, 500 at most).
            ("GET", "/api/v1/events/recent") => {
                let n = r
                    .param("n")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(100usize)
                    .min(500);
                Response::json(Value::Array(this.events.newest(n)).to_string())
            }
            // The sound cards with inputs, for the sound's source.
            // This machine's addresses, to send a stream from (§S.30).
            ("GET", "/api/v1/addresses") => {
                let a: Vec<Value> =
                    idn_open_protocol::discovery::ifaces::ipv4_multicast_interfaces()
                        .into_iter()
                        .filter(|i| !i.loopback)
                        .map(|i| json!({"name": i.name, "address": i.addr.to_string()}))
                        .collect();
                Response::json(json!({"addresses": a}).to_string())
            }
            ("GET", "/api/v1/cards") => {
                #[cfg(feature = "card")]
                let cards: Vec<Value> = idn_audio::devices()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|d| d.input.map(|i| json!({"name": d.name, "id": d.id, "channels": i.channels, "default": d.default_input})))
                    .collect();
                // Playback devices, for the monitor (§S.24).
                #[cfg(feature = "card")]
                let outputs: Vec<Value> = idn_audio::devices()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|d| d.output.map(|o| json!({"name": d.name, "id": d.id, "channels": o.channels, "default": d.default_output})))
                    .collect();
                #[cfg(not(feature = "card"))]
                let (cards, outputs): (Vec<Value>, Vec<Value>) = (Vec::new(), Vec::new());
                Response::json(json!({"cards": cards, "outputs": outputs}).to_string())
            }
            // A scene's picture, drawn now (a client UI's scene cards): ?id=N&w=320.
            ("GET", "/api/v1/scene.jpg") => {
                let id = r
                    .param("id")
                    .and_then(|v| v.parse::<u32>().ok())
                    .unwrap_or(0);
                let w = r
                    .param("w")
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(320);
                match this.sw.scene_jpeg(id, w) {
                    Ok(j) => Response::ok("image/jpeg", j).with("Cache-Control", "no-store"),
                    Err(e) => Response::json_err("404 Not Found", &e),
                }
            }
            // An input's picture for the title designer: ?id=N&w=640.
            ("GET", "/api/v1/input.jpg") => {
                let id = r
                    .param("id")
                    .and_then(|v| v.parse::<u32>().ok())
                    .unwrap_or(0);
                let w = r
                    .param("w")
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(640);
                match this.sw.input_jpeg(id, w) {
                    Ok(j) => Response::ok("image/jpeg", j).with("Cache-Control", "no-store"),
                    Err(e) => Response::json_err("404 Not Found", &e),
                }
            }
            // Title templates, by name (§S.19): the styles built in (`"built_in": true`) and
            // the saved ones (a saved one of a built-in style's name in its place).
            ("GET", "/api/v1/title_templates") => {
                let mut all = rules::builtin_templates();
                all.extend(this.saved_templates());
                Response::json(Value::Object(all).to_string()).with("Cache-Control", "no-store")
            }
            // The filters, their settings, ranges and defaults (§S.17).
            ("GET", "/api/v1/filters_schema") => {
                Response::json(crate::filter::schema_json().to_string())
            }
            // The commands and their bodies, as a JSON schema.
            // The font families titles can use.
            ("GET", "/api/v1/fonts") => {
                Response::json(json!({"fonts": this.sw.font_families()}).to_string())
            }
            ("GET", "/api/v1/multiview.jpg") => match this.sw.multiview_jpeg() {
                Some(j) => Response::ok("image/jpeg", j.as_slice().to_vec())
                    .with("Cache-Control", "no-store"),
                // The first one is drawn within a frame or two of being asked for.
                None => Response::json_err("503 Service Unavailable", "starting"),
            },
            // This machine's screens and the outputs they can show (idn-display §D.3).
            ("GET", "/api/v1/screens") => {
                let outputs: Vec<Value> = this
                    .sw
                    .outputs()
                    .into_iter()
                    .map(|(kind, name, _)| json!({"kind": kind, "name": name}))
                    .collect();
                match this.displays.list() {
                    Ok(mut l) => {
                        l["outputs"] = Value::Array(outputs);
                        Response::json(l.to_string())
                    }
                    Err(e) => Response::json(
                        json!({"screens": [], "error": e, "outputs": outputs}).to_string(),
                    ),
                }
            }
            // The sound's levels (§S.24).
            ("GET", "/api/v1/meters") => Response::json(this.sw.meters().to_string()),
            // The monitor's sound for Listen: from frame ?from=N, waiting up to 0.2 s; the
            // first frame's number (8 bytes, little-endian), then 16-bit stereo at the
            // switcher's rate (the X-IDN-Rate header: 44100, 48000 or 96000; §N.40).
            ("GET", "/api/v1/monitor/listen") => {
                let from = r.param("from").and_then(|v| v.parse().ok()).unwrap_or(0);
                let (first, pcm) = this.sw.monitor_listen(from, Duration::from_millis(200));
                let mut body = Vec::with_capacity(8 + pcm.len() * 2);
                body.extend_from_slice(&first.to_le_bytes());
                for v in pcm {
                    body.extend_from_slice(&v.to_le_bytes());
                }
                let mut resp = Response::ok("application/octet-stream", body);
                resp.headers
                    .push(("Cache-Control".into(), "no-store".into()));
                resp.headers
                    .push(("X-IDN-Rate".into(), crate::engine::rate().to_string()));
                resp
            }
            _ => return None,
        })
    }
}

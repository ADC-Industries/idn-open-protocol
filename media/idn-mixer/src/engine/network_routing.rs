//! The Routing view's Network tab: the routing matrix (idn-routing.md
//! §A.6) from the desk's own panel, through this machine's IDN Node, which keeps the routes
//! and checks them (as from its own panel): the network's sources onto the desk's inputs,
//! and the desk's outputs onto this machine's receivers.

use super::*;
use idn_open_protocol::xml::Element;

impl Console {
    /// The matrix's meters: this machine's transmitters' levels (§A.15), `{name: [dB, …]}`.
    pub fn routing_levels(&self) -> Result<Value, String> {
        let node = self.node()?;
        let l = node.levels().map_err(|e| format!("the IDN Node: {e}"))?;
        Ok(idn_open_protocol::node::meters::levels_json(&l))
    }

    /// The matrix's data: this machine's transmitters and receivers with their routes, the
    /// network's other sources (for columns of their own), and which receiver and
    /// transmitter are this desk's. An error without an IDN Node.
    pub fn routing(&self) -> Result<Value, String> {
        let node = self.node()?;
        let status = node.status().map_err(|e| format!("the IDN Node: {e}"))?;
        let machine = status.attr("name").unwrap_or("").to_owned();
        let mut v = json!({
            "audio": idn_open_protocol::node::routing::matrix_json(&status, &machine),
            "desk": {
                "receiver": self.shared.cfg.name,
                "transmitter": self.shared.out.name(),
            },
        });
        // Sources elsewhere on the network: the browser the desk already runs for its
        // switchers (the same one, while it is held).
        // Their channel counts (§M.28): the site's transmitters as its machines report
        // them, through the Node (none outside a site, or for a source not on a site
        // machine: null).
        let counts: std::collections::HashMap<String, u64> = node
            .request(Element::new("site_transmitters"))
            .map(|r| {
                r.children_named("tx")
                    .filter_map(|t| {
                        Some((
                            t.attr("name")?.to_owned(),
                            t.attr("channels")?.parse().ok()?,
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let network: Vec<Value> =
            idn_open_protocol::discovery::shared(idn_open_protocol::discovery::BrowserConfig {
                interface: None,
                discovery_server: self.shared.cfg.discovery_server.clone(),
            })
            .map(|b| {
                b.sources()
                    .into_iter()
                    .map(|x| json!({"channels": counts.get(&x.name), "name": x.name}))
                    .collect()
            })
            .unwrap_or_default();
        v["network"] = json!(network);
        Ok(v)
    }

    /// Route changes, as the matrix sends them: `{receiver, changes: [{ch, tx, tx_ch}]}`
    /// (`tx` empty: the route cleared). Each goes to the Node, which may refuse one (a
    /// machine its site locked).
    pub(super) fn route(&self, v: &Value) -> Result<(), String> {
        let node = self.node()?;
        let receiver = v["receiver"].as_str().unwrap_or("").trim();
        if receiver.is_empty() {
            return Err("receiver: a receiver's name".into());
        }
        let changes = v["changes"].as_array().ok_or("changes: a list")?;
        // Only the desk's own routes (bug run 5): onto its receiver, or from its
        // transmitter onto another; the rest is the Node's and the Site Manager's.
        let ours = receiver == self.shared.cfg.name;
        let current = if ours {
            None
        } else {
            Some(self.routes_of(node, receiver)?)
        };
        let mut checked = Vec::with_capacity(changes.len());
        for c in changes {
            let ch = c["ch"]
                .as_u64()
                .filter(|&n| n < 256)
                .ok_or("each change needs ch, 0 to 255")?;
            let mut e = Element::new("route")
                .with_attr("receiver", receiver)
                .with_attr("ch", &ch.to_string());
            let tx = c["tx"].as_str().unwrap_or("").trim();
            if !ours {
                // Set: from the desk, over nothing or the desk's own; cleared: a route that
                // is from the desk now (another source's route on that channel isn't the
                // desk's to replace, bug round 2026-11-09).
                let now_from = current
                    .as_ref()
                    .and_then(|r| r.iter().find(|(c, _)| *c == ch))
                    .map(|(_, t)| t.as_str())
                    .unwrap_or("");
                let mine = if tx.is_empty() {
                    self.is_desks_transmitter(now_from)
                } else {
                    self.is_desks_transmitter(tx)
                        && (now_from.is_empty() || self.is_desks_transmitter(now_from))
                };
                if !mine {
                    return Err(format!(
                        "{receiver} channel {}: the desk routes only its own outputs to \
                         other receivers",
                        ch + 1
                    ));
                }
            }
            if !tx.is_empty() {
                let tx_ch = c["tx_ch"]
                    .as_u64()
                    .filter(|&n| n < 256)
                    .ok_or("tx_ch: 0 to 255")?;
                e = e.with_attr("tx", tx).with_attr("tx_ch", &tx_ch.to_string());
            }
            checked.push(e);
        }
        // All checked first, then sent as one request: the Node applies all or none (one
        // at a time, a refusal part-way left a drag run half made).
        let mut all = Element::new("routes");
        for e in checked {
            all = all.with_child(e);
        }
        node.request(all)
            .map_err(|e| format!("the IDN Node: {e}"))?;
        Ok(())
    }

    /// Whether `tx` names this desk's transmitter: by itself, or with this machine's
    /// name (`HOST (Mixer)`; any machine's desk called "Mixer" passed before, bug round
    /// 2026-11-09).
    fn is_desks_transmitter(&self, tx: &str) -> bool {
        let own = self.shared.out.name();
        let host = idn_open_protocol::discovery::hostname().to_uppercase();
        !tx.is_empty() && (tx == own || tx.eq_ignore_ascii_case(&format!("{host} ({own})")))
    }

    /// A receiver's routes on this machine: (channel, transmitter).
    fn routes_of(&self, node: &Node, receiver: &str) -> Result<Vec<(u64, String)>, String> {
        let status = node.status().map_err(|e| format!("the IDN Node: {e}"))?;
        let rx = status
            .children_named("audio_rx")
            .find(|x| x.attr("name") == Some(receiver))
            .ok_or_else(|| format!("no receiver {receiver} on this machine"))?;
        Ok(rx
            .children_named("route")
            .filter_map(|r| {
                Some((
                    r.attr("ch")?.parse().ok()?,
                    r.attr("tx").unwrap_or("").to_owned(),
                ))
            })
            .collect())
    }

    /// A receiver's latency on this machine (idn-routing.md §A.12): `{receiver, ms}`, `ms`
    /// null for its program's own.
    pub(super) fn receiver_latency(&self, v: &Value) -> Result<(), String> {
        let node = self.node()?;
        let receiver = v["receiver"].as_str().unwrap_or("").trim();
        if receiver.is_empty() {
            return Err("receiver: a receiver's name".into());
        }
        // The desk's own receiver, or one the desk feeds (the rest are the Node's panel's
        // and the site's).
        if receiver != self.shared.cfg.name
            && !self
                .routes_of(node, receiver)?
                .iter()
                .any(|(_, t)| self.is_desks_transmitter(t))
        {
            return Err(format!(
                "{receiver}: the desk sets the latency of its own receiver and those it feeds"
            ));
        }
        let ms = match &v["ms"] {
            Value::Null => String::new(),
            m => m
                .as_u64()
                .filter(|n| (1..=500).contains(n))
                .ok_or("ms: 1 to 500, or null")?
                .to_string(),
        };
        node.request(
            Element::new("receiver_latency")
                .with_attr("receiver", receiver)
                .with_attr("ms", &ms),
        )
        .map(|_| ())
        .map_err(|e| format!("the IDN Node: {e}"))
    }

    fn node(&self) -> Result<&Arc<Node>, String> {
        self.shared
            .cfg
            .node
            .as_ref()
            .ok_or_else(|| "no IDN Node on this machine: the desk has no network inputs".into())
    }
}

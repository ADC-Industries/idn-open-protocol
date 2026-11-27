//! What each side offers the other: the catalogue sent, offers taken in, outputs made for the far side's sources.

use super::*;

impl Session {
    /// This side's sources that may cross, when the list changed.
    pub(super) fn send_catalogue(&self, b: &Browser) {
        let offered = offered_here().lock_ok().clone();
        let now: HashMap<String, Vec<String>> = b
            .sources()
            .into_iter()
            .filter(|s| {
                !s.routing
                    && !offered.contains(&s.name)
                    && groups_match(&s.groups, &self.settings.groups)
            })
            .map(|s| (s.name, s.groups))
            .collect();
        let mut last = self.catalogue.lock_ok();
        if *last != now {
            let list: Vec<Value> = now
                .iter()
                .map(|(n, g)| json!({"name": n, "groups": g}))
                .collect();
            *last = now;
            drop(last);
            self.send_ctl(json!({"t": "catalogue", "sources": list}));
        }
    }

    /// The far side's catalogue: offer each source here, drop those gone.
    pub(super) fn offer(&self, list: Vec<Value>) {
        let mut offers = self.offers.lock_ok();
        let mut offered = offered_here().lock_ok();
        let names: HashSet<String> = list
            .iter()
            .filter_map(|s| s["name"].as_str().map(str::to_owned))
            .collect();
        offers.retain(|n, _| {
            let keep = names.contains(n);
            if !keep {
                offered.remove(n);
            }
            keep
        });
        for s in &list {
            let Some(full) = s["name"].as_str() else {
                continue;
            };
            if offers.contains_key(full) {
                continue;
            }
            let Some((host, name)) = split_name(full) else {
                continue;
            };
            let groups: Vec<String> = s["groups"]
                .as_array()
                .map(|g| {
                    g.iter()
                        .filter_map(|x| x.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            match self.output_for(host, name, groups) {
                Ok(output) => {
                    offered.insert(full.to_owned());
                    offers.insert(
                        full.to_owned(),
                        Offer {
                            output: Arc::new(output),
                            tally: (false, false),
                            subscribed: false,
                            unused_since: None,
                            retry_at: None,
                            receiving: Default::default(),
                        },
                    );
                }
                Err(e) => log(&format!("can't offer {full}: {e}")),
            }
        }
    }

    pub(super) fn output_for(
        &self,
        host: &str,
        name: &str,
        groups: Vec<String>,
    ) -> io::Result<Output> {
        let groups = if groups.is_empty() {
            vec!["public".to_owned()]
        } else {
            groups
        };
        let key = format!("{host}\n{}", groups.join(","));
        let p = {
            let mut ps = self.publishers.lock_ok();
            match ps.get(&key) {
                Some(p) => p.clone(),
                None => {
                    let p = Publisher::new(PublisherConfig {
                        host_name: Some(host.to_owned()),
                        discovery_server: self.settings.discovery_server.clone(),
                        secure: Some(self.settings.secure.clone()),
                        groups,
                        ..Default::default()
                    })?;
                    ps.insert(key, p.clone());
                    p
                }
            }
        };
        let mut cfg = OutputConfig::new(name);
        // Receivers here that take H.264/HEVC get the link's stream as it is.
        let ff: Arc<dyn EncoderFactory> = self.ff.clone();
        cfg.codec = Some(idn_open_protocol::codec::CodecOutput::new(ff))
            .filter(|c| c.encoder.codecs().contains(&self.settings.codec));
        Output::new(&p, cfg)
    }

    /// Asks for far sources someone here receives; lets go of those no one has for 10 s.
    pub(super) fn use_offers(&self) {
        let mut offers = self.offers.lock_ok();
        let mut msgs = Vec::new();
        for (name, o) in offers.iter_mut() {
            // The receivers' tally and metadata back to the real source (X.12.3).
            if o.subscribed {
                let t = o.output.tally();
                if t != o.tally {
                    o.tally = t;
                    msgs.push(
                        json!({"t": "tally", "source": name, "program": t.0, "preview": t.1}),
                    );
                }
                for (_, xml) in o.output.recv_metadata() {
                    msgs.push(json!({"t": "meta", "source": name, "xml": xml}));
                }
            }
            if o.output.receivers() > 0 {
                o.unused_since = None;
                if !o.subscribed && o.retry_at.is_none_or(|t| Instant::now() >= t) {
                    o.subscribed = true;
                    o.retry_at = None;
                    msgs.push(json!({"t": "subscribe", "source": name}));
                }
            } else if o.subscribed {
                let t = *o.unused_since.get_or_insert_with(Instant::now);
                if t.elapsed() > Duration::from_secs(10) {
                    o.subscribed = false;
                    o.unused_since = None;
                    msgs.push(json!({"t": "unsubscribe", "source": name}));
                }
            }
        }
        drop(offers);
        for m in msgs {
            self.send_ctl(m);
        }
    }
}

//! The state the service reports (`GET /api/v1/state`), with its CPU and GPU use.

use super::*;

impl Service {
    /// This program's GPU use since the last look (at most every 500 ms), % of the
    /// busiest engine; `None` without the system's per-process GPU accounting.
    pub(super) fn gpu_percent(&self) -> Option<f64> {
        let mut g = self.gpu_seen.lock_ok();
        if g.0
            .is_some_and(|t| t.elapsed() < Duration::from_millis(500))
        {
            return g.1;
        }
        let p = g.2.percent().map(|p| (p * 10.0).round() / 10.0);
        (g.0, g.1) = (Some(Instant::now()), p);
        p
    }

    /// This program's CPU use since the last call (at most every 500 ms), % of one core.
    pub(super) fn cpu_percent(&self) -> f64 {
        let mut last = self.cpu_seen.lock_ok();
        let (now, used) = (Instant::now(), idn_open_protocol::process_cpu_time());
        if let Some((at, _, pct)) = *last
            && now.duration_since(at) < Duration::from_millis(500)
        {
            return pct;
        }
        let pct = match *last {
            Some((at, before, _)) => {
                let wall = now.duration_since(at).as_secs_f64().max(1e-3);
                (used.saturating_sub(before).as_secs_f64() / wall * 1000.0).round() / 10.0
            }
            None => 0.0,
        };
        *last = Some((now, used, pct));
        pct
    }

    /// The whole state (`GET /api/v1/state`): the engine's, the streams', media's,
    /// the show's, setups' and undo's.
    pub fn state(&self) -> Value {
        let show_v = self.show_version.load(std::sync::atomic::Ordering::Relaxed);
        let mut s = self.sw.state();
        // The engine's version (the timers' seconds count) and the show's changes together
        // (see the long poll): a client showing timers sees a new version each second.
        s["version"] = json!(s["version"].as_u64().unwrap_or(0) + show_v);
        s["streams"] = self.streams.state();
        // This program's CPU use since the last look, % of one core (a client UI's footer).
        s["cpu_percent"] = json!(self.cpu_percent());
        // Its GPU use since the last look, % of the busiest engine (null: not known here).
        s["gpu_percent"] = json!(self.gpu_percent());
        // The format from the next start, when another (`format`).
        s["format_next"] = json!(self.format_next());
        // Saved destinations, without their keys ("Go live" with the ticked ones).
        s["destinations"] = self.destinations_state();
        // Encoders running for the streams and recordings (each ISO and mix has its own).
        #[cfg(feature = "ffmpeg")]
        {
            s["encodes"] = json!(
                self.streams
                    .gateways()
                    .iter()
                    .map(|g| g.encodes())
                    .sum::<usize>()
            );
        }
        s["mqtt"] = self.mqtt.get().map_or(Value::Null, |m| m.view());
        s["starters"] = starters_list(STARTERS);
        s["stream_alerts"] = Value::Array(self.streams.alerts.lock_ok().iter().cloned().collect());
        // What is wrong now (§S.37).
        s["problems"] = Value::Array(self.problems_of(&s));
        s["media"] = self.media.state();
        s["screens"] = self.displays.status();
        {
            let log = self.undo.lock_ok();
            s["setups"] = json!({
                "list": self.setups(),
                "current": std::fs::read_to_string(state_dir().join(SETUP_CURRENT)).ok().map(|n| n.trim().to_owned()),
            });
            s["undo"] = json!({
                "back": log.back.last().map(|x| x.what.clone()),
                "forward": log.forward.last().map(|x| x.what.clone()),
            });
        }
        {
            let r = self.show.lock_ok();
            let mut status = r.status();
            status["error"] = json!(
                self.show_errors
                    .lock_ok()
                    .iter()
                    .map(|(k, e)| format!(
                        "{}. {}: {e}",
                        k + 1,
                        r.show.cues.get(*k).map_or("", |c| c.name.as_str())
                    ))
                    .collect::<Vec<_>>()
                    .join(" · ")
            );
            status["saved"] = json!(self.saved_shows().keys().collect::<Vec<_>>());
            status["planning_center"] = json!(self.pco().is_some());
            // The desks a cue can recall a scene on, by name (§S.27).
            status["desks"] = self
                .sw
                .desks()
                .into_iter()
                .map(|(name, heard)| json!({"name": name, "heard": heard}))
                .collect();
            status["show"] = r.show.to_json();
            s["show"] = status;
            // IDN Data's feeds on the network, for a title's data (§S.20).
            s["data_feeds"] = json!(self.sw.data_feeds());
            // The other sources, for an input's Add: not this switcher's own outputs.
            let name = s["name"].as_str().unwrap_or("").to_owned();
            let outputs: Vec<String> = self.sw.outputs().into_iter().map(|o| o.1).collect();
            // This machine as its outputs name it (the Node's machine name, else the host's).
            let host = outputs
                .first()
                .and_then(|o| o.split_once(" ("))
                .map_or_else(idn_open_protocol::discovery::hostname, |(h, _)| {
                    h.to_owned()
                });
            s["network_sources"] = json!(
                self.sw
                    .network_sources()
                    .into_iter()
                    .filter(|n| !rules::own_source(n, &name, &host, &outputs))
                    .collect::<Vec<_>>()
            );
            // The show's version, for `show_set`'s and the cue edits' `if_version`.
            s["show"]["version"] = json!(rules::show_version(&r.show));
        }
        self.state_extras(&mut s);
        s
    }

    /// What the state adds to the engine's and the streams': each title's summary, each
    /// virtual shot's crop, each stream's kind and sent-rate history.
    pub(super) fn state_extras(&self, s: &mut Value) {
        let (w, h) = (
            s["format"]["width"].as_u64().unwrap_or(16) as usize,
            s["format"]["height"].as_u64().unwrap_or(9) as usize,
        );
        let sizes: std::collections::BTreeMap<u64, (usize, usize)> = s["inputs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| {
                Some((
                    i["id"].as_u64()?,
                    (
                        i["width"].as_u64().unwrap_or(0) as usize,
                        i["height"].as_u64().unwrap_or(0) as usize,
                    ),
                ))
            })
            .collect();
        for i in s["inputs"].as_array_mut().into_iter().flatten() {
            if i["source"]["title"].is_object() {
                i["summary"] = json!(rules::title_summary(&i["source"]["title"]));
            }
            // (Looked up, not indexed: indexing would add `"virtual": null` to each.)
            if let Some(vs) = i.pointer_mut("/source/virtual").filter(|v| v.is_object()) {
                let (sw, sh) = vs["input"]
                    .as_u64()
                    .and_then(|id| sizes.get(&id).copied())
                    .unwrap_or((0, 0));
                vs["crop"] = rules::shot_crop(&vs["view"], sw, sh, w, h);
                vs["to_crop"] = rules::shot_crop(&vs["to"], sw, sh, w, h);
            }
        }
        let history = self.streams.history.lock_ok();
        for d in s["streams"].as_array_mut().into_iter().flatten() {
            let h = d["id"].as_u64().and_then(|id| history.get(&id));
            d["sent_history"] = h.map_or(json!([]), |h| {
                h.iter()
                    .map(|(at, kbps)| json!({"at": at, "kbps": kbps}))
                    .collect()
            });
        }
    }
}

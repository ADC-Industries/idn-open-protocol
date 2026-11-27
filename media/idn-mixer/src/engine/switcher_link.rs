//! The desk and switchers: their inputs as the desk's, and the desk's feeds to them
//! (§M.22).

use super::*;

impl Console {
    /// Brings switcher output `name` (its full source name) onto two free desk inputs, or
    /// takes it off them (it stays off until brought in again).
    pub fn switcher_input(&self, name: &str, on: bool) -> Result<(), String> {
        use crate::switchers::{Known, free_pair, routes_for};
        let rx = self
            .shared
            .receiver
            .lock_ok()
            .clone()
            .ok_or("no IDN Node: no routing")?;
        let mut saved = self.shared.switchers.lock_ok().clone();
        let cur = saved.outputs.get(name).and_then(|k| k.inputs);
        if on && cur.is_none() {
            let taken: Vec<u16> = saved
                .outputs
                .values()
                .filter_map(|k| k.inputs)
                .flatten()
                .collect();
            let pair =
                free_pair(&rx.routes(), rx.channels(), &taken).ok_or("no two desk inputs free")?;
            send_routes(&self.shared, &routes_for(rx.name(), name, pair, true))?;
            let k = saved.outputs.entry(name.to_owned()).or_insert(Known {
                inputs: None,
                seen: unix_now(),
                confirmed: false,
                takes_feed: false,
            });
            k.inputs = Some(pair);
            k.confirmed = false;
        } else if !on {
            if let Some(pair) = cur {
                send_routes(&self.shared, &routes_for(rx.name(), name, pair, false))?;
            }
            let k = saved.outputs.entry(name.to_owned()).or_insert(Known {
                inputs: None,
                seen: unix_now(),
                confirmed: false,
                takes_feed: false,
            });
            k.inputs = None;
            k.confirmed = false;
        }
        *self.shared.switchers.lock_ok() = saved;
        save_switchers(&self.shared);
        self.changed();
        Ok(())
    }

    /// A switcher output's sound from a desk bus (§M.18): `main`, `aux:N`, `group:N`,
    /// `matrix:N`, or `None` (back to the switcher's own). The bus goes on two free desk
    /// outputs (from the top) if the output patch has it on none; the switcher routes
    /// those to the output itself.
    pub fn switcher_feed(&self, name: &str, bus: Option<&str>) -> Result<(), String> {
        if let Some(bus) = bus {
            let j =
                crate::switchers::bus_pair(bus).ok_or("bus: main, aux:N, group:N or matrix:N")?;
            if feed_outputs(&self.settings(), bus).is_none() {
                let s = self.settings();
                let free: Vec<usize> = (0..s.outputs.len())
                    .rev()
                    .filter(|&o| s.outputs[o].is_empty())
                    .take(2)
                    .collect();
                let [r, l] = free[..] else {
                    return Err("no two free desk outputs for it (Routing → Output patch)".into());
                };
                self.patch_output(l + 1, &format!("bus:{j}"))?;
                self.patch_output(r + 1, &format!("bus:{}", j + 1))?;
            }
        }
        {
            let mut sw = self.shared.switchers.lock_ok();
            match bus {
                Some(b) => sw.feeds.insert(name.to_owned(), b.to_owned()),
                None => sw.feeds.remove(name),
            };
        }
        save_switchers(&self.shared);
        send_feeds(&self.shared);
        self.changed();
        Ok(())
    }

    /// Switchers' outputs as desk inputs by themselves (§M.17): on or off (those already
    /// in stay).
    pub fn switcher_inputs(&self, on: bool) {
        self.shared.switchers.lock_ok().on = on;
        save_switchers(&self.shared);
        self.changed();
    }

    /// Feeds audio this process holds straight into the desk's inputs, with no network hop
    /// and no routing matrix — program sound as a mixer channel, say. `name` identifies the
    /// feed (reuse the same name for the same source, so its lane and drift correction
    /// carry over); `channels` is one `Vec` of samples per channel; they land on desk
    /// inputs from `first` (1-based, as a channel's `source` patch counts). Call it per
    /// block at the desk's rate. It plays by buffer level at the configured latency. A
    /// channel plays a fed input when its `source` points at that desk input and the
    /// channel is shown (without a Node every channel is shown by default).
    pub fn feed(
        &self,
        name: &str,
        first: usize,
        sample_rate: u32,
        channels: &[Vec<f32>],
    ) -> Result<(), String> {
        if first == 0 {
            return Err("desk inputs are 1-based".into());
        }
        if channels.is_empty() {
            return Ok(());
        }
        let last = first - 1 + channels.len();
        let inputs = self.shared.input.channels() as usize;
        if last > inputs {
            return Err(format!(
                "the desk has {inputs} inputs; this feed reaches {last}"
            ));
        }
        let map: Vec<(usize, u16)> = (0..channels.len())
            .map(|i| (i, (first - 1 + i) as u16))
            .collect();
        let mut feeds = self.shared.feeds.lock_ok();
        let lane = feeds.entry(name.to_owned()).or_insert_with(|| {
            self.shared.input.lane(&LaneConfig {
                latency_ms: self.shared.cfg.latency_ms,
                max_correction_ppm: 1000.0,
                ..Default::default()
            })
        });
        lane.push(sample_rate, channels, &map);
        Ok(())
    }

    /// Drops a feed added with [`feed`](Self::feed), so its lane is freed.
    pub fn feed_stop(&self, name: &str) {
        self.shared.feeds.lock_ok().remove(name);
    }
}

//! From the insert point on: a channel's mute, fader and pan, into the main, aux, group,
//! FX and stream buses (its sends at their taps, §M.28).

use super::*;

impl Desk {
    /// From the insert point on: mute, fader, pan; into the main bus, the aux buses
    /// (sends) and the groups.
    // (Inlined: its one caller is the block, on the clock thread; out of line the desk
    // took 2% longer a block.)
    #[inline(always)]
    #[allow(clippy::needless_range_loop)] // parallel lists, as in `process`
    pub(super) fn channel_mix(&mut self, active: &[bool], chain_tier: bool, ramp: f32) {
        let s = &self.settings;
        let n = s.channels.len();
        let secs = self.block as f32 / rate();
        for i in 0..n {
            // (At rest in its bank: its gains kept for when it wakes.)
            if !active[i] && self.resting[i]
                || !active[i] && !chain_tier && self.strips[i].gains.iter().all(|g| *g == 0.0)
            {
                self.direct[i].fill(0.0);
                self.meters.channel_out[i] = Meter::default();
                continue;
            }
            let l = lead(s, i);
            let c = &s.channels[l];
            let pair = c.link && i % 2 == 0 && i + 1 < n || l != i;
            let st = &mut self.strips[i];
            let x = &self.pre[i];
            // Its DCA groups: their faders added to its own, any one's mute muting it.
            let groups = || {
                s.dca
                    .iter()
                    .zip(&c.dca)
                    .filter(|(_, on)| **on)
                    .map(|(g, _)| g)
            };
            // And the automix, duckers and audio-follows-video (§M.12): after the fader,
            // so post-fader sends follow them and pre-fader (monitor) sends don't.
            let level = if c.fader <= -144.0 || groups().any(|g| g.fader <= -144.0) {
                -144.0
            } else {
                (c.fader + groups().map(|g| g.fader).sum::<f32>() + self.auto.gain_db[l])
                    .max(-144.0)
            };
            // Muted (by itself, a DCA or a mute group): nothing goes anywhere.
            let muted = c.mute
                || groups().any(|g| g.mute)
                || s.mute_groups
                    .iter()
                    .zip(&c.mute_groups)
                    .any(|(g, on)| *on && g.on);
            let live = if muted { 0.0 } else { 1.0 };
            let fader = db(level) * live;
            // Where it sits: a pair's sides hard left and right, by the balance.
            let place = |p: f32| {
                // Kept within the range (a pair's balance from OSC or the API at 50 made
                // a gain of −49; NaN made silence of NaN; deep bug run 2026-11-09).
                let p = if p.is_finite() {
                    p.clamp(-1.0, 1.0)
                } else {
                    0.0
                };
                if pair {
                    let (bl, br) = ((1.0 - p).min(1.0), (1.0 + p).min(1.0));
                    if l == i { (bl, 0.0) } else { (0.0, br) }
                } else {
                    pan_law(p)
                }
            };
            let (pl, pr) = place(c.pan);
            // Target gains: main L, main R, each aux's L and R, each group's, each FX bus's,
            // the stream bus's.
            let mut want = [0f32; 2 + 2 * AUX + 2 * GROUPS + 2 * FX + 2];
            // Where each target's sound is taken (§M.28): 0 after the processing (before
            // the fader), 1 the input, 2 before the EQ, 3 after it.
            let mut from = [0u8; 2 + 2 * AUX + 2 * GROUPS + 2 * FX + 2];
            let tap_src = |t: Option<SendTap>| match t {
                Some(SendTap::Input) => 1u8,
                Some(SendTap::PreEq) => 2,
                Some(SendTap::PostEq) => 3,
                _ => 0,
            };
            // Before the fader: every tap but the last; none: as the send or its bus says.
            let pre_of = |sd: &SendTo, bus_pre: bool| match sd.tap {
                Some(SendTap::PostFader) => false,
                Some(_) => true,
                None => sd.pre.unwrap_or(bus_pre),
            };
            if c.main {
                (want[0], want[1]) = (fader * pl, fader * pr);
            }
            for (k, b) in s.aux.iter().enumerate() {
                let sd = &c.sends[k];
                let pre = pre_of(sd, b.pre);
                (from[2 + 2 * k], from[3 + 2 * k]) = (tap_src(sd.tap), tap_src(sd.tap));
                let g = if sd.on {
                    db(sd.level) * live * if pre { 1.0 } else { db(level) }
                } else {
                    0.0
                };
                // A mono aux: both its channels carry the sum, unpanned.
                let (l, r) = match (b.stereo, sd.pan) {
                    (false, _) => (1.0, 1.0),
                    (true, Some(p)) => place(p),
                    (true, None) => (pl, pr),
                };
                want[2 + 2 * k] = g * l;
                want[3 + 2 * k] = g * r;
            }
            for (k, on) in c.groups.iter().enumerate().take(GROUPS) {
                if *on {
                    want[MONITOR + 2 * k] = fader * pl;
                    want[MONITOR + 2 * k + 1] = fader * pr;
                }
            }
            // FX sends: stereo, placed as the channel (or as the send says).
            for (k, b) in s.fx.iter().enumerate() {
                let sd = &c.fx_sends[k];
                if !sd.on {
                    continue;
                }
                let pre = pre_of(sd, b.pre);
                let t = tap_src(sd.tap);
                (
                    from[MONITOR + 2 * GROUPS + 2 * k],
                    from[MONITOR + 2 * GROUPS + 2 * k + 1],
                ) = (t, t);
                let g = db(sd.level) * live * if pre { 1.0 } else { db(level) };
                let (l, r) = sd.pan.map_or((pl, pr), place);
                want[MONITOR + 2 * GROUPS + 2 * k] = g * l;
                want[MONITOR + 2 * GROUPS + 2 * k + 1] = g * r;
            }
            // The stream bus: stereo, placed as the channel (or as the send says).
            let sd = &c.stream;
            if sd.on {
                let pre = pre_of(sd, s.stream.pre);
                let t = tap_src(sd.tap);
                (
                    from[MONITOR + 2 * GROUPS + 2 * FX],
                    from[MONITOR + 2 * GROUPS + 2 * FX + 1],
                ) = (t, t);
                let g = db(sd.level) * live * if pre { 1.0 } else { db(level) };
                let (l, r) = sd.pan.map_or((pl, pr), place);
                want[MONITOR + 2 * GROUPS + 2 * FX] = g * l;
                want[MONITOR + 2 * GROUPS + 2 * FX + 1] = g * r;
            }
            for (j, (&w, prev)) in want.iter().zip(st.gains.iter_mut()).enumerate() {
                // Nothing to this bus, now or last block: skipped (most of a channel's
                // targets).
                if w == 0.0 && *prev == 0.0 {
                    continue;
                }
                let bus = &mut self.buses[gain_target(j)];
                let x: &[f32] = match from[j] {
                    1 => &self.tap_in[i],
                    2 => &self.tap_pre_eq[i],
                    3 => &self.tap_post_eq[i],
                    _ => x,
                };
                if w == *prev {
                    for (o, &v) in bus.iter_mut().zip(x.iter()) {
                        *o += v * w;
                    }
                    continue;
                }
                // Ramping: each sample's gain from its index (no running sum, so the
                // loop vectorises), straight to the target over the blocks the desk's
                // ramp has left (about 2 ms for a change, 0.2 s for a snapshot: `Desk::set`).
                let step = (w - *prev) / (self.block as f32 * ramp);
                let g0 = *prev;
                for (k, (o, &v)) in bus.iter_mut().zip(x.iter()).enumerate() {
                    *o += v * (g0 + step * (k + 1) as f32);
                }
                *prev = if ramp > 1.0 {
                    g0 + step * self.block as f32
                } else {
                    w
                };
            }
            // The post-fader tap (direct outs, the meter) ramped as the sends are (F7-7).
            {
                let st = &mut self.strips[i];
                let g0 = st.post_gain.unwrap_or(fader);
                let step = (fader - g0) / (self.block as f32 * ramp);
                for (k, (p, &v)) in self.post.iter_mut().zip(x.iter()).enumerate() {
                    *p = v * (g0 + step * (k + 1) as f32);
                }
                st.post_gain = Some(if ramp > 1.0 {
                    g0 + step * self.block as f32
                } else {
                    fader
                });
            }
            self.meters.channel_out[i].take(&self.post, secs);
            self.direct[i].copy_from_slice(&self.post);
            // Soloed: on the headphones, before its fader (PFL) or after it and its pan
            // (AFL).
            // (Only a channel on the desk: one parked or hidden while soloed is still
            // mixed here, as silence, and put that on the headphones for good: lab
            // 477(c).)
            if c.solo && active[i] {
                self.solo_heard = true;
                let (sl, sr) = if s.solo_afl {
                    (fader * pl, fader * pr)
                } else if pair {
                    if l == i { (1.0, 0.0) } else { (0.0, 1.0) }
                } else {
                    pan_law(s.channels[i].pan)
                };
                for (k, &v) in x.iter().enumerate() {
                    self.solo[0][k] += v * sl;
                    self.solo[1][k] += v * sr;
                }
            }
        }
    }
}

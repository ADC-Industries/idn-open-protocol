//! A desk's block: every channel through its chain into the buses, the buses through
//! theirs, the meters; and its automation (snapshots recalled over time).

use super::*;

impl Desk {
    /// One block: `input[i]` (desk input i, self.block samples each; missing inputs are
    /// silence) to `out` (the desk outputs, by the output patch).
    ///
    /// At rest: once every input and output has been exactly silent for [`REST_AFTER`]
    /// (the meters and loudness windows settled, every tail died away), with no plugins
    /// and no fader ramp running, the desk outputs silence without running its chains, until a
    /// sample arrives or a setting changes (an idle desk took ~5% of a core). A channel's
    /// fade-in waits for its sound, as it does parked.
    pub fn process(&mut self, input: &[Vec<f32>], out: &mut [Vec<f32>]) {
        // A round-trip test (§M.28.2): its input listened to, its output after the block.
        let testing = self.round_trip_active();
        if testing {
            self.round_trip_listen(input);
        }
        self.process_inner(input, out);
        if testing {
            self.round_trip_send(out);
        }
    }

    fn process_inner(&mut self, input: &[Vec<f32>], out: &mut [Vec<f32>]) {
        let quiet_in = input.iter().all(|c| c.iter().all(|&v| v == 0.0));
        let settings = Arc::as_ptr(&self.settings) as usize;
        let still =
            quiet_in && settings == self.quiet_settings && self.chains.is_empty() && self.ramp <= 1;
        if still && self.quiet_blocks >= self.rest_after {
            for o in out.iter_mut() {
                o.fill(0.0);
            }
            return;
        }
        self.process_block(input, out);
        let quiet_out = out.iter().all(|o| o.iter().all(|&v| v == 0.0));
        if still && quiet_out {
            self.quiet_blocks = self.quiet_blocks.saturating_add(1);
        } else {
            self.quiet_blocks = 0;
            self.quiet_settings = settings;
        }
    }

    // A channel's state is in parallel lists (settings, strips, signals, meters).
    #[allow(clippy::needless_range_loop)]
    fn process_block(&mut self, input: &[Vec<f32>], out: &mut [Vec<f32>]) {
        let secs = self.block as f32 / rate();
        // (A clone of the settings' Arc, so that the stages below can take `self`.)
        let settings = self.settings.clone();
        let s = &*settings;
        let n = s.channels.len();
        for b in &mut self.buses {
            b.fill(0.0);
        }
        for b in &mut self.solo {
            b.fill(0.0);
        }
        // Soloed and heard this block (set in `channel_mix`): a soloed channel at rest or
        // parked put silence on the headphones for good (deep bug run 2026-11-09).
        self.solo_heard = false;
        let ramp = self.ramp.max(1) as f32;
        // Last block's plugin results, if they came in time (else that block goes on dry,
        // counted late). At most one block is outstanding: if the last didn't come back,
        // this one isn't sent (the host is behind).
        let seq = self.seq;
        self.seq += 1;
        let plugins = self.plugins.clone().filter(|_| !self.chains.is_empty());
        let results = match (&plugins, self.sent) {
            (Some(p), true) => p.take(seq - 1, plugin_wait(self.block)),
            _ => None,
        };
        let got = results.is_some();
        let send = plugins.is_some() && (!self.sent || got);
        let mut results = results.unwrap_or_default();
        // A plugin's NaN or infinity would stay in every filter after it (the bus silent
        // or full scale for the rest of the run, bypassed or not): silence instead.
        for v in results.iter_mut().flat_map(|r| r.data.iter_mut()) {
            if !v.is_finite() {
                *v = 0.0;
            }
        }
        let results = results;
        let block = self.block;
        let result = |id: u32| {
            results
                .iter()
                .find(|r| r.chain == id && r.frames as usize == block)
        };
        let expect = std::mem::take(&mut self.expect);
        let late = |id: u32| expect.contains(&id) && result(id).is_none();
        // The chains' signals this block, sent at the end: into the buffers of blocks
        // before (no allocation per block).
        let mut out_chains = Outgoing {
            buf: std::mem::take(&mut self.outgoing),
            n: 0,
            send,
            block,
        };
        // (Kept between blocks: nothing allocated per block on the clock thread.)
        let mut active = std::mem::take(&mut self.active_buf);
        active.clear();
        active.extend((0..n).map(|i| self.active(i)));
        // Banks of four at rest (§M.24): their channels skip their processing this block.
        self.rest_banks(input, &active);
        let mut run = std::mem::take(&mut self.run_buf);
        run.clear();
        run.extend((0..n).map(|i| active[i] && !self.resting[i]));
        // Noise suppression adds two frames (20 ms): while any channel runs it, the others'
        // input delay grows by as much, so every channel stays in line.
        let any_denoise = (0..n).any(|i| active[i] && s.channels[lead(s, i)].denoise);
        self.channel_inputs(input, &active, &run, any_denoise);
        // Inserts before the processing (§M.28): those channels' processing runs on what
        // their inserts give back, after them; the rest now.
        let mut pre_ins = std::mem::take(&mut self.pre_ins_buf);
        pre_ins.clear();
        pre_ins.extend((0..n).map(|i| {
            let l = lead(s, i);
            run[i]
                && s.channels[l].insert_point == InsertPoint::Pre
                && self.chains.contains_key(&channel_chain(l))
        }));
        let any_pre = pre_ins.iter().any(|&p| p);
        if any_pre {
            let mut now = std::mem::take(&mut self.post_ins_buf);
            now.clear();
            now.extend(run.iter().zip(&pre_ins).map(|(&r, &p)| r && !p));
            self.channel_dynamics(&run, &now, true);
            self.post_ins_buf = now;
        } else {
            self.channel_dynamics(&run, &run, true);
        }
        // The channels' inserts: this block's signal out, the last's results in.
        for i in 0..n {
            let c = &s.channels[i];
            if i % 2 == 1 && s.channels[i - 1].link {
                continue;
            }
            let count = if c.link && i + 1 < n { 2 } else { 1 };
            out_chains.add(&self.chains, channel_chain(i), &self.at[i..i + count]);
        }
        let chain_tier = self.chains.range(0..MAIN_CHAIN).next().is_some();
        let longest = self
            .chains
            .range(0..MAIN_CHAIN)
            .map(|(_, &l)| l)
            .max()
            .unwrap_or(0);
        for i in 0..n {
            let l = lead(s, i);
            if !chain_tier {
                self.pre[i].copy_from_slice(&self.at[i]);
                continue;
            }
            let id = channel_chain(l);
            let latency = self.chains.get(&id).copied();
            self.pre[i].copy_from_slice(&self.prev[i]);
            self.dry[i].run(&mut self.pre[i], latency.unwrap_or(0) as usize);
            match result(id).filter(|r| (r.channels as usize) > i - l) {
                Some(r) => self.pre[i].copy_from_slice(r.channel(i - l)),
                None if i == l && late(id) => self.meters.channel_late[i] += 1,
                None => {}
            }
            self.delay[i].run(&mut self.pre[i], (longest - latency.unwrap_or(0)) as usize);
        }
        // The inserts-first channels' processing, on what came back (their input, sent to
        // the chain, stays in `at` for the next block's dry path).
        if any_pre {
            for i in (0..n).filter(|&i| pre_ins[i]) {
                std::mem::swap(&mut self.at[i], &mut self.pre[i]);
            }
            self.channel_dynamics(&run, &pre_ins, false);
            for i in (0..n).filter(|&i| pre_ins[i]) {
                std::mem::swap(&mut self.at[i], &mut self.pre[i]);
            }
        }
        self.pre_ins_buf = pre_ins;
        std::mem::swap(&mut self.at, &mut self.prev);
        Desk::automate(
            s,
            &self.pre,
            &mut self.auto,
            &self.on_air,
            &mut self.meters.channel_auto,
            &run,
            secs,
        );
        self.channel_mix(&run, chain_tier, ramp);
        if self.ramp > 1 {
            self.ramp -= 1;
        }
        // Talkback: the talkback channel (after its processing, before its fader and
        // mute) to the chosen aux buses and the main bus, faded in and out over a block.
        let tb = &s.talkback;
        let src = tb.channel.checked_sub(1).filter(|&c| c < n);
        let target = if tb.on && src.is_some() {
            db(tb.level)
        } else {
            0.0
        };
        if let Some(src) = src.filter(|_| target > 0.0 || self.talk > 0.0) {
            // (Over the desk's ramp, as the faders.)
            let step = (target - self.talk) / (self.block as f32 * ramp);
            let dests = (0..AUX)
                .filter(|&k| tb.aux.get(k).copied().unwrap_or(false))
                .map(|k| 2 + 2 * k)
                .chain(tb.main.then_some(0));
            for d in dests {
                let mut g = self.talk;
                for k in 0..self.block {
                    g += step;
                    let v = self.pre[src][k] * g;
                    self.buses[d][k] += v;
                    self.buses[d + 1][k] += v;
                }
            }
        }
        self.talk = if ramp > 1.0 {
            self.talk + (target - self.talk) / ramp
        } else {
            target
        };
        // The groups: EQ, dynamics, their inserts, fader; then into the main bus. While
        // the groups have plugins, the channels' own main and aux sums wait for them.
        // A group, aux bus or matrix mix nothing feeds is skipped (its sum is silence).
        // (One pass over the channels for every bus: each bus looking over every channel
        // and its gains was 32 × 32 scans a block, a fixed cost 1500 times a second at 32
        // samples.)
        let (mut group_fed, mut aux_fed, mut fx_fed) = ([false; GROUPS], [false; AUX], [false; FX]);
        let mut stream_fed = false;
        for i in 0..n {
            if !(run[i] || !self.resting[i] && self.strips[i].gains.iter().any(|g| *g != 0.0)) {
                continue;
            }
            let c = &s.channels[lead(s, i)];
            for (f, on) in group_fed.iter_mut().zip(&c.groups) {
                *f |= *on;
            }
            for (f, sd) in aux_fed.iter_mut().zip(&c.sends) {
                *f |= sd.on;
            }
            for (f, sd) in fx_fed.iter_mut().zip(&c.fx_sends) {
                *f |= sd.on;
            }
            stream_fed |= c.stream.on;
        }
        for (k, f) in aux_fed.iter_mut().enumerate() {
            *f |= self.talk > 0.0 && tb.aux.get(k).copied().unwrap_or(false);
        }
        for (k, g) in s.groups.iter().enumerate() {
            if group_fed[k] {
                bus_dynamics(g, &mut self.groups[k], group_out(k), &mut self.buses);
            }
            out_chains.add(
                &self.chains,
                GROUP_CHAIN + k as u32,
                &self.buses[group_out(k)..group_out(k) + 2],
            );
        }
        // The FX buses alike: their plugins (reverb, delay) in the same stage.
        for (k, f) in s.fx.iter().enumerate() {
            // Its built-in effect (§M.14), fed or still ringing out.
            let ringing = self.fx[k].fx.running(&f.effect, fx_fed[k]);
            if ringing {
                let o = fx_out(k);
                let (a, b) = self.buses.split_at_mut(o + 1);
                self.fx[k].fx.run(&f.effect, rate(), &mut a[o], &mut b[0]);
            }
            if fx_fed[k] || ringing {
                bus_dynamics(f, &mut self.fx[k], fx_out(k), &mut self.buses);
            }
            out_chains.add(
                &self.chains,
                FX_CHAIN + k as u32,
                &self.buses[fx_out(k)..fx_out(k) + 2],
            );
        }
        let groups: [usize; GROUPS + FX] = std::array::from_fn(|i| {
            if i < GROUPS {
                1 + AUX + i
            } else {
                1 + AUX + GROUPS + MATRIX + (i - GROUPS)
            }
        });
        let lag = run_stage(
            &mut self.group_stage,
            &groups,
            &self.chains,
            &result,
            &late,
            &mut self.buses,
            &mut self.meters.bus_late,
        );
        self.lag_sums.run(&mut self.buses[..MONITOR], lag);
        self.lag_stream_sums
            .run(&mut self.buses[STREAM..STREAM + 2], lag);
        // Groups and FX returns: their faders, then into the main bus.
        let returns = s
            .groups
            .iter()
            .zip(self.groups.iter_mut())
            .enumerate()
            .map(|(k, (g, st))| (g, st, group_out(k)))
            .chain(
                s.fx.iter()
                    .zip(self.fx.iter_mut())
                    .enumerate()
                    .map(|(k, (f, st))| (f, st, fx_out(k))),
            );
        for (g, st, o) in returns {
            bus_fader(g, st, o, &mut self.buses, ramp);
            if g.to_main {
                let (main, rest) = self.buses.split_at_mut(2);
                for side in 0..2 {
                    for (m, &v) in main[side].iter_mut().zip(&rest[o - 2 + side]) {
                        *m += v;
                    }
                }
            }
        }
        // The main and aux buses: EQ, dynamics, inserts, fader. The groups wait for them.
        bus_dynamics(&s.main, &mut self.main, 0, &mut self.buses);
        for (k, b) in s.aux.iter().enumerate() {
            if aux_fed[k] {
                bus_dynamics(b, &mut self.aux[k], 2 + 2 * k, &mut self.buses);
            }
        }
        if stream_fed {
            bus_dynamics(&s.stream, &mut self.stream, STREAM, &mut self.buses);
        }
        for k in 0..=AUX {
            out_chains.add(
                &self.chains,
                MAIN_CHAIN + k as u32,
                &self.buses[2 * k..2 * k + 2],
            );
        }
        out_chains.add(&self.chains, STREAM_CHAIN, &self.buses[STREAM..STREAM + 2]);
        // (The stream bus in the main and aux stage: its chain, and its lag alike.)
        let mains: [usize; AUX + 2] =
            std::array::from_fn(|i| if i <= AUX { i } else { STREAM_STRIP });
        let lag = run_stage(
            &mut self.bus_stage,
            &mains,
            &self.chains,
            &result,
            &late,
            &mut self.buses,
            &mut self.meters.bus_late,
        );
        self.lag_groups
            .run(&mut self.buses[group_out(0)..group_out(GROUPS)], lag);
        self.lag_fx.run(&mut self.buses[fx_out(0)..fx_out(FX)], lag);
        bus_fader(&s.main, &mut self.main, 0, &mut self.buses, ramp);
        // Dim and mono, after the main fader, ramped as the fader is (F7-7).
        // (The dim's amount; talkback's own dim of the main while it is on, §M.28.)
        let (dim, mono) = (
            if s.main.dim { db(s.main.dim_db) } else { 1.0 }
                * if s.talkback.on && s.talkback.dim_main < 0.0 {
                    db(s.talkback.dim_main)
                } else {
                    1.0
                },
            if s.main.mono { 1.0 } else { 0.0 },
        );
        if dim != 1.0 || mono != 0.0 || self.main.dim != 1.0 || self.main.mono != 0.0 {
            let (l, r) = self.buses.split_at_mut(1);
            let n = l[0].len().max(1) as f32;
            // (As `bus_fader`: over the blocks the desk's ramp has left.)
            let steps = n * ramp;
            let (dg, dm) = (
                (dim - self.main.dim) / steps,
                (mono - self.main.mono) / steps,
            );
            let (mut g, mut m) = (self.main.dim, self.main.mono);
            for (a, b) in l[0].iter_mut().zip(r[0].iter_mut()) {
                g += dg;
                m += dm;
                let c = (*a + *b) * 0.5;
                *a = (*a + (c - *a) * m) * g;
                *b = (*b + (c - *b) * m) * g;
            }
            let reached = ramp <= 1.0;
            self.main.dim = if reached { dim } else { g };
            self.main.mono = if reached { mono } else { m };
        }
        for (k, b) in s.aux.iter().enumerate() {
            bus_fader(b, &mut self.aux[k], 2 + 2 * k, &mut self.buses, ramp);
        }
        bus_fader(&s.stream, &mut self.stream, STREAM, &mut self.buses, ramp);
        // The matrix mixes: their sources at their levels, then EQ and dynamics.
        for (k, m) in s.matrix.iter().enumerate() {
            let o = matrix_out(k);
            // Its sources lie before it (main, auxes, groups) and after it (the FX returns
            // follow the matrix mixes): only the part before was read, and an FX return
            // indexed past it, stopping the desk's sound.
            let (before, rest) = self.buses.split_at_mut(o);
            let (dst, after) = rest.split_at_mut(2);
            // Each source's level and on/off ramped as faders are (F7-7).
            let gains = &mut self.matrix[k].src_gain;
            for (j, from) in m.sources.iter().enumerate().take(MATRIX_SOURCES) {
                let target = if from.on { db(from.level) } else { 0.0 };
                let g0 = gains[j];
                if g0 == 0.0 && target == 0.0 {
                    continue;
                }
                let n = dst[0].len().max(1) as f32;
                let step = (target - g0) / (n * ramp);
                for side in 0..2 {
                    let at = source_out(j) + side;
                    let src = if at < o {
                        &before[at]
                    } else {
                        &after[at - o - 2]
                    };
                    let mut g = g0;
                    for (d, &v) in dst[side].iter_mut().zip(src) {
                        g += step;
                        *d += v * g;
                    }
                }
                gains[j] = if ramp > 1.0 { g0 + step * n } else { target };
            }
            if m.sources.iter().any(|f| f.on) {
                bus_dynamics(m, &mut self.matrix[k], o, &mut self.buses);
            }
            out_chains.add(&self.chains, MATRIX_CHAIN + k as u32, &self.buses[o..o + 2]);
        }
        // The headphones: the soloed channels and the buses listened to, else the main
        // mix; then their EQ and dynamics.
        let mut listened_at = [0usize; BUS_STRIPS];
        let mut n_listened = 0;
        for k in (0..BUS_STRIPS - 1).filter(|&k| s.bus(k).is_some_and(|b| b.listen)) {
            listened_at[n_listened] = k;
            n_listened += 1;
        }
        let listened = &listened_at[..n_listened];
        {
            let (src, rest) = self.buses.split_at_mut(MONITOR);
            let (mon, after) = rest.split_at_mut(2);
            if self.solo_heard || !listened.is_empty() {
                for (m, solo) in mon.iter_mut().zip(&self.solo) {
                    m.copy_from_slice(solo);
                }
                for &k in listened {
                    let first = bus_strip(k).1;
                    for side in 0..2 {
                        let from = if first < MONITOR {
                            &src[first + side]
                        } else {
                            &after[first - MONITOR - 2 + side]
                        };
                        for (m, &v) in mon[side].iter_mut().zip(from) {
                            *m += v;
                        }
                    }
                }
            } else {
                mon[0].copy_from_slice(&src[0]);
                mon[1].copy_from_slice(&src[1]);
            }
        }
        bus_dynamics(&s.monitor, &mut self.monitor, MONITOR, &mut self.buses);
        out_chains.add(
            &self.chains,
            MONITOR_CHAIN,
            &self.buses[MONITOR..MONITOR + 2],
        );
        // The last stage's inserts; the other buses wait for them.
        let last: [usize; MATRIX + 1] = std::array::from_fn(|i| {
            if i < MATRIX {
                1 + AUX + GROUPS + i
            } else {
                BUS_STRIPS - 1
            }
        });
        let lag = run_stage(
            &mut self.out_stage,
            &last,
            &self.chains,
            &result,
            &late,
            &mut self.buses,
            &mut self.meters.bus_late,
        );
        self.lag_rest.run(&mut self.buses[..MONITOR], lag);
        self.lag_rest_groups
            .run(&mut self.buses[group_out(0)..group_out(GROUPS)], lag);
        self.lag_rest_fx
            .run(&mut self.buses[fx_out(0)..fx_out(FX)], lag);
        self.lag_rest_stream
            .run(&mut self.buses[STREAM..STREAM + 2], lag);
        for (k, m) in s.matrix.iter().enumerate() {
            bus_fader(m, &mut self.matrix[k], matrix_out(k), &mut self.buses, ramp);
        }
        bus_fader(
            &s.monitor,
            &mut self.monitor,
            MONITOR,
            &mut self.buses,
            ramp,
        );
        for (j, b) in self.buses.iter().enumerate() {
            self.meters.outputs[j].take(b, secs);
        }
        let strips = std::iter::once(&self.main)
            .chain(&self.aux)
            .chain(&self.groups)
            .chain(&self.matrix)
            .chain(&self.fx)
            .chain(std::iter::once(&self.stream))
            .chain(std::iter::once(&self.monitor));
        for (g, st) in self.meters.bus_gr.iter_mut().zip(strips) {
            *g = (st.comp.reduction, st.lim.reduction);
        }
        self.loudness.take(&self.buses[0], &self.buses[1]);
        (self.meters.lufs_m, self.meters.lufs_s) =
            (self.loudness.momentary(), self.loudness.short_term());
        self.stream_loudness
            .take(&self.buses[STREAM], &self.buses[STREAM + 1]);
        (self.meters.stream_lufs.0, self.meters.stream_lufs.1) = (
            self.stream_loudness.momentary(),
            self.stream_loudness.short_term(),
        );
        // Integrated: a few times a second is plenty.
        if self.seq.is_multiple_of(50) {
            self.meters.lufs_i = self.loudness.integrated();
            self.meters.stream_lufs.2 = self.stream_loudness.integrated();
        }
        // This block to the plugins.
        if let Some(p) = &plugins {
            if send {
                p.submit(seq, &out_chains.buf[..out_chains.n]);
            }
            self.sent = send;
            let mut e = expect;
            e.clear();
            e.extend(self.chains.keys().copied());
            self.expect = e;
        } else {
            self.sent = false;
        }
        self.outgoing = out_chains.buf;
        // The output patch, read with each new settings (`set_normalised`); here only if
        // the outputs changed since.
        let key = self.settings_gen;
        if self.patch.0 != key || self.patch.1.len() != self.outputs {
            self.patch = (
                key,
                (0..self.outputs)
                    .map(|o| Tap::parse(s.outputs.get(o).map_or("", String::as_str)))
                    .collect(),
            );
        }
        for (o, dst) in out.iter_mut().enumerate().take(self.outputs) {
            dst.resize(self.block, 0.0);
            let tap = self.patch.1[o];
            let from: Option<&Vec<f32>> = match tap {
                Tap::Bus(j) => self.buses.get(j),
                Tap::Pre(c) => self.pre.get(c.wrapping_sub(1)),
                Tap::Input(c) => self.tap_in.get(c.wrapping_sub(1)),
                Tap::PreEq(c) => self.tap_pre_eq.get(c.wrapping_sub(1)),
                Tap::PostEq(c) => self.tap_post_eq.get(c.wrapping_sub(1)),
                Tap::Post(c) => self.direct.get(c.wrapping_sub(1)),
                Tap::In(i) => input.get(i.wrapping_sub(1)),
                Tap::None => None,
            };
            match from {
                Some(v) if v.len() >= self.block => dst.copy_from_slice(&v[..self.block]),
                _ => dst.fill(0.0),
            }
            // A raw input tap is cleaned as a strip's input is: no NaN or infinity from a
            // float source reaches a card or the network.
            if matches!(tap, Tap::In(_)) {
                for v in dst.iter_mut() {
                    *v = if v.is_finite() {
                        v.clamp(-64.0, 64.0)
                    } else {
                        0.0
                    };
                }
            }
            // Its delay (§M.15), to the sample.
            let ms = s.output_delay.get(o).copied().unwrap_or(0.0);
            if let Some(d) = self.out_delay.get_mut(o) {
                d.run(dst, (ms * rate() / 1000.0).round() as usize);
            }
        }
        self.active_buf = active;
        self.run_buf = run;
    }

    /// Per channel, up to the insert point: its input, trim, delay, filters, gate, EQ,
    /// compressor, limiter. A parked channel is silence, with nothing run.
    // (Inlined: its one caller is the block, on the clock thread; out of line the desk
    // took 2% longer a block.)
    #[inline(always)]
    #[allow(clippy::needless_range_loop)] // parallel lists, as in `process`
    fn channel_inputs(
        &mut self,
        input: &[Vec<f32>],
        active: &[bool],
        run: &[bool],
        any_denoise: bool,
    ) {
        let s = &self.settings;
        let n = s.channels.len();
        let secs = self.block as f32 / rate();
        let ramp = self.ramp.max(1) as f32;
        for i in 0..n {
            // At rest in its bank (§M.24): silence through, its strip as it was; muted, its
            // input still metered (cheaply: its source, trimmed).
            if active[i] && !run[i] {
                self.at[i].fill(0.0);
                // (Its taps too: a send or an output taken there read the last block it
                // ran; bug run P, bug review 2.)
                self.tap_in[i].fill(0.0);
                self.tap_pre_eq[i].fill(0.0);
                self.tap_post_eq[i].fill(0.0);
                self.meters.channel_gr[i] = (0.0, 0.0, 0.0, 0.0);
                self.meters.channel_out[i] = Meter::default();
                if self.rested_muted[i] {
                    let own = &s.channels[i];
                    let trim = db(s.channels[lead(s, i)].trim);
                    let src = if self.on_b[i] {
                        own.input_b
                    } else {
                        own.source.unwrap_or(i + 1)
                    };
                    match input
                        .get(src.wrapping_sub(1))
                        .filter(|v| v.len() >= self.block)
                    {
                        Some(v) => {
                            let x = &mut self.at[i];
                            for (o, &v) in x.iter_mut().zip(&v[..self.block]) {
                                *o = if v.is_finite() {
                                    v.clamp(-64.0, 64.0) * trim
                                } else {
                                    0.0
                                };
                            }
                            self.meters.channel_in[i].take(&self.at[i], secs);
                            self.at[i].fill(0.0);
                        }
                        None => self.meters.channel_in[i] = Meter::default(),
                    }
                } else {
                    self.meters.channel_in[i] = Meter::default();
                }
                continue;
            }
            if !active[i] {
                // (Its taps with it: an output patched to one played its last block over
                // and over.)
                self.at[i].fill(0.0);
                self.tap_in[i].fill(0.0);
                self.tap_pre_eq[i].fill(0.0);
                self.tap_post_eq[i].fill(0.0);
                self.meters.channel_in[i] = Meter::default();
                self.meters.channel_gr[i] = (0.0, 0.0, 0.0, 0.0);
                self.on_b[i] = false;
                self.strips[i].fade_in = channel_fade();
                continue;
            }
            // A linked pair's second channel uses the first's settings.
            let c = &s.channels[lead(s, i)];
            let st = &mut self.strips[i];
            let x = &mut self.at[i];
            let own = &s.channels[i];
            let a = own.source.unwrap_or(i + 1);
            let b = own.input_b;
            let alive = |k: usize| self.live.get(k.wrapping_sub(1)).copied().unwrap_or(false);
            let use_b = b > 0 && (own.use_b || own.failover && !alive(a) && alive(b));
            self.on_b[i] = use_b;
            let source = if use_b { b } else { a };
            match input.get(source.wrapping_sub(1)) {
                // A source's NaN or infinity (IDF3 float carries them as they are) or an
                // absurd level (a fowt scale of 1e38) would stay in every filter of the
                // strip: silence, and +36 dBFS at most.
                Some(v) if v.len() >= self.block => {
                    for (o, &s) in x.iter_mut().zip(&v[..self.block]) {
                        *o = if s.is_finite() {
                            s.clamp(-64.0, 64.0)
                        } else {
                            0.0
                        };
                    }
                }
                _ => x.fill(0.0),
            }
            // Coming on: its input faded in from silence (every tap and send after it).
            if st.fade_in > 0 {
                for v in x.iter_mut() {
                    let all = channel_fade().max(st.fade_in);
                    let done = all - st.fade_in;
                    *v *= (done + 1) as f32 / all as f32;
                    st.fade_in = st.fade_in.saturating_sub(1);
                    if st.fade_in == 0 {
                        break;
                    }
                }
            }
            self.tap_in[i].copy_from_slice(x);
            // Ramped over 2 ms at least (F7-7).
            let trim = db(c.trim) * if c.polarity { -1.0 } else { 1.0 };
            let from = st.trim_now.unwrap_or(trim);
            if from == trim {
                for v in x.iter_mut() {
                    *v *= trim;
                }
            } else {
                let steps = (x.len() as f32 * ramp).max(ramp_samples() as f32);
                let step = (trim - from) / steps;
                let mut g = from;
                for v in x.iter_mut() {
                    g += step;
                    if (step > 0.0 && g > trim) || (step < 0.0 && g < trim) {
                        g = trim;
                    }
                    *v *= g;
                }
                st.trim_now = Some(g);
            }
            if from == trim {
                st.trim_now = Some(trim);
            }
            // The suppression switched once the channel has faded out.
            if st.nr_on != c.denoise && st.nr_gain <= 0.0 {
                st.nr_on = c.denoise;
                st.nr_wait = if st.nr_on {
                    // What the suppressor still holds from before (its whole delay at
                    // this rate: bug run J2-1, at 44.1 and 96 kHz that is more than the
                    // network's 30 ms) and a frame more.
                    idn_dsp::denoise::latency(rate_hz())
                        + idn_dsp::denoise::FRAME * rate_hz() as usize / 48_000
                } else {
                    0
                };
            }
            let line_up = if any_denoise && !st.nr_on {
                // The suppression's delay at the desk's rate (§N.40: converted to and from
                // the network's 48 kHz at another).
                idn_dsp::denoise::latency(rate_hz())
            } else {
                0
            };
            st.delay.run(
                x,
                (c.delay.clamp(0.0, 250.0) * rate() / 1000.0) as usize + line_up,
            );
            // (Kept once made: dropped here, its memory was freed on the clock thread;
            // F7-8.)
            if st.nr_on {
                // The dry sound alongside, delayed as the suppression delays it (kept
                // running, so a change of the amount blends at once; §M.28).
                let dry = &mut self.nr_scratch[..x.len()];
                dry.copy_from_slice(x);
                st.nr_dry.run(dry, idn_dsp::denoise::latency(rate_hz()));
                st.denoise
                    .get_or_insert_with(|| idn_dsp::denoise::RateDenoiser::new(rate_hz()))
                    .run(x);
                let amount = (c.denoise_amount / 100.0).clamp(0.0, 1.0);
                if amount < 1.0 {
                    for (v, &d) in x.iter_mut().zip(dry.iter()) {
                        *v = d + (*v - d) * amount;
                    }
                }
            }
            // The switch's fade: 5 ms out, then (after the wait) 5 ms in.
            if st.nr_on != c.denoise || st.nr_gain < 1.0 || st.nr_wait > 0 {
                let step = 1.0 / (0.005 * rate());
                for v in x.iter_mut() {
                    if st.nr_on != c.denoise {
                        st.nr_gain = (st.nr_gain - step).max(0.0);
                    } else if st.nr_wait > 0 {
                        st.nr_wait -= 1;
                    } else {
                        st.nr_gain = (st.nr_gain + step).min(1.0);
                    }
                    *v *= st.nr_gain;
                }
            }
            self.meters.channel_in[i].take(x, secs);
        }
    }

    /// The automix's, duckers' and audio-follows-video's gains this block (§M.12), from
    /// each channel's sound before its fader (`pre`), into `st.gain_db`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn automate(
        s: &Settings,
        pre: &[Vec<f32>],
        st: &mut AutoState,
        on_air: &Option<std::collections::BTreeSet<String>>,
        meter: &mut [f32],
        active: &[bool],
        secs: f32,
    ) {
        let n = s.channels.len();
        // Each channel's power this block (a pair's two sides together, on its first).
        let block_pow = |i: usize| -> f32 {
            let l = lead(s, i);
            let mut p: f32 = pre[i].iter().map(|v| v * v).sum::<f32>() / pre[i].len().max(1) as f32;
            if s.channels[l].link && l == i && i + 1 < n {
                p = (p + pre[i + 1].iter().map(|v| v * v).sum::<f32>()
                    / pre[i + 1].len().max(1) as f32)
                    / 2.0;
            }
            p
        };
        let (a_share, a_key) = ((-secs / 0.030).exp(), (-secs / 0.010).exp());
        // (Kept in the state between blocks: no allocation per block.)
        let mut gain = std::mem::take(&mut st.gain_scratch);
        gain.clear();
        gain.resize(n, 0.0);
        for i in (0..n).filter(|&i| lead(s, i) == i) {
            let p = if active[i] { block_pow(i) } else { 0.0 };
            st.share_pow[i] = a_share * st.share_pow[i] + (1.0 - a_share) * p;
            st.key_pow[i] = a_key * st.key_pow[i] + (1.0 - a_key) * p;
        }
        // The automix: each member's share of the members' (weighted) power, as a gain;
        // all quiet, an equal share each (the number of open mics' attenuation).
        if s.automix {
            // (Walked twice, not collected: no allocation on the clock each block, deep
            // bug run 2026-11-09.)
            let members = || {
                (0..n).filter(|&i| lead(s, i) == i).filter(|&i| {
                    let c = &s.channels[i];
                    c.automix && active[i] && !c.mute && c.fader > -144.0
                })
            };
            const FLOOR: f32 = 1e-9;
            let w = |i: usize| 10f32.powf(s.channels[i].automix_weight / 10.0);
            let total: f32 = members().map(|i| w(i) * st.share_pow[i] + FLOOR).sum();
            for i in members() {
                let share = (w(i) * st.share_pow[i] + FLOOR) / total;
                gain[i] += 10.0 * share.log10();
            }
        }
        // The duckers: the loudest key over the threshold holds the targets down.
        for (k, d) in s.duckers.iter().enumerate().take(DUCKERS) {
            if !d.on {
                st.duck_db[k] = 0.0;
                st.duck_hold[k] = 0.0;
                continue;
            }
            let key = (0..n)
                .filter(|&i| d.keys.get(i).copied().unwrap_or(false))
                .map(|i| st.key_pow[lead(s, i)])
                .fold(0.0f32, f32::max);
            let over = 10.0 * key.max(1e-12).log10() > d.threshold;
            if over {
                st.duck_hold[k] = d.hold / 1000.0;
            } else {
                st.duck_hold[k] = (st.duck_hold[k] - secs).max(0.0);
            }
            let target = if over || st.duck_hold[k] > 0.0 {
                d.depth
            } else {
                0.0
            };
            let tau = if target < st.duck_db[k] {
                d.attack
            } else {
                d.release
            } / 1000.0;
            st.duck_db[k] += (target - st.duck_db[k]) * (1.0 - (-secs / tau).exp());
            for i in (0..n).filter(|&i| d.targets.get(i).copied().unwrap_or(false)) {
                let l = lead(s, i);
                // A channel two duckers hold: the deeper one.
                gain[l] = gain[l].min(st.duck_db[k] + gain[l].max(0.0));
            }
        }
        // Audio follows video: up while its input is on air, down (or off) when not.
        for i in (0..n).filter(|&i| lead(s, i) == i) {
            let c = &s.channels[i];
            let want = match (on_air, s.afv.on && !c.follows.is_empty()) {
                (Some(on), true) if !on.contains(&c.follows) => {
                    if s.afv.off <= -144.0 {
                        0.0
                    } else {
                        db(s.afv.off)
                    }
                }
                _ => 1.0,
            };
            let step = if s.afv.fade <= 0.0 {
                1.0
            } else {
                secs * 1000.0 / s.afv.fade
            };
            let g = &mut st.afv[i];
            *g = if want > *g {
                (*g + step).min(want)
            } else {
                (*g - step).max(want)
            };
            gain[i] += 20.0 * g.max(1e-8).log10();
        }
        for (i, (g, m)) in st.gain_db.iter_mut().zip(meter.iter_mut()).enumerate() {
            // A pair's second side as its first.
            *g = gain[lead(s, i)].max(-144.0);
            *m = *g;
        }
        st.gain_scratch = gain;
    }
}

/// The chains' signals this block, for the plugins (sent at the end): into the buffers
/// of blocks before, no allocation per block. Only chains built in the host, and only
/// when this block is sent.
struct Outgoing {
    buf: Vec<ChainAudio>,
    n: usize,
    send: bool,
    /// Samples a block (the desk's).
    block: usize,
}

impl Outgoing {
    fn add(&mut self, chains: &BTreeMap<u32, u32>, id: u32, chans: &[Vec<f32>]) {
        if !self.send || !chains.contains_key(&id) {
            return;
        }
        if self.n == self.buf.len() {
            self.buf.push(ChainAudio {
                chain: 0,
                channels: 0,
                frames: 0,
                data: Vec::with_capacity(2 * self.block),
            });
        }
        let c = &mut self.buf[self.n];
        (c.chain, c.channels, c.frames) = (id, chans.len() as u16, self.block as u32);
        c.data.clear();
        for ch in chans {
            c.data.extend_from_slice(ch);
        }
        self.n += 1;
    }
}

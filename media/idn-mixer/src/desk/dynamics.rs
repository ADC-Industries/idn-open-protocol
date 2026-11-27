//! A channel's processing: its filters, gate, EQ, compressor, de-esser and limiter, with
//! their keys (§M.28); a linked pair together.

use super::*;

impl Desk {
    /// Then its filters, gate, EQ, compressor, de-esser and limiter. A linked pair runs
    /// both sides together: the louder side drives its gate, compressor and limiter and
    /// the same is taken off both, as on a bus (each side alone moved the stereo image
    /// with every hit); its de-esser stays per side.
    // (Inlined: its one caller is the block, on the clock thread; out of line the desk
    // took 2% longer a block.)
    #[inline(always)]
    #[allow(clippy::needless_range_loop)] // parallel lists, as in `process`
    /// `active`: the channels running this block (their sound is a key); `doing`: those
    /// processed in this call (§M.28: inserts before the processing run it after them);
    /// `take_keys`: the keys taken now (the block's first call: the second, for the
    /// inserts-first channels, keeps them as they were before any processing).
    pub(super) fn channel_dynamics(&mut self, active: &[bool], doing: &[bool], take_keys: bool) {
        let s = &self.settings;
        let n = s.channels.len();
        // The keys (§M.28): the channels another's gate or compressor listens to, as they
        // are before any processing this block.
        // (A key channel not on the desk, or resting, is silence: its buffer holds an old
        // block, which a ducker went on hearing; bug run P.)
        for c in s.channels.iter().filter(|_| take_keys) {
            for k in [c.gate.key, c.comp.key] {
                if let Some(src) = self.at.get(k.wrapping_sub(1))
                    && (c.gate.on && k == c.gate.key || c.comp.on && k == c.comp.key)
                {
                    if active[k - 1] {
                        self.keys[k - 1].copy_from_slice(src);
                    } else {
                        self.keys[k - 1].fill(0.0);
                    }
                }
            }
        }
        let keys = &self.keys;
        // A stage's key sample: its key channel's (0: `own`), through its filters.
        let key_of = |key: usize, own: f32, k: usize| -> f32 {
            match keys.get(key.wrapping_sub(1)) {
                Some(b) if key > 0 => b[k],
                _ => own,
            }
        };
        for i in 0..n {
            if !doing[i] || i % 2 == 1 && s.channels[i - 1].link && doing[i - 1] {
                continue;
            }
            if s.channels[i].link && i % 2 == 0 && i + 1 < n && doing[i + 1] {
                let c = &s.channels[i];
                let (sa, sb) = self.strips.split_at_mut(i + 1);
                let (st0, st1) = (&mut sa[i], &mut sb[0]);
                let (xa, xb) = self.at.split_at_mut(i + 1);
                let (x0, x1) = (&mut xa[i], &mut xb[0]);
                let (pa, pb) = self.tap_pre_eq.split_at_mut(i + 1);
                let (pre0, pre1) = (&mut pa[i], &mut pb[0]);
                let (qa, qb) = self.tap_post_eq.split_at_mut(i + 1);
                let (post0, post1) = (&mut qa[i], &mut qb[0]);
                let makeup = db(c.comp.makeup);
                let comp_mix = (c.comp.mix / 100.0).clamp(0.0, 1.0);
                let (plain_gate, plain_comp) = (c.gate.plain(), c.comp.plain());
                let want = effective(&c.eq, c.eq_on);
                let mut moving = st0.eq_now != want || st1.eq_now != want;
                let louder = |a: f32, b: f32| if a.abs() >= b.abs() { a } else { b };
                for k in 0..self.block {
                    if moving {
                        moving = glide(&mut st0.eq_now, &want);
                        st1.eq_now = st0.eq_now.clone();
                        retune(&mut st0.eq, &st0.eq_now);
                        retune(&mut st1.eq, &st1.eq_now);
                    }
                    if k == 0 {
                        for st in [&mut *st0, &mut *st1] {
                            for (f, b) in st.eq.iter_mut().zip(&st.eq_now) {
                                if !audible(b) {
                                    f.reset();
                                }
                            }
                        }
                    }
                    let mut y0 = st0.lpf.run(st0.hpf.run(x0[k]));
                    let mut y1 = st1.lpf.run(st1.hpf.run(x1[k]));
                    // Each stage switched on or off fades in or out over 2 ms (F7-7).
                    let gm = wet(&mut st0.dyn_mix[0], c.gate.on);
                    if gm > 0.0 {
                        let gt = &c.gate;
                        let g = if plain_gate {
                            let _ = st0.gate.gate(
                                louder(y0, y1),
                                rate(),
                                gt.threshold,
                                gt.range,
                                gt.attack,
                                gt.release,
                            );
                            st0.gate.gain()
                        } else {
                            let key = st0.gate_key.run(
                                key_of(gt.key, louder(y0, y1), k),
                                rate(),
                                gt.key_hpf,
                                gt.key_lpf,
                            );
                            st0.gate.gate_keyed(
                                key,
                                rate(),
                                gt.threshold,
                                gt.range,
                                gt.attack,
                                gt.release,
                                gt.hold,
                                gt.mode,
                                gt.ratio,
                            )
                        };
                        let g = 1.0 + (g - 1.0) * gm;
                        y0 *= g;
                        y1 *= g;
                    }
                    pre0[k] = y0;
                    pre1[k] = y1;
                    for (f, b) in st0.eq.iter_mut().zip(&st0.eq_now) {
                        if audible(b) {
                            y0 = f.run(y0);
                        }
                    }
                    for (f, b) in st1.eq.iter_mut().zip(&st1.eq_now) {
                        if audible(b) {
                            y1 = f.run(y1);
                        }
                    }
                    post0[k] = y0;
                    post1[k] = y1;
                    let cm = wet(&mut st0.dyn_mix[1], c.comp.on);
                    if cm > 0.0 {
                        let cp = &c.comp;
                        let g = if plain_comp {
                            let _ = st0.comp.compress(
                                louder(y0, y1),
                                rate(),
                                cp.threshold,
                                cp.ratio,
                                cp.attack,
                                cp.release,
                                cp.knee,
                            );
                            st0.comp.gain()
                        } else {
                            let key = st0.comp_key.run(
                                key_of(cp.key, louder(y0, y1), k),
                                rate(),
                                cp.key_hpf,
                                0.0,
                            );
                            st0.comp.compress_keyed(
                                key,
                                rate(),
                                cp.threshold,
                                cp.ratio,
                                cp.attack,
                                cp.release,
                                cp.knee,
                                cp.detect == Detect::Rms,
                                cp.auto_release,
                            )
                        };
                        let g = 1.0 + (g * makeup - 1.0) * cm * comp_mix;
                        y0 *= g;
                        y1 *= g;
                    }
                    if c.deesser.on {
                        let d = &c.deesser;
                        y0 = st0
                            .deess
                            .run(y0, rate(), d.freq, d.threshold, d.range, d.split);
                        y1 = st1
                            .deess
                            .run(y1, rate(), d.freq, d.threshold, d.range, d.split);
                    }
                    let lm = wet(&mut st0.dyn_mix[2], c.limiter.on);
                    if lm > 0.0 {
                        let _ = st0.lim.limit(
                            louder(y0, y1),
                            rate(),
                            c.limiter.ceiling,
                            c.limiter.release,
                        );
                        let g = 1.0 + (st0.lim.gain() - 1.0) * lm;
                        y0 *= g;
                        y1 *= g;
                    }
                    x0[k] = y0;
                    x1[k] = y1;
                }
                for (side, x) in [(i, &*x0), (i + 1, &*x1)] {
                    if self.analyze == Some(side) {
                        for &v in x.iter() {
                            self.scope[self.scope_at] = v;
                            self.scope_at = (self.scope_at + 1) % SCOPE;
                        }
                    }
                }
                let faded = st0.dyn_mix;
                for st in [&mut *st0, &mut *st1] {
                    if !c.gate.on && faded[0] == 0.0 {
                        st.gate = Dynamics::default();
                    }
                    if !c.comp.on && faded[1] == 0.0 {
                        st.comp = Dynamics::default();
                    }
                    if !c.limiter.on && faded[2] == 0.0 {
                        st.lim = Dynamics::default();
                    }
                    if !c.deesser.on {
                        st.deess = DeEss::default();
                    }
                }
                // The second side's own gate, compressor and limiter don't run: the pair's
                // reduction is the first's.
                let shared = (st0.gate.reduction, st0.comp.reduction, st0.lim.reduction);
                self.meters.channel_gr[i] = (shared.0, shared.1, shared.2, st0.deess.reduction());
                self.meters.channel_gr[i + 1] =
                    (shared.0, shared.1, shared.2, st1.deess.reduction());
                continue;
            }
            let c = &s.channels[lead(s, i)];
            let st = &mut self.strips[i];
            let x = &mut self.at[i];
            let (tap_pre, tap_post) = (&mut self.tap_pre_eq[i], &mut self.tap_post_eq[i]);
            let makeup = db(c.comp.makeup);
            let comp_mix = (c.comp.mix / 100.0).clamp(0.0, 1.0);
            let (plain_gate, plain_comp) = (c.gate.plain(), c.comp.plain());
            let want = effective(&c.eq, c.eq_on);
            let mut moving = st.eq_now != want;
            for (k, v) in x.iter_mut().enumerate() {
                if moving {
                    moving = glide(&mut st.eq_now, &want);
                    retune(&mut st.eq, &st.eq_now);
                }
                if k == 0 {
                    for (f, b) in st.eq.iter_mut().zip(&st.eq_now) {
                        if !audible(b) {
                            f.reset();
                        }
                    }
                }
                let mut y = st.lpf.run(st.hpf.run(*v));
                // Each stage switched on or off fades in or out over 2 ms (F7-7).
                let gm = wet(&mut st.dyn_mix[0], c.gate.on);
                if gm > 0.0 {
                    let gt = &c.gate;
                    if plain_gate {
                        // (The plain gate as it always was: the keyed one's call cost a
                        // fifth of a busy desk's time with none of it used; profile 13.)
                        let p =
                            st.gate
                                .gate(y, rate(), gt.threshold, gt.range, gt.attack, gt.release);
                        y += (p - y) * gm;
                    } else {
                        let key =
                            st.gate_key
                                .run(key_of(gt.key, y, k), rate(), gt.key_hpf, gt.key_lpf);
                        let g = st.gate.gate_keyed(
                            key,
                            rate(),
                            gt.threshold,
                            gt.range,
                            gt.attack,
                            gt.release,
                            gt.hold,
                            gt.mode,
                            gt.ratio,
                        );
                        y *= 1.0 + (g - 1.0) * gm;
                    }
                }
                tap_pre[k] = y;
                for (f, b) in st.eq.iter_mut().zip(&st.eq_now) {
                    if audible(b) {
                        y = f.run(y);
                    }
                }
                tap_post[k] = y;
                let cm = wet(&mut st.dyn_mix[1], c.comp.on);
                if cm > 0.0 {
                    let cp = &c.comp;
                    if plain_comp {
                        // (As it always was; see the gate.)
                        let p = st.comp.compress(
                            y,
                            rate(),
                            cp.threshold,
                            cp.ratio,
                            cp.attack,
                            cp.release,
                            cp.knee,
                        ) * makeup;
                        y += (p - y) * cm * comp_mix;
                    } else {
                        let key = st
                            .comp_key
                            .run(key_of(cp.key, y, k), rate(), cp.key_hpf, 0.0);
                        let g = st.comp.compress_keyed(
                            key,
                            rate(),
                            cp.threshold,
                            cp.ratio,
                            cp.attack,
                            cp.release,
                            cp.knee,
                            cp.detect == Detect::Rms,
                            cp.auto_release,
                        );
                        y *= 1.0 + (g * makeup - 1.0) * cm * comp_mix;
                    }
                }
                if c.deesser.on {
                    let d = &c.deesser;
                    y = st
                        .deess
                        .run(y, rate(), d.freq, d.threshold, d.range, d.split);
                }
                let lm = wet(&mut st.dyn_mix[2], c.limiter.on);
                if lm > 0.0 {
                    let p = st
                        .lim
                        .limit(y, rate(), c.limiter.ceiling, c.limiter.release);
                    y += (p - y) * lm;
                }
                *v = y;
            }
            // The analyzer's channel: its processed sound kept for the spectrum.
            if self.analyze == Some(i) {
                for &v in x.iter() {
                    self.scope[self.scope_at] = v;
                    self.scope_at = (self.scope_at + 1) % SCOPE;
                }
            }
            // A stage switched off reduces nothing, and starts afresh when on again (its
            // meter used to stay where it was).
            if !c.gate.on && st.dyn_mix[0] == 0.0 {
                st.gate = Dynamics::default();
            }
            if !c.comp.on && st.dyn_mix[1] == 0.0 {
                st.comp = Dynamics::default();
            }
            if !c.limiter.on && st.dyn_mix[2] == 0.0 {
                st.lim = Dynamics::default();
            }
            if !c.deesser.on {
                st.deess = DeEss::default();
            }
            self.meters.channel_gr[i] = (
                st.gate.reduction,
                st.comp.reduction,
                st.lim.reduction,
                st.deess.reduction(),
            );
        }
    }
}

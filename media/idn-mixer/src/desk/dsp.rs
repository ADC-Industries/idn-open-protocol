//! The desk's signal arithmetic: filters, pan law, gain and level glides, a bus's
//! dynamics and fader, a stage of plugins.

use super::*;

pub(super) fn eq_filters(bands: &[Band; EQ_BANDS]) -> [Filter; EQ_BANDS] {
    std::array::from_fn(|i| {
        let b = &bands[i];
        Filter::new(b.shape, rate(), b.freq, b.gain, b.q)
    })
}

pub(super) fn retune(f: &mut [Filter; EQ_BANDS], bands: &[Band; EQ_BANDS]) {
    for (f, b) in f.iter_mut().zip(bands) {
        f.retune(b.shape, rate(), b.freq, b.gain, b.q);
    }
}

/// The equal-power pan law: (left, right) gains, −3 dB each at the centre.
pub(super) fn pan_law(p: f32) -> (f32, f32) {
    let p = if p.is_finite() {
        p.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let a = (p + 1.0) * std::f32::consts::FRAC_PI_4;
    (a.cos(), a.sin())
}

/// The channel whose settings channel `i` uses: a linked pair's first.
pub(super) fn lead(s: &Settings, i: usize) -> usize {
    if i % 2 == 1 && s.channels[i - 1].link {
        i - 1
    } else {
        i
    }
}

/// A shelf or peak: a band with a gain (0 dB passes the sound unchanged).
pub(super) fn has_gain(s: Shape) -> bool {
    matches!(s, Shape::Peak | Shape::Veq | Shape::LowShelf | Shape::HighShelf)
}

/// Whether an EQ band (as played, see [`effective`]) changes anything: on, and not a
/// shelf or peak at 0 dB (most bands sit there: left out, they cost nothing).
pub(super) fn audible(b: &Band) -> bool {
    b.on && (b.gain != 0.0 || !has_gain(b.shape))
}

/// The bands as the filters are to play them: a shelf or peak switched off (or with the
/// EQ off) is one at 0 dB, so that switching it glides too.
pub(super) fn effective(bands: &[Band; EQ_BANDS], eq_on: bool) -> [Band; EQ_BANDS] {
    std::array::from_fn(|j| {
        let b = &bands[j];
        let on = eq_on && b.on;
        Band {
            on: on || has_gain(b.shape),
            gain: if on { b.gain } else { 0.0 },
            ..b.clone()
        }
    })
}

/// One sample's step of the EQ (`now`, as played) toward its settings (`want`): at most
/// 1/80 dB, 1/480 octave and 1.0025 × Q, so 6 dB takes 10 ms; a new shape, or a pass
/// filter switched, at once. An instant change was a click, and a step
/// every 8 samples still a faint buzz. Gives whether it still differs.
pub(super) fn glide(now: &mut [Band; EQ_BANDS], want: &[Band; EQ_BANDS]) -> bool {
    // `from` toward `to` by at most `max`, exactly `to` once within it.
    fn toward(from: f32, to: f32, max: f32) -> f32 {
        if (to - from).abs() <= max {
            to
        } else {
            from + max.copysign(to - from)
        }
    }
    let mut moving = false;
    for (n, w) in now.iter_mut().zip(want) {
        if n == w {
            continue;
        }
        if n.shape != w.shape || n.on != w.on {
            *n = w.clone();
            continue;
        }
        n.gain = toward(n.gain, w.gain, 0.0125);
        let lf = toward(
            n.freq.max(1.0).ln(),
            w.freq.max(1.0).ln(),
            std::f32::consts::LN_2 / 480.0,
        );
        n.freq = if lf == w.freq.max(1.0).ln() {
            w.freq
        } else {
            lf.exp()
        };
        let lq = toward(n.q.max(0.01).ln(), w.q.max(0.01).ln(), 0.0025);
        n.q = if lq == w.q.max(0.01).ln() {
            w.q
        } else {
            lq.exp()
        };
        moving |= n != w;
    }
    moving
}

/// A bus's EQ, compressor and limiter on its two channels (`l`, `l + 1`), stereo-linked.
pub(super) fn bus_dynamics(bus: &Bus, st: &mut BusState, l: usize, buses: &mut [Vec<f32>]) {
    let (a, b) = buses.split_at_mut(l + 1);
    let (left, right) = (&mut a[l], &mut b[0]);
    // The graphic EQ (§M.14): filters only, so before or after the EQ alike.
    st.geq.run(&bus.geq, rate(), left, right);
    let want = effective(&bus.eq, bus.eq_on);
    let mut moving = st.eq_now != want;
    for k in 0..left.len() {
        if moving {
            moving = glide(&mut st.eq_now, &want);
            for side in &mut st.eq {
                retune(side, &st.eq_now);
            }
        }
        if k == 0 {
            for (j, band) in st.eq_now.iter().enumerate() {
                if !audible(band) {
                    st.eq[0][j].reset();
                    st.eq[1][j].reset();
                }
            }
        }
        let (mut x, mut y) = (left[k], right[k]);
        for (j, band) in st.eq_now.iter().enumerate() {
            if audible(band) {
                x = st.eq[0][j].run(x);
                y = st.eq[1][j].run(y);
            }
        }
        // Switched on or off: faded over 2 ms (+6 dB of makeup came and went at once; F7-7).
        let cm = wet(&mut st.dyn_mix[0], bus.comp.on);
        if cm > 0.0 {
            // Linked: the louder side drives both.
            let m = if x.abs() >= y.abs() { x } else { y };
            let _ = st.comp.compress(
                m,
                rate(),
                bus.comp.threshold,
                bus.comp.ratio,
                bus.comp.attack,
                bus.comp.release,
                bus.comp.knee,
            );
            let g = 1.0 + (db(-st.comp.reduction + bus.comp.makeup) - 1.0) * cm;
            x *= g;
            y *= g;
        }
        let lm = wet(&mut st.dyn_mix[1], bus.limiter.on);
        if lm > 0.0 {
            let m = if x.abs() >= y.abs() { x } else { y };
            let _ = st
                .lim
                .limit(m, rate(), bus.limiter.ceiling, bus.limiter.release);
            let g = 1.0 + (st.lim.gain() - 1.0) * lm;
            x *= g;
            y *= g;
        }
        left[k] = x;
        right[k] = y;
    }
    if !bus.comp.on && st.dyn_mix[0] == 0.0 {
        st.comp = Dynamics::default();
    }
    if !bus.limiter.on && st.dyn_mix[1] == 0.0 {
        st.lim = Dynamics::default();
    }
}

/// A bus's mute and fader on its two channels, ramped straight to the target over the
/// blocks the desk's ramp has left (`ramp`: about 2 ms for a change, 0.2 s for a snapshot).
pub(super) fn bus_fader(bus: &Bus, st: &mut BusState, l: usize, buses: &mut [Vec<f32>], ramp: f32) {
    let target = if bus.mute { 0.0 } else { db(bus.fader) };
    let (left, right) = buses.split_at_mut(l + 1);
    let block = left[l].len();
    let step = (target - st.gain) / (block as f32 * ramp);
    let mut g = st.gain;
    for (a, b) in left[l].iter_mut().zip(right[0].iter_mut()).take(block) {
        g += step;
        *a *= g;
        *b *= g;
    }
    st.gain = if ramp > 1.0 { g } else { target };
}

/// A stage's share of the signal (0 off, 1 on) one sample on toward `on`: a stage
/// switched on or off fades over [`ramp_samples`] (bug round F, F7-7). The share now.
#[inline(always)]
pub(super) fn wet(mix: &mut f32, on: bool) -> f32 {
    // 2 ms at the desk's rate.
    let step = 500.0 / super::rate();
    *mix = if on {
        (*mix + step).min(1.0)
    } else {
        (*mix - step).max(0.0)
    };
    *mix
}

/// The fewest samples a gain change (a fader, a mute) is spread over: 2 ms at the desk's
/// rate (it was 96 samples, 1 ms at 96 kHz; bug run J2-9). Within one block, a change at
/// 32 samples took 0.67 ms and clicked (bug run 5, 3-3b).
pub(super) fn ramp_samples() -> usize {
    (super::rate_hz() / 500) as usize
}

/// One bus stage (§M.11) on `strips` (bus strip numbers): each one's chain result in place
/// of its signal, or its signal delayed to line up. Gives the stage's latency in samples
/// (0 while none of its buses has plugins: nothing is delayed).
pub(super) fn run_stage<'r>(
    stage: &mut Stage,
    strips: &[usize],
    chains: &BTreeMap<u32, u32>,
    result: &impl Fn(u32) -> Option<&'r ChainAudio>,
    late: &impl Fn(u32) -> bool,
    buses: &mut [Vec<f32>],
    bus_late: &mut [u64],
) -> usize {
    let longest = strips
        .iter()
        .filter_map(|&k| chains.get(&bus_strip(k).0))
        .max()
        .copied();
    let Some(longest) = longest else {
        return 0;
    };
    let slots = 2 * strips.len();
    let block = buses.first().map_or(0, Vec::len);
    // (Only when the stage's shape changes: the `vec!` given to `resize` was made, and
    // freed, every block on the clock thread; bug round F, F7-8.)
    if stage.prev.len() != slots || stage.prev.first().is_some_and(|p| p.len() != block) {
        stage.prev = vec![vec![0.0; block]; slots];
    }
    if stage.dry.len() != slots {
        stage.dry.resize(slots, Delay::default());
        stage.delay.resize(slots, Delay::default());
    }
    for (si, &k) in strips.iter().enumerate() {
        let (id, first) = bus_strip(k);
        let latency = chains.get(&id).copied().unwrap_or(0);
        let r = result(id).filter(|r| r.channels == 2);
        if r.is_none() && late(id) {
            bus_late[k] += 1;
        }
        for side in 0..2 {
            let j = 2 * si + side;
            let x = &mut buses[first + side];
            // `x` becomes the last block's (the dry fallback); `prev[j]` this block's, kept
            // for the next.
            std::mem::swap(x, &mut stage.prev[j]);
            stage.dry[j].run(x, latency as usize);
            if let Some(r) = r {
                x.copy_from_slice(r.channel(side));
            }
            stage.delay[j].run(x, (longest - latency) as usize);
        }
    }
    block + longest as usize
}

//! Banks of four channels at rest (idn-mixer.md §M.24): which banks
//! rest this block, and what waking from a rest does.

use super::*;

impl Desk {
    /// Muted by itself, a DCA or a mute group (as the channel mix takes it).
    pub(super) fn muted(&self, i: usize) -> bool {
        let s = &self.settings;
        let c = &s.channels[lead(s, i)];
        c.mute
            || s.dca.iter().zip(&c.dca).any(|(g, on)| *on && g.mute)
            || s.mute_groups
                .iter()
                .zip(&c.mute_groups)
                .any(|(g, on)| *on && g.on)
    }

    /// Banks of four channels at rest (§M.24). A bank rests while each
    /// of its channels is parked, muted for [`MUTE_REST`] (a muted channel sends nothing
    /// anywhere), or exactly silent in and out for [`REST_AFTER`]; and none has plugins,
    /// is soloed, is the talkback channel or is tapped by the output patch. It wakes the
    /// block that changes: a sample in, an unmute. A strip that rested muted starts afresh
    /// (its delay line and filters held sound from before the mute), faded in.
    pub(super) fn rest_banks(&mut self, input: &[Vec<f32>], active: &[bool]) {
        let n = self.settings.channels.len();
        if self.resting.len() != n {
            self.ch_silent = vec![0; n];
            self.ch_muted = vec![0; n];
            self.resting = vec![false; n];
            self.rested_muted = vec![false; n];
            self.rest_stale = vec![false; n];
        }
        if self.tapped.0 != self.settings_gen || self.tapped.1.len() != n {
            // (Its list reused: every fader move is a new generation, on the clock thread.)
            let mut t = std::mem::take(&mut self.tapped.1);
            t.clear();
            t.resize(n, false);
            for o in &self.settings.outputs {
                if let Tap::Pre(c) | Tap::Input(c) | Tap::PreEq(c) | Tap::PostEq(c) | Tap::Post(c) =
                    Tap::parse(o)
                    && let Some(x) = t.get_mut(c.wrapping_sub(1))
                {
                    *x = true;
                }
            }
            // The IEM flow's channels (§M.26), sent whatever their fader and mute.
            for &i in self.iem.iter() {
                if let Some(x) = t.get_mut(i) {
                    *x = true;
                }
            }
            // A ducker's key (its sound before the fader and mute keys it) and the
            // analyzer's channel keep their bank running, as a tap does.
            for d in self.settings.duckers.iter().filter(|d| d.on) {
                for (x, k) in t.iter_mut().zip(&d.keys) {
                    *x |= *k;
                }
            }
            self.tapped = (self.settings_gen, t);
        }
        let zero = |v: Option<&Vec<f32>>| v.is_none_or(|v| v.iter().all(|&x| x == 0.0));
        for i in 0..n {
            let muted = self.muted(i);
            self.ch_muted[i] = if muted {
                self.ch_muted[i].saturating_add(1)
            } else {
                0
            };
            let own = &self.settings.channels[i];
            let src = own.source.unwrap_or(i + 1);
            let b = (own.input_b > 0)
                .then(|| input.get(own.input_b - 1))
                .flatten();
            let quiet = zero(input.get(src.wrapping_sub(1))) && zero(b) && zero(self.prev.get(i));
            self.ch_silent[i] = if quiet {
                self.ch_silent[i].saturating_add(1)
            } else {
                0
            };
        }
        let s = self.settings.clone();
        let talk = s.talkback.channel.checked_sub(1);
        for bank in (0..n).step_by(BANK) {
            let chans = bank..(bank + BANK).min(n);
            let rests = chans.clone().all(|i| {
                let l = lead(&s, i);
                if self.tapped.1[i]
                    || self.tapped.1.get(l).is_some_and(|t| *t)
                    || self.analyze == Some(i)
                    || s.channels[l].solo
                    || talk == Some(i)
                    || self.chains.contains_key(&channel_chain(l))
                {
                    return false;
                }
                !active[i]
                    || self.ch_muted[i] >= self.mute_rest
                    || self.ch_silent[i] >= self.rest_after
            });
            for i in chans {
                if rests && !self.resting[i] {
                    self.rested_muted[i] = self.ch_muted[i] > 0;
                    // (A silent rest begins after 10 s of silence through the strip.)
                    self.rest_stale[i] =
                        self.ch_muted[i] > 0 && self.ch_silent[i] < self.rest_after;
                } else if rests {
                    // While resting, its input metered as long as it is muted.
                    self.rested_muted[i] = self.ch_muted[i] > 0;
                } else if self.resting[i] && (self.rest_stale[i] || self.ch_muted[i] > 0) {
                    // Waking with old sound in its strip, or muted (its gains, frozen at
                    // rest, would let the first moment through): afresh, faded in.
                    let mut st = fresh_strip(&s.channels[lead(&s, i)]);
                    st.fade_in = channel_fade();
                    // Its suppressor kept, not made again on the clock thread (bug run
                    // K6-2: two sinc converters and the suppressor's state a channel, in
                    // one block); what it still holds is waited out, silent.
                    st.denoise = self.strips[i].denoise.take();
                    if st.nr_on && st.denoise.is_some() {
                        st.nr_gain = 0.0;
                        st.nr_wait = idn_dsp::denoise::latency(rate_hz())
                            + idn_dsp::denoise::FRAME * rate_hz() as usize / 48_000;
                    }
                    self.strips[i] = st;
                }
                if !rests {
                    self.rested_muted[i] = false;
                    self.rest_stale[i] = false;
                }
                self.resting[i] = rests;
            }
        }
    }

    /// The banks at rest now (§M.24), for the state and tests.
    pub fn resting_banks(&self) -> usize {
        self.resting
            .chunks(BANK)
            .filter(|b| !b.is_empty() && b[0])
            .count()
    }
}

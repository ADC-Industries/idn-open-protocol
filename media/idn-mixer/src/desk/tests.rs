use super::*;
use crate::dsp::to_db;
use idn_common::sync::LockOk as _;

mod inserts;
mod levels;
mod processing;
mod resting;
mod routing;

fn sine(freq: f32, amp: f32, start: usize) -> Vec<f32> {
    (0..BLOCK)
        .map(|i| amp * (2.0 * std::f32::consts::PI * freq * (start + i) as f32 / rate()).sin())
        .collect()
}

/// Runs `blocks` blocks of `f(block index)` into channels and gives the last block's
/// outputs.
fn run(d: &mut Desk, blocks: usize, f: impl Fn(usize) -> Vec<Vec<f32>>) -> Vec<Vec<f32>> {
    let mut out = vec![vec![0.0; BLOCK]; OUTPUTS];
    for b in 0..blocks {
        d.process(&f(b), &mut out);
    }
    out
}

fn peak(v: &[f32]) -> f32 {
    v.iter().fold(0f32, |m, x| m.max(x.abs()))
}

/// A stand-in for the plugin host: each chain a gain of 2 after `latency` samples;
/// with `drop`, results never come; with `nan`, they are NaN.
#[derive(Default)]
struct Fake {
    latency: u32,
    drop: std::sync::atomic::AtomicBool,
    nan: std::sync::atomic::AtomicBool,
    lines: std::sync::Mutex<BTreeMap<(u32, usize), std::collections::VecDeque<f32>>>,
    done: std::sync::Mutex<BTreeMap<u64, Vec<ChainAudio>>>,
}

impl Inserts for Fake {
    fn submit(&self, seq: u64, chains: &[ChainAudio]) {
        let mut chains = chains.to_vec();
        let mut lines = self.lines.lock_ok();
        for c in &mut chains {
            let frames = c.frames as usize;
            for ch in 0..c.channels as usize {
                let line = lines
                    .entry((c.chain, ch))
                    .or_insert_with(|| vec![0.0; self.latency as usize].into());
                for v in &mut c.data[ch * frames..(ch + 1) * frames] {
                    line.push_back(*v * 2.0);
                    *v = line.pop_front().unwrap();
                    if self.nan.load(std::sync::atomic::Ordering::Relaxed) {
                        *v = f32::NAN;
                    }
                }
            }
        }
        if !self.drop.load(std::sync::atomic::Ordering::Relaxed) {
            self.done.lock_ok().insert(seq, chains);
        }
    }

    fn take(&self, seq: u64, _wait: Duration) -> Option<Vec<ChainAudio>> {
        self.done.lock_ok().remove(&seq)
    }
}

fn impulse_at(block: usize, at: usize) -> impl Fn(usize) -> Vec<Vec<f32>> {
    move |b| {
        let mut v = vec![0.0; BLOCK];
        if b == block {
            v[at] = 0.5;
        }
        vec![v.clone(), v]
    }
}

/// Settings with every channel's EQ off and hard left (0 dB on main L), the buses' EQ off.
fn plain(d: &Desk) -> Settings {
    let mut s = d.settings().clone();
    for c in &mut s.channels {
        c.eq_on = false;
        c.pan = -1.0;
    }
    // No built-in effects either: the FX returns pass their sends as they are.
    for f in &mut s.fx {
        f.effect.kind = crate::fx::EffectKind::None;
    }
    for b in std::iter::once(&mut s.main)
        .chain(&mut s.aux)
        .chain(&mut s.groups)
        .chain(&mut s.matrix)
        .chain(std::iter::once(&mut s.monitor))
    {
        b.eq_on = false;
    }
    s
}

fn level(v: &[f32], amp: f32) -> f32 {
    to_db(peak(v)) - to_db(amp)
}

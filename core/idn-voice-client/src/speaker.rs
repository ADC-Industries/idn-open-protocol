//! The speaker's side: one stream per (kind, talker, channel), each its jitter buffer and
//! Opus decoder, mixed into 48 kHz mono, each channel at its own volume. Pulled by the
//! app's audio thread; packets put in by the network's.
//!
//! Catching up (a resume's backlog, a stall's growth): while frames beyond the target wait,
//! a frame that decodes near silent is skipped, and past [`SPLICE_EXCESS`] two are played
//! as one, crossfaded, at most one in [`SPLICE_EVERY`] (about 1.25× speed).

use crate::FRAME_SAMPLES;
use crate::codec::Decoder;
use crate::jitter::{Jitter, Pop};
use std::collections::HashMap;

/// Frames beyond the target past which speech is sped up too, not only silence skipped.
pub const SPLICE_EXCESS: u32 = 5;
/// Output frames per splice at most (5 in, 4 out: 1.25×).
pub const SPLICE_EVERY: u32 = 4;
/// RMS below which a frame counts as silent for skipping (about −50 dBFS).
const SILENT_RMS: f32 = 0.003;
/// Silent frames skipped in one output frame at most.
const SKIP_MAX: usize = 4;
/// Frames concealed in a stall before going quiet (Opus's concealment fades anyway).
const CONCEAL_MAX: u32 = 5;
/// An idle stream's decoder is let go after this many frames (30 s).
const FORGET_FRAMES: u32 = 1_500;
/// Streams decoded at most (a decoder each); past it the longest idle is let go.
const MAX_STREAMS: usize = 32;

/// One stream's key: kind, talker, channel.
pub(crate) type Key = (u8, u32, u32);

struct Stream {
    jitter: Jitter,
    dec: Decoder,
    idle_for: u32,
    /// Output frames since the last splice.
    since_splice: u32,
    channel: u32,
}

/// A frame decoded: whether it holds sound.
enum Got {
    Nothing,
    Sound,
}

impl Stream {
    /// One frame of what `p` says into `out`.
    fn decode(&mut self, p: Pop, out: &mut [f32]) -> Got {
        let r = match &p {
            Pop::Silence => return Got::Nothing,
            Pop::Play(b) => self.dec.decode(Some(b.as_slice()), false, out),
            Pop::Lost(Some(next)) => self.dec.decode(Some(next.as_slice()), true, out),
            Pop::Lost(None) => self.dec.decode(None, false, out),
            Pop::Stall(n) if *n <= CONCEAL_MAX => self.dec.decode(None, false, out),
            Pop::Stall(_) => return Got::Nothing,
        };
        // (A packet that doesn't decode: quiet for its frame.)
        match r {
            Ok(()) => Got::Sound,
            Err(_) => Got::Nothing,
        }
    }

    /// The next output frame into `out` (false: nothing to add).
    fn next(&mut self, out: &mut [f32], spare: &mut [f32]) -> bool {
        let p = self.jitter.pop();
        let played = matches!(p, Pop::Play(_));
        if let Got::Nothing = self.decode(p, out) {
            return false;
        }
        self.since_splice = self.since_splice.saturating_add(1);
        if !played || self.jitter.excess() == 0 {
            return true;
        }
        // Behind: silent frames go, while there is still more than the target waiting.
        let mut skipped = 0;
        while rms(out) < SILENT_RMS && self.jitter.excess() > 0 && skipped < SKIP_MAX {
            let p = self.jitter.pop();
            if let Got::Nothing = self.decode(p, out) {
                return false;
            }
            skipped += 1;
        }
        // Far behind: two frames as one, the first fading into the second.
        if self.jitter.excess() >= SPLICE_EXCESS && self.since_splice >= SPLICE_EVERY {
            let p = self.jitter.pop();
            if let Got::Sound = self.decode(p, spare) {
                let n = out.len() as f32;
                for (i, (o, s)) in out.iter_mut().zip(spare.iter()).enumerate() {
                    let w = i as f32 / n;
                    *o = *o * (1.0 - w) + s * w;
                }
                self.since_splice = 0;
            }
        }
        true
    }
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
}

/// Jitter settings, ms: (start, max).
pub(crate) type Limits = (u32, u32);

/// What the speaker reports.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SpeakerStats {
    pub streams: usize,
    pub packets: u64,
    /// The most waiting in any stream, and its target, ms.
    pub buffered_ms: u32,
    pub target_ms: u32,
    pub lost: u64,
    pub stalls: u64,
    pub late: u64,
    pub duplicates: u64,
    pub dropped: u64,
}

pub(crate) struct Speaker {
    streams: HashMap<Key, Stream>,
    /// The site's jitter settings, and each channel's own.
    site: Limits,
    channels: HashMap<u32, Limits>,
    volumes: HashMap<u32, f32>,
    /// The mixed frame being handed out, and how far.
    frame: Vec<f32>,
    at: usize,
    one: Vec<f32>,
    spare: Vec<f32>,
    /// Counters of streams let go.
    gone: SpeakerStats,
}

impl Speaker {
    pub fn new() -> Speaker {
        Speaker {
            streams: HashMap::new(),
            site: (200, 2000),
            channels: HashMap::new(),
            volumes: HashMap::new(),
            frame: vec![0.0; FRAME_SAMPLES],
            at: FRAME_SAMPLES,
            one: vec![0.0; FRAME_SAMPLES],
            spare: vec![0.0; FRAME_SAMPLES],
            gone: SpeakerStats::default(),
        }
    }

    /// The site's jitter settings.
    pub fn site_limits(&self) -> Limits {
        self.site
    }

    fn limits(&self, channel: u32) -> Limits {
        self.channels.get(&channel).copied().unwrap_or(self.site)
    }

    /// The site's jitter settings and each channel's (from `hello` and `channels`).
    pub fn set_limits(&mut self, site: Limits, channels: HashMap<u32, Limits>) {
        self.site = site;
        self.channels = channels;
        for s in self.streams.values_mut() {
            let (start, max) = self.channels.get(&s.channel).copied().unwrap_or(site);
            s.jitter.set_limits(start, max);
        }
    }

    pub fn set_volume(&mut self, channel: u32, gain: f32) {
        // (A NaN would silence the whole mix through the sum.)
        if gain.is_finite() {
            self.volumes.insert(channel, gain.clamp(0.0, 4.0));
        }
    }

    /// A replay ended: its stream's next frames may count from 0 again.
    pub fn replay_ended(&mut self) {
        for (k, s) in self.streams.iter_mut() {
            if k.0 == 2 {
                s.jitter.end();
            }
        }
    }

    /// A packet from the server.
    pub fn put(&mut self, key: Key, seq: u32, packet: Vec<u8>) {
        if !self.streams.contains_key(&key) {
            if self.streams.len() >= MAX_STREAMS {
                // The one quiet longest goes (with its counters).
                let quietest = self
                    .streams
                    .iter()
                    .max_by_key(|(_, s)| (s.jitter.idle(), s.idle_for))
                    .map(|(k, _)| *k);
                if let Some(k) = quietest
                    && let Some(s) = self.streams.remove(&k)
                {
                    add(&mut self.gone, &s.jitter);
                }
            }
            let (start, max) = self.limits(key.2);
            let Ok(dec) = Decoder::new(key.0 == 3) else {
                return;
            };
            self.streams.insert(
                key,
                Stream {
                    jitter: Jitter::new(start, max),
                    dec,
                    idle_for: 0,
                    since_splice: SPLICE_EVERY,
                    channel: key.2,
                },
            );
        }
        if let Some(s) = self.streams.get_mut(&key) {
            s.idle_for = 0;
            s.jitter.push(seq, packet);
        }
    }

    /// Fills `out` with the mix.
    pub fn pull(&mut self, out: &mut [f32]) {
        let mut done = 0;
        while done < out.len() {
            if self.at == FRAME_SAMPLES {
                self.mix();
            }
            let n = (out.len() - done).min(FRAME_SAMPLES - self.at);
            out[done..done + n].copy_from_slice(&self.frame[self.at..self.at + n]);
            self.at += n;
            done += n;
        }
    }

    /// The next 20 ms.
    fn mix(&mut self) {
        self.frame.fill(0.0);
        self.at = 0;
        let mut forget = Vec::new();
        for (k, s) in self.streams.iter_mut() {
            if s.next(&mut self.one, &mut self.spare) {
                let gain = self.volumes.get(&s.channel).copied().unwrap_or(1.0);
                for (m, v) in self.frame.iter_mut().zip(self.one.iter()) {
                    *m += v * gain;
                }
            }
            if s.jitter.idle() {
                s.idle_for += 1;
                if s.idle_for > FORGET_FRAMES {
                    forget.push(*k);
                }
            }
        }
        for k in forget {
            if let Some(s) = self.streams.remove(&k) {
                add(&mut self.gone, &s.jitter);
            }
        }
        // Talkers together may pass full scale: held to it softly, not wrapped.
        for m in self.frame.iter_mut() {
            if m.abs() > 0.9 {
                *m = m.signum() * (0.9 + 0.1 * ((m.abs() - 0.9) / 0.1).tanh());
            }
        }
    }

    pub fn stats(&self) -> SpeakerStats {
        let mut st = self.gone;
        st.streams = self.streams.len();
        st.buffered_ms = 0;
        st.target_ms = 0;
        for s in self.streams.values() {
            add(&mut st, &s.jitter);
            let w = s.jitter.waiting() * crate::FRAME_MS;
            if w >= st.buffered_ms {
                st.buffered_ms = w;
                st.target_ms = s.jitter.target() * crate::FRAME_MS;
            }
        }
        st
    }
}

fn add(st: &mut SpeakerStats, j: &Jitter) {
    st.packets += j.counters.packets;
    st.lost += j.counters.lost;
    st.stalls += j.counters.stalls;
    st.late += j.counters.late;
    st.duplicates += j.counters.duplicates;
    st.dropped += j.counters.dropped;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Encoder;

    fn tone(frames: usize, amp: f32) -> Vec<Vec<u8>> {
        let mut enc = Encoder::new(48_000, 10).unwrap();
        let pcm: Vec<f32> = (0..frames * FRAME_SAMPLES)
            .map(|i| amp * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0).sin())
            .collect();
        pcm.chunks(FRAME_SAMPLES)
            .map(|c| enc.encode(c).unwrap().to_vec())
            .collect()
    }

    fn rms_of(x: &[f32]) -> f32 {
        rms(x)
    }

    #[test]
    fn two_talkers_are_mixed_at_their_channel_s_volume() {
        let mut sp = Speaker::new();
        sp.set_limits((40, 2000), HashMap::from([(7, (20, 2000))]));
        sp.set_volume(7, 0.0);
        let a = tone(50, 0.3);
        for (i, p) in a.iter().enumerate() {
            sp.put((1, 10, 3), i as u32, p.clone());
            sp.put((1, 11, 7), i as u32, p.clone());
        }
        // Odd lengths, as an audio callback asks.
        let mut out = vec![0.0; 48_000 / 2];
        for c in out.chunks_mut(333) {
            sp.pull(c);
        }
        let loud = rms_of(&out[FRAME_SAMPLES * 10..FRAME_SAMPLES * 20]);
        assert!(loud > 0.12 && loud < 0.35, "rms {loud}: one talker heard");
        assert_eq!(sp.stats().streams, 2);
    }

    #[test]
    fn a_backlog_is_caught_up_faster_than_real_time() {
        let mut sp = Speaker::new();
        sp.set_limits((200, 2000), HashMap::new());
        // 3 s of a resume's backlog at once.
        for (i, p) in tone(150, 0.3).iter().enumerate() {
            sp.put((1, 5, 1), i as u32, p.clone());
        }
        let mut out = vec![0.0; FRAME_SAMPLES];
        let mut pulls = 0;
        while sp.stats().buffered_ms > 200 + 100 {
            sp.pull(&mut out);
            pulls += 1;
            assert!(rms_of(&out) > 0.05, "pull {pulls}: sound all along");
        }
        // 150 frames down to ~15 in about 4 frames of output per 5 of input.
        assert!(pulls < 125, "{pulls} pulls to catch up");
        // Silence is skipped quicker still.
        let mut sp = Speaker::new();
        sp.set_limits((200, 2000), HashMap::new());
        for (i, p) in tone(150, 0.0).iter().enumerate() {
            sp.put((1, 5, 1), i as u32, p.clone());
        }
        let mut pulls = 0;
        while sp.stats().buffered_ms > 200 + 100 {
            sp.pull(&mut out);
            pulls += 1;
        }
        assert!(pulls < 40, "{pulls} pulls to skip silence");
    }
}

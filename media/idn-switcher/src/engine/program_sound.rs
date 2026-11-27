//! The program's sound each frame (§S.12, §S.26): the heard inputs' lanes, the stamp it
//! carries against the picture, the routed feeds, gain and noise suppression, the
//! monitor and the sound-track recordings.

use super::*;

/// What the program's sound keeps from frame to frame.
#[derive(Default)]
pub(super) struct ProgramSound {
    /// The heard inputs' lanes and their latency.
    audio_from: Vec<(u32, u32)>,
    /// The heard input the program's sound is stamped by (§S.12).
    lead: Option<u32>,
    /// The routed lanes' latency.
    lane_latency: u32,
    /// The inputs in the monitor and their lanes' latency (§S.24).
    monitor_from: Vec<(u32, u32)>,
    /// The stream sound's noise suppression, both sides, while it is on.
    denoise: Option<[idn_dsp::denoise::RateDenoiser; 2]>,
    /// The last lags between the sound and the shown picture (100 ns), newest last.
    lags: std::collections::VecDeque<i64>,
    /// The gain the last frame ended on (`None` before the first).
    last_gain: Option<f32>,
}

/// One frame's program sound: both channels, how much earlier than the picture it is
/// stamped (100 ns), and the sound of each mix with a feed of its own.
pub(super) struct Sound {
    pub(super) channels: Vec<Vec<f32>>,
    pub(super) offset: i64,
    pub(super) mix_sound: BTreeMap<u32, AudioBlock>,
}

impl ProgramSound {
    /// This frame's `n` samples, for the picture stamped `stamps` (timecode, timestamp).
    pub(super) fn frame(
        &mut self,
        sh: &Shared,
        s: &State,
        inputs: &BTreeMap<u32, Arc<Slot>>,
        scene_defs: &BTreeMap<u32, Vec<crate::scene::Item>>,
        n: usize,
        stamps: (i64, i64),
    ) -> Sound {
        let ProgramSound {
            audio_from,
            lead,
            lane_latency,
            monitor_from,
            denoise,
            lags,
            last_gain,
        } = self;
        // The sound: the chosen input's lane, this frame's share of samples.
        let want = match &s.audio {
            _ if s.take_feed && routes_hold_program(sh) => None,
            AudioSource::None | AudioSource::Routed | AudioSource::Card { .. } => None,
            AudioSource::Input(id) => Some(*id),
            AudioSource::FollowProgram => s.program,
        };
        // Which mix slots have a feed of their own on the receiver (§S.26).
        let routes = sh
            .receiver
            .lock_ok()
            .as_ref()
            .map(|rx| rx.routes())
            .unwrap_or_default();
        let fed = |slot: usize| routes.iter().any(|r| (r.ch as usize) / 2 == slot + 1);
        // The program's own pair (channels 1–2) routed, and the program to take it by
        // itself (an option, §S.26).
        let program_routed =
            s.audio == AudioSource::Routed || (s.take_feed && routes.iter().any(|r| r.ch < 2));
        let routed = program_routed || s.mixes.values().any(|m| fed(m.slot));
        if sh.routed_on.swap(routed, Ordering::Relaxed) && !routed {
            sh.routed.lock_ok().clear();
        }
        let global_latency = SOUND_BUFFER_MS + s.delay_ms;
        if *lane_latency != global_latency {
            // The routed lanes made again at the next block, with the new delay (held,
            // or adapting again without one).
            sh.routed_delay.store(s.delay_ms, Ordering::Relaxed);
            sh.routed.lock_ok().clear();
            *lane_latency = global_latency;
        }
        // What the stream hears: the input's sound — a virtual shot's or preset's, its
        // camera's — or a scene's inputs', each at its items' level (§S.12). Each heard
        // input's lane at the stream's delay plus its own sync offset (per-source sync);
        // lanes of inputs heard before and still are kept.
        let heard = want.map_or_else(Vec::new, |id| sound_of(id, scene_defs, inputs));
        let lanes: Vec<(u32, u32)> = heard
            .iter()
            .map(|&(id, _)| {
                let own = inputs[&id].audio_offset_ms.load(Ordering::Relaxed);
                (id, global_latency + own)
            })
            .collect();
        if lanes != *audio_from {
            for old in audio_from.iter().filter(|o| !lanes.contains(o)) {
                if let Some(sl) = inputs.get(&old.0) {
                    *sl.lane.lock_ok() = None;
                }
            }
            for &(id, latency_ms) in lanes.iter().filter(|l| !audio_from.contains(l)) {
                *inputs[&id].lane.lock_ok() = Some(sh.mixer.lane(&LaneConfig {
                    latency_ms,
                    max_correction_ppm: 1000.0,
                    // (Its level is in the program's stamps: never stepped once ~7 s
                    // after a cut, lab 467-B.)
                    keep_start_level: true,
                    ..Default::default()
                }));
            }
            *audio_from = lanes;
        }
        for &(id, g) in &heard {
            inputs[&id].gain.store(g.to_bits(), Ordering::Relaxed);
        }
        // The input the sound is stamped by: kept while it is heard, at any level (a level
        // change that made another the loudest moved the stamps: a 36 ms hole in a
        // recording).
        if !lead.is_some_and(|l| heard.iter().any(|h| h.0 == l)) {
            *lead = heard.first().map(|h| h.0);
            lags.clear();
        }
        *sh.heard.lock_ok() = heard;
        // The next sample's time in the source (the end of the last block pushed, less
        // what the lane holds) against the picture's: the program's sound is stamped that
        // much earlier than its picture, so receivers that line them up by timestamps hear
        // it with the picture it came with (the sound lagged by its buffer, 30–40 ms).
        // (In a scene, by its lead input's.)
        let lag = lead.and_then(|i| inputs.get(&i)).and_then(|slot| {
            let lane = slot.lane.lock_ok();
            let tail = (*slot.audio_tail.lock_ok())?;
            let front = tail - (lane.as_ref()?.queued().as_nanos() / 100) as i64;
            // The picture this frame shows from it, else (not shown) its newest.
            let pic = match slot.shown_ts.swap(0, Ordering::Relaxed) {
                0 => slot.picture_ts.load(Ordering::Relaxed),
                t => t,
            };
            // Half a second apart or more: not one clock (or no pictures).
            (pic != 0)
                .then_some(front - pic)
                .filter(|d| d.abs() < 5_000_000)
        });
        // Their median over half a second: where pictures arrive close to the frame clock's
        // tick, some frames show a picture one newer than the rest, and a smoothed mean
        // put the sound a few ms off for every frame (+4 ms); the median
        // follows what most frames show, and a real change within 8 frames.
        match lag {
            Some(d) => {
                lags.push_back(d);
                if lags.len() > 15 {
                    lags.pop_front();
                }
            }
            None => lags.clear(),
        }
        let offset = {
            let mut l: Vec<i64> = lags.iter().copied().collect();
            l.sort_unstable();
            l.get(l.len() / 2).copied().unwrap_or(0)
        };
        // The routed sound, once a frame while anything takes it: the program's pair, then
        // the mix slots'.
        let mut rbuf = Vec::new();
        if routed {
            rbuf = vec![0f32; n * FEED_CHANNELS];
            sh.routed_mixer.fill(&mut rbuf);
        }
        let side = |c: usize| -> Vec<f32> {
            rbuf.iter()
                .skip(c)
                .step_by(FEED_CHANNELS)
                .copied()
                .collect()
        };
        let mut mix = vec![0f32; n * 2];
        if program_routed {
            for (i, v) in mix.iter_mut().enumerate() {
                *v = rbuf[(i / 2) * FEED_CHANNELS + i % 2];
            }
        } else {
            sh.mixer.fill(&mut mix);
        }
        // Each mix with a feed of its own sends that instead of the stream's sound.
        let mix_sound: BTreeMap<u32, AudioBlock> = s
            .mixes
            .iter()
            .filter(|(_, m)| fed(m.slot))
            .map(|(&id, m)| {
                let c = 2 + 2 * m.slot;
                (
                    id,
                    AudioBlock {
                        sample_rate: rate(),
                        channels: vec![side(c), side(c + 1)],
                    },
                )
            })
            .collect();
        let gain = 10f32.powf(s.gain_db / 20.0) * (1.0 - s.ftb_level);
        let mut channels: Vec<Vec<f32>> = (0..2)
            .map(|c| mix.iter().skip(c).step_by(2).copied().collect())
            .collect();
        // Noise suppression makes the sound 20 ms older: stamped that much
        // earlier too, so it stays with its picture.
        let offset = if s.audio_denoise {
            let d = denoise.get_or_insert_with(|| {
                [
                    idn_dsp::denoise::RateDenoiser::new(rate()),
                    idn_dsp::denoise::RateDenoiser::new(rate()),
                ]
            });
            for (ch, dn) in channels.iter_mut().zip(d.iter_mut()) {
                dn.run(ch);
            }
            offset - idn_dsp::denoise::latency(rate()) as i64 * 10_000_000 / rate() as i64
        } else {
            *denoise = None;
            offset
        };
        // From the last frame's gain to this one's across the frame: one step a frame
        // was heard as zipper noise in a fade to black, a click on a gain change (bug
        // round F, F7-6).
        let from = last_gain.unwrap_or(gain);
        *last_gain = Some(gain);
        for ch in &mut channels {
            let len = ch.len().max(1) as f32;
            for (i, v) in ch.iter_mut().enumerate() {
                *v *= from + (gain - from) * (i + 1) as f32 / len;
            }
        }
        monitor_frame(
            sh,
            s,
            inputs,
            scene_defs,
            &channels,
            n,
            global_latency,
            monitor_from,
        );
        // Sound-track recordings: the program sound as streamed, then each chosen input's
        // own sound (its lane at the latency it would have as the stream's), stamped as the
        // program's sound.
        {
            let regs = sh.tracks.lock_ok();
            if !regs.is_empty() {
                for slot in inputs.values() {
                    let want = global_latency + slot.audio_offset_ms.load(Ordering::Relaxed);
                    for (_, lane, lat) in slot.track_lanes.lock_ok().iter_mut() {
                        if *lat != want {
                            lane.set_latency(want);
                            *lat = want;
                        }
                    }
                }
                for reg in regs.iter() {
                    let mut tracks = channels.clone();
                    for m in &reg.mixers {
                        let mut buf = vec![0f32; n * 2];
                        m.fill(&mut buf);
                        tracks.push(buf.iter().step_by(2).copied().collect());
                        tracks.push(buf.iter().skip(1).step_by(2).copied().collect());
                    }
                    let _ = reg.tx.try_send(idn_open_protocol::api::TappedFrame::sound(
                        tracks,
                        rate(),
                        sh.cfg.fps,
                        stamps.0 + offset,
                        stamps.1 + offset,
                    ));
                }
            }
        }
        Sound {
            channels,
            offset,
            mix_sound,
        }
    }
}

/// Whether a route is on the program's own pair (channels 1–2) of the switcher's receiver.
fn routes_hold_program(sh: &Shared) -> bool {
    sh.receiver
        .lock_ok()
        .as_ref()
        .is_some_and(|rx| rx.routes().iter().any(|r| r.ch < 2))
}

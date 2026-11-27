//! The switcher's API for the monitor, sound, meters, the multiview and captions.

use super::*;

impl Switcher {
    /// The monitor (§S.24): what the operator hears, as streamed — the stream's sound,
    /// preview's input's sound (`preview`), or soloed inputs' instead (`solo`); its
    /// volume, mute and sound card. Changes only what is given.
    pub fn monitor(
        &self,
        preview: Option<bool>,
        solo: Option<Vec<u32>>,
        volume_db: Option<f32>,
        mute: Option<bool>,
        card: Option<Option<String>>,
    ) -> Result<(), String> {
        if let Some(ids) = &solo {
            for &id in ids {
                self.slot(id)?;
            }
        }
        if let Some(card) = &card {
            self.monitor_card_open(card.clone())?;
        }
        self.edit(|s| {
            let m = &mut s.monitor;
            if let Some(p) = preview {
                m.preview = p;
            }
            if let Some(mut ids) = solo {
                ids.dedup();
                m.solo = ids;
            }
            if let Some(v) = volume_db {
                m.volume_db = v.clamp(-60.0, 12.0);
            }
            if let Some(v) = mute {
                m.mute = v;
            }
            if let Some(c) = card {
                m.card = c;
            }
            Ok(())
        })
    }

    /// The monitor's sound card: `Some(device)` (`""` the default) opens it, `None` closes.
    pub(super) fn monitor_card_open(&self, card: Option<String>) -> Result<(), String> {
        #[cfg(feature = "card")]
        {
            // Stopped and started with no lock held, the lock taken only to look and to
            // swap: the frame clock takes it every frame, and program, mixes and
            // recordings froze while the old card's thread was joined and the new one
            // opened.
            let device = card.map(|d| Some(d).filter(|d| !d.is_empty() && d != "default"));
            let card = &self.shared.monitor_card;
            if let Some(d) = &device
                && card.lock_ok().as_ref().is_some_and(|c| c.device == *d)
            {
                return Ok(());
            }
            let old = card.lock_ok().take();
            drop(old);
            if let Some(d) = device {
                // The frame clock gives it a frame's sound at a time: a frame and 20 ms
                // against the clock's jitter.
                let (num, den) = self.shared.cfg.fps;
                let frame_ms = 1000 * den.max(1) / num.max(1);
                let new = crate::card::CardOut::start(d, frame_ms + 20)?;
                *card.lock_ok() = Some(new);
            }
            Ok(())
        }
        #[cfg(not(feature = "card"))]
        match card {
            Some(_) => Err("built without sound cards".into()),
            None => Ok(()),
        }
    }

    /// The monitor's sound card's state, while there is one.
    pub(super) fn monitor_card_status(&self) -> Value {
        #[cfg(feature = "card")]
        if let Some(c) = self.shared.monitor_card.lock_ok().as_ref() {
            return c.status();
        }
        Value::Null
    }

    /// The monitor's sound for a browser (§S.24): from frame `from` on (0, or one no
    /// longer kept: the newest 100 ms), waiting up to `wait` for some. Returns the first
    /// frame's number and 16-bit stereo, interleaved, at 48 kHz. Asking keeps the sound
    /// coming for 3 s.
    pub fn monitor_listen(&self, from: u64, wait: Duration) -> (u64, Vec<i16>) {
        let mut out = self.shared.monitor_out.lock_ok();
        if out.listened.is_none_or(|t| t.elapsed() >= LISTEN_KEEP) {
            // Starting again: what was kept before is old.
            out.ring.clear();
        }
        out.listened = Some(Instant::now());
        let deadline = Instant::now() + wait;
        while out.end <= from || out.ring.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            out = self
                .shared
                .monitor_ready
                .wait_timeout(out, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        let kept = (out.ring.len() / 2) as u64;
        let start = out.end - kept;
        let first = if from >= start && from < out.end {
            from
        } else {
            out.end.saturating_sub(kept.min(rate() as u64 / 10))
        };
        let skip = ((first - start) * 2) as usize;
        (first, out.ring.iter().skip(skip).copied().collect())
    }

    /// Solos an input in the monitor, or takes it out of the solo.
    pub fn monitor_solo(&self, id: u32, on: bool) -> Result<(), String> {
        self.slot(id)?;
        self.edit(|s| {
            s.monitor.solo.retain(|&i| i != id);
            if on {
                s.monitor.solo.push(id);
            }
            Ok(())
        })
    }

    /// The sound card's state, while it is the sound.
    pub(super) fn card_status(&self) -> Value {
        #[cfg(feature = "card")]
        if let Some(c) = self.shared.card.lock_ok().as_ref() {
            return c.status();
        }
        Value::Null
    }

    /// The stream's sound: where from, a gain (dB) and a delay (ms, 0–500).
    /// The program sound's gain (dB) and delay (ms) now.
    pub fn audio_trim(&self) -> (f32, u32) {
        let s = self.shared.state.lock_ok();
        (s.gain_db, s.delay_ms)
    }

    pub fn audio(&self, source: AudioSource, gain_db: f32, delay_ms: u32) -> Result<(), String> {
        if let AudioSource::Input(id) = source {
            self.slot(id)?;
        }
        if source == AudioSource::Routed && self.shared.router.lock_ok().is_none() {
            return Err("no routable receiver: the switcher needs the machine's IDN Node".into());
        }
        let delay = delay_ms.min(500);
        {
            #[cfg(feature = "card")]
            let mut card = self.shared.card.lock_ok();
            match &source {
                #[cfg(feature = "card")]
                AudioSource::Card { device, first } => {
                    let latency = SOUND_BUFFER_MS + delay;
                    match card.as_ref() {
                        // The same card: only its delay changes.
                        Some(c) if c.device == *device && c.first == *first => {
                            c.lane.set_latency(latency)
                        }
                        _ => {
                            *card = None;
                            let lane = self.shared.mixer.lane(&LaneConfig {
                                latency_ms: latency,
                                max_correction_ppm: 1000.0,
                                keep_start_level: true,
                                ..Default::default()
                            });
                            *card = Some(crate::card::CardIn::start(device.clone(), *first, lane)?);
                        }
                    }
                }
                #[cfg(not(feature = "card"))]
                AudioSource::Card { .. } => return Err("built without sound cards".into()),
                #[cfg(feature = "card")]
                _ => *card = None,
                #[cfg(not(feature = "card"))]
                _ => {}
            }
        }
        self.edit(|s| {
            s.audio = source;
            s.gain_db = gain_db.clamp(-60.0, 20.0);
            s.delay_ms = delay;
            Ok(())
        })
    }

    /// Whether the program takes a desk's feed by itself while one is routed to its pair
    /// (§S.26): its own source again when the feed ends. Off by default.
    pub fn take_feed(&self, on: bool) -> Result<(), String> {
        self.edit(|s| {
            s.take_feed = on;
            Ok(())
        })
    }

    /// Noise suppression on the stream's sound (an RNNoise-class network, shared with the
    /// mixer): steady noise out of a camera or room mic, speech kept. Adds 20 ms, which
    /// the sound's stamps account for (it stays in sync); the sound dips for 20 ms as it
    /// comes on (`idn-dsp`, in every build).
    pub fn audio_denoise(&self, on: bool) -> Result<(), String> {
        self.edit(|s| {
            s.audio_denoise = on;
            Ok(())
        })
    }

    /// The sound's levels (§S.24): each input's own, the stream's as it goes out and the
    /// monitor's, peak dBFS per side. Asking keeps the monitor running for 3 s (for its
    /// meter, with no one listening).
    pub fn meters(&self) -> Value {
        let inputs = self.shared.inputs.lock_ok().clone();
        let mut out = self.shared.monitor_out.lock_ok();
        out.metered = Some(Instant::now());
        json!({
            "inputs": inputs.iter().map(|(id, slot)| (id.to_string(), json!(slot.meter.lock_ok().db())))
                .collect::<serde_json::Map<_, _>>(),
            "stream": out.stream.db(),
            "monitor": out.monitor.db(),
        })
    }

    /// The multiview as a JPEG (960 × 540) for an API client; asking keeps it drawn for 5 s.
    /// `None` until the first one is drawn.
    pub fn multiview_jpeg(&self) -> Option<Arc<Vec<u8>>> {
        let sh = &self.shared;
        *sh.multiview_wanted.lock_ok() = Instant::now() + Duration::from_secs(5);
        let (n, f) = sh.multiview.lock_ok().clone();
        let f = f?;
        let mut cache = sh.multiview_jpeg.lock_ok();
        if let Some((m, j)) = cache.as_ref()
            && *m == n
        {
            return Some(j.clone());
        }
        let small = f.scaled((0.0, 0.0, 1.0, 1.0), 960, 540);
        let j = Arc::new(crate::jpeg::encode(&small, 75));
        *cache = Some((n, j.clone()));
        Some(j)
    }

    /// Captions on or off, and the seconds with no new line before they clear (0: never).
    pub fn captions(&self, on: Option<bool>, clear_after: Option<f64>) {
        let mut c = self.shared.captions.lock_ok();
        if let Some(on) = on {
            if !on && c.on {
                c.captioner.clear();
            }
            c.on = on;
        }
        if let Some(s) = clear_after {
            c.clear_after = s.clamp(0.0, 600.0);
        }
        drop(c);
        let mut s = self.shared.state.lock_ok();
        bump(&mut s, &self.shared.changed);
    }

    /// A line of closed captions on the program (§S.29): CEA-608 roll-up in the streams'
    /// and recordings' video, two rows at the bottom, as fast as line 21 carries them
    /// (about 15 characters a second).
    pub fn caption(&self, text: &str) -> Result<(), String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("a caption: some text".into());
        }
        let mut c = self.shared.captions.lock_ok();
        if !c.on {
            return Err("captions are off".into());
        }
        c.captioner.say(text);
        c.recent.push_back(text.chars().take(200).collect());
        if c.recent.len() > 8 {
            c.recent.pop_front();
        }
        drop(c);
        let mut s = self.shared.state.lock_ok();
        bump(&mut s, &self.shared.changed);
        Ok(())
    }

    /// Takes the captions from a captions source (its final lines, as said; G2), or not
    /// (`None`: typed or sent ones only).
    pub fn captions_from(&self, from: Option<String>) {
        self.shared.captions.lock_ok().from = from.filter(|f| !f.trim().is_empty());
        let mut s = self.shared.state.lock_ok();
        bump(&mut s, &self.shared.changed);
    }
}

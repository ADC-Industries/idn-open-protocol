//! A file's sound: packets decoded and sent.

use super::*;

impl Play {
    pub(super) fn on_audio_packet(&mut self, p: &ReadPacket) {
        let tb = self.astream.as_ref().unwrap().time_base;
        if let (Some(config), Some(pts)) = (&self.aac_config, p.pts) {
            let s = self.astream.as_ref().unwrap();
            self.coded_audio.push_back((
                Self::us(tb, pts) + self.loop_offset,
                CodedAudio {
                    data: p.data.clone(),
                    config: config.clone(),
                    samples: 1024,
                    channels: s.channels,
                    sample_rate: s.sample_rate,
                },
            ));
            while self.coded_audio.len() > 64 {
                self.coded_audio.pop_front();
            }
        }
        let Some(d) = self.adec.as_mut() else { return };
        if d.send(p).is_err() {
            return;
        }
        self.audio_out();
    }

    /// The sound the decoder has ready, queued at its times.
    pub(super) fn audio_out(&mut self) {
        let tb = self.astream.as_ref().unwrap().time_base;
        // This pass's end: sound past it is the encoder's padding (an AAC file's last
        // frame ran 10 ms over, into the next pass's first sound).
        let pass_end = self
            .demux
            .duration_us
            .filter(|d| *d > 0)
            .map(|d| self.loop_offset + d);
        while let Ok(Some((f, ts))) = self.adec.as_mut().unwrap().receive() {
            // A whole AAC frame's sound (the start's priming and the end's padding come out
            // shorter, or are cut below).
            let mut whole = f.samples() == 1024;
            let Ok(ch) = Resampler::run(&mut self.resampler, f) else {
                continue;
            };
            let Some(ts) = ts else { continue };
            let t = Self::us(tb, ts) + self.loop_offset;
            if t < self.skip_until || ch[0].is_empty() {
                continue;
            }
            let [mut l, mut r] = ch;
            if let Some(e) = pass_end {
                if t >= e {
                    continue;
                }
                let keep = ((e - t) * 48_000 / 1_000_000) as usize;
                whole &= keep >= l.len();
                l.truncate(keep);
                r.truncate(keep);
            }
            // Its AAC frame (same time); older ones never got their sound.
            while self.coded_audio.front().is_some_and(|c| c.0 < t) {
                self.coded_audio.pop_front();
            }
            // Passed on as AAC only when it is the frame's whole sound: a receiver decodes
            // every AAC frame to 1024 samples, so a pass's priming and padding (32 ms of an
            // AAC file's 20 s) were heard again at every loop, and OBS's sound drifted
            // 10.7 ms a loop. The cut ones go as samples.
            let coded = match self.coded_audio.front() {
                Some(c) if c.0 == t => self.coded_audio.pop_front().map(|c| c.1).filter(|_| whole),
                _ => None,
            };
            // A mono file's sound stays mono (its AAC frames decode to one channel: the
            // two forms would switch at every loop's cut frames).
            let mono = self.astream.as_ref().is_some_and(|s| s.channels == 1);
            self.queue.insert(
                (t, 1),
                Item::Audio(
                    AudioBlock {
                        sample_rate: 48_000,
                        channels: if mono { vec![l] } else { vec![l, r] },
                    },
                    coded,
                ),
            );
        }
    }
}

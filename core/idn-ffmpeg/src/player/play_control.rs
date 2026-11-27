//! A file player's commands: looping, restarting.

use super::*;

impl Play {
    /// At the end of a looped file: what the decoders still hold comes out, the file is
    /// read again from the start, and its times continue after this pass's end, so the
    /// media keeps flowing through the seam. `false` if the file can't be looped (no
    /// duration and nothing played).
    pub(super) fn loop_around(&mut self) -> io::Result<bool> {
        // A still (a 4K picture file was opened and decoded again every
        // pass, about 10 a second, and fell behind): its picture again, a period on.
        if let Some(f) = self.still.clone() {
            let period = self.vstream.as_ref().map_or(40_000, |s| {
                1_000_000 * s.frame_rate.1.max(1) as i64 / s.frame_rate.0.max(1) as i64
            });
            let t = self.last_video_t + period.max(1);
            self.last_video_t = t;
            self.loop_offset = t;
            self.queue.insert((t, 0), Item::Video(f));
            return Ok(true);
        }
        // The decoders' last pictures and sound of this pass: to the download helpers like
        // any others when on the GPU (downloading them here held playback up for about
        // a second at every seam), queued with the lateness rule.
        if let Some(tb) = self.vstream.as_ref().map(|s| s.time_base)
            && let Some(mut d) = self.vdec.take()
        {
            d.drain();
            loop {
                let (frame, ts, gpu) = match d.receive_owned() {
                    Ok(Some(x)) => x,
                    Ok(None) => break,
                    Err(e) => {
                        // Said: a picture lost at the seam shows here.
                        idn_open_protocol::log::log(&format!(
                            "player: at the end of the file: {e}"
                        ));
                        break;
                    }
                };
                let Some(ts) = ts else { continue };
                let t = Self::us(tb, ts) + self.loop_offset;
                // Counted like any picture's time: a one-picture file (a still) has only
                // this one, and its loop's length comes from it.
                self.last_video_t = self.last_video_t.max(t);
                self.take_picture(frame, gpu, t);
            }
            d.flush();
            self.vdec = Some(d);
        }
        // The pass's end, from the file's duration or else its last picture (counted
        // after the drain above: a still's one picture only comes out there).
        let duration = self
            .demux
            .duration_us
            .filter(|d| *d > 0)
            .map(|d| self.loop_offset + d)
            .or_else(|| {
                let period = self.vstream.as_ref().map(|s| {
                    1_000_000 * s.frame_rate.1.max(1) as i64 / s.frame_rate.0.max(1) as i64
                });
                period.map(|p| self.last_video_t + p)
            })
            .filter(|end| *end > self.loop_offset);
        let Some(end) = duration else {
            return Ok(false);
        };
        // The sound the decoder still holds plays too: flushed, the pass's last AAC frame or
        // two (about 21 ms) went missing at every seam, and receivers keeping the sound to
        // a sample count heard it a frame off after the loop.
        if let Some(d) = self.adec.as_mut() {
            d.drain();
            self.audio_out();
            if let Some(d) = self.adec.as_mut() {
                d.flush();
            }
        }
        // One picture this pass and no sound: a still, kept from here on.
        if self.pass_pictures == 1 && self.astream.is_none() {
            self.still = self.last_picture.take();
        }
        self.pass_pictures = 0;
        if self.still.is_some() {
            return Ok(true);
        }
        self.demux.rewind()?;
        self.park_retiring();
        self.coded.clear();
        self.coded_audio.clear();
        self.loop_offset = end;
        self.skip_until = end;
        self.at_end = false;
        Ok(true)
    }

    /// Starts again at media time `at` (µs): a loop or a seek.
    pub(super) fn restart(&mut self, at: i64) -> io::Result<()> {
        if at <= 0 {
            self.demux.rewind()?;
        } else if let Err(e) = self.demux.seek(at) {
            // A file that can't seek there (a raw H.264 stream has no index): nothing
            // has changed yet, so it plays on where it was (it used to end the player).
            idn_open_protocol::log::log(&format!(
                "player: can't seek this file to {:.1} s ({e}); playing on",
                at as f64 / 1e6
            ));
            return Ok(());
        }
        for d in [&mut self.vdec, &mut self.adec].into_iter().flatten() {
            d.flush();
        }
        self.queue.clear();
        if let Some(dl) = self.downloads.as_mut() {
            dl.generation += 1;
            dl.pending.clear();
        }
        self.coded.clear();
        self.coded_audio.clear();
        self.at_end = false;
        self.park_retiring();
        self.loop_offset = 0;
        self.last_video_t = at;
        self.skip_until = at;
        // Timecodes keep going up.
        self.tc0 = self.last_tc + 400_000 - at * 10;
        // Pre-roll: decode a few pictures before the clock starts again, or the first
        // ones after the seam come out late (decoder delay) and the loop shows a gap of
        // a few hundred ms.
        self.clock_started = false;
        let mut reads = 0;
        let pictures = |p: &Self| {
            p.queue.keys().filter(|k| k.1 == 0).count()
                + p.downloads.as_ref().map_or(0, |d| d.pending.len())
        };
        while !self.at_end && reads < 64 && pictures(self) < 8 {
            self.read_one()?;
            reads += 1;
        }
        // The pre-rolled pictures in the queue before the clock starts.
        let end = Instant::now() + Duration::from_secs(1);
        while self
            .downloads
            .as_ref()
            .is_some_and(|d| !d.pending.is_empty())
            && Instant::now() < end
        {
            self.collect_downloads(Some(Duration::from_millis(20)));
        }
        self.media0 = self.queue.keys().next().map_or(at, |k| k.0);
        self.wall0 = Instant::now();
        self.clock_started = true;
        Ok(())
    }

    pub(super) fn commands(&mut self) -> io::Result<()> {
        loop {
            let Some(c) = self.ctl.cmds.lock_ok().pop_front() else {
                return Ok(());
            };
            match c {
                Cmd::Pause if !self.paused => {
                    self.paused = true;
                    // Where it stopped, to carry on from there.
                    self.media0 += self.wall0.elapsed().as_micros() as i64;
                    self.ctl.status.lock_ok().state = PlayerState::Paused;
                }
                Cmd::Resume if self.paused => {
                    self.paused = false;
                    self.wall0 = Instant::now();
                }
                // A later seek already waiting: this one is skipped (a slider dragged sends
                // dozens, and each pre-rolls the decoder: the player lagged behind it).
                Cmd::Seek(_)
                    if self
                        .ctl
                        .cmds
                        .lock_ok()
                        .iter()
                        .any(|c| matches!(c, Cmd::Seek(_))) => {}
                Cmd::Seek(to) => {
                    self.restart(to.as_micros() as i64)?;
                    // Paused: show where it now is, not stay on the old picture.
                    self.show_next = self.paused;
                }
                _ => {}
            }
        }
    }
}

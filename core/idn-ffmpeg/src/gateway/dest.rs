//! A destination: its packet queue, and the thread that connects, writes and reconnects.

use super::*;

impl Dest {
    /// A destination with a new id (adaptive down to `adaptive` Mbit/s from `target`
    /// bit/s, sent from `via`).
    pub(super) fn new(
        dest: Destination,
        adaptive: Option<f64>,
        via: Option<IpAddr>,
        target: u64,
    ) -> Arc<Dest> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let d = Arc::new(Dest {
            id,
            target: dest,
            queue: Mutex::default(),
            ready: Condvar::new(),
            status: Mutex::new(DestinationStatus {
                id,
                target: String::new(),
                state: State::Connecting,
                bytes: 0,
                reconnects: 0,
                dropped: 0,
                skipped_pictures: 0,
                skipping: false,
                paused: false,
                adaptive: adaptive.is_some(),
                target_kbps: (target / 1000) as u32,
                video_kbps: (target / 1000) as u32,
                sent_kbps: 0,
                lag_ms: 0,
                via,
            }),
            stop: AtomicBool::new(false),
            need_key: AtomicBool::new(false),
            part: AtomicU64::new(0),
            split_s: AtomicU64::new(0),
            split_mb: AtomicU64::new(0),
            paused: AtomicBool::new(false),
            via,
            slow: AtomicBool::new(false),
            meter: Mutex::new((Instant::now(), 0, Instant::now())),
            adapt: adaptive
                .map(|floor| Mutex::new(Adapt::new(target, (floor * 1e6) as u64, Instant::now()))),
        });
        d.status.lock_ok().target = d.target.describe();
        d
    }

    /// Queues a packet; `true` if the destination was too slow and lost packets (it then
    /// needs a keyframe).
    pub(super) fn offer(&self, p: Arc<Pkt>) -> bool {
        let mut q = self.queue.lock_ok();
        // A stream more than a few seconds behind skips ahead: viewers stay near live (a
        // recording waits for its disk).
        let behind = !self.target.is_file()
            && q.front()
                .is_some_and(|f| f.made.elapsed() > STREAM_MAX_WAIT);
        let overflowed = q.len() >= MAX_QUEUED || behind;
        if overflowed {
            // Too slow: start again from the next keyframe.
            let n = q.len() as u64;
            q.clear();
            self.status.lock_ok().dropped += n;
            self.need_key.store(true, Ordering::Relaxed);
            self.slow.store(true, Ordering::Relaxed);
        }
        q.push_back(p);
        self.ready.notify_one();
        overflowed
    }

    /// The URL to write: a recording whose encoder changed goes on in a new file
    /// (`name-2.mp4`), the one so far left complete, instead of being overwritten.
    pub(super) fn url(&self) -> String {
        let part = self.part.load(Ordering::Relaxed);
        match &self.target {
            Destination::File(path, _) if part > 0 => {
                let stem = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let ext = path
                    .extension()
                    .map(|e| format!(".{}", e.to_string_lossy()))
                    .unwrap_or_default();
                path.with_file_name(format!("{stem}-{}{ext}", part + 1))
                    .to_string_lossy()
                    .into_owned()
            }
            d => d.url(),
        }
    }

    pub(super) fn set(&self, s: State) {
        self.status.lock_ok().state = s;
    }

    /// Done or failed for good (its thread has ended).
    pub(super) fn ended(&self) -> bool {
        matches!(self.status.lock_ok().state, State::Done | State::Failed(_))
    }

    /// Its status, with the wait and the send rate as of now (a connection that hangs
    /// shows as one: its wait grows, its rate falls).
    pub(super) fn status(&self) -> DestinationStatus {
        let lag = self
            .queue
            .lock_ok()
            .front()
            .map_or(Duration::ZERO, |p| p.made.elapsed());
        let mut s = self.status.lock_ok().clone();
        s.lag_ms = lag.as_millis().min(u32::MAX as u128) as u32;
        let (since, bytes, last_write) = *self.meter.lock_ok();
        let age = since.elapsed();
        s.sent_kbps = stale_rate(
            s.sent_kbps,
            s.bytes.saturating_sub(bytes),
            age,
            last_write.elapsed(),
        );
        s
    }

    /// Counts a write into the send meter (a new reading every two seconds).
    pub(super) fn metered(&self) {
        let mut m = self.meter.lock_ok();
        let age = m.0.elapsed();
        m.2 = Instant::now();
        if age >= Duration::from_secs(2) {
            let mut st = self.status.lock_ok();
            st.sent_kbps =
                (st.bytes.saturating_sub(m.1) as f64 * 8.0 / 1000.0 / age.as_secs_f64()) as u32;
            *m = (Instant::now(), st.bytes, m.2);
        }
    }

    /// An adaptive stream: the packet's wait (and drops) into its rate control; a new rate
    /// goes to the encoder.
    pub(super) fn adapt(&self, g: &Group, waited: Duration) {
        let Some(a) = &self.adapt else { return };
        let mut a = a.lock_ok();
        let now = Instant::now();
        let new = if self.slow.swap(false, Ordering::Relaxed) {
            a.overflow(now)
        } else {
            a.sample(waited, now)
        };
        if let Some(r) = new {
            g.bitrate.store(r, Ordering::Relaxed);
            self.status.lock_ok().video_kbps = (r / 1000) as u32;
            log(&format!(
                "gateway {}: video at {:.1} Mbit/s (asked {:.1})",
                self.target.describe(),
                r as f64 / 1e6,
                a.target as f64 / 1e6
            ));
        }
    }

    pub(super) fn write_loop(&self, g: &Group) {
        // Failed connections in a row: the wait before the next try (G4: back on air soon).
        let mut failures = 0usize;
        let mut first = true;
        loop {
            if self.stop.load(Ordering::Relaxed) {
                self.set(State::Done);
                return;
            }
            let Some(streams) = g.params.lock_ok().clone() else {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            };
            if !first {
                self.status.lock_ok().reconnects += 1;
            }
            first = false;
            self.set(State::Connecting);
            // A recording takes every sound track; a stream (RTMP/SRT) the first only.
            let atb = (1, streams.audio_rate as i32);
            let n_extra = if self.target.is_file() {
                streams.extra_audio.len()
            } else {
                0
            };
            let mut specs = vec![
                StreamSpec {
                    params: &streams.video,
                    time_base: VIDEO_TB,
                },
                StreamSpec {
                    params: &streams.audio,
                    time_base: atb,
                },
            ];
            specs.extend(streams.extra_audio[..n_extra].iter().map(|p| StreamSpec {
                params: p,
                time_base: atb,
            }));
            let mut options = self.target.options(self.via);
            // An adaptive RTMP stream: about a second of it in the system's send buffer, at
            // least 1 MB (Linux lets it grow to several MB, seconds of a slow upload
            // unseen before the rate could follow).
            if let (Some(a), Destination::Rtmp(_)) = (&self.adapt, &self.target) {
                let bytes = (a.lock_ok().target / 8).max(1 << 20);
                options.push(("send_buffer_size", bytes.to_string()));
            }
            let options: Vec<(&str, &str)> =
                options.iter().map(|(k, v)| (*k, v.as_str())).collect();
            let mux = Muxer::open(
                self.target.format(),
                &self.url(),
                &specs,
                &options,
                Duration::from_secs(10),
            );
            let mut mux = match mux {
                Ok(m) => m,
                Err(e) => {
                    let msg = e.to_string();
                    log(&format!("gateway {}: {msg}", self.target.describe()));
                    if self.target.is_file() {
                        self.set(State::Failed(msg));
                        return;
                    }
                    self.set(State::Retrying(msg));
                    self.sleep(retry_after(failures));
                    failures += 1;
                    continue;
                }
            };
            // Start on a keyframe, with times from zero (what piled up while it was away is
            // not the connection's slowness).
            self.queue.lock_ok().clear();
            self.need_key.store(false, Ordering::Relaxed);
            self.slow.store(false, Ordering::Relaxed);
            g.keyframe.store(true, Ordering::Relaxed);
            // Where the file's time starts: (video, 90 kHz; sound, samples).
            let mut base: Option<(i64, i64)> = None;
            // Bytes written when this part started, for the size-based auto-split.
            let mut part_start_bytes = 0u64;
            // The sound packets before the first keyframe (about 2 s), and keyframes passed
            // over for want of the sound packet they fall in.
            let mut held_audio: VecDeque<Arc<Pkt>> = VecDeque::new();
            let mut passed = 0;
            // Per muxer stream: video, the first sound track, the later ones.
            let mut last = vec![i64::MIN; 2 + n_extra];
            let mut wrote = false;
            // A pause: the file's time to carry on from (video, 90 kHz; sound, samples), and
            // each stream's last packet length, to put the next one right after.
            let mut resume: Option<(i64, i64)> = None;
            let mut dur = vec![1_024i64; 2 + n_extra];
            dur[0] = 3_000;
            let r: io::Result<()> = 'run: loop {
                let p = {
                    let mut q = self.queue.lock_ok();
                    if q.is_empty() {
                        q = self
                            .ready
                            .wait_timeout(q, Duration::from_millis(200))
                            .unwrap()
                            .0;
                    }
                    q.pop_front()
                };
                if self.stop.load(Ordering::Relaxed) && p.is_none() {
                    break Ok(());
                }
                if g.params
                    .lock_ok()
                    .as_ref()
                    .is_some_and(|s| s.version != streams.version)
                {
                    // New encoder settings: a new header, and for a recording a new file.
                    if wrote && self.target.is_file() {
                        self.part.fetch_add(1, Ordering::Relaxed);
                    }
                    break Ok(());
                }
                let Some(p) = p else { continue };
                self.adapt(g, p.made.elapsed());
                // A sound track this destination doesn't carry (a stream: the first only).
                if !p.video && p.track > n_extra {
                    continue;
                }
                // Paused: drop what comes; the first time, note where the file's time
                // stops, so that after it the next keyframe starts as at the beginning,
                // its time carrying on (one file, no gap).
                if self.paused.load(Ordering::Relaxed) {
                    if base.is_some() && wrote {
                        resume = Some((last[0] + dur[0], last[1] + dur[1]));
                        base = None;
                        held_audio.clear();
                        passed = 0;
                    }
                    continue;
                }
                // After a drop, wait for the next keyframe (both streams: lip sync).
                if self.need_key.load(Ordering::Relaxed) {
                    if p.video && p.key {
                        self.need_key.store(false, Ordering::Relaxed);
                    } else {
                        continue;
                    }
                }
                let rate = streams.audio_rate as i64;
                if base.is_none() {
                    if !p.video {
                        held_audio.push_back(p);
                        if held_audio.len() > 100 * (1 + n_extra) {
                            held_audio.pop_front();
                        }
                        continue;
                    }
                    if !p.key {
                        continue;
                    }
                    // The file starts with the sound packet the keyframe falls in, at 0
                    // exactly: a fragmented MP4 can't say that sound starts later than
                    // the pictures (the muxer moved its first packet to 0, so players
                    // that play sound end to end heard it 0-21 ms early, different every
                    // recording); the first picture can start a little
                    // late, being shown by its time.
                    // Its sound packet is among those held: sound runs ahead of the pictures
                    // (the GPU's encoder takes longer), so not the newest (that one came
                    // after the keyframe: the file started on the pictures, its first sound
                    // moved to 0, 0-21 ms early). None held for it: the next
                    // keyframe, asked for (three times at most, sound held or not yet
                    // come; a source without sound starts on the pictures).
                    let key_in_samples = p.pts * rate / 90_000;
                    let at = held_audio.iter().rposition(|a| {
                        a.track == 0
                            && a.pts <= key_in_samples
                            && key_in_samples - a.pts < rate / 10
                    });
                    match at {
                        Some(i) => {
                            let a = held_audio.drain(..=i).next_back().expect("one at least");
                            let (rv, ra) = resume.take().unwrap_or((0, 0));
                            base = Some((a.pts * 90_000 / rate - rv, a.pts - ra));
                            if let Err(e) = mux.write(
                                1,
                                &a.data,
                                ra,
                                AAC_FRAME,
                                (1, rate as i32),
                                true,
                                Duration::from_secs(10),
                            ) {
                                break Err(e);
                            }
                            last[1] = ra;
                            self.status.lock_ok().bytes += a.data.len() as u64;
                            // The sound after it that came before the keyframe.
                            for q in held_audio.drain(..) {
                                let pts = q.pts - a.pts + ra;
                                let si = 1 + q.track;
                                if pts <= last[si] {
                                    continue;
                                }
                                last[si] = pts;
                                if let Err(e) = mux.write(
                                    si,
                                    &q.data,
                                    pts,
                                    AAC_FRAME,
                                    (1, rate as i32),
                                    true,
                                    Duration::from_secs(10),
                                ) {
                                    break 'run Err(e);
                                }
                                self.status.lock_ok().bytes += q.data.len() as u64;
                            }
                        }
                        // No sound yet either (it came after the keyframe): started on the
                        // pictures, the first sound packet a few ms in was moved to 0 by the
                        // fragmented MP4, a hole of 3-17 ms after it.
                        None if passed < 3 && (!held_audio.is_empty() || g.expects_sound()) => {
                            passed += 1;
                            g.keyframe.store(true, Ordering::Relaxed);
                            continue;
                        }
                        None => {
                            let (rv, ra) = resume.take().unwrap_or((0, 0));
                            base = Some((p.pts - rv, key_in_samples - ra));
                        }
                    }
                    self.set(State::Live);
                    failures = 0;
                    // A part's start (not a resume after a pause): its size counts from here.
                    if !wrote {
                        part_start_bytes = self.status.lock_ok().bytes;
                    }
                }
                // Auto-split a recording into parts at a keyframe once past the limit. The
                // new part reopens on the next keyframe (its own `base`, bytes from here).
                if let Some((bv, _)) = base
                    && p.video
                    && p.key
                    && self.target.is_file()
                {
                    let elapsed_s = ((p.pts - bv) / 90_000).max(0) as u64;
                    let split_s = self.split_s.load(Ordering::Relaxed);
                    let split_mb = self.split_mb.load(Ordering::Relaxed);
                    let grown = self.status.lock_ok().bytes.saturating_sub(part_start_bytes);
                    if (split_s > 0 && elapsed_s >= split_s)
                        || (split_mb > 0 && grown >= split_mb.saturating_mul(1_000_000))
                    {
                        self.part.fetch_add(1, Ordering::Relaxed);
                        break Ok(());
                    }
                }
                let (bv, ba) = base.unwrap();
                let (i, pts, tb) = if p.video {
                    (0, p.pts - bv, VIDEO_TB)
                } else {
                    (1 + p.track, p.pts - ba, (1, streams.audio_rate as i32))
                };
                if pts < 0 || pts <= last[i] {
                    continue; // from before this destination's start
                }
                if last[i] != i64::MIN {
                    dur[i] = (pts - last[i]).clamp(1, if i == 0 { 90_000 } else { 48_000 });
                }
                last[i] = pts;
                // Sound: an AAC frame; a picture: as long as the one before it.
                let length = if i == 0 { dur[0] } else { AAC_FRAME };
                if let Err(e) =
                    mux.write(i, &p.data, pts, length, tb, p.key, Duration::from_secs(10))
                {
                    break Err(e);
                }
                wrote = true;
                self.status.lock_ok().bytes += p.data.len() as u64;
                self.metered();
            };
            drop(mux); // the trailer: a recording is complete
            match r {
                Ok(()) if self.stop.load(Ordering::Relaxed) => {
                    self.set(State::Done);
                    return;
                }
                Ok(()) => {}
                Err(e) => {
                    let msg = e.to_string();
                    log(&format!("gateway {}: {msg}", self.target.describe()));
                    if self.target.is_file() {
                        self.set(State::Failed(msg));
                        return;
                    }
                    self.set(State::Retrying(msg));
                    self.sleep(retry_after(failures));
                    failures += 1;
                }
            }
        }
    }

    pub(super) fn sleep(&self, d: Duration) {
        let end = Instant::now() + d;
        while Instant::now() < end && !self.stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// The send rate to show when the meter's last reading is `age` old: nothing written for
/// 3 s is 0 (what was written before it stopped, spread over an ever longer time, never
/// reached 0, and "nothing to send" was never said: lab 478(b)); else, with a reading
/// over 3 s old, `since_reading` bytes over that time; else the reading as it is.
fn stale_rate(reading_kbps: u32, since_reading: u64, age: Duration, idle: Duration) -> u32 {
    if idle >= Duration::from_secs(3) {
        0
    } else if age > Duration::from_secs(3) {
        (since_reading as f64 * 8.0 / 1000.0 / age.as_secs_f64()) as u32
    } else {
        reading_kbps
    }
}

#[cfg(test)]
mod rate_tests {
    use super::*;

    #[test]
    fn a_destination_that_stopped_writing_shows_no_rate() {
        let s = Duration::from_secs;
        // Writing: the reading, or the bytes since it once it is old.
        assert_eq!(stale_rate(6000, 0, s(1), Duration::from_millis(20)), 6000);
        assert_eq!(
            stale_rate(6000, 3_000_000, s(4), Duration::from_millis(20)),
            6000
        );
        // Stopped: 0 from 3 s on, whatever was written before the stop.
        assert_eq!(stale_rate(6000, 400_000, s(2), s(2)), 6000);
        assert_eq!(stale_rate(6000, 400_000, s(5), s(3)), 0);
        assert_eq!(stale_rate(6000, 400_000, s(60), s(58)), 0);
    }
}

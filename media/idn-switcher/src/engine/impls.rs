//! The state types' small methods, defaults and drops.

use super::*;

impl Default for SwitcherConfig {
    fn default() -> Self {
        SwitcherConfig {
            name: "Switcher".into(),
            width: 1920,
            height: 1080,
            fps: (30, 1),
            preview_output: false,
            multiview_output: false,
            idle_after: Duration::from_secs(10),
            secure: None,
            mdns: None,
            discovery_server: None,
            codec: None,
            out_codec: None,
            node: None,
            gpu: true,
            #[cfg(feature = "gpu")]
            gpu_device: None,
            hdr: idn_open_protocol::color::Transfer::Sdr,
        }
    }
}

impl ViewRun {
    /// Where it is now on a `sw` × `sh` picture shown at `w` × `h`: the move between the
    /// views as they can be shown (a move from x 0.2 to 0.8 at zoom 2 stood still for its
    /// first and last fifth, x being held to 0.25–0.75, then started and stopped
    /// abruptly).
    pub(super) fn now_on(&self, sw: usize, sh: usize, w: usize, h: usize, at: Instant) -> View {
        ViewRun {
            from: self.from.reachable(sw, sh, w, h),
            to: self.to.reachable(sw, sh, w, h),
            ..*self
        }
        .at(at)
        .0
    }

    /// Where it is now, eased in and out; whether it is still moving.
    pub(super) fn now(&self) -> (View, bool) {
        self.at(Instant::now())
    }

    /// Where it is at `at`, eased in and out; whether it is still moving then.
    pub(super) fn at(&self, at: Instant) -> (View, bool) {
        let t = if self.secs <= 0.0 {
            1.0
        } else {
            (at.saturating_duration_since(self.start).as_secs_f32() / self.secs).min(1.0)
        };
        let e = t * t * (3.0 - 2.0 * t);
        let l = |a: f32, b: f32| a + (b - a) * e;
        // Zoom moves evenly on a log scale (a zoom from 1 to 4 halfway is 2).
        let z = (self.from.zoom.ln() + (self.to.zoom.ln() - self.from.zoom.ln()) * e).exp();
        (
            View {
                x: l(self.from.x, self.to.x),
                y: l(self.from.y, self.to.y),
                zoom: z,
            },
            t < 1.0,
        )
    }
}

impl Timer {
    pub(super) fn elapsed(&self) -> Duration {
        self.banked + self.since.map_or(Duration::ZERO, |t| t.elapsed())
    }

    /// The seconds it shows, and whether it is at its end.
    pub(super) fn value(&self, now: &crate::clock::LocalTime) -> (u64, bool) {
        match self.mode {
            TimerMode::Down(total) => {
                let left = total.saturating_sub(self.elapsed());
                (
                    left.as_secs() + u64::from(left.subsec_nanos() > 0),
                    left.is_zero(),
                )
            }
            TimerMode::Up => (self.elapsed().as_secs(), false),
            TimerMode::Until { hour, minute } => {
                let at = hour as u32 * 3600 + minute as u32 * 60;
                let left = at.saturating_sub(now.of_day());
                (left as u64, left == 0)
            }
        }
    }
}

impl Drop for InputTap {
    fn drop(&mut self) {
        if let Some(sh) = self.shared.upgrade() {
            let slot = sh.inputs.lock_ok().get(&self.input).cloned();
            if let Some(slot) = slot {
                slot.iso.lock_ok().retain(|(i, _, _)| *i != self.id);
            }
        }
        // Its sender gone (or the input removed with it), the worker ends.
        if let Some(t) = self.worker.take() {
            let _ = t.join();
        }
    }
}

impl Drop for TracksTap {
    fn drop(&mut self) {
        if let Some(sh) = self.shared.upgrade() {
            sh.tracks.lock_ok().retain(|r| r.id != self.id);
            for slot in sh.inputs.lock_ok().values() {
                slot.track_lanes.lock_ok().retain(|(i, ..)| *i != self.id);
            }
        }
        if let Some(t) = self.worker.take() {
            let _ = t.join();
        }
    }
}

impl LayerFade {
    /// Its level at `at` (0 hidden, 1 shown), and whether it is still under way.
    pub(super) fn level(&self, at: Instant) -> (f32, bool) {
        let t = if self.secs <= 0.0 {
            1.0
        } else {
            (at.saturating_duration_since(self.start).as_secs_f32() / self.secs).min(1.0)
        };
        (self.from + (self.to - self.from) * t, t < 1.0)
    }
}

impl Default for Monitor {
    fn default() -> Self {
        Monitor {
            preview: false,
            solo: Vec::new(),
            volume_db: 0.0,
            mute: false,
            card: None,
        }
    }
}

impl Meter {
    /// A block of sound (a mono one on both sides).
    pub(super) fn add(&mut self, channels: &[Vec<f32>]) {
        if self
            .since
            .is_none_or(|t| t.elapsed() >= Duration::from_millis(150))
        {
            self.before = self.now;
            self.now = [0.0; 2];
            self.since = Some(Instant::now());
        }
        for (side, peak) in self.now.iter_mut().enumerate() {
            if let Some(ch) = channels.get(side).or(channels.first()) {
                *peak = ch.iter().fold(*peak, |m, v| m.max(v.abs()));
            }
        }
    }

    /// Each side's peak, dBFS (−144 for silence or nothing for 0.3 s).
    pub(super) fn db(&self) -> [f32; 2] {
        let fresh = self
            .since
            .is_some_and(|t| t.elapsed() < Duration::from_millis(300));
        let db = |v: f32| {
            if fresh && v > 1e-7 {
                (20.0 * v.log10()).max(-144.0)
            } else {
                -144.0
            }
        };
        [0, 1].map(|i| db(self.now[i].max(self.before[i])))
    }
}

impl Worst {
    pub(super) fn note(&mut self, frame_us: u64, stall_us: u64) {
        let now = Instant::now();
        if self
            .since
            .is_none_or(|t| now - t >= Duration::from_secs(10))
        {
            if self.since.is_some() {
                self.last = (self.frame, self.stall);
            }
            *self = Worst {
                since: Some(now),
                frame: 0,
                stall: 0,
                last: self.last,
            };
        }
        self.frame = self.frame.max(frame_us);
        self.stall = self.stall.max(stall_us);
    }
}

impl Default for Captions {
    fn default() -> Self {
        Captions {
            on: true,
            from: None,
            clear_after: 6.0,
            captioner: Default::default(),
            recent: Default::default(),
        }
    }
}

impl std::ops::Deref for Composed {
    type Target = Frame;
    fn deref(&self) -> &Frame {
        match self {
            Composed::Shared(f) => f,
            Composed::Owned(f) => f,
        }
    }
}

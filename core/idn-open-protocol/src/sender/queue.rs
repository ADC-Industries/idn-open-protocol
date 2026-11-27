//! Frames on their way to a receiver: its queue (the newest picture wins), and its use of
//! this machine's rings (§E.26).

use super::*;

impl MachineRing {
    /// The control page, made on first need.
    pub(super) fn control(&mut self) -> Option<Arc<crate::machine::Control>> {
        if self.control.is_none() {
            self.control = Some(Arc::new(crate::machine::Control::create().ok()?));
            self.generation += 1;
        }
        self.control.clone()
    }
}

impl<'a> RingUse<'a> {
    pub(super) fn new(cache: &'a EncodeCache) -> Self {
        cache.machine.lock_ok().users += 1;
        cache.machine_live.store(true, Ordering::Relaxed);
        RingUse(cache)
    }
}

impl Drop for RingUse<'_> {
    fn drop(&mut self) {
        let mut m = self.0.machine.lock_ok();
        m.users = m.users.saturating_sub(1);
        if m.users == 0 {
            // Freed; the generation keeps counting, so a new set is offered as new.
            let generation = m.generation + 1;
            *m = MachineRing {
                generation,
                ..Default::default()
            };
            self.0.machine_live.store(false, Ordering::Relaxed);
            self.0.numbers.lock_ok().1.clear();
        }
    }
}

impl<'a> Take<'a> {
    pub(super) fn new(cache: &'a EncodeCache, gpu: bool) -> Self {
        let mut m = cache.machine.lock_ok();
        if gpu {
            m.gpu_takers += 1;
        } else {
            m.cpu_takers += 1;
        }
        Take { cache, gpu }
    }
}

impl Drop for Take<'_> {
    fn drop(&mut self) {
        let mut m = self.cache.machine.lock_ok();
        if self.gpu {
            m.gpu_takers = m.gpu_takers.saturating_sub(1);
        } else {
            m.cpu_takers = m.cpu_takers.saturating_sub(1);
        }
    }
}

impl InRing {
    pub(super) fn there(self) -> bool {
        self != InRing::No
    }
}

/// `f` without its picture (its audio, if any, to keep).
pub(crate) fn audio_part(old: &MediaFrame) -> Option<Arc<MediaFrame>> {
    (!old.audio.is_empty() || old.hx_audio.is_some()).then(|| {
        Arc::new(MediaFrame {
            captions: Vec::new(),
            fourcc: old.fourcc,
            width: 0,
            height: 0,
            planes: Arc::default(),
            frame_rate: old.frame_rate,
            audio: old.audio.clone(),
            sample_rate: old.sample_rate,
            timecode: old.timecode,
            timestamp: old.timestamp,
            metadata: None,
            interlaced: false,
            hx_video: None,
            hx_audio: old.hx_audio.clone(),
            content_id: None,
            precise: None,
            #[cfg(feature = "gpu")]
            gpu: None,
        })
    })
}

impl FrameQueue {
    const VIDEO: usize = 2;
    /// Frames of any kind (audio sent on its own comes in many small blocks).
    pub(super) const MAX: usize = 64;

    fn is_video(f: &MediaFrame) -> bool {
        !f.planes.is_empty() && f.hx_video.is_none()
    }

    /// Queues `f`; returns the number of frames dropped to make room, or `None` once the
    /// receiver's thread has ended.
    pub(super) fn push(&self, mut f: Arc<MediaFrame>) -> Option<u64> {
        let mut g = self.q.lock_ok();
        let (q, closed, hx_need_key) = &mut *g;
        if *closed {
            return None;
        }
        let mut dropped = 0;
        if Self::is_video(&f) && q.iter().filter(|m| Self::is_video(m)).count() >= Self::VIDEO {
            let i = q.iter().position(|m| Self::is_video(m)).unwrap();
            let old = q.remove(i).unwrap();
            dropped += 1;
            // Keep the audio that came with the picture.
            if let Some(a) = audio_part(&old) {
                q.insert(i, a);
            }
        }
        if q.len() >= Self::MAX && q.iter().any(|m| m.hx_video.is_some()) {
            // Too far behind on HX (R1-20): drop the pictures before the newest queued
            // keyframe (the stream restarts there), or all of them and wait for the next.
            // A keyframe at the very front frees nothing: the full queue would then lose
            // it to the pop below and send its pictures without it (a broken picture
            // until the next keyframe). Then all go, and the next keyframe restarts.
            let key_at = q
                .iter()
                .rposition(|m| m.hx_video.as_ref().is_some_and(|h| h.1))
                .filter(|&i| i > 0);
            let cut = key_at.unwrap_or(q.len());
            let rest = q.split_off(cut);
            dropped += q.iter().filter(|m| m.hx_video.is_some()).count() as u64;
            *q = q
                .drain(..)
                .filter_map(|m| {
                    if m.hx_video.is_some() {
                        audio_part(&m)
                    } else {
                        Some(m)
                    }
                })
                .chain(rest)
                .collect();
            *hx_need_key = key_at.is_none();
        }
        if let Some((_, key, _)) = &f.hx_video {
            if *key {
                *hx_need_key = false;
            } else if *hx_need_key {
                dropped += 1;
                match audio_part(&f) {
                    Some(a) => f = a,
                    None => return Some(dropped),
                }
            }
        }
        if q.len() >= Self::MAX {
            q.pop_front();
            dropped += 1;
        }
        q.push_back(f);
        self.ready.notify_one();
        Some(dropped)
    }

    /// The receiving side is gone: nothing pushed here is ever taken.
    pub(super) fn is_closed(&self) -> bool {
        self.q.lock_ok().1
    }

    /// Closed from the sending side (its encoder stopped): the receiver's session ends, and
    /// the receiver connects again.
    pub(super) fn close(&self) {
        let mut g = self.q.lock_ok();
        g.1 = true;
        g.0.clear();
        self.ready.notify_all();
    }

    pub(super) fn recv_timeout(&self, t: Duration) -> Option<Arc<MediaFrame>> {
        let mut g = self.q.lock_ok();
        let end = Instant::now() + t;
        loop {
            if let Some(f) = g.0.pop_front() {
                return Some(f);
            }
            let left = end.checked_duration_since(Instant::now())?;
            g = self
                .ready
                .wait_timeout(g, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}

impl Drop for FrameRx {
    fn drop(&mut self) {
        let mut g = self.0.q.lock_ok();
        g.1 = true;
        g.0.clear();
    }
}

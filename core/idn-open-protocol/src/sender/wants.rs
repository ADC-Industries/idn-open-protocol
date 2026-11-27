//! What a sender's receivers and taps want: pictures, how much sound, and its status.

use super::*;

impl Sender {
    /// Calls `f` with every frame sent from now on (pictures and audio), until
    /// [`untap`](Self::untap). `f` must return at once (queue the work elsewhere).
    pub fn tap(&self, f: FrameTap) -> u64 {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        self.taps.lock_ok().push((id, f));
        id
    }

    /// As [`tap`](Self::tap), wanting pictures only while `wanted` says so (a gateway
    /// with nowhere to send them): its pictures may then stay on the GPU
    /// ([`samples_wanted`](Self::samples_wanted)).
    pub fn tap_while(&self, wanted: Arc<dyn Fn() -> bool + Send + Sync>, f: FrameTap) -> u64 {
        let id = self.tap(f);
        self.tap_wanted.lock_ok().insert(id, wanted);
        id
    }

    /// Stops a [`tap`](Self::tap).
    pub fn untap(&self, id: u64) {
        self.taps.lock_ok().retain(|t| t.0 != id);
        self.tap_wanted.lock_ok().remove(&id);
        #[cfg(feature = "gpu")]
        self.gpu_taps.lock_ok().remove(&id);
    }

    /// Whether anything wants this output's pictures with their samples, in this memory:
    /// a receiver over the network, or on this machine reading shared memory other than
    /// on the GPU, multicast, a tap not on the GPU that wants pictures now. While nothing
    /// does, an application with its picture on the GPU need not bring it back
    /// (`Output::send_planes_gpu_sizes`).
    pub fn samples_wanted(&self) -> bool {
        self.samples_wanted_but(false)
    }

    /// As [`samples_wanted`](Self::samples_wanted), not counting receivers taking pictures
    /// through this machine's picture ring (§E.26): while it is false and `samples_wanted`
    /// true, only they want the samples, which [`send_filled`](Self::send_filled) writes
    /// straight into the ring.
    pub fn samples_wanted_off_ring(&self) -> bool {
        self.samples_wanted_but(true)
    }

    pub(super) fn samples_wanted_but(&self, ring: bool) -> bool {
        if self.multicast.is_some() {
            return true;
        }
        // The taps and what each wants, copied out and asked with no lock held: the
        // `wanted` closures are a gateway's own (bug run 4: they were called under three
        // of the output's locks, taken in another order than `gpu_only_why`'s).
        #[cfg(feature = "gpu")]
        let gpu = self.gpu_taps.lock_ok().clone();
        let ids: Vec<_> = self.taps.lock_ok().iter().map(|t| t.0).collect();
        let wanted: Vec<_> = {
            let w = self.tap_wanted.lock_ok();
            ids.iter()
                .filter(|id| {
                    #[cfg(feature = "gpu")]
                    if gpu.contains(id) {
                        return false;
                    }
                    let _ = id;
                    true
                })
                .map(|id| w.get(id).cloned())
                .collect()
        };
        let tap_wants = wanted.iter().any(|w| w.as_ref().is_none_or(|w| w()));
        if tap_wants {
            return true;
        }
        // (Receivers whose SpeedHQ the GPU encodes take pictures only there, §E.27.)
        #[cfg(feature = "gpu")]
        let (gpu_shq, gpu_made) = (self.cache.gpu_shq_ready(), self.cache.gpu_shq_made());
        #[cfg(not(feature = "gpu"))]
        let (gpu_shq, gpu_made) = (false, false);
        #[cfg(feature = "gpu")]
        let codec_gpu = self.codec_hub.as_ref().is_some_and(|h| h.gpu_ready());
        self.clients.lock_ok().iter().any(|c| {
            let sh = &c.shared;
            if c.tx.is_closed()
                || !sh.active.load(Ordering::Relaxed)
                || !sh.video.load(Ordering::Relaxed)
            {
                return false;
            }
            if sh.machine_ok.lock_ok().is_some() {
                // A receiver that took the rings: on the GPU, it wants none here; else the
                // picture ring is written from the samples. (Counted out while the GPU
                // encoded SpeedHQ for it, a network receiver beside it kept the samples
                // from ever reaching the ring, and it was served over the connection.)
                return !sh.machine_gpu.load(Ordering::Relaxed)
                    && !(ring && sh.ring_pictures.load(Ordering::Relaxed));
            }
            // A receiver of a shared encoder that takes pictures from the GPU wants none
            // here (§E.27).
            #[cfg(feature = "gpu")]
            if sh.coded.load(Ordering::Relaxed) && codec_gpu {
                return false;
            }
            // (One that may take the ring waits for the first picture the GPU encodes:
            // the ring needs samples for it to switch to.)
            let ready = gpu_shq || (gpu_made && !sh.ext.lock_ok().has("machine"));
            !(ready && sh.gpu_shq.load(Ordering::Relaxed))
        })
    }

    /// Latest health reports from receivers with `stats/1` active: (receiver name, report).
    /// Monitoring snapshot: receivers, encryption, skips, tally and totals.
    /// Whether anything needs this output's decoded pictures: a tap (recorder, gateway),
    /// or a receiver that wants video and isn't on a shared encoder passing compressed
    /// pictures through (SpeedHQ, an encode, one still being set up). A source of
    /// compressed pictures (a file) may skip making them while none does.
    pub fn needs_pictures(&self) -> bool {
        let why = self.needs_why();
        let mut last = self.needs_why.lock_ok();
        if *last != why {
            crate::log::log(&match &why {
                Some(w) => format!("pictures needed: {w}"),
                None => "pictures not needed: every receiver passes compressed pictures".into(),
            });
            *last = why.clone();
        }
        why.is_some()
    }

    pub(super) fn needs_why(&self) -> Option<String> {
        if !self.taps.lock_ok().is_empty() {
            return Some("a tap (recording or stream)".into());
        }
        self.clients.lock_ok().iter().find_map(|c| {
            let sh = &c.shared;
            if c.tx.is_closed()
                || !sh.active.load(Ordering::Relaxed)
                || !sh.video.load(Ordering::Relaxed)
            {
                return None;
            }
            let name = sh.name.lock_ok().clone();
            match sh.slot.lock_ok().as_ref() {
                None => Some(format!("receiver {name} (SpeedHQ)")),
                Some(s) if !s.passing.load(Ordering::Relaxed) => Some(format!(
                    "receiver {name} ({} encoded here, not passed through)",
                    s.key.codec.name()
                )),
                Some(_) => None,
            }
        })
    }

    pub fn status(&self) -> SenderStatus {
        let mut st = SenderStatus::default();
        for c in self.clients.lock_ok().iter().filter(|c| {
            c.shared.active.load(Ordering::Relaxed) && c.shared.writer.lock_ok().is_some()
        }) {
            st.receivers += 1;
            let on_ring = c.shared.machine_ok.lock_ok().is_some();
            st.machine_readers += usize::from(on_ring);
            st.gpu_readers += usize::from(on_ring && c.shared.machine_gpu.load(Ordering::Relaxed));
            st.encrypted += usize::from(c.shared.encrypted.load(Ordering::Relaxed));
            st.skipped += c.shared.skipped.load(Ordering::Relaxed);
            let (p, v) = *c.shared.tally.lock_ok();
            st.on_program |= p;
            st.on_preview |= v;
            st.per_receiver.push((
                c.shared.name.lock_ok().clone(),
                c.shared.quality.load(Ordering::Relaxed),
                c.shared.skipped.load(Ordering::Relaxed),
                c.shared.audio_dropped.load(Ordering::Relaxed),
            ));
        }
        let t = &self.totals;
        st.sent = (
            t.video_frames.load(Ordering::Relaxed),
            t.video_bytes.load(Ordering::Relaxed),
            t.audio_frames.load(Ordering::Relaxed),
            t.audio_bytes.load(Ordering::Relaxed),
        );
        st.metadata_sent = (
            t.metadata_frames.load(Ordering::Relaxed),
            t.metadata_bytes.load(Ordering::Relaxed),
        );
        st.shq_encodes = self.cache.shq_encodes.load(Ordering::Relaxed);
        if let Some(hub) = self.cache.codec.get() {
            st.codec_encoders = hub
                .slots
                .lock_ok()
                .iter()
                .filter(|s| !s.subscribers.lock_ok().is_empty())
                .count();
            st.codec_encoded = hub.encoded.load(Ordering::Relaxed);
        }
        st
    }

    pub fn receiver_reports(&self) -> Vec<(String, crate::ext::ReceiverReport)> {
        self.clients
            .lock_ok()
            .iter()
            .filter_map(|c| {
                c.shared
                    .report
                    .lock_ok()
                    .clone()
                    .map(|r| (c.shared.name.lock_ok().clone(), r))
            })
            .collect()
    }

    pub fn receivers(&self) -> usize {
        self.clients
            .lock_ok()
            .iter()
            .filter(|c| {
                c.shared.active.load(Ordering::Relaxed) && c.shared.writer.lock_ok().is_some()
            })
            .count()
    }

    /// Compressed video (§E.18): access units passed through as they came (R.12), and
    /// encoded here.
    pub fn codec_counts(&self) -> (u64, u64) {
        self.codec_hub.as_ref().map_or((0, 0), |h| {
            (
                h.passed.load(Ordering::Relaxed),
                h.encoded.load(Ordering::Relaxed),
            )
        })
    }

    /// A receiver gets compressed video from a shared encoder that may take pictures from
    /// the GPU (one hasn't failed to lately): the application sends them there too.
    #[cfg(feature = "gpu")]
    pub(super) fn codec_gpu_sessions(&self) -> bool {
        self.codec_hub.as_ref().is_some_and(|h| h.gpu_try())
            && self.clients.lock_ok().iter().any(|c| {
                let sh = &c.shared;
                !c.tx.is_closed()
                    && sh.active.load(Ordering::Relaxed)
                    && sh.video.load(Ordering::Relaxed)
                    && sh.coded.load(Ordering::Relaxed)
            })
    }

    /// A receiver gets SpeedHQ from this output's own encode (not a shared hardware
    /// encoder's, not the rings on this machine).
    #[cfg(feature = "gpu")]
    pub(super) fn shq_sessions(&self) -> bool {
        self.clients.lock_ok().iter().any(|c| {
            let sh = &c.shared;
            !c.tx.is_closed()
                && sh.active.load(Ordering::Relaxed)
                && sh.video.load(Ordering::Relaxed)
                && !sh.coded.load(Ordering::Relaxed)
                && sh.machine_ok.lock_ok().is_none()
        })
    }
}

//! The application's calls on a running sender: frames and metadata out to its receivers,
//! departures, and its end.

use super::*;

impl Sender {
    /// Whether IDN receivers can connect over the link (`link/1`, §E.11): the UDP port
    /// with this sender's port number is listened on.
    pub fn link_on(&self) -> bool {
        self.link.is_some()
    }

    /// Received metadata messages kept until taken; older ones are dropped.
    pub const METADATA_KEPT: usize = 256;
    /// Bytes of received metadata kept until taken; older messages are dropped.
    pub const METADATA_BYTES_KEPT: usize = 4 << 20;

    /// Sends a kind-3 metadata message to every receiver that wants metadata, on the
    /// control connection (§1.4). Never blocks: a thread does the writing. Returns false
    /// if 64 messages are already waiting (this one is dropped).
    pub fn send_metadata(&self, xml: &str) -> bool {
        let tx = self.metadata_out.get_or_init(|| {
            let (tx, rx) = sync_channel::<String>(64);
            let (clients, stop, totals) =
                (self.clients.clone(), self.stop.clone(), self.totals.clone());
            let _ = std::thread::Builder::new()
                .name("idn-metadata".into())
                .spawn(move || {
                    while let Ok(xml) = rx.recv() {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        let frame =
                            Frame::text(kind::METADATA, crate::frame::TIMECODE_SYNTHESIZE, &xml)
                                .encode();
                        let writers: Vec<_> = clients
                            .lock_ok()
                            .iter()
                            .filter(|c| !c.shared.no_text.load(Ordering::Relaxed))
                            .filter_map(|c| c.shared.writer.lock_ok().clone())
                            .collect();
                        for w in writers {
                            let mut w = w.lock_ok();
                            if w.write_all(&frame).is_ok() {
                                totals.metadata_frames.fetch_add(1, Ordering::Relaxed);
                                totals
                                    .metadata_bytes
                                    .fetch_add(frame.len() as u64, Ordering::Relaxed);
                            } else {
                                // (Cut mid-message: shut, as in `send_text`; F5-5.)
                                let _ = w.shutdown(std::net::Shutdown::Both);
                            }
                        }
                    }
                });
            tx
        });
        tx.try_send(xml.to_owned()).is_ok()
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Receivers in an accepted session. Connections still negotiating, or being held
    /// after a refusal (§E.8.5), are not counted.
    /// Group key changes of the encrypted multicast stream so far (§E.17.5), and the
    /// receivers on it now; `None` when it isn't encrypted.
    pub fn mc_keys(&self) -> Option<(u64, usize)> {
        self.mc_group
            .as_ref()
            .map(|g| (g.key_changes(), g.members()))
    }

    /// Encrypted multicast messages re-sent to receivers that lost them (§E.17.6).
    pub fn mc_resent(&self) -> u64 {
        self.mc_group.as_ref().map_or(0, |g| g.resent())
    }

    /// Offers a frame to every receiver; a receiver that is behind skips it.
    /// As [`send`](Self::send), for a picture that also came compressed (`coded`):
    /// inputs that take its codec at full size get it as it is (R.12). Needs a
    /// `content_id`.
    pub fn send_coded(&self, f: MediaFrame, coded: crate::codec::CodedPicture) {
        self.note_coded(&f, coded);
        self.send(f);
    }

    /// `coded` kept as `f`'s compressed picture, for inputs that take it as it is: what
    /// [`send_coded`](Self::send_coded) does before sending `f` (sent some other way: on
    /// the GPU).
    pub(crate) fn note_coded(&self, f: &MediaFrame, coded: crate::codec::CodedPicture) {
        if let (Some(h), Some(id)) = (&self.codec_hub, f.content_id) {
            let mut c = h.coded.lock_ok();
            c.push_back((id, Arc::new(coded)));
            while c.len() > 8 {
                c.pop_front();
            }
        }
    }

    /// As [`send`](Self::send) for sound that also came as AAC (`coded`): inputs that
    /// take AAC get it as it is (§E.19.2). Needs a `content_id`.
    pub fn send_coded_audio(&self, f: MediaFrame, coded: crate::codec::CodedAudio) {
        if let Some(id) = f.content_id {
            let mut c = self.cache.coded_audio.lock_ok();
            c.push_back((id, Arc::new(coded)));
            while c.len() > 64 {
                c.pop_front();
            }
        }
        self.send(f);
    }

    pub fn send(&self, f: MediaFrame) {
        let f = self.numbered(f);
        self.dispatch(f);
    }

    /// As [`send`](Self::send), `f`'s planes having their sizes only: `fill` writes the
    /// samples straight into this machine's picture ring (§E.26) for the receivers taking
    /// pictures there, called once with each plane's place (rows packed) before this
    /// returns. For a picture whose samples are in memory only the application reads (a
    /// GPU's mapped read-back) while only those receivers want them
    /// ([`samples_wanted_off_ring`](Self::samples_wanted_off_ring) false). Nothing else
    /// gets the picture.
    pub fn send_filled(&self, f: MediaFrame, fill: &mut crate::machine::PlaneFill) {
        let f = self.numbered(f);
        self.ring_filled(&f, fill);
        self.dispatch(f);
    }

    /// `f`'s picture written into the picture ring by `fill`, while the rings are offered.
    pub(super) fn ring_filled(&self, f: &Arc<MediaFrame>, fill: &mut crate::machine::PlaneFill) {
        if self.on_this_machine() && !f.planes.is_empty() {
            let _ = self.cache.machine_picture_by(f, Some(fill));
        }
    }

    /// `f` as sent: its colour description added, numbered for the rings.
    pub(super) fn numbered(&self, mut f: MediaFrame) -> Arc<MediaFrame> {
        if let Some(ci) = &self.color_info
            && f.width > 0
        {
            f.metadata = Some(crate::ext::with_color_info(f.metadata.as_deref(), ci));
        }
        let f = Arc::new(f);
        self.cache.number(&f);
        f
    }

    /// `f` to the taps, multicast, the encoders and every receiver.
    pub(super) fn dispatch(&self, f: Arc<MediaFrame>) {
        // The GPU's encoder warmed for this size ahead of a receiver (once a size).
        #[cfg(feature = "gpu")]
        if f.width > 0 && self.cache.gpu.lock_ok().is_some() {
            self.cache.warm_gpu_shq_size(&f);
        }
        // A picture on the GPU only: to the taps there and the receivers taking it there.
        let missing = f.samples_missing();
        if !f.planes.is_empty() || !f.audio.is_empty() {
            // Called outside the lock (a tap that taps or untaps deadlocked the sending
            // thread), each on its own: one that panics poisons nothing and stops no other.
            #[cfg(feature = "gpu")]
            let gpu = self.gpu_taps.lock_ok().clone();
            let taps: Vec<FrameTap> = self
                .taps
                .lock_ok()
                .iter()
                .filter(|t| {
                    // (A tap on the GPU takes a picture without samples from its copy
                    // there; without one, as while no recorder wants pictures, it has
                    // nothing to take: lab 348-A.)
                    #[cfg(feature = "gpu")]
                    if gpu.contains(&t.0) && f.gpu.is_some() {
                        return true;
                    }
                    let _ = t;
                    !missing
                })
                .map(|t| t.1.clone())
                .collect();
            for t in taps {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| t(&f)));
            }
        }
        if let Some(m) = self.multicast.as_ref().filter(|_| !missing) {
            m.push(f.clone());
        }
        if let Some(h) = &self.codec_hub {
            h.push(&f);
        }
        self.clients
            .lock_ok()
            // Receivers of a shared encoder get its frames instead (§E.18.3); a session
            // that ended goes (one on an encoder stayed, so the file player
            // went on making pictures for it).
            .retain(|c| {
                if c.tx.is_closed() {
                    return false;
                }
                c.shared.coded.load(Ordering::Relaxed)
                    || match c.tx.push(f.clone()) {
                        None => false,
                        Some(dropped) => {
                            c.shared.skipped.fetch_add(dropped, Ordering::Relaxed);
                            true
                        }
                    }
            });
    }

    /// Announces a planned departure to receivers with `resume/1` active (§E.4), then
    /// stops serving.
    pub fn depart(&self, d: crate::ext::Departure) {
        depart_all(&self.clients, &self.stop, &self.departing, d);
    }
}

/// Says goodbye to the receivers (§E.4), then stops: the byes go out before any session
/// sees `stop` and closes its connection.
pub(super) fn depart_all(
    clients: &Mutex<Vec<ClientSlot>>,
    stop: &AtomicBool,
    departing: &AtomicBool,
    d: crate::ext::Departure,
) {
    if departing.swap(true, Ordering::Relaxed) {
        return; // already departed
    }
    let msg = crate::ext::bye(d);
    // Collect the writers first: writing while holding `clients` would block every
    // sender (and this departure) behind one stalled receiver.
    let writers: Vec<_> = clients
        .lock_ok()
        .iter()
        .filter(|c| c.shared.ext.lock_ok().has("resume"))
        .filter_map(|c| c.shared.writer.lock_ok().clone())
        .collect();
    for w in writers {
        // A writer busy with a frame for a receiver that is behind is skipped: the bye
        // is a courtesy, departing must not wait for it.
        if let Some(mut s) = w.try_lock_ok() {
            let _ = s.write_all(&Frame::text(kind::CONTROL, 0, &msg).encode());
        }
    }
    // Links closed too, `GOING_AWAY` after the bye (§E.11.6): a process stopped by a
    // signal exits next, and its links' receivers otherwise waited for the idle timeout
    // to notice. A moment for the close to leave.
    // New sessions refused first (a receiver told of the departure retried at once and
    // reached this sender while it was still exiting), and the bye given a moment ahead
    // of the close, which drops what the link hasn't sent.
    stop.store(true, Ordering::Relaxed);
    let links: Vec<_> = clients
        .lock_ok()
        .iter()
        .filter_map(|c| c.shared.link.lock_ok().clone())
        .collect();
    if !links.is_empty() {
        std::thread::sleep(Duration::from_millis(30));
        for l in &links {
            l.close(crate::link::code::GOING_AWAY, "going away");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // (It looks at `stop` every 20 ms at most; never from itself.)
        if let Some(t) = self.accepting.lock_ok().take()
            && t.thread().id() != std::thread::current().id()
        {
            let _ = t.join();
        }
    }
}

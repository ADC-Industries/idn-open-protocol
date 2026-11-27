//! An output's sound: blocks sent as they come or at their time, coded or not, and how
//! much the receivers want.

use super::*;

impl Output {
    /// Sends audio on its own, independently of video: in blocks of any length (up to a
    /// second), as an audio callback delivers them. Never blocks; goes out at once.
    ///
    /// `timecode` is in 100 ns units. `None`: on the output's clock, which video timecodes
    /// use too (so recorders can line them up): continuing sample-accurately from the
    /// previous block while blocks arrive on time, and re-synchronised to the clock after
    /// a gap or drift of more than 50 ms. Receivers playing live line audio up with video
    /// by the timestamp, which is the time of the call.
    ///
    /// Errors: [`SendError::BadAudio`] for no channels, more than 256, channels of
    /// different lengths, an empty or over-long block, or a zero sample rate;
    /// [`SendError::Closed`] once the output is stopping.
    pub fn send_audio(&self, audio: AudioBlock, timecode: Option<i64>) -> Result<(), SendError> {
        self.send_audio_inner(audio, timecode, None, None)
    }

    /// As [`send_audio`](Self::send_audio), with the block's timestamp (100 ns since
    /// 1970, UTC: when its first sample was taken) given rather than now: for sound that
    /// waited in a buffer, so receivers line it up with pictures by their timestamps.
    pub fn send_audio_at(
        &self,
        audio: AudioBlock,
        timecode: Option<i64>,
        timestamp: i64,
    ) -> Result<(), SendError> {
        self.send_audio_inner(audio, timecode, None, Some(timestamp))
    }

    /// As [`send_audio`](Self::send_audio), for sound that is also available as AAC (a
    /// file's): IDN inputs that get compressed video from this output and decode AAC get
    /// the AAC frame as it is (§E.19.2); everyone else gets `audio`. `audio` must be the
    /// same sound, decoded.
    pub fn send_audio_coded(
        &self,
        audio: AudioBlock,
        timecode: Option<i64>,
        coded: crate::codec::CodedAudio,
    ) -> Result<(), SendError> {
        self.send_audio_inner(audio, timecode, Some(coded), None)
    }

    /// As [`send_audio_coded`](Self::send_audio_coded), with the block's timestamp given
    /// (as [`send_audio_at`](Self::send_audio_at)).
    pub fn send_audio_coded_at(
        &self,
        audio: AudioBlock,
        timecode: Option<i64>,
        coded: crate::codec::CodedAudio,
        timestamp: i64,
    ) -> Result<(), SendError> {
        self.send_audio_inner(audio, timecode, Some(coded), Some(timestamp))
    }

    /// Says where this output's sound comes from (idn-routing.md §A.16): a sound card's
    /// inputs, a network device's channels, a desk's buses. Shown with the transmitter in
    /// the Site Manager's matrix; sent again (and kept for a reconnect) when it changes.
    /// Nothing happens without the machine's Node.
    pub fn set_audio_origin(&self, origin: &crate::node::routing::AudioOrigin) {
        if let Some(r) = &self._node {
            let a = origin.attrs();
            let attrs: Vec<(&str, &str)> = a.iter().map(|(k, v)| (*k, v.as_str())).collect();
            r.update(&attrs);
        }
    }

    /// The Node lists this output as a transmitter once its channel count is known, and
    /// again when it changes (idn-routing.md §A.3).
    pub(super) fn tell_node_channels(&self, a: &AudioBlock) {
        let ch = a.channels.len() as u32;
        if let Some(r) = &self._node
            && self.node_channels.swap(ch, Ordering::Relaxed) != ch
        {
            let channels = ch.to_string();
            let labels = crate::node::routing::labels_attr(&self.audio_labels);
            let mut attrs = vec![("audio_channels", channels.as_str())];
            if !labels.is_empty() {
                attrs.push(("labels", labels.as_str()));
            }
            r.update(&attrs);
        }
        // The matrix's meters, while one watches (node/meters.rs).
        if let Some(m) = &self.meter
            && crate::node::meters::on()
        {
            m.add(&a.channels);
        }
    }

    pub(super) fn send_audio_inner(
        &self,
        audio: AudioBlock,
        timecode: Option<i64>,
        coded: Option<crate::codec::CodedAudio>,
        timestamp: Option<i64>,
    ) -> Result<(), SendError> {
        if self.shared.stop.load(Ordering::Relaxed) {
            return Err(SendError::Closed);
        }
        check_audio(&audio)?;
        self.tell_node_channels(&audio);
        let n = audio.channels[0].len() as i64;
        let timecode = {
            let mut clock = self.shared.audio_timecode.lock_ok();
            let rate = audio.sample_rate;
            let next = clock.filter(|c| c.rate == rate).map(|c| c.next());
            let tc = timecode.unwrap_or_else(|| {
                clock_tc(self.shared.epoch, Instant::now(), next, 500_000, 500_000)
            });
            // Continuing: count samples from the base, so block rounding never adds up
            // (R1-49); otherwise this block is the new base.
            *clock = Some(match *clock {
                Some(c) if next == Some(tc) => AudioClock {
                    samples: c.samples + n as u64,
                    ..c
                },
                _ => AudioClock {
                    base: tc,
                    samples: n as u64,
                    rate,
                },
            });
            tc
        };
        static AUDIO_IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let m = MediaFrame {
            captions: Vec::new(),
            fourcc: Fourcc::Shq2,
            width: 0,
            height: 0,
            planes: Arc::default(),
            frame_rate: self.shared.format.lock_ok().frame_rate,
            audio: audio.channels,
            sample_rate: audio.sample_rate,
            timecode,
            timestamp: timestamp.unwrap_or_else(now_100ns),
            // A shared clock: receivers on the same grandmaster may play by time (§C.4).
            metadata: crate::node::clock::grandmaster().map(|gm| crate::ext::clock_metadata(&gm)),
            interlaced: false,
            hx_video: None,
            hx_audio: None,
            // Ids of their own (the top two bits set), for the AAC that may go with it.
            content_id: coded
                .is_some()
                .then(|| (3 << 62) | AUDIO_IDS.fetch_add(1, Ordering::Relaxed)),
            precise: None,
            #[cfg(feature = "gpu")]
            gpu: None,
        };
        match coded {
            Some(c) => self.sender.send_coded_audio(m, c),
            None => self.sender.send(m),
        }
        Ok(())
    }

    /// Whether anything wants this output's pictures with their samples in this memory:
    /// a receiver over the network or reading this machine's shared memory other than
    /// on the GPU, multicast, a tap that wants pictures now. While nothing does, a
    /// picture composed on the GPU can be sent with
    /// [`send_planes_gpu_sizes`](Output::send_planes_gpu_sizes), never read back.
    pub fn samples_wanted(&self) -> bool {
        self.sender.samples_wanted()
    }

    /// As [`samples_wanted`](Output::samples_wanted), not counting receivers on this
    /// machine taking pictures through its shared memory (§E.26). While this is false and
    /// `samples_wanted` true, only they want the samples: a picture read back from the GPU
    /// can be written straight into the shared memory with
    /// [`send_planes_filled`](Output::send_planes_filled), never copied into this
    /// process's memory first.
    pub fn samples_wanted_off_ring(&self) -> bool {
        self.sender.samples_wanted_off_ring()
    }
}

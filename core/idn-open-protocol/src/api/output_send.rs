//! An output's frames and messages: pictures handed over, taps, metadata both ways,
//! tally, live settings and statistics.

use super::*;

impl Output {
    /// The full name receivers see, `HOST (name)`.
    pub fn name(&self) -> &str {
        &self.full_name
    }

    /// Its pictures' colour (primaries, transfer, matrix, bits), as configured.
    pub fn color_info(&self) -> &crate::ext::ColorInfo {
        &self.color
    }

    /// Hands a frame to the output. Never blocks: if the previous frame hasn't been taken
    /// yet, it is replaced (the newest frame wins). Conversion and encoding happen on the
    /// output's own thread.
    ///
    /// Errors: [`SendError::BadFrame`] if the size, stride or buffer length don't match;
    /// [`SendError::Closed`] once the output is stopping.
    pub fn send(&self, frame: VideoFrame) -> Result<(), SendError> {
        if self.shared.stop.load(Ordering::Relaxed) {
            return Err(SendError::Closed);
        }
        if frame.width == 0 || frame.height == 0 || frame.width > 8192 || frame.height > 8192 {
            return Err(SendError::BadFrame(format!(
                "size {}x{}",
                frame.width, frame.height
            )));
        }
        if let Some(p) = &frame.p216 {
            if !frame.width.is_multiple_of(2) || p.len() < 2 * frame.width * frame.height {
                return Err(SendError::BadFrame(format!(
                    "P216: {} samples for {}x{} (even width needed)",
                    p.len(),
                    frame.width,
                    frame.height
                )));
            }
        } else if frame.stride < frame.width * 4
            // Checked: a huge stride must be refused here, not wrap and panic later.
            || frame
                .stride
                .checked_mul(frame.height - 1)
                .and_then(|n| n.checked_add(frame.width * 4))
                .is_none_or(|need| frame.rgba.len() < need)
        {
            return Err(SendError::BadFrame(format!(
                "{} bytes for {}x{} with stride {}",
                frame.rgba.len(),
                frame.width,
                frame.height,
                frame.stride
            )));
        }
        if let Some(a) = &frame.audio {
            check_audio(a)?;
            self.tell_node_channels(a);
        }
        // Counted only once accepted (R1-51).
        self.shared.submitted.fetch_add(1, Ordering::Relaxed);
        *self.shared.size.lock_ok() = (frame.width, frame.height);
        let mut frame = frame;
        let own = frame.audio.as_ref().map_or(0, |a| a.channels[0].len());
        let mut carried = 0;
        let mut slot = self.shared.slot.lock_ok();
        if let Some((old, _, _)) = slot.take() {
            self.shared.replaced.fetch_add(1, Ordering::Relaxed);
            // The replaced frame's picture is dropped, but not its audio: it goes out
            // ahead of the new frame's, when the formats allow joining them.
            frame.audio = match (old.audio, frame.audio.take()) {
                (Some(mut o), Some(n))
                    if o.sample_rate == n.sample_rate && o.channels.len() == n.channels.len() =>
                {
                    // At most a second is kept if the output falls far behind.
                    let keep = (o.sample_rate as usize).min(crate::media::MAX_AUDIO_SAMPLES);
                    for (a, b) in o.channels.iter_mut().zip(n.channels) {
                        a.extend(b);
                        let excess = a.len().saturating_sub(keep);
                        a.drain(..excess);
                    }
                    Some(o)
                }
                (o, None) => o,
                (_, n) => n,
            };
            carried = frame
                .audio
                .as_ref()
                .map_or(0, |a| a.channels[0].len())
                .saturating_sub(own);
        }
        // The picture's time is when it was handed over, not when the worker gets to it.
        if frame.timestamp.is_none() {
            frame.timestamp = Some(now_100ns());
        }
        *slot = Some((frame, Instant::now(), carried));
        drop(slot);
        self.shared.ready.notify_one();
        Ok(())
    }

    /// Sends an XML metadata message to the connected receivers (those that haven't turned
    /// metadata off), e.g. a custom element for another application of yours. Never
    /// blocks; goes out at once, on each receiver's control connection. Receivers that
    /// connect later don't get it.
    ///
    /// Errors: [`SendError::BadMetadata`] if the text is empty, contains a NUL, doesn't
    /// look like XML (`<…>`) or is over 1 MB, or if 64 messages are already waiting to go
    /// out; [`SendError::Closed`] once the output is stopping.
    pub fn send_metadata(&self, xml: &str) -> Result<(), SendError> {
        if self.shared.stop.load(Ordering::Relaxed) {
            return Err(SendError::Closed);
        }
        let x = xml.trim();
        if !(x.starts_with('<') && x.ends_with('>')) || xml.contains('\0') || xml.len() > 1 << 20 {
            return Err(SendError::BadMetadata(
                "not a single XML element of up to 1 MB".into(),
            ));
        }
        if !self.sender.send_metadata(xml) {
            return Err(SendError::BadMetadata("64 messages already waiting".into()));
        }
        Ok(())
    }

    /// Whether some receiver has this output on program, and on preview (their tally).
    pub fn tally(&self) -> (bool, bool) {
        let st = self.sender.status();
        (st.on_program, st.on_preview)
    }

    /// Metadata messages receivers have sent to this output since the last call, oldest
    /// first, as (receiver name, XML). At most the latest 256 are kept. Tally and other
    /// protocol messages are handled internally and don't appear here.
    pub fn recv_metadata(&self) -> Vec<(String, String)> {
        self.recv_messages()
            .into_iter()
            .map(|m| (m.receiver, m.xml))
            .collect()
    }

    /// As [`Output::recv_metadata`], and whether each came from a device of this
    /// machine's site: over a session authenticated with the site's device certificates
    /// (`secure/2`). For commands only the site's own machines may give (a mixer's snapshot
    /// from a cue list, idn-mixer §M.21). Takes the same messages as `recv_metadata`.
    pub fn recv_messages(&self) -> Vec<ReceiverMessage> {
        std::mem::take(&mut *self.sender.metadata.lock_ok())
            .into_iter()
            .map(|(receiver, xml, trust)| ReceiverMessage {
                receiver,
                xml,
                site: trust == crate::sender::Trust::Site,
                linked: trust == crate::sender::Trust::Linked,
            })
            .collect()
    }

    /// Keyboard and mouse events receivers sent since the last call (R.25, with
    /// [`OutputConfig::remote_control`]), in order, from devices of this machine's site
    /// or computers linked to it (§E.28) only, and how many messages of control came from
    /// elsewhere and were left out.
    /// Takes the messages [`Output::recv_messages`] would: an output that takes control
    /// reads its metadata here.
    pub fn recv_control(&self) -> (Vec<crate::remote::Control>, usize) {
        let (mut events, mut refused) = (Vec::new(), 0);
        for m in self.recv_messages() {
            if let Some(ev) = crate::remote::from_xml(&m.xml) {
                if m.site || m.linked {
                    events.extend(ev);
                } else {
                    refused += 1;
                }
            }
        }
        (events, refused)
    }

    /// As [`recv_control`](Output::recv_control), waiting up to `timeout` for some to
    /// come: control goes on as it arrives (R.25 stage 2).
    pub fn wait_control(
        &self,
        timeout: std::time::Duration,
    ) -> (Vec<crate::remote::Control>, usize) {
        let end = Instant::now() + timeout;
        loop {
            let (lock, cv) = &crate::sender::METADATA_ARRIVED;
            let seen = *lock.lock_ok();
            let got = self.recv_control();
            if !got.0.is_empty() || got.1 > 0 {
                return got;
            }
            let now = Instant::now();
            if now >= end {
                return got;
            }
            // (Anything that came after `seen` was read wakes this at once.)
            let g = lock.lock_ok();
            let _ = cv.wait_timeout_while(g, end - now, |n| *n == seen);
        }
    }

    /// Sends a Y'CbCr picture as it is, bypassing the RGBA conversion (and the one-frame
    /// mailbox: it goes out at once). For players and gateways that have planes already,
    /// such as decoded video files (`idn-ffmpeg`). With [`PlanarFrame::coded`], IDN
    /// inputs that take that codec get the compressed picture instead of a new encode.
    ///
    /// Errors: [`SendError::BadFrame`] if the planes don't match the layout and size.
    pub fn send_planes(&self, f: PlanarFrame) -> Result<(), SendError> {
        self.send_planes_by(f, true, |m| self.sender.send(m))
    }

    /// [`send_planes`](Output::send_planes), the frame (unless compressed) sent by `send`.
    /// (`samples`: the planes have them; else their sizes alone.)
    pub(super) fn send_planes_by(
        &self,
        f: PlanarFrame,
        samples: bool,
        send: impl FnOnce(MediaFrame),
    ) -> Result<(), SendError> {
        if self.shared.stop.load(Ordering::Relaxed) {
            return Err(SendError::Closed);
        }
        let (w, h) = (f.width, f.height);
        // Chroma's size; with alpha (SHQ7, SHQ9) a fourth plane the luma's size (a file's
        // alpha, a stinger's, is taken here too).
        let ((cw, ch), alpha) = match f.layout {
            Fourcc::Shq0 => ((w.div_ceil(2), h.div_ceil(2)), false),
            Fourcc::Shq2 => ((w.div_ceil(2), h), false),
            Fourcc::Shq4 => ((w, h), false),
            Fourcc::Shq7 => ((w.div_ceil(2), h), true),
            Fourcc::Shq9 => ((w, h), true),
        };
        // Compressed only (no planes): for a source that skips making pictures while
        // nothing needs them ([`Output::needs_pictures`]).
        let coded_only = f.planes.is_empty() && f.coded.is_some();
        let ok = w > 0
            && h > 0
            && w <= 8192
            && h <= 8192
            && (coded_only
                || f.planes.len() == 3 + usize::from(alpha)
                    && f.planes
                        .iter()
                        .zip([(w, h), (cw, ch), (cw, ch), (w, h)])
                        .all(|(p, (pw, ph))| {
                            (p.width, p.height) == (pw, ph)
                                && p.data.len() == if samples { pw * ph } else { 0 }
                        }));
        if !ok {
            return Err(SendError::BadFrame(format!(
                "planes don't match {w}x{h} {:?}",
                f.layout
            )));
        }
        if let Some(a) = &f.audio {
            check_audio(a)?;
            self.tell_node_channels(a);
        }
        if !coded_only {
            // For the monitoring properties, as `send` does.
            *self.shared.size.lock_ok() = (w, h);
        }
        let rate = self.shared.format.lock_ok().frame_rate;
        let timecode = f.timecode.unwrap_or_else(|| self.timecode_now());
        let (audio, sample_rate) = match f.audio {
            Some(a) => (a.channels, a.sample_rate),
            None => (Vec::new(), 48_000),
        };
        static CONTENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let m = MediaFrame {
            captions: self.shared.captions.lock_ok().take().unwrap_or_default(),
            fourcc: f.layout,
            width: w,
            height: h,
            planes: f.planes,
            frame_rate: rate,
            audio,
            sample_rate,
            timecode,
            timestamp: f.timestamp.unwrap_or_else(now_100ns),
            metadata: None,
            interlaced: false,
            hx_video: None,
            hx_audio: None,
            // Ids of their own (the top bit set), apart from the RGBA path's.
            content_id: Some((1 << 63) | CONTENT.fetch_add(1, Ordering::Relaxed)),
            precise: None,
            #[cfg(feature = "gpu")]
            gpu: None,
        };
        // Sent by `send` either way: a compressed picture with its picture on the GPU
        // went as `send_coded`, without it, and the switcher's input of a file got
        // neither samples nor the GPU's picture (lab 321-B, 321-C).
        if let Some(c) = f.coded {
            self.sender.note_coded(&m, c);
        }
        send(m);
        self.shared.submitted.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Closed captions to carry with the next picture sent with
    /// [`send_planes`](Output::send_planes): A/53 `cc_data` ([`crate::cea608::Captioner`]
    /// makes it a frame at a time). They reach this output's taps (a stream's or
    /// recording's encoder puts them in the video); receivers don't get them.
    pub fn caption_next(&self, cc: Vec<u8>) {
        *self.shared.captions.lock_ok() = Some(cc).filter(|c| !c.is_empty());
    }

    /// Whether anything needs this output's pictures as samples: a [`tap`](Output::tap),
    /// or a receiver getting SpeedHQ or an encode (not one passed the compressed pictures
    /// of [`PlanarFrame::coded`] as they are). While it doesn't, a source of compressed
    /// pictures (a file player) may send them without planes and skip decoding them; a
    /// receiver that then needs them gets pictures from the next one with planes.
    pub fn needs_pictures(&self) -> bool {
        self.sender.needs_pictures()
    }

    /// Calls `f` with each picture and audio block this output sends, from now on, until
    /// the returned [`Tap`] is dropped: what a recorder or streaming gateway needs
    /// (`idn-ffmpeg`). `f` runs on the output's thread and must return at once (hand
    /// the frame to another thread).
    pub fn tap(&self, f: impl Fn(&TappedFrame) + Send + Sync + 'static) -> Tap {
        self.tap_by(None, false, f)
    }

    /// As [`tap`](Output::tap), wanting pictures only while `wanted` says so (a gateway
    /// with no recording or stream): meanwhile an application whose pictures are on the
    /// GPU may leave them there ([`samples_wanted`](Output::samples_wanted)). A picture
    /// left there doesn't reach this tap.
    pub fn tap_while(
        &self,
        wanted: impl Fn() -> bool + Send + Sync + 'static,
        f: impl Fn(&TappedFrame) + Send + Sync + 'static,
    ) -> Tap {
        self.tap_by(Some(Arc::new(wanted)), false, f)
    }

    pub(super) fn tap_by(
        &self,
        wanted: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
        on_gpu: bool,
        f: impl Fn(&TappedFrame) + Send + Sync + 'static,
    ) -> Tap {
        let call: crate::sender::FrameTap = Arc::new(move |m: &Arc<MediaFrame>| {
            f(&TappedFrame {
                layout: m.fourcc,
                width: m.width,
                height: m.height,
                planes: m.planes.clone(),
                frame_rate: m.frame_rate,
                timecode: m.timecode,
                timestamp: m.timestamp,
                audio: m.audio.clone(),
                sample_rate: m.sample_rate,
                captions: m.captions.clone(),
                #[cfg(feature = "gpu")]
                gpu: m.gpu.clone(),
                precise: m.precise.clone(),
            })
        });
        #[cfg(feature = "gpu")]
        let id = match (wanted, on_gpu) {
            (Some(w), true) => self.sender.tap_gpu_while(w, call),
            (Some(w), false) => self.sender.tap_while(w, call),
            (None, true) => self.sender.tap_gpu(call),
            (None, false) => self.sender.tap(call),
        };
        #[cfg(not(feature = "gpu"))]
        let id = {
            let _ = on_gpu;
            match wanted {
                Some(w) => self.sender.tap_while(w, call),
                None => self.sender.tap(call),
            }
        };
        Tap {
            sender: self.sender.clone(),
            id,
        }
    }

    /// As [`send_planes`](Output::send_planes), `f`'s planes with their sizes and no
    /// samples: `fill` writes them straight into this machine's shared memory (§E.26),
    /// called once with each plane's place there (its rows packed, Y, Cb, Cr) before this
    /// returns; it says whether it wrote them. For use while only receivers reading that
    /// memory want samples ([`samples_wanted_off_ring`](Output::samples_wanted_off_ring)
    /// false): a receiver or tap of another kind gets no picture from this frame.
    pub fn send_planes_filled(
        &self,
        f: PlanarFrame,
        fill: &mut crate::machine::PlaneFill,
    ) -> Result<(), SendError> {
        self.send_planes_by(f, false, |m| self.sender.send_filled(m, fill))
    }

    /// The output's clock now, in 100 ns units: the timecode a frame or audio block
    /// handed over at this moment gets by default. Use it to stamp media that was
    /// captured earlier (subtract its age), so that audio and video meet on one timeline.
    pub fn timecode_now(&self) -> i64 {
        clock_tc(self.shared.epoch, Instant::now(), None, 0, 0)
    }

    /// Turns the alpha channel on or off from the next frame on (see
    /// [`OutputConfig::alpha`]). Connected receivers stay connected and simply get the
    /// new format.
    pub fn set_alpha(&self, alpha: bool) {
        self.shared.format.lock_ok().alpha = alpha;
    }

    /// Changes the frame rate the stream is labelled with, from the next frame on (see
    /// [`OutputConfig::frame_rate`]). Receivers stay connected. A zero numerator or
    /// denominator is ignored.
    pub fn set_frame_rate(&self, frame_rate: (u32, u32)) {
        if frame_rate.0 > 0 && frame_rate.1 > 0 {
            self.shared.format.lock_ok().frame_rate = frame_rate;
        }
    }

    /// The TCP port receivers connect to (useful with `port: 0`, and for manual
    /// `ip:port` entry on the receiving side).
    pub fn port(&self) -> u16 {
        self.sender.port()
    }

    /// Whether inputs of this process opened with [`InputConfig::local`] take this
    /// output's frames as they are (the default) or receive it over the network like any
    /// other. Off for an output whose own compressed pictures are cheaper to receive than
    /// its samples: a file passing its pictures through needs no decode of its own then.
    /// Inputs already open keep their way.
    ///
    /// [`InputConfig::local`]: crate::input::InputConfig::local
    pub fn set_local_inputs(&self, on: bool) {
        if on {
            crate::local::register(&self.sender);
        } else {
            crate::local::unregister(self.sender.port());
        }
    }

    /// Receivers currently connected and receiving.
    pub fn receivers(&self) -> usize {
        self.sender.receivers()
    }

    /// Health reports from connected IDN receivers, by receiver name: frames received,
    /// lost and late, decode time and transport. Updated about once a second. NDI
    /// receivers don't report.
    pub fn receiver_reports(&self) -> Vec<(String, crate::ext::ReceiverReport)> {
        self.sender.receiver_reports()
    }

    /// Withdraws the output and tells receivers why. With
    /// [`Departure::Restart`](crate::ext::Departure::Restart), IDN receivers keep retrying
    /// quickly and reconnect within about a second of the output reappearing.
    pub fn close(self, why: crate::ext::Departure) {
        self.sender.depart(why);
        // Drop does the rest; the departure has already been announced.
    }

    /// Counters since the output was created.
    pub fn stats(&self) -> OutputStats {
        let st = self.sender.status();
        OutputStats {
            submitted: self.shared.submitted.load(Ordering::Relaxed),
            replaced: self.shared.replaced.load(Ordering::Relaxed),
            repeated: self.shared.repeated.load(Ordering::Relaxed),
            mc_key_changes: self.sender.mc_keys().map_or(0, |k| k.0),
            mc_receivers: self.sender.mc_keys().map_or(0, |k| k.1),
            mc_resent: self.sender.mc_resent(),
            codec_encoded: self.sender.codec_counts().1,
            codec_passed: self.sender.codec_counts().0,
            machine_readers: st.machine_readers,
            gpu_readers: st.gpu_readers,
            shq_encodes: st.shq_encodes,
            codec_encoders: st.codec_encoders,
        }
    }
}

//! Pictures and sound from a source on this machine through its shared memory (§E.26)
//! and its GPU ring (§E.27).

use super::*;

impl Worker {
    /// Item `seq` of the source's rings (§E.26): a picture (`sound` false) or a block of
    /// sound, handed over as one that came over the connection is. One gone from its slot
    /// (this input fell behind) is counted and dropped.
    pub(super) fn machine_item(&mut self, epoch: u64, sound: bool, seq: u64) {
        let Some(m) = self.machine.as_mut().filter(|m| m.epoch == epoch) else {
            return;
        };
        let (seen, from) = if sound {
            (&mut m.seen.1, m.from.1)
        } else {
            (&mut m.seen.0, m.from.0)
        };
        *seen = (*seen).max(seq);
        // Before the source said it left the connection: it came over the connection.
        if from.is_none_or(|f| seq < f) {
            return;
        }
        // The picture ring imported as a GPU buffer, the first time (S2).
        #[cfg(feature = "gpu")]
        if !sound
            && m.host.is_none()
            && let (Some(hand), Some(ring)) = (&self.cfg.gpu, &m.pic)
        {
            let (ptr, len) = ring.memory();
            // SAFETY: the buffer is dropped before the ring (field order), and every
            // copy from it is waited for before the picture is handed over.
            let imported = unsafe { crate::gpu::import_host(&hand.device, ptr, len) };
            let msg = match &imported {
                Ok(Some(_)) => "<- media: the GPU reads pictures from shared memory itself".to_owned(),
                Ok(None) => "<- media: pictures from shared memory copied to the GPU (this GPU can't read it)".to_owned(),
                Err(e) => format!("<- media: pictures from shared memory copied to the GPU ({e})"),
            };
            m.host = Some(imported.ok().flatten());
            self.log(msg);
        }
        let Some(m) = self.machine.as_ref().filter(|m| m.epoch == epoch) else {
            return;
        };
        if sound {
            match m
                .sound
                .as_ref()
                .and_then(|r| crate::machine::get_sound(r, seq))
            {
                Some(snd) => self.machine_sound(snd),
                None => self.stat(|s| s.dropped += 1),
            }
        } else {
            // For an input that draws on the GPU, straight from the ring to the GPU.
            #[cfg(feature = "gpu")]
            if let Some(hand) = &self.cfg.gpu {
                if self.cfg.skip_late && self.tx.video_waiting.load(Ordering::Relaxed) > 0 {
                    self.stat(|s| s.late += 1);
                    return;
                }
                let fresh = self.uploads.is_none();
                let up = self
                    .uploads
                    .get_or_insert_with(|| crate::gpu::Uploads::new(hand));
                let got = m.pic.as_ref().and_then(|r| {
                    let host = m.host.as_ref().and_then(|h| h.as_ref());
                    crate::machine::get_picture_gpu(r, seq, up, host)
                });
                if fresh {
                    self.log("<- media: pictures from shared memory go straight to the GPU");
                }
                match got {
                    Some((sh, Some(g))) => {
                        self.machine_gpu_picture(sh, g, "video via machine (upload)")
                    }
                    Some((sh, None)) => self.machine_picture(sh),
                    None => self.stat(|s| s.video_dropped += 1),
                }
                return;
            }
            match m
                .pic
                .as_ref()
                .and_then(|r| crate::machine::get_picture(r, seq))
            {
                Some(sh) => self.machine_picture(sh),
                None => self.stat(|s| s.video_dropped += 1),
            }
        }
    }

    /// A block from the source's sound ring: the channels this input asked for (as the
    /// source picks them for the connection, `channels/1`), as float sound.
    pub(super) fn machine_sound(&mut self, snd: crate::machine::Sound) {
        let received = Instant::now();
        let total = snd.channels.len();
        let wanted = self
            .channels_requested
            .clone()
            .or_else(|| self.cfg.audio_channels.clone());
        let (channels, list_meta) = match wanted.as_deref() {
            Some(list) => {
                let kept: Vec<u16> = list
                    .iter()
                    .copied()
                    .filter(|&c| (c as usize) < total)
                    .collect();
                // Moved out of the block, not copied (a channel asked for twice: copied).
                let mut src = snd.channels;
                let mut picked: Vec<Vec<f32>> = Vec::with_capacity(kept.len());
                for (i, &c) in kept.iter().enumerate() {
                    let v = match kept[..i].iter().position(|&d| d == c) {
                        Some(j) => picked[j].clone(),
                        None => std::mem::take(&mut src[c as usize]),
                    };
                    picked.push(v);
                }
                (picked, Some(crate::ext::channels_metadata(&kept, total)))
            }
            None => (snd.channels, None),
        };
        if channels.is_empty() {
            return;
        }
        let ah = AudioHeader {
            fourcc: crate::media::FOURCC_IDF3,
            samples: channels[0].len() as u32,
            channels: channels.len() as u32,
            sample_rate: snd.sample_rate,
            timecode: snd.timecode,
            timestamp: snd.timestamp,
            metadata: crate::ext::join_metadata(list_meta, snd.metadata.as_deref()),
        };
        if self.last_video.is_none() {
            self.last_via = "machine"; // sound only
        }
        self.last_audio = Some(ah.clone());
        self.stat(|s| {
            s.audio += 1;
            *s.via.entry("audio via machine".into()).or_default() += 1;
        });
        let ts = ah.timestamp;
        let ev = InputEvent::Audio(AudioIn {
            header: ah,
            channels,
            via: "machine",
            received,
        });
        self.present(ev, false, ts, None, 0, received);
    }

    /// A picture from the source's ring (§E.26): handed over as a decoded one is.
    pub(super) fn machine_picture(&mut self, sh: crate::machine::Shared) {
        if self.cfg.skip_late && self.tx.video_waiting.load(Ordering::Relaxed) > 0 {
            self.stat(|s| s.late += 1);
            return;
        }
        // Pictures from the connection still decoding on the side are older (the first
        // ring pictures overtook them): handed over first.
        self.collect_ahead(true);
        // On the GPU (§E.27): copied from the source's slot into a buffer of this input's,
        // then its stamp looked at again, as a slot of the CPU ring is.
        #[cfg(feature = "gpu")]
        if let Some((slot, gseq)) = sh.gpu
            && let Some((slots, control)) = self.machine.as_ref().and_then(|m| m.gpu.as_ref())
        {
            let sizes: Vec<(u32, u32)> = sh
                .planes
                .iter()
                .map(|p| (p.width as u32, p.height as u32))
                .collect();
            let (planes, _) = crate::gpu::slot_layout(&sizes);
            let stamp = control.gpu_stamp(slot as usize);
            let current = || stamp.is_some_and(|s| s.load(Ordering::Acquire) == gseq * 2);
            let got = if current() {
                slots.copy_out(slot as usize, planes).ok()
            } else {
                None
            };
            match got.filter(|_| current()) {
                Some(g) => return self.machine_gpu_picture(sh, g, "video via machine (GPU)"),
                None if sh.gpu_only => {
                    self.stat(|s| s.video_dropped += 1);
                    return;
                }
                // Written over meanwhile: the samples are here too.
                None => {}
            }
        }
        if sh.gpu_only {
            // On the GPU only, and this input doesn't take it there (it said so; the first
            // pictures after a change of rings).
            self.stat(|s| s.video_dropped += 1);
            return;
        }
        let vh = sh.header();
        self.last_via = "machine";
        self.stat(|s| {
            s.video += 1;
            *s.via.entry("video via machine".into()).or_default() += 1;
        });
        self.show(vh, sh.picture(), "machine", Instant::now());
    }

    /// A picture from the source's rings handed over on the GPU (`g`), counted under `via`.
    #[cfg(feature = "gpu")]
    pub(super) fn machine_gpu_picture(
        &mut self,
        sh: crate::machine::Shared,
        g: crate::gpu::GpuPicture,
        via: &'static str,
    ) {
        if self.cfg.skip_late && self.tx.video_waiting.load(Ordering::Relaxed) > 0 {
            self.stat(|s| s.late += 1);
            return;
        }
        self.collect_ahead(true); // as for a picture in memory
        let vh = sh.header();
        self.last_via = "machine";
        self.stat(|s| {
            s.video += 1;
            *s.via.entry(via.into()).or_default() += 1;
        });
        self.next_gpu = Some(g);
        let mut picture = sh.picture();
        picture.planes.iter_mut().for_each(|p| p.data.clear());
        // No RGBA from the GPU's picture: its samples aren't here.
        let (color, conv) = self.picture_color(&vh, picture.height, None);
        self.deliver(vh, picture, None, color, conv, "machine", Instant::now())
    }

    /// The source's GPU ring offered with its rings (§E.27), imported on this input's GPU;
    /// `None` without a GPU given, on another GPU, or when it can't be.
    #[cfg(feature = "gpu")]
    pub(super) fn open_gpu_ring(
        &mut self,
        offer: &crate::control::Element,
        control: &str,
    ) -> Option<(crate::gpu::GpuSlots, crate::machine::Control)> {
        let hand = self.cfg.gpu.clone()?;
        if offer.attr("gpu").is_none() {
            // Said, so that shared memory isn't a mystery: a switcher
            // passing a picture through as it came has none on the GPU. It offers again
            // when it has.
            self.log(
                "<- media: the source has no pictures on the GPU now (gpu/1): through shared memory",
            );
            return None;
        }
        match crate::gpu::GpuSlots::open(&hand, |k| offer.attr(k))
            .and_then(|s| Ok((s, crate::machine::Control::open(control)?)))
        {
            Ok((Some(slots), c)) => {
                self.log("<- media: pictures GPU to GPU on this machine (gpu/1)");
                Some((slots, c))
            }
            Ok((None, _)) => None,
            Err(err) => {
                self.log(format!(
                    "<- media: the source's GPU pictures: {err}; through shared memory"
                ));
                None
            }
        }
    }
}

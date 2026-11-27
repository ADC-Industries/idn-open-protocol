//! An input's sound: decoded and handed over.

use super::*;

impl Worker {
    pub(super) fn on_audio(&mut self, f: Frame, via: &'static str, received: Instant) {
        if self.last_video.is_none() {
            self.last_via = via; // sound only
        }
        self.stat(|s| s.audio_bytes += f.payload.len() as u64);
        let ah = match AudioHeader::parse(&f.ext) {
            Ok(h) => h,
            Err(e) => {
                self.stat(|s| s.audio_errors += 1);
                return self.emit(InputEvent::Error(format!("audio: bad header ({e})")));
            }
        };
        if !crate::media::AUDIO_RATES.contains(&ah.sample_rate) {
            self.stat(|s| s.audio_errors += 1);
            return self.emit(InputEvent::Error(format!(
                "audio: {} Hz is not a sample rate this plays",
                ah.sample_rate
            )));
        }
        self.last_audio = Some(ah.clone());
        if ah.fourcc == hx::FOURCC_AAC {
            match hx::CompressedPacket::parse(&f.payload) {
                // With an AAC decoder (§E.19.2): float audio like any other.
                Ok(p)
                    if self
                        .cfg
                        .codec
                        .as_ref()
                        .is_some_and(|c| c.decoder.decodes_aac()) =>
                {
                    match self.decode_aac(p.extra, p.data) {
                        Ok(channels) if channels.is_empty() || channels[0].is_empty() => {}
                        Ok(channels) => {
                            self.stat(|s| s.audio += 1);
                            let ts = ah.timestamp;
                            let ev = InputEvent::Audio(AudioIn {
                                header: AudioHeader {
                                    samples: channels[0].len() as u32,
                                    channels: channels.len() as u32,
                                    ..ah
                                },
                                channels,
                                via,
                                received,
                            });
                            self.present(ev, false, ts, None, 0, received);
                        }
                        Err(e) => {
                            self.aac = None;
                            self.stat(|s| s.audio_errors += 1);
                            self.emit(InputEvent::Error(format!("audio: AAC: {e}")));
                        }
                    }
                }
                Ok(p) => {
                    self.stat(|s| s.audio += 1);
                    let ev = CompressedAudio {
                        config: p.extra.to_vec(),
                        data: p.data.to_vec(),
                        header: ah,
                        received,
                    };
                    self.emit(InputEvent::CompressedAudio(ev));
                }
                Err(_) => {
                    self.stat(|s| s.audio_errors += 1);
                    self.emit(InputEvent::Error(format!(
                        "audio: bad AAC packet ({} bytes)",
                        f.payload.len()
                    )));
                }
            }
            return;
        }
        let decoded = if ah.fourcc == crate::media::FOURCC_IDF3 {
            crate::media::idf3_decode(&ah, &f.payload)
        } else {
            fowt_decode(&ah, &f.payload)
        };
        match decoded {
            Ok(channels) => {
                self.stat(|s| s.audio += 1);
                let ts = ah.timestamp;
                let ev = InputEvent::Audio(AudioIn {
                    header: ah,
                    channels,
                    via,
                    received,
                });
                self.present(ev, false, ts, None, 0, received);
            }
            Err(e) => {
                self.stat(|s| s.audio_errors += 1);
                self.emit(InputEvent::Error(format!(
                    "audio: {e} ({} bytes)",
                    f.payload.len()
                )));
            }
        }
    }

    /// Decodes an AAC frame with the input's decoder, opened for this AudioSpecificConfig.
    pub(super) fn decode_aac(&mut self, config: &[u8], frame: &[u8]) -> io::Result<Vec<Vec<f32>>> {
        if self.aac.as_ref().is_none_or(|(c, _)| c != config) {
            let factory = self.cfg.codec.as_ref().expect("checked").decoder.clone();
            self.aac = Some((config.to_vec(), factory.open_aac(config)?));
        }
        let (_, d) = self.aac.as_mut().expect("opened above");
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| d.decode(frame)))
            .unwrap_or_else(|_| Err(io::Error::other("decoder failed (internal error)")))
    }
}

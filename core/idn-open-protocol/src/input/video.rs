//! An input's pictures: SpeedHQ decoded (here or ahead on other threads), changed
//! regions, compressed video, colour, and handing pictures over.

use super::*;

impl Worker {
    pub(super) fn on_video(&mut self, f: Frame, via: &'static str, received: Instant) {
        self.stat(|s| s.video_bytes += f.payload.len() as u64);
        // Whatever the format (also HX), and before any skip (R1-40).
        self.last_via = via;
        let vh = match VideoHeader::parse(&f.ext) {
            Ok(h) => h,
            Err(e) => {
                self.stat(|s| s.video_errors += 1);
                return self.emit(InputEvent::Error(format!("video: bad header ({e})")));
            }
        };
        self.last_video = Some(vh.clone());
        if Codec::from_fourcc(vh.fourcc).is_some() || vh.fourcc == crate::regions::FOURCC {
            self.collect_ahead(true); // earlier pictures first
        }
        if let Some(codec) = Codec::from_fourcc(vh.fourcc) {
            // The size limit as for SpeedHQ (bug run 2026-11-24: compressed pictures had
            // none; the decoder's own limit is set too, as the stream's own size counts).
            let l = self.cfg.max_picture;
            if vh.width > l.max_width
                || vh.height > l.max_height
                || u64::from(vh.width) * u64::from(vh.height) > l.max_pixels
            {
                self.stat(|s| s.video_errors += 1);
                return self.emit(InputEvent::Error(format!(
                    "video {}: {}x{} is larger than this input takes",
                    String::from_utf8_lossy(&vh.fourcc),
                    vh.width,
                    vh.height
                )));
            }
            let pkt = match hx::CompressedPacket::parse(&f.payload) {
                Ok(p) => p,
                Err(e) => {
                    self.stat(|s| s.video_errors += 1);
                    return self.emit(InputEvent::Error(format!(
                        "video {}: bad compressed packet ({e})",
                        String::from_utf8_lossy(&vh.fourcc)
                    )));
                }
            };
            self.stat(|s| {
                s.video += 1;
                s.hx_keyframes += pkt.keyframe() as usize;
            });
            // Loss detection by the frame counter (§7.1.4): after a gap, or before the
            // first keyframe, discard non-keyframes and ask for a keyframe.
            let in_sequence = self
                .hx_last_counter
                .is_some_and(|c| pkt.counter == c.wrapping_add(1));
            if self.queue.hx_broken.swap(false, Ordering::Relaxed) {
                // Frames were dropped in the application queue.
                if !self.hx_need_key {
                    self.hx_wait = Some("frames dropped in the input queue");
                }
                self.hx_need_key = true;
            }
            if !pkt.keyframe() && (self.hx_need_key || !in_sequence) {
                if !self.hx_need_key {
                    self.log(format!(
                        "<- video: HX counter gap ({:?} -> {}); waiting for a keyframe",
                        self.hx_last_counter, pkt.counter
                    ));
                    self.hx_wait = None;
                }
                self.hx_need_key = true;
                self.stat(|s| s.hx_discarded += 1);
                // File transfers with this source's device make room (§E.12.5).
                crate::channel::video_trouble(
                    self.link.as_ref().map(|l| l.peer_fingerprint.as_str()),
                );
                self.ask_keyframe();
                self.hx_last_counter = Some(pkt.counter);
                return;
            }
            self.hx_need_key = false;
            self.hx_wait = None;
            self.hx_last_counter = Some(pkt.counter);
            // Keyframes always reach the decoder with the parameter sets in-band, also from
            // senders that put them only in the extra data (§7.1.1).
            let data = if codec == Codec::Av1 {
                std::borrow::Cow::Borrowed(pkt.data)
            } else {
                pkt.decoder_input(codec == Codec::Hevc)
            };
            if self
                .cfg
                .codec
                .as_ref()
                .is_some_and(|c| c.decoder.codecs().contains(&codec))
            {
                return self.decode_compressed(
                    codec,
                    &data,
                    pkt.keyframe(),
                    pkt.pts,
                    vh,
                    via,
                    received,
                );
            }
            let ev = CompressedVideo {
                codec,
                keyframe: pkt.keyframe(),
                data: data.into_owned(),
                extra: pkt.extra.to_vec(),
                pts: pkt.pts,
                dts: pkt.dts,
                header: vh,
                received,
            };
            return self.emit(InputEvent::Compressed(ev));
        }
        // Changed regions (§E.15.4): a picture lost on arrival leaves the reference stale.
        if self.tx.video_lost.swap(false, Ordering::Relaxed) {
            self.reference = None;
        }
        if vh.fourcc == crate::regions::FOURCC {
            return self.on_regions(f, vh, via, received);
        }
        // A newer picture is already waiting: skip this one. Fields are skipped in pairs:
        // a first field when its second and a newer picture are waiting, then that second.
        if std::mem::take(&mut self.skip_second_field) && vh.field_id == 2 {
            self.stat(|s| s.late += 1);
            return;
        }
        let waiting = self.tx.video_waiting.load(Ordering::Relaxed);
        if self.cfg.skip_late
            && ((vh.field_id == 0 && waiting > 0) || (vh.field_id == 1 && waiting > 1))
        {
            self.skip_second_field = vh.field_id == 1;
            self.stat(|s| s.late += 1);
            // Region updates that follow need this picture (§E.15.4).
            self.reference = None;
            return;
        }
        // Above the rate wanted: not decoded at all (SpeedHQ pictures stand alone). Not
        // with fields (they go in pairs) or changed regions (each is the next's reference).
        if vh.field_id == 0
            && !(self.cfg.regions && self.regions_peer)
            && let Some(t) = &mut self.thin
        {
            t.arrived(vh.timecode);
            if !t.keep() {
                self.stat(|s| s.thinned += 1);
                return;
            }
        }
        if self.decodes_ahead(&vh) {
            return self.decode_ahead(f, vh, via, received);
        }
        // Pictures decoding on the side are handed over first.
        self.collect_ahead(true);
        let t_dec = Instant::now();
        self.last_via = via;
        // A decoder bug on hostile or odd input costs this one frame, not the input: a
        // panic here used to end the worker thread for good.
        let (precise, limits, half) = (
            self.precise_now(),
            self.cfg.max_picture,
            self.half_size(&vh),
        );
        let coefs = self.as_coefficients(&vh);
        let (decoded, cpu) = crate::pool::metered(|| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if coefs {
                    // (Legacy payloads: decoded as usual.)
                    match shq::decode_coefficients(&vh.shq_params(), &f.payload, &limits) {
                        Err(shq::DecodeError::Unsupported) => {
                            shq::decode(&vh.shq_params(), &f.payload, &limits)
                        }
                        r => r.map(Picture::from_coefficients),
                    }
                } else if precise {
                    shq::decode_precise(&vh.shq_params(), &f.payload, &limits)
                } else if half {
                    shq::decode_half(&vh.shq_params(), &f.payload, &limits)
                } else {
                    shq::decode(&vh.shq_params(), &f.payload, &limits)
                }
            }))
        });
        let Ok(decoded) = decoded else {
            self.stat(|s| s.video_errors += 1);
            return self.emit(InputEvent::Error(format!(
                "video {} {}x{}: decoder failed (internal error), frame skipped",
                String::from_utf8_lossy(&vh.fourcc),
                vh.width,
                vh.height
            )));
        };
        match decoded {
            Ok(picture) => {
                self.dec_time += t_dec.elapsed().as_secs_f64();
                self.dec_n += 1;
                self.decode_cpu = cpu;
                // The reference for changed regions (§E.15.4): progressive pictures only.
                self.reference = (self.cfg.regions && self.regions_peer && vh.field_id == 0)
                    .then(|| picture.pooled_clone());
                self.stat(|s| s.video += 1);
                self.show(vh, picture, via, received);
            }
            Err(e) => {
                self.reference = None;
                self.stat(|s| s.video_errors += 1);
                self.emit(InputEvent::Error(format!(
                    "video {} {}x{} field {}: decode error {e} ({} bytes, B0 {:?})",
                    String::from_utf8_lossy(&vh.fourcc),
                    vh.width,
                    vh.height,
                    vh.field_id,
                    f.payload.len(),
                    f.payload.first()
                )));
            }
        }
    }

    /// A region update (`regions/1`, §E.15.4): the changed rectangles are decoded and
    /// pasted into the reference picture, which is handed over whole. Without a matching
    /// reference the update is dropped and a whole picture asked for.
    pub(super) fn on_regions(
        &mut self,
        f: Frame,
        mut vh: VideoHeader,
        via: &'static str,
        received: Instant,
    ) {
        let t_dec = Instant::now();
        let (w, h) = (vh.width as usize, vh.height as usize);
        let upd = match crate::regions::Update::parse(&f.payload, w, h) {
            Ok(u) => u,
            Err(e) => {
                self.reference = None;
                self.stat(|s| s.video_errors += 1);
                self.ask_refresh();
                return self.emit(InputEvent::Error(format!("video IDNR {w}x{h}: {e}")));
            }
        };
        let fits = self.reference.as_ref().is_some_and(|r| {
            (r.width, r.height) == (w, h)
                && crate::shq::Fourcc::from_bytes(upd.base) == Some(r.fourcc)
        });
        if !fits {
            self.reference = None;
            return self.ask_refresh();
        }
        vh.fourcc = upd.base;
        if let Some(v) = &mut self.last_video {
            v.fourcc = upd.base;
        }
        let (precise, limits) = (self.precise_now(), self.cfg.max_picture);
        let reference = self.reference.as_mut().expect("checked above");
        let applied = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for (r, payload) in &upd.regions {
                let p = crate::shq::VideoParams {
                    fourcc: upd.base,
                    width: r.w as u32,
                    height: r.h as u32,
                    field_id: 0,
                };
                let part = if precise {
                    crate::shq::decode_precise(&p, payload, &limits)
                } else {
                    crate::shq::decode(&p, payload, &limits)
                }
                .map_err(|e| e.to_string())?;
                if !crate::regions::paste(&mut reference.planes, w, h, *r, &part.planes) {
                    return Err(format!("region {}x{} doesn't fit its rectangle", r.w, r.h));
                }
                let pasted = match (&mut reference.precise, &part.precise) {
                    (Some(d), Some(s)) => crate::regions::paste16(d, w, h, *r, s),
                    _ => false,
                };
                if !pasted {
                    reference.precise = None;
                }
            }
            Ok(())
        }));
        let failed = match applied {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e),
            Err(_) => Some("decoder failed (internal error)".to_owned()),
        };
        if let Some(e) = failed {
            self.reference = None;
            self.stat(|s| s.video_errors += 1);
            self.ask_refresh();
            return self.emit(InputEvent::Error(format!(
                "video IDNR {w}x{h}: {e}, frame skipped"
            )));
        }
        // Applied even when late, so that the next update still fits.
        if self.cfg.skip_late && self.tx.video_waiting.load(Ordering::Relaxed) > 0 {
            self.stat(|s| s.late += 1);
            return;
        }
        let picture = self
            .reference
            .as_ref()
            .expect("checked above")
            .pooled_clone();
        self.dec_time += t_dec.elapsed().as_secs_f64();
        self.dec_n += 1;
        self.last_via = via;
        self.stat(|s| s.video += 1);
        self.show(vh, picture, via, received);
    }

    /// Decodes compressed video with the input's decoder (§E.18.4) and hands the picture
    /// over like a SpeedHQ one. A decoder error waits for, and asks for, a keyframe.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn decode_compressed(
        &mut self,
        codec: Codec,
        data: &[u8],
        keyframe: bool,
        pts: i64,
        vh: VideoHeader,
        via: &'static str,
        received: Instant,
    ) {
        let t_dec = Instant::now();
        if self.decoder.as_ref().is_none_or(|(c, _)| *c != codec) {
            let factory = self.cfg.codec.as_ref().expect("checked").decoder.clone();
            // Given a GPU: a decoder that may keep its pictures there (§E.27).
            #[cfg(feature = "gpu")]
            let opened = match &self.cfg.gpu {
                Some(g) => factory.open_gpu(codec, g),
                None => factory.open(codec),
            };
            #[cfg(not(feature = "gpu"))]
            let opened = factory.open(codec);
            match opened {
                Ok(d) => self.decoder = Some((codec, d)),
                Err(e) => {
                    self.stat(|s| s.video_errors += 1);
                    return self.emit(InputEvent::Error(format!(
                        "{} decoder failed to open: {e}",
                        codec.name()
                    )));
                }
            }
        }
        let (_, d) = self.decoder.as_mut().expect("opened above");
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            d.decode(data, keyframe, pts)
        }));
        // Kept until its picture comes out: a pipelined decoder hands pictures over
        // later than their access units (Decoded::pts).
        self.codec_meta.push_back((pts, vh, via, received));
        if self.codec_meta.len() > 32 {
            self.codec_meta.pop_front();
        }
        self.decoded(codec, r, Some(t_dec));
    }

    /// Pictures a pipelined decoder finished without new input.
    pub(super) fn poll_decoder(&mut self) {
        while let Some((codec, d)) = self.decoder.as_mut() {
            let codec = *codec;
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| d.poll()));
            if matches!(r, Ok(Ok(None))) {
                return;
            }
            self.decoded(codec, r, None);
        }
    }

    /// A decoder's answer: a picture, with the header of the access unit it came from,
    /// handed over like a SpeedHQ one; or an error, which waits for a keyframe.
    pub(super) fn decoded(
        &mut self,
        codec: Codec,
        r: std::thread::Result<io::Result<Option<crate::codec::Decoded>>>,
        t_dec: Option<Instant>,
    ) {
        let meta = match &r {
            Ok(Ok(Some(p))) => match p.pts {
                Some(pts) => {
                    while self.codec_meta.front().is_some_and(|m| m.0 != pts) {
                        self.codec_meta.pop_front();
                    }
                    self.codec_meta.pop_front()
                }
                None => self
                    .codec_meta
                    .pop_back()
                    .inspect(|_| self.codec_meta.clear()),
            },
            _ => self.codec_meta.back().cloned(),
        };
        let Some((_, vh, via, received)) = meta else {
            return; // a picture whose access unit is unknown (cannot happen)
        };
        let (w, h) = (vh.width as usize, vh.height as usize);
        // On the GPU (§E.27): the planes have their sizes, no samples.
        #[cfg(feature = "gpu")]
        let on_gpu = match &r {
            Ok(Ok(Some(_))) => self.decoder.as_mut().and_then(|(_, d)| d.take_gpu()),
            _ => None,
        };
        #[cfg(not(feature = "gpu"))]
        let on_gpu: Option<()> = None;
        let samples = |n: usize, len: usize| len == n || (on_gpu.is_some() && len == 0);
        let picture = match r {
            Ok(Ok(None)) => return,
            Ok(Ok(Some(p)))
                if p.planes.len() == 3
                    && samples(p.width * p.height, p.planes[0].data.len())
                    && p.planes[1..].iter().all(|c| {
                        // 4:2:0, or 4:2:2 (a GPU decoder's, chroma every row).
                        c.width == p.width.div_ceil(2)
                            && c.height == p.planes[1].height
                            && (c.height == p.height.div_ceil(2) || c.height == p.height)
                            && samples(c.width * c.height, c.data.len())
                    }) =>
            {
                let tags = (p.full_range, p.matrix);
                if self.dec_color_logged != Some(tags) {
                    self.log(format!(
                        "<- video: {} decoder reports range {}, matrix {}",
                        codec.name(),
                        match p.full_range {
                            Some(true) => "full",
                            Some(false) => "limited",
                            None => "unspecified",
                        },
                        p.matrix.unwrap_or("unspecified")
                    ));
                    self.dec_color_logged = Some(tags);
                }
                self.dec_color = Some(tags);
                Picture {
                    fourcc: if p.planes[1].height == p.height && p.height > 1 {
                        shq::Fourcc::Shq2
                    } else {
                        shq::Fourcc::Shq0
                    },
                    width: p.width,
                    height: p.height,
                    format: shq::Format::New,
                    quality: 0,
                    short_dc: false,
                    high_precision: false,
                    planes: p.planes,
                    precise: None,
                    coefficients: None,
                }
            }
            other => {
                let why = match other {
                    Ok(Err(e)) => e.to_string(),
                    Err(_) => "internal error".to_owned(),
                    _ => "a picture of the wrong shape".to_owned(),
                };
                self.decoder = None;
                self.codec_meta.clear();
                self.hx_need_key = true;
                self.hx_wait = None; // the error below says why
                self.stat(|s| s.video_errors += 1);
                self.ask_keyframe();
                return self.emit(InputEvent::Error(format!(
                    "{} {w}x{h}: decode failed ({why}); waiting for a keyframe",
                    codec.name()
                )));
            }
        };
        if let Some(t) = t_dec {
            self.dec_time += t.elapsed().as_secs_f64();
            self.dec_n += 1;
        }
        self.last_via = via;
        // On the GPU: handed over as it is, with no RGBA made from it.
        #[cfg(feature = "gpu")]
        if let Some(g) = on_gpu {
            self.next_gpu = Some(g);
            let dec_color = self.dec_color.take();
            let (color, conv) = self.picture_color(&vh, picture.height, dec_color);
            return self.deliver(vh, picture, None, color, conv, via, received);
        }
        self.show(vh, picture, via, received);
    }

    /// Asks the source for a whole picture (§E.15.4), at most every 250 ms.
    pub(super) fn ask_refresh(&mut self) {
        if self
            .refresh_asked
            .is_some_and(|t| t.elapsed() < Duration::from_millis(250))
        {
            return;
        }
        self.refresh_asked = Some(Instant::now());
        self.stat(|s| s.region_refreshes += 1);
        self.send_active(kind::CONTROL, crate::regions::REFRESH_REQUEST);
    }

    /// A buffer the application gave back ([`Input::recycle_rgba`]), else a new one.
    pub(super) fn spare_rgba(&self) -> Vec<u8> {
        self.queue.spare_rgba.lock_ok().pop().unwrap_or_default()
    }

    pub(super) fn rgba_failed(&mut self, vh: &VideoHeader) {
        self.stat(|s| s.video_errors += 1);
        self.emit(InputEvent::Error(format!(
            "video {} {}x{}: RGBA conversion failed (internal error), frame skipped",
            String::from_utf8_lossy(&vh.fourcc),
            vh.width,
            vh.height
        )));
    }

    /// Hands a decoded picture to the application: colour, RGBA if asked for, then on
    /// arrival or at its presentation time (§E.14).
    pub(super) fn show(
        &mut self,
        vh: VideoHeader,
        picture: Picture,
        via: &'static str,
        received: Instant,
    ) {
        let dec_color = self.dec_color.take();
        let (color, conv) = self.picture_color(&vh, picture.height, dec_color);
        // Guarded like the decode: a conversion bug skips the frame, not the input.
        let rgba = match self.cfg.rgba.map(|a| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut buf = self.spare_rgba();
                planes_to_rgba_into(
                    &picture.planes,
                    picture.width,
                    picture.height,
                    a,
                    &conv,
                    &mut buf,
                );
                buf
            }))
        }) {
            None => None,
            Some(Ok(rgba)) => Some(rgba),
            Some(Err(_)) => return self.rgba_failed(&vh),
        };
        self.deliver(vh, picture, rgba, color, conv, via, received);
    }

    /// A picture's colour description, and how to convert it to RGBA.
    pub(super) fn picture_color(
        &mut self,
        vh: &VideoHeader,
        height: usize,
        dec_color: Option<(Option<bool>, Option<&'static str>)>,
    ) -> (crate::ext::ColorInfo, ColorConfig) {
        let mut color = self.color.clone().unwrap_or_default();
        let mut named_matrix = color.apply_frame_metadata(vh.metadata.as_deref());
        // A compressed stream's own tags (VUI) stand in for x_idn_color when the sender
        // has none (§7.1.2); frame metadata naming a matrix still wins. Untagged: limited
        // range, BT.709 from 720 lines up and BT.601 below.
        let mut tagged = false;
        if self.color.is_none()
            && let Some((range, matrix)) = dec_color
        {
            if let Some(full) = range {
                color.range = if full { "full" } else { "limited" }.into();
                tagged = true;
            }
            if !named_matrix {
                color.matrix = match matrix {
                    Some(m) => m,
                    None if height < 720 => "bt_601",
                    None => "bt_709",
                }
                .into();
                named_matrix = true;
            }
        }
        let conv = conversion_config(
            &self.cfg.color,
            &color,
            self.color.is_some() || tagged,
            named_matrix.then_some(color.matrix.as_str()),
        );
        (color, conv)
    }

    /// Hands a picture over, on arrival or at its presentation time.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn deliver(
        &mut self,
        vh: VideoHeader,
        picture: Picture,
        rgba: Option<Vec<u8>>,
        mut color: crate::ext::ColorInfo,
        rgb: ColorConfig,
        via: &'static str,
        received: Instant,
    ) {
        // Without x_idn_color, the wrapper's high-precision bit is the only hint
        // (§3.3.1 bit 50): NDI sets it for 16-bit input.
        if self.color.is_none() && picture.high_precision {
            color.bits = 16;
        }
        self.hdr_seen = color.bits > 8 || color.transfer.starts_with("bt_2100");
        self.hdr_match = self
            .cfg
            .precise_hdr
            .is_some_and(|t| crate::color::Transfer::from_name(&color.transfer) == t);
        let (ts, rate, field) = (
            vh.timestamp,
            (vh.frame_rate_n, vh.frame_rate_d),
            vh.field_id,
        );
        // Never back in time when the way changes (within 2 s: a sender's clock stepped
        // back further is the sender's, and its pictures still come).
        if let Some((last, way)) = self.delivered
            && way != via
            && ts <= last
            && last - ts < 20_000_000
        {
            return;
        }
        self.delivered = Some((ts, via));
        self.session_media = true;
        let ev = InputEvent::Video(VideoIn {
            color,
            header: vh,
            picture,
            rgba,
            rgb,
            via,
            received,
            decode_cpu: std::mem::take(&mut self.decode_cpu),
            #[cfg(feature = "gpu")]
            gpu: self.next_gpu.take(),
        });
        self.present(ev, true, ts, Some(rate), field, received);
    }

    /// Pictures decoded with fraction bits: always when asked, or (`precise_hdr`) for a
    /// source in that HDR transfer.
    pub(super) fn precise_now(&self) -> bool {
        self.cfg.precise || self.hdr_match
    }

    /// Whether this picture goes as coefficients ([`InputConfig::coefficients`]): a
    /// progressive SpeedHQ 4:2:2 or 4:2:0 frame (NDI's above 1080p) at full size, with
    /// nothing asked of its samples here (fraction bits for a source above 8 bits, RGBA,
    /// regions' references).
    pub(super) fn as_coefficients(&self, vh: &VideoHeader) -> bool {
        self.cfg.coefficients
            && !(self.cfg.precise && self.hdr_seen)
            && !self.hdr_match
            && self.cfg.rgba.is_none()
            && !(self.cfg.regions && self.regions_peer)
            && vh.field_id == 0
            && matches!(&vh.fourcc, b"SHQ2" | b"SHQ7" | b"SHQ0")
            && !self.half_size(vh)
    }

    /// Whether to decode this picture at half size: [`InputConfig::max_size`] is half
    /// its size or less (the source didn't scale it, e.g. an NDI source).
    pub(super) fn half_size(&self, vh: &VideoHeader) -> bool {
        let Some((mw, mh)) = self
            .cfg
            .max_size
            .filter(|_| !(self.cfg.precise && self.hdr_seen) && !self.hdr_match)
        else {
            return false;
        };
        let (w, h) = (vh.width.max(1) as f64, vh.height.max(1) as f64);
        (mw as f64 / w).min(mh as f64 / h) <= 0.5
    }

    /// Whether this SpeedHQ picture is decoded on the side: whole frames (fields go in
    /// pairs), and not when changed regions need each picture as their reference.
    pub(super) fn decodes_ahead(&self, vh: &VideoHeader) -> bool {
        self.cfg.parallel_decode != Some(false)
            && vh.field_id == 0
            && !(self.cfg.regions && self.regions_peer)
    }

    /// Starts decoding a picture on the side; with both threads busy, it waits as the
    /// next one (replacing an older waiting one, which is late, unless every frame is to
    /// be decoded: then this waits for a thread).
    pub(super) fn decode_ahead(
        &mut self,
        f: Frame,
        vh: VideoHeader,
        via: &'static str,
        received: Instant,
    ) {
        self.reference = None;
        if self.ahead.in_flight.len() >= AHEAD {
            if self.cfg.skip_late {
                if self.ahead.next.replace((f, vh, via, received)).is_some() {
                    self.stat(|s| s.late += 1);
                }
                return;
            }
            while self.ahead.in_flight.len() >= AHEAD {
                self.collect_one_ahead();
            }
        }
        self.dispatch_ahead(f, vh, via, received);
    }

    pub(super) fn dispatch_ahead(
        &mut self,
        f: Frame,
        vh: VideoHeader,
        via: &'static str,
        received: Instant,
    ) {
        let (color, conv) = self.picture_color(&vh, vh.height as usize, None);
        let seq = self.ahead.seq;
        self.ahead.seq += 1;
        let (precise, limits, alpha) = (self.precise_now(), self.cfg.max_picture, self.cfg.rgba);
        let half = self.half_size(&vh);
        let coefs = self.as_coefficients(&vh);
        let params = vh.shq_params();
        let mut buf = alpha.map(|_| self.spare_rgba());
        let (results, wake) = (self.ahead.results.0.clone(), self.tx.tx.clone());
        let (bytes, b0) = (f.payload.len(), f.payload.first().copied());
        let payload = f.payload;
        self.ahead.run(Box::new(move || {
            let t = Instant::now();
            let (picture, cpu) = crate::pool::metered(|| {
                std::panic::catch_unwind(|| {
                    if coefs {
                        match shq::decode_coefficients(&params, &payload, &limits) {
                            Err(shq::DecodeError::Unsupported) => {
                                shq::decode(&params, &payload, &limits)
                            }
                            r => r.map(Picture::from_coefficients),
                        }
                    } else if precise {
                        shq::decode_precise(&params, &payload, &limits)
                    } else if half {
                        shq::decode_half(&params, &payload, &limits)
                    } else {
                        shq::decode(&params, &payload, &limits)
                    }
                })
                .map_err(|_| ())
            });
            let secs = t.elapsed().as_secs_f64(); // decoding alone, as for other pictures
            let rgba = match (&picture, alpha, buf.take()) {
                (Ok(Ok(p)), Some(a), Some(mut buf)) => Some(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        planes_to_rgba_into(&p.planes, p.width, p.height, a, &conv, &mut buf);
                        buf
                    }))
                    .map_err(|_| ()),
                ),
                _ => None,
            };
            let _ = results.send((
                seq,
                AheadDone {
                    picture,
                    rgba,
                    secs,
                    cpu,
                },
            ));
            let _ = wake.send(Incoming::Decoded);
        }));
        self.ahead.in_flight.push_back((
            seq,
            AheadPicture {
                vh,
                color,
                rgb: conv,
                via,
                received,
                bytes,
                b0,
            },
        ));
    }

    /// Hands over the pictures decoded on the side, in order, and starts the waiting
    /// one. With `all`, first waits for every picture still decoding.
    pub(super) fn collect_ahead(&mut self, all: bool) {
        while let Ok((seq, d)) = self.ahead.results.1.try_recv() {
            self.ahead.done.insert(seq, d);
        }
        self.hand_over_ahead();
        while all && !self.ahead.in_flight.is_empty() {
            self.collect_one_ahead();
        }
    }

    /// Waits for the oldest picture decoding on the side and hands it over.
    pub(super) fn collect_one_ahead(&mut self) {
        let Some(&(oldest, _)) = self.ahead.in_flight.front() else {
            return;
        };
        while !self.ahead.done.contains_key(&oldest) {
            match self.ahead.results.1.recv() {
                Ok((seq, d)) => {
                    self.ahead.done.insert(seq, d);
                }
                Err(_) => return, // cannot happen: `results.0` lives here
            }
        }
        self.hand_over_ahead();
    }

    pub(super) fn hand_over_ahead(&mut self) {
        while let Some(&(seq, _)) = self.ahead.in_flight.front() {
            let Some(d) = self.ahead.done.remove(&seq) else {
                break;
            };
            let (_, p) = self.ahead.in_flight.pop_front().expect("checked");
            self.finish_ahead(p, d);
        }
        if self.ahead.in_flight.len() < AHEAD
            && let Some((f, vh, via, received)) = self.ahead.next.take()
        {
            self.dispatch_ahead(f, vh, via, received);
        }
    }

    pub(super) fn finish_ahead(&mut self, p: AheadPicture, d: AheadDone) {
        let vh = p.vh;
        match d.picture {
            Err(()) => {
                self.stat(|s| s.video_errors += 1);
                self.emit(InputEvent::Error(format!(
                    "video {} {}x{}: decoder failed (internal error), frame skipped",
                    String::from_utf8_lossy(&vh.fourcc),
                    vh.width,
                    vh.height
                )));
            }
            Ok(Err(e)) => {
                self.stat(|s| s.video_errors += 1);
                self.emit(InputEvent::Error(format!(
                    "video {} {}x{} field {}: decode error {e} ({} bytes, B0 {:?})",
                    String::from_utf8_lossy(&vh.fourcc),
                    vh.width,
                    vh.height,
                    vh.field_id,
                    p.bytes,
                    p.b0
                )));
            }
            Ok(Ok(picture)) => {
                self.dec_time += d.secs;
                self.dec_n += 1;
                self.decode_cpu = d.cpu;
                self.stat(|s| s.video += 1);
                match d.rgba {
                    Some(Err(())) => self.rgba_failed(&vh),
                    rgba => self.deliver(
                        vh,
                        picture,
                        rgba.and_then(Result::ok),
                        p.color,
                        p.rgb,
                        p.via,
                        p.received,
                    ),
                }
            }
        }
    }
}

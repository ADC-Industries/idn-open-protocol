//! A group: one encode (video and audio) and the destinations it feeds.

use super::*;

impl Group {
    /// Sound is to be waited for at a file's start: the source has some, or it is too soon
    /// to tell (it comes a little after the pictures). A source without sound starts on
    /// its pictures at once, not three keyframes later.
    pub(super) fn expects_sound(&self) -> bool {
        self.heard_sound.load(Ordering::Relaxed) || self.started.elapsed() < Duration::from_secs(2)
    }
}

impl Group {
    pub(super) fn push(&self, f: TappedFrame) {
        let mut q = self.input.lock_ok();
        if !f.planes.is_empty()
            && q.iter().filter(|x| !x.planes.is_empty()).count() >= MAX_WAITING_PICTURES
        {
            // Keep the oldest picture's audio, drop its picture.
            if let Some(old) = q.iter_mut().find(|x| !x.planes.is_empty()) {
                old.planes = Arc::default();
                old.width = 0;
                self.skipped.fetch_add(1, Ordering::Relaxed);
                *self.last_skip.lock_ok() = Some(Instant::now());
            }
        }
        if q.len() >= MAX_WAITING {
            q.pop_front();
        }
        q.push_back(f);
        self.ready.notify_one();
    }

    pub(super) fn distribute(&self, p: Pkt) {
        let p = Arc::new(p);
        let mut overflowed = false;
        for d in self.dests.lock_ok().iter() {
            overflowed |= d.offer(p.clone());
        }
        if overflowed {
            self.keyframe.store(true, Ordering::Relaxed);
        }
    }

    /// Encodes until the gateway goes or no destination is left.
    pub(super) fn encode_loop(self: Arc<Self>, ff: Arc<Ffmpeg>) {
        // However this thread ends, a panic included, the group has ended: a destination
        // added later starts a new one (lab 348-A: later recordings joined a group whose
        // thread had died, and were empty).
        struct Ended<'a>(&'a AtomicBool);
        impl Drop for Ended<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let _ended = Ended(&self.stop);
        let mut venc: Option<OpenEncoder> = None;
        // The encoder open is 10-bit (an HDR output's 16-bit pictures, R.24).
        let mut venc_hdr = false;
        // No 10-bit encoder here (or it failed): the HDR output recorded or streamed in
        // 8 bits, SDR (its 8-bit program is SDR), rather than not at all.
        let mut hdr_failed = false;
        // 10-bit encode errors in a row: the encoder reopened each time, 8 bits at three.
        let mut hdr_errors = 0u32;
        // After a change of bit rate: the new encoder's parameter sets, to go in front of
        // its first keyframe (the destinations keep their connection and header).
        let mut headers: Option<Vec<u8>> = None;
        let mut aenc: Option<AacEncoder> = None;
        // The second and later tracks' encoders, fed in step with the first.
        let mut extra: Vec<AacEncoder> = Vec::new();
        let tracks = self.settings.audio_tracks.clamp(1, 8) as usize;
        let mut scaler: Option<Scaler> = None;
        let mut scaled: Option<Frame> = None;
        // (And for an HDR output's 16-bit pictures.)
        let mut scaler16: Option<Scaler> = None;
        let mut scaled16: Option<Frame> = None;
        let mut t0: Option<i64> = None;
        let (mut last_vpts, mut last_audio) = (-1i64, None::<Instant>);
        // Sound to be lined up exactly with its timestamp: the first, and after silence.
        let mut realign = true;
        // The sound against its timestamps over the last second: since when, the sum of
        // the offsets (samples) and their count.
        let mut adrift: (Instant, i64, i64) = (Instant::now(), 0, 0);
        // Streams go out at 48 kHz whatever the program's rate (§N.40):
        // sound at another rate converted for them (a sinc filter: not the nearest-sample
        // conversion a preview uses). A gateway only recording keeps its rate.
        let mut to_48k: Option<(u32, usize, idn_open_protocol::audio::resample::Rate)> = None;
        // The rate the sound last came at (K7-2).
        let mut sound_rate: Option<u32> = None;
        // A small lasting step being eased out (see below): +1 lengthening the blocks, −1
        // shortening them, 0 not.
        let mut easing: i64 = 0;
        let mut version = 0;
        let mut pending_pts: VecDeque<i64> = VecDeque::new();
        let mut idle_since: Option<Instant> = None;
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return;
            }
            // Ends a while after the last destination was removed (decided under the
            // `dests` lock, where destinations join: none can join a group that ended).
            // Every destination ended (a recording failed, its disk full) but not removed:
            // nothing encoded for it (the encoder ran on for nobody, deep bug run
            // 2026-11-09); one added later starts on a keyframe.
            let live;
            {
                let ds = self.dests.lock_ok();
                live = ds.iter().any(|d| !d.ended());
                if ds.is_empty() {
                    let t = *idle_since.get_or_insert_with(Instant::now);
                    if t.elapsed() > Duration::from_secs(5) {
                        self.stop.store(true, Ordering::Relaxed);
                        return;
                    }
                } else {
                    idle_since = None;
                }
            }
            let f = {
                let mut q = self.input.lock_ok();
                if q.is_empty() {
                    q = self
                        .ready
                        .wait_timeout(q, Duration::from_millis(200))
                        .unwrap()
                        .0;
                }
                q.pop_front()
            };
            let Some(mut f) = f else { continue };
            // The sound's own time against the frame's, 100 ns (K7-3).
            let mut audio_shift: i64 = 0;
            // A group with streams sends 48 kHz (with recordings only while the sound is
            // at 48 kHz, K7-1).
            if self.streams.load(Ordering::Relaxed) && f.sample_rate != 48_000 && f.sample_rate > 0
            {
                if !f.audio.is_empty() {
                    let ch = f.audio.len();
                    if to_48k
                        .as_ref()
                        .is_none_or(|(r, c, _)| *r != f.sample_rate || *c != ch)
                    {
                        to_48k = idn_open_protocol::audio::resample::Rate::new(
                            f.sample_rate,
                            48_000,
                            ch,
                            480,
                            0.0,
                        )
                        .ok()
                        .map(|r| (f.sample_rate, ch, r));
                    }
                    if let Some((_, _, conv)) = to_48k.as_mut() {
                        let len = f.audio.first().map_or(0, Vec::len) as i64;
                        conv.push(&f.audio);
                        let mut out = vec![Vec::new(); ch];
                        let _ = conv.pull(|o, n| {
                            for (d, s) in out.iter_mut().zip(o) {
                                d.extend_from_slice(&s[..n]);
                            }
                        });
                        // The block out starts where its sound was taken: before the
                        // input's end by what the converter holds and its filter's delay
                        // (bug run K7-3: stamped as the input, 0–480 frames read as drift,
                        // eased, and the sound settled ~5 ms late).
                        let n = out.first().map_or(0, Vec::len) as i64;
                        let r_in = i64::from(f.sample_rate);
                        audio_shift = (len - conv.pending() as i64) * 10_000_000 / r_in
                            - (conv.output_delay() as i64 + n) * 10_000_000 / 48_000;
                        f.audio = out;
                    }
                }
                f.sample_rate = 48_000;
            }
            if !live {
                self.keyframe.store(true, Ordering::Relaxed);
                continue;
            }
            let t0 = *t0.get_or_insert(f.timestamp);
            // A second's grace before silence stands in: the sound may come apart from
            // the pictures, and a little after them.
            let heard = *last_audio.get_or_insert_with(Instant::now);
            // The rate the sound comes at: a picture's own (48 kHz when it has no sound)
            // says nothing, so pictures and silence keep the sound's (bug run K7-2: the
            // first picture opened AAC at 48 kHz and the first sound reopened both
            // encoders; a silence split a 96 kHz recording at 48 and back).
            if !f.audio.is_empty() && f.sample_rate >= 8000 {
                sound_rate = Some(f.sample_rate);
            }
            let rate = sound_rate.unwrap_or_else(|| {
                if self.streams.load(Ordering::Relaxed) {
                    48_000
                } else {
                    // Not the machine's: an input's recording comes at the input's rate,
                    // mostly 48 kHz (bug run L4-1: opened at the machine's 96, both
                    // encoders reopened at the first sound); a program's gateway is told.
                    match self.rate.load(Ordering::Relaxed) {
                        0 => 48_000,
                        hz => hz,
                    }
                }
            });
            // Pictures and no sound yet: the AAC encoder opens now (at the sound's rate as
            // far as it is known), so the pictures can start the file; waiting for the
            // sound or the silence a second later, a source without sound lost its first
            // second. Sound at another rate opens it again.
            if aenc.is_none() && !f.planes.is_empty() {
                let more: io::Result<Vec<AacEncoder>> = (1..tracks)
                    .map(|_| AacEncoder::open(rate, self.settings.audio_kbps, true))
                    .collect();
                if let Ok((a, more)) = AacEncoder::open(rate, self.settings.audio_kbps, true)
                    .and_then(|a| Ok((a, more?)))
                {
                    aenc = Some(a);
                    extra = more;
                }
            }
            // Audio: AAC, lined up with the video by timestamp.
            if !f.audio.is_empty()
                || (!f.planes.is_empty() && heard.elapsed() > Duration::from_secs(1))
            {
                if aenc.as_ref().is_none_or(|a| a.rate != rate) {
                    let more: io::Result<Vec<AacEncoder>> = (1..tracks)
                        .map(|_| AacEncoder::open(rate, self.settings.audio_kbps, true))
                        .collect();
                    match AacEncoder::open(rate, self.settings.audio_kbps, true)
                        .and_then(|a| Ok((a, more?)))
                    {
                        Ok((a, more)) => {
                            aenc = Some(a);
                            extra = more;
                            venc = None; // new parameters: reopen both, then the muxers
                        }
                        Err(e) => {
                            log(&format!("gateway: AAC encoder failed: {e}"));
                            continue;
                        }
                    }
                }
                let a = aenc.as_mut().unwrap();
                let pts = (f.timestamp + audio_shift - t0).max(0) * rate as i64 / 10_000_000;
                // No sound from the output for a second: silence, so platforms get audio.
                let silence;
                let mut eased: Vec<Vec<f32>> = Vec::new();
                // Each later track: aligned or kept in step exactly as the first.
                let mut realigned = false;
                let channels: Vec<&[f32]> = if f.audio.is_empty() {
                    a.sync_to(pts, rate as i64 / 10);
                    for e in &mut extra {
                        e.sync_to(pts, rate as i64 / 10);
                    }
                    let fps = f.frame_rate.0 as f64 / f.frame_rate.1.max(1) as f64;
                    silence = vec![0f32; (rate as f64 / fps.max(1.0)) as usize];
                    // The 48 kHz conversion started again with the sound (bug run M7-5: what
                    // it held came out ahead of the sound's new start).
                    to_48k = None;
                    realign = true;
                    vec![&silence, &silence]
                } else {
                    last_audio = Some(Instant::now());
                    self.heard_sound.store(true, Ordering::Relaxed);
                    // The first sound (and after silence) exactly where its timestamp
                    // puts it; then the sample count is the clock, the timestamps only
                    // catching a restart or a gap (over 100 ms adrift), never jitter,
                    // and a step that lasts (over 2 ms on average for a second: a
                    // source that started over, its sound moved against its pictures;
                    // at 5 ms a restart's step of 3-4 ms stayed in the recording).
                    adrift.1 += a.drift(pts);
                    adrift.2 += 1;
                    if adrift.0.elapsed() >= Duration::from_secs(1) {
                        // A step up to 50 ms (the program's sound buffer settling, a few
                        // ms to a block: lined up at once it was a dip of that length in
                        // the recording) is eased out instead; more, lined up.
                        // By the second's mean, not each block's (which jitters: eased
                        // by it, the pitch swung either way); eased until the
                        // mean is within 0.5 ms.
                        let mean = adrift.1 / adrift.2;
                        if mean.abs() > rate as i64 / 500 {
                            if mean.abs() <= rate as i64 / 20 {
                                easing = -mean.signum();
                            } else {
                                realign = true;
                            }
                        } else if mean.abs() <= rate as i64 / 2000 {
                            easing = 0;
                        }
                        adrift = (Instant::now(), 0, 0);
                    }
                    realigned = std::mem::take(&mut realign);
                    let skip = if realigned {
                        adrift = (Instant::now(), 0, 0);
                        easing = 0;
                        a.align(pts)
                    } else {
                        a.sync_to(pts, rate as i64 / 10);
                        0
                    };
                    // Easing: each block a thousandth longer or shorter (1.7 cents; at 1%,
                    // 17 cents was heard on held notes): 1 ms a second.
                    if easing != 0
                        && !realigned
                        && let Some(n) = f.audio.first().map(Vec::len).filter(|&n| n >= 100)
                    {
                        let m = (n as i64 + easing * (n as i64 / 1000).max(1)) as usize;
                        eased = f.audio.iter().map(|ch| stretch(ch, m)).collect();
                    }
                    for e in &mut extra {
                        if realigned {
                            e.align(pts);
                        } else {
                            e.sync_to(pts, rate as i64 / 10);
                        }
                    }
                    if eased.is_empty() {
                        f.audio.iter().map(|c| &c[skip.min(c.len())..]).collect()
                    } else {
                        eased.iter().map(Vec::as_slice).collect()
                    }
                };
                // The later tracks' channels: 3–4, 5–6, … (one alone: mono; none: silence).
                let n = channels.first().map_or(0, |c| c.len());
                let quiet = vec![0f32; n];
                for (k, e) in extra.iter_mut().enumerate() {
                    let (l, r) = (channels.get(2 * k + 2), channels.get(2 * k + 3));
                    let pair: [&[f32]; 2] = match (l, r) {
                        (Some(l), Some(r)) => [l, r],
                        (Some(l), None) => [l, l],
                        _ => [&quiet, &quiet],
                    };
                    match e.push(&pair) {
                        Ok(pkts) => {
                            for (data, pts) in pkts {
                                self.distribute(Pkt {
                                    made: Instant::now(),
                                    video: false,
                                    track: k + 1,
                                    data,
                                    pts,
                                    key: true,
                                });
                            }
                        }
                        Err(e) => log(&format!("gateway: AAC track {}: {e}", k + 2)),
                    }
                }
                let _ = realigned;
                match a.push(&channels) {
                    Ok(pkts) => {
                        for (data, pts) in pkts {
                            self.distribute(Pkt {
                                made: Instant::now(),
                                video: false,
                                track: 0,
                                data,
                                pts,
                                key: true,
                            });
                        }
                    }
                    Err(e) => log(&format!("gateway: AAC: {e}")),
                }
            }
            if f.planes.is_empty() {
                continue;
            }
            // Sizes without samples and no picture on the GPU to draw from: nothing to
            // encode (lab 348-A: such pictures reached the encoder after a recording
            // stopped, and it panicked).
            #[cfg(feature = "gpu")]
            let on_gpu = f.gpu.is_some();
            #[cfg(not(feature = "gpu"))]
            let on_gpu = false;
            if !on_gpu && f.planes[0].data.is_empty() {
                continue;
            }
            // Video: the GPU, at the wanted size.
            let (w, h) = fit(f.width, f.height, self.settings.size);
            let wanted = self.bitrate.load(Ordering::Relaxed);
            // Only the bit rate changed (an adaptive stream): a new encoder, the same stream.
            let rate_only = venc
                .as_ref()
                .is_some_and(|v| v.1 == (w, h) && v.2 != wanted);
            if venc.as_ref().is_none_or(|v| v.1 != (w, h)) || rate_only {
                let fps = f.frame_rate.0 as f64 / f.frame_rate.1.max(1) as f64;
                let cfg = EncoderConfig {
                    codec: self.settings.codec,
                    width: w,
                    height: h,
                    frame_rate: f.frame_rate,
                    bitrate: wanted,
                    keyframe_interval: (fps * self.settings.keyframe_interval.as_secs_f64())
                        .round()
                        .max(1.0) as u32,
                };
                // Pictures on the application's GPU: an encoder on that device where
                // there is one (Windows: Direct3D 12), else the usual one (§E.27).
                #[cfg(feature = "gpu")]
                let on_gpu = f
                    .gpu
                    .as_ref()
                    .and_then(|_| self.gpu.lock_ok().as_ref().and_then(|g| g()))
                    .and_then(|hand| crate::open_encoder_for_files_on_gpu(&cfg, &hand));
                #[cfg(not(feature = "gpu"))]
                let on_gpu = None;
                // An HDR output's pictures (16-bit beside the 8-bit): 10 bits, BT.2100.
                let hdr = (*self.hdr.lock_ok())
                    .filter(|_| f.precise.is_some() && !self.sdr_only && !hdr_failed);
                let opened = match (on_gpu, hdr) {
                    (_, Some(t)) => crate::open_encoder_for_files_hdr(&ff, &cfg, t),
                    (Some(e), None) => Ok(e),
                    (None, None) => crate::open_encoder_for_files(&ff, &cfg),
                };
                let opened_hdr = hdr.is_some();
                match opened {
                    Ok(e) if rate_only => {
                        // Not every GPU encoder takes a new rate as it runs; a new one
                        // does, on all. Its parameter sets may differ (the rate is in
                        // them): they go in band, in front of its first keyframe.
                        let old = self.params.lock_ok().as_ref().map(|s| s.video.extradata());
                        let new = e.parameters().map(|p| p.extradata()).unwrap_or_default();
                        headers =
                            (old.as_deref() != Some(&new[..]) && annex_b(&new)).then_some(new);
                        venc = Some((e.encoder, (w, h), wanted));
                        venc_hdr = opened_hdr;
                        self.keyframe.store(true, Ordering::Relaxed);
                        pending_pts.clear();
                    }
                    Err(e) if rate_only => {
                        log(&format!("gateway: a new bit rate: {e}"));
                        let v = venc.as_mut().unwrap();
                        v.2 = wanted; // not tried again until the rate changes again
                    }
                    Ok(e) => {
                        let (Some(a), Ok(vp)) = (aenc.as_ref(), e.parameters()) else {
                            continue;
                        };
                        let Ok(ap) = a.parameters() else { continue };
                        let Ok(extra_audio) = extra
                            .iter()
                            .map(|e| e.parameters())
                            .collect::<io::Result<Vec<_>>>()
                        else {
                            continue;
                        };
                        version += 1;
                        *self.params.lock_ok() = Some(Arc::new(Streams {
                            video: vp,
                            audio: ap,
                            extra_audio,
                            audio_rate: a.rate,
                            version,
                        }));
                        venc = Some((e.encoder, (w, h), wanted));
                        venc_hdr = opened_hdr;
                        headers = None;
                        self.keyframe.store(true, Ordering::Relaxed);
                        pending_pts.clear();
                    }
                    Err(e) if opened_hdr => {
                        log(&format!(
                            "gateway: no 10-bit encoder for the HDR program ({e}): {} in 8 bits, SDR",
                            self.settings.codec.name()
                        ));
                        hdr_failed = true;
                        continue;
                    }
                    Err(e) => {
                        log(&format!(
                            "gateway: {} encoder failed: {e}",
                            self.settings.codec.name()
                        ));
                        std::thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                }
            }
            let mut pts = (f.timestamp - t0).max(0) * 9 / 1000;
            if pts <= last_vpts {
                pts = last_vpts + 1;
            }
            last_vpts = pts;
            pending_pts.push_back(pts);
            let force = self.keyframe.swap(false, Ordering::Relaxed);
            let (e, _, _) = venc.as_mut().unwrap();
            // Closed captions with the picture, into the video (switcher §S.29).
            e.captions_next(&f.captions);
            // 10 bits (R.24): the picture's 16-bit planes, scaled to the encoder's size
            // when the output has a size of its own.
            if venc_hdr {
                let r = match &f.precise {
                    Some(p) if (f.width, f.height) == (w, h) => e.encode16(p, pts, force),
                    Some(p) => {
                        match crate::encode::scale_p216(p, (w, h), &mut scaler16, &mut scaled16) {
                            Ok(sp) => e.encode16(&sp, pts, force),
                            Err(err) => {
                                log(&format!("gateway: scaling a 16-bit picture: {err}"));
                                pending_pts.pop_back();
                                if force {
                                    self.keyframe.store(true, Ordering::Relaxed);
                                }
                                continue;
                            }
                        }
                    }
                    None => {
                        pending_pts.pop_back();
                        if force {
                            self.keyframe.store(true, Ordering::Relaxed);
                        }
                        continue;
                    }
                };
                match r {
                    Ok(out) => {
                        // (Only packets out show the encoder works: a fresh one holds the
                        // first pictures, and failing after them it reopened for ever.)
                        if !out.is_empty() {
                            hdr_errors = 0;
                        }
                        for p in out {
                            let pts = pending_pts.pop_front().unwrap_or(last_vpts);
                            let data = match headers.take_if(|_| p.keyframe) {
                                Some(mut hd) => {
                                    hd.extend_from_slice(&p.data);
                                    hd
                                }
                                None => p.data,
                            };
                            self.distribute(Pkt {
                                made: Instant::now(),
                                video: true,
                                track: 0,
                                data,
                                pts,
                                key: p.keyframe,
                            });
                        }
                    }
                    Err(err) => {
                        hdr_errors += 1;
                        if hdr_errors >= 3 {
                            log(&format!(
                                "gateway: video encode (10-bit): {err}; 8 bits, SDR, from now"
                            ));
                            hdr_failed = true;
                        } else {
                            log(&format!(
                                "gateway: video encode (10-bit): {err}; the encoder opened again"
                            ));
                        }
                        venc = None;
                    }
                }
                continue;
            }
            // On the application's GPU (§E.27): drawn into the encoder there, scaled on the
            // way; else its samples (read back once when it came without them).
            #[cfg(feature = "gpu")]
            let hand = f
                .gpu
                .as_ref()
                .and_then(|_| self.gpu.lock_ok().as_ref().and_then(|g| g()));
            #[cfg(feature = "gpu")]
            let on_gpu = match (&f.gpu, &hand) {
                (Some(pic), Some(hand)) => e.encode_gpu(pic, hand, pts, force),
                _ => None,
            };
            #[cfg(not(feature = "gpu"))]
            let on_gpu: Option<io::Result<Vec<idn_open_protocol::codec::Encoded>>> = None;
            let result = match on_gpu {
                Some(r) => r,
                None => {
                    #[cfg(feature = "gpu")]
                    let f = match (&f.gpu, &hand) {
                        (Some(pic), Some(hand))
                            if f.planes.first().is_some_and(|p| p.data.is_empty()) =>
                        {
                            match read_back(&f, pic, hand) {
                                Ok(r) => r,
                                Err(e) => {
                                    log(&format!("gateway: reading a picture back: {e}"));
                                    continue;
                                }
                            }
                        }
                        // On the GPU only, and the output gone with its GPU: nothing to
                        // read it back with.
                        (Some(_), None) if f.planes.first().is_some_and(|p| p.data.is_empty()) => {
                            pending_pts.pop_back();
                            if force {
                                self.keyframe.store(true, Ordering::Relaxed);
                            }
                            continue;
                        }
                        _ => f,
                    };
                    // An HDR output in 8 bits (an RTMP H.264 stream, no 10-bit encoder):
                    // its 8-bit planes are PQ/HLG-coded, so tone-mapped to SDR BT.709
                    // first, as SDR screens show it.
                    let hdr_now = (*self.hdr.lock_ok()).filter(|_| f.precise.is_some());
                    let f = match hdr_now {
                        Some(t) => {
                            let mut g = f.clone();
                            g.planes = Arc::new(hdr_to_sdr(&f, t));
                            g.precise = None;
                            std::borrow::Cow::Owned(g)
                        }
                        None => std::borrow::Cow::Borrowed(&f),
                    };
                    let planes = if (w, h) == (f.width, f.height) {
                        f.planes.clone()
                    } else {
                        match scale(&f, w, h, &mut scaler, &mut scaled) {
                            Ok(p) => Arc::new(p),
                            Err(e) => {
                                log(&format!("gateway: scaling: {e}"));
                                // As the 16-bit path: this picture's pts out, and a
                                // keyframe asked for still asked for.
                                pending_pts.pop_back();
                                if force {
                                    self.keyframe.store(true, Ordering::Relaxed);
                                }
                                continue;
                            }
                        }
                    };
                    e.encode(&planes, f.layout, pts, force)
                }
            };
            match result {
                Ok(out) => {
                    for p in out {
                        let pts = pending_pts.pop_front().unwrap_or(last_vpts);
                        let data = match headers.take_if(|_| p.keyframe) {
                            Some(mut h) => {
                                h.extend_from_slice(&p.data);
                                h
                            }
                            None => p.data,
                        };
                        self.distribute(Pkt {
                            made: Instant::now(),
                            video: true,
                            track: 0,
                            data,
                            pts,
                            key: p.keyframe,
                        });
                    }
                }
                Err(err) => {
                    log(&format!("gateway: video encode: {err}"));
                    venc = None;
                }
            }
        }
    }
}

/// An HDR picture's 8-bit planes (PQ or HLG, BT.2020) as SDR BT.709 planes: tone-mapped
/// as the library does for SDR screens (BT.2408's 203 nits as white).
fn hdr_to_sdr(
    f: &TappedFrame,
    t: idn_open_protocol::color::Transfer,
) -> Vec<idn_open_protocol::shq::Plane> {
    use idn_open_protocol::color::{self, AlphaMode, ColorConfig, Matrix, Primaries};
    let hdr = ColorConfig {
        matrix: Matrix::Bt2020,
        transfer: t,
        primaries: Primaries::Bt2020,
        sdr_white_nits: 203.0,
        ..ColorConfig::default()
    };
    let rgba = color::planes_to_rgba(&f.planes, f.width, f.height, AlphaMode::Opaque, &hdr);
    color::rgba_to_planes(
        &rgba,
        f.width,
        f.height,
        f.width * 4,
        f.layout,
        AlphaMode::Opaque,
        &ColorConfig::default(),
    )
}

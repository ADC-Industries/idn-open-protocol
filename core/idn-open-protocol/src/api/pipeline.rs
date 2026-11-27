//! An output's own thread: each frame sent is checked, encoded and handed to the sender,
//! and its state reported to the Node.

use super::*;

pub(super) fn worker(shared: Arc<Shared>, sender: Arc<Sender>, cfg: OutputConfig) {
    let _stop_on_panic = StopOnPanic(&shared);
    let mut st = WorkerState::default();
    loop {
        let item = {
            let mut slot = shared.slot.lock_ok();
            loop {
                if shared.stop.load(Ordering::Relaxed) {
                    return;
                }
                if let Some(f) = slot.take() {
                    break f;
                }
                slot = shared
                    .ready
                    .wait(slot)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        };
        // A frame whose conversion panics (a bug) is dropped and logged; the output keeps
        // running: drop frames, never stall.
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            process(&shared, &sender, &cfg, &mut st, item)
        }));
        if r.is_err() {
            st.last = None;
            crate::log::log(&format!(
                "output {}: dropped a frame: conversion failed (internal error)",
                cfg.name
            ));
        }
    }
}

/// Converts one handed-over frame (and its audio) and offers it to the receivers.
pub(super) fn process(
    shared: &Shared,
    sender: &Sender,
    cfg: &OutputConfig,
    st: &mut WorkerState,
    item: (VideoFrame, Instant, usize),
) {
    let (mut frame, handed_over, carried) = item;
    let format = *shared.format.lock_ok();
    let (fourcc, frame_dur) = (format.fourcc(), format.frame_dur() as i64);
    let timecode = frame.timecode.unwrap_or_else(|| {
        clock_tc(
            shared.epoch,
            handed_over,
            st.expected,
            frame_dur / 2,
            frame_dur / 8,
        )
    });
    st.expected = Some(timecode + frame_dur);
    // Audio carried over from replaced frames starts before this frame: it goes out
    // on its own, its timecode moved back by the carried samples, so every sample
    // keeps its time (the first sample of a frame's own audio is at the frame's
    // timecode).
    if carried > 0
        && let Some(a) = frame.audio.take()
    {
        sender.send(MediaFrame {
            captions: Vec::new(),
            fourcc,
            width: 0,
            height: 0,
            planes: Arc::default(),
            frame_rate: format.frame_rate,
            timecode: timecode - carried as i64 * 10_000_000 / a.sample_rate as i64,
            audio: a.channels,
            sample_rate: a.sample_rate,
            timestamp: frame.timestamp.unwrap_or_else(now_100ns),
            metadata: None,
            interlaced: false,
            hx_video: None,
            hx_audio: None,
            content_id: None,
            precise: None,
            #[cfg(feature = "gpu")]
            gpu: None,
        });
    }
    if let Some(d) = &frame.p216 {
        let precise = p216_planes(d, frame.width, frame.height);
        let planes = Arc::new(precise.iter().map(crate::shq::Plane16::to_8bit).collect());
        st.last = None;
        st.content_id += 1;
        let (audio, sample_rate) = match frame.audio {
            Some(a) => (a.channels, a.sample_rate),
            None => (Vec::new(), 48_000),
        };
        sender.send(MediaFrame {
            captions: Vec::new(),
            fourcc: Fourcc::Shq2,
            width: frame.width,
            height: frame.height,
            planes,
            frame_rate: format.frame_rate,
            audio,
            sample_rate,
            timecode,
            timestamp: frame.timestamp.unwrap_or_else(now_100ns),
            metadata: None,
            interlaced: false,
            hx_video: None,
            hx_audio: None,
            content_id: Some(st.content_id),
            precise: Some(Arc::new(precise)),
            #[cfg(feature = "gpu")]
            gpu: None,
        });
        return;
    }
    // Identical to the previous frame? Then reuse its planes and encoded payload.
    let same = st.last.as_ref().is_some_and(|(buf, w, h, s, a, f, _)| {
        *w == frame.width
            && *h == frame.height
            && *s == frame.stride
            && *a == frame.alpha
            && *f == fourcc
            && (Arc::ptr_eq(buf, &frame.rgba) || **buf == *frame.rgba)
    });
    let planes = if same {
        shared.repeated.fetch_add(1, Ordering::Relaxed);
        st.last.as_ref().unwrap().6.clone()
    } else {
        st.content_id += 1;
        let p = Arc::new(rgba_to_planes(
            &frame.rgba,
            frame.width,
            frame.height,
            frame.stride,
            fourcc,
            frame.alpha,
            &cfg.color,
        ));
        st.last = Some((
            frame.rgba.clone(),
            frame.width,
            frame.height,
            frame.stride,
            frame.alpha,
            fourcc,
            p.clone(),
        ));
        p
    };
    let (audio, sample_rate) = match frame.audio {
        Some(a) => (a.channels, a.sample_rate),
        None => (Vec::new(), 48_000),
    };
    sender.send(MediaFrame {
        captions: Vec::new(),
        fourcc,
        width: frame.width,
        height: frame.height,
        planes,
        frame_rate: format.frame_rate,
        audio,
        sample_rate,
        timecode,
        timestamp: frame.timestamp.unwrap_or_else(now_100ns),
        metadata: None,
        interlaced: false,
        hx_video: None,
        hx_audio: None,
        content_id: Some(st.content_id),
        precise: None,
        #[cfg(feature = "gpu")]
        gpu: None,
    });
}

/// Updates an output's monitor properties once a second (§E.5.1) until it stops.
pub(super) fn report_status(
    shared: &Shared,
    sender: &Sender,
    props: &Mutex<std::collections::BTreeMap<String, String>>,
) {
    // `replaced` is loaded first: it never exceeds `submitted`, which only grows, so the
    // difference can't underflow when a send lands between the two loads.
    let processed = || {
        let replaced = shared.replaced.load(Ordering::Relaxed);
        shared
            .submitted
            .load(Ordering::Relaxed)
            .saturating_sub(replaced)
    };
    let (mut at, mut last) = (std::time::Instant::now(), processed());
    while !shared.stop.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let dt = at.elapsed().as_secs_f64();
        if dt < 1.0 {
            continue;
        }
        let now = processed();
        let fps = now.saturating_sub(last) as f64 / dt;
        (at, last) = (std::time::Instant::now(), now);
        let st = sender.status();
        let (w, h) = *shared.size.lock_ok();
        let format = *shared.format.lock_ok();
        let b = |v: bool| v.to_string();
        let mut p = props.lock_ok();
        for (k, v) in [
            (
                "connection-state",
                if st.receivers > 0 {
                    "connected"
                } else {
                    "disconnected"
                }
                .to_owned(),
            ),
            ("connected-receivers", st.receivers.to_string()),
            ("video-resolution", format!("{w}x{h}")),
            (
                "video-codec",
                if format.alpha { "shq7" } else { "shq2" }.to_owned(),
            ),
            ("video-has-alpha", b(format.alpha)),
            (
                "video-frame-rate",
                format!("{}/{}", format.frame_rate.0, format.frame_rate.1),
            ),
            ("audio-present", b(st.sent.2 > 0)),
            ("tally-on-program", b(st.on_program)),
            ("tally-on-preview", b(st.on_preview)),
            ("video-frames-sent", st.sent.0.to_string()),
            ("video-bytes-sent", st.sent.1.to_string()),
            ("audio-frames-sent", st.sent.2.to_string()),
            ("audio-bytes-sent", st.sent.3.to_string()),
            ("metadata-frames-sent", st.metadata_sent.0.to_string()),
            ("metadata-bytes-sent", st.metadata_sent.1.to_string()),
            ("x-idn-fps", format!("{fps:.2}")),
            ("x-idn-receivers-encrypted", st.encrypted.to_string()),
            ("x-idn-frames-skipped", st.skipped.to_string()),
            // Same-machine pictures (§E.26): receivers on the ring, and the encoding done.
            ("x-idn-machine-receivers", st.machine_readers.to_string()),
            ("x-idn-gpu-receivers", st.gpu_readers.to_string()),
            ("x-idn-shq-encodes", st.shq_encodes.to_string()),
            ("x-idn-codec-encoders", st.codec_encoders.to_string()),
        ] {
            p.insert(k.to_owned(), v);
        }
    }
}

/// Audio the wire can carry: 1–256 channels of equal length, 1 sample to 1 s (at most
/// [`crate::media::MAX_AUDIO_SAMPLES`], which receivers accept), a rate receivers play
/// ([`crate::media::AUDIO_RATES`]; bug run 2026-11-24: any rate but 0 was sent, and every
/// IDN receiver dropped it without the sender knowing).
pub(super) fn check_audio(a: &AudioBlock) -> Result<(), SendError> {
    let n = a.channels.first().map_or(0, Vec::len);
    let bad = if a.channels.is_empty() || a.channels.len() > 256 {
        Some(format!("{} channels (1 to 256)", a.channels.len()))
    } else if a.channels.iter().any(|c| c.len() != n) {
        Some("channels of different lengths".into())
    } else if !crate::media::AUDIO_RATES.contains(&a.sample_rate) {
        Some(format!(
            "{} Hz (receivers play {} to {} Hz)",
            a.sample_rate,
            crate::media::AUDIO_RATES.start(),
            crate::media::AUDIO_RATES.end()
        ))
    } else if n == 0 || n > (a.sample_rate as usize).min(crate::media::MAX_AUDIO_SAMPLES) {
        Some(format!("{n} samples at {} Hz", a.sample_rate))
    } else {
        None
    };
    bad.map_or(Ok(()), |m| Err(SendError::BadAudio(m)))
}

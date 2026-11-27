//! Tests of inputs: configuration, the event queue, fields, limits and the inbox.

use super::*;

#[test]
fn lossless_by_default_but_not_for_previews() {
    let c = InputConfig::new("HOST (Desk)");
    assert!(c.lossless_audio && c.wants_lossless());
    let tile = InputConfig {
        low_bandwidth: true,
        ..InputConfig::new("HOST (Desk)")
    };
    assert!(!tile.wants_lossless());
    let thumb = InputConfig {
        picture_every: Some(60),
        ..InputConfig::new("HOST (Desk)")
    };
    assert!(!thumb.wants_lossless());
    let off = InputConfig {
        lossless_audio: false,
        ..InputConfig::new("HOST (Desk)")
    };
    assert!(!off.wants_lossless());
}

fn hx(keyframe: bool, pts: i64) -> InputEvent {
    InputEvent::Compressed(CompressedVideo {
        header: VideoHeader {
            fourcc: *b"H264",
            width: 16,
            height: 16,
            frame_rate_n: 30,
            frame_rate_d: 1,
            aspect_ratio: 1.0,
            field_id: 0,
            stream: 1,
            timecode: pts,
            timestamp: 0,
            metadata: None,
        },
        codec: Codec::H264,
        keyframe,
        data: Vec::new(),
        extra: Vec::new(),
        pts,
        dts: pts,
        received: Instant::now(),
    })
}

#[test]
fn dropping_an_hx_frame_never_leaves_a_broken_sequence() {
    let q = Queue {
        q: Mutex::default(),
        ready: Condvar::new(),
        cap: 3,
        stats: Default::default(),
        hx_broken: AtomicBool::new(false),
        mc_rejected: Arc::default(),
        spare_rgba: Mutex::default(),
    };
    // Keyframe 0, then P-frames 1 and 2: the queue is full.
    for (k, n) in [(true, 0), (false, 1), (false, 2)] {
        q.push(hx(k, n));
    }
    // P-frame 3 overflows it: keyframe 0 goes, so 1 and 2 (and 3) can't be decoded.
    q.push(hx(false, 3));
    let media = q.q.lock_ok().iter().filter(|e| e.is_media()).count();
    assert_eq!(media, 0, "no orphaned P-frames are delivered");
    assert!(
        q.hx_broken.load(Ordering::Relaxed),
        "the worker waits for a keyframe"
    );
    assert_eq!(q.stats.lock_ok().dropped, 4);
    // The next keyframe is delivered.
    q.push(hx(true, 4));
    assert!(matches!(q.q.lock_ok().back(), Some(InputEvent::Compressed(c)) if c.keyframe));
}

#[test]
fn fields_weave_only_when_they_match() {
    let field = |w: usize, h: usize, v: u8| Picture {
        fourcc: crate::shq::Fourcc::Shq2,
        width: w,
        height: h,
        format: crate::shq::Format::New,
        quality: 90,
        short_dc: false,
        high_precision: false,
        planes: vec![
            Plane {
                width: w,
                height: h,
                data: vec![v; w * h],
            },
            Plane {
                width: w.div_ceil(2),
                height: h,
                data: vec![v; w.div_ceil(2) * h],
            },
            Plane {
                width: w.div_ceil(2),
                height: h,
                data: vec![v; w.div_ceil(2) * h],
            },
        ],
        precise: None,
        coefficients: None,
    };
    let f = weave_fields(&field(4, 3, 1), &field(4, 2, 2)).expect("odd height: 3 + 2");
    assert_eq!((f.width, f.height), (4, 5));
    let rows: Vec<u8> = f.planes[0].data.chunks(4).map(|r| r[0]).collect();
    assert_eq!(rows, [1, 2, 1, 2, 1]);
    assert!(
        weave_fields(&field(4, 2, 1), &field(6, 2, 2)).is_none(),
        "widths differ"
    );
    assert!(
        weave_fields(&field(4, 2, 1), &field(4, 4, 2)).is_none(),
        "heights differ"
    );
    let mut bad = field(4, 2, 2);
    bad.planes[1].data.pop();
    assert!(
        weave_fields(&field(4, 2, 1), &bad).is_none(),
        "plane data short"
    );
}

#[test]
fn inputs_refuse_pictures_beyond_4k_by_default() {
    let limits = InputConfig::new("X").max_picture;
    let try_size = |width, height| {
        let p = shq::VideoParams {
            fourcc: *b"SHQ2",
            width,
            height,
            field_id: 0,
        };
        // A tiny payload: only the size check matters here.
        shq::decode(&p, &[150, 0, 0, 0], &limits)
    };
    for (w, h) in [
        (8192, 4320),
        (7680, 4320),
        (4097, 2160),
        (4096, 2161),
        (16, 8192),
    ] {
        assert!(
            matches!(try_size(w, h), Err(shq::DecodeError::TooLarge)),
            "{w}x{h}"
        );
    }
    for (w, h) in [(1920, 1080), (3840, 2160), (4096, 2160), (2160, 3840)] {
        assert!(
            !matches!(try_size(w, h), Err(shq::DecodeError::TooLarge)),
            "{w}x{h}"
        );
    }
}

fn queue() -> Arc<Queue> {
    Arc::new(Queue {
        q: Mutex::new(VecDeque::new()),
        ready: Condvar::new(),
        cap: 8,
        stats: Default::default(),
        hx_broken: AtomicBool::new(false),
        mc_rejected: Arc::default(),
        spare_rgba: Mutex::default(),
    })
}

#[test]
fn unread_text_events_are_bounded_and_states_kept() {
    let q = queue();
    q.push(InputEvent::State(ConnectionState::Connecting));
    for i in 0..1000 {
        q.push(InputEvent::Log(format!("line {i}")));
    }
    let big = "x".repeat(1 << 20);
    for _ in 0..10 {
        q.push(InputEvent::Metadata {
            kind: 3,
            xml: big.clone(),
        });
    }
    let ev = q.q.lock_ok();
    let texts: Vec<_> = ev.iter().filter_map(InputEvent::text_len).collect();
    assert!(texts.len() <= OTHER_KEPT);
    assert!(texts.iter().sum::<usize>() <= OTHER_BYTES_KEPT);
    assert!(matches!(ev[0], InputEvent::State(_)), "state events stay");
    assert!(
        matches!(ev.back(), Some(InputEvent::Metadata { .. })),
        "the newest stay"
    );
    assert_eq!(q.stats.lock_ok().events_dropped, 1010 - texts.len());
}

#[test]
fn the_worker_inbox_holds_a_bounded_number_of_pictures() {
    let q = queue();
    let (tx, rx) = mpsc::channel();
    let inbox = Inbox {
        tx,
        video_waiting: Arc::default(),
        audio_waiting: Arc::default(),
        video_lost: Arc::default(),
        queue: q.clone(),
    };
    let picture = || Frame {
        kind: kind::VIDEO,
        scrambled: false,
        version: 0,
        ext: Vec::new(),
        payload: Vec::new(),
    };
    for _ in 0..MAX_VIDEO_WAITING + 10 {
        inbox
            .send(Incoming::Media(MediaKind::Video, "udp", Ok(picture()), 0))
            .unwrap();
    }
    inbox
        .send(Incoming::Media(
            MediaKind::Audio,
            "udp",
            Ok(Frame {
                kind: kind::AUDIO,
                ..picture()
            }),
            0,
        ))
        .unwrap();
    assert_eq!(
        rx.try_iter().count(),
        MAX_VIDEO_WAITING + 1,
        "audio still gets in"
    );
    assert_eq!(q.stats.lock_ok().late, 10);
}

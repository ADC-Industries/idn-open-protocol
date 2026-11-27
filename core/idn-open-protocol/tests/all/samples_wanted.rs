//! An output knows whether anything wants its pictures' samples in memory (§E.27): an
//! idle tap (a gateway with nothing to record) doesn't; a tap or a receiver does.

use idn_open_protocol::api::{Output, OutputConfig, PlanarFrame, Publisher, PublisherConfig};
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use idn_open_protocol::shq::{Fourcc, Plane};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[test]
fn samples_are_wanted_by_taps_that_want_them_and_by_receivers() {
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let out = Output::new(&publisher, OutputConfig::new("Program")).unwrap();
    assert!(!out.samples_wanted(), "nothing takes it");
    let recording = Arc::new(AtomicBool::new(false));
    let r = recording.clone();
    let idle = out.tap_while(move || r.load(Ordering::Relaxed), |_| {});
    assert!(!out.samples_wanted(), "a gateway with nothing to record");
    recording.store(true, Ordering::Relaxed);
    assert!(out.samples_wanted(), "a gateway recording");
    drop(idle);
    let tap = out.tap(|_| {});
    assert!(out.samples_wanted(), "a plain tap");
    drop(tap);
    assert!(!out.samples_wanted());
    // A receiver over the network wants them once its session has had a picture.
    let input = Input::open(InputConfig::new(&format!("127.0.0.1:{}", out.port()))).unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    while !out.samples_wanted() {
        assert!(Instant::now() < end, "the receiver's session doesn't count");
        let plane = |w: usize, v: u8| Plane {
            width: w,
            height: 36,
            data: vec![v; w * 36],
        };
        let _ = out.send_planes(PlanarFrame {
            layout: Fourcc::Shq2,
            width: 64,
            height: 36,
            planes: Arc::new(vec![plane(64, 120), plane(32, 128), plane(32, 128)]),
            timestamp: None,
            timecode: None,
            audio: None,
            coded: None,
        });
        let _ = input.recv(Duration::from_millis(50));
    }
}

/// A receiver on this machine taking pictures through the ring wants them only there
/// (`samples_wanted_off_ring` false), and a picture filled straight into the ring
/// (`send_planes_filled`) reaches it with its samples.
#[test]
fn a_receiver_through_the_ring_wants_samples_only_there() {
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let out = Output::new(&publisher, OutputConfig::new("Ring")).unwrap();
    let input = Input::open(InputConfig::new(&format!("127.0.0.1:{}", out.port()))).unwrap();
    let plane = |w: usize, v: u8, samples: bool| Plane {
        width: w,
        height: 36,
        data: if samples { vec![v; w * 36] } else { Vec::new() },
    };
    let frame = |samples: bool| PlanarFrame {
        layout: Fourcc::Shq2,
        width: 64,
        height: 36,
        planes: Arc::new(vec![
            plane(64, 120, samples),
            plane(32, 100, samples),
            plane(32, 150, samples),
        ]),
        timestamp: None,
        timecode: None,
        audio: None,
        coded: None,
    };
    // Until its pictures come through the ring.
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < end, "never through the ring alone");
        let _ = out.send_planes(frame(true));
        while let Some(ev) = input.recv(Duration::from_millis(20)) {
            let _ = ev;
        }
        if out.samples_wanted() && !out.samples_wanted_off_ring() {
            break;
        }
    }
    // Filled straight into the ring: the receiver gets the samples written.
    let mut got = None;
    let end = Instant::now() + Duration::from_secs(10);
    while got.is_none() {
        assert!(Instant::now() < end, "no filled picture came");
        let _ = out.send_planes_filled(frame(false), &mut |planes| {
            for (p, v) in planes.iter_mut().zip([60u8, 90, 200]) {
                p.fill(v);
            }
            true
        });
        while let Some(ev) = input.recv(Duration::from_millis(20)) {
            if let InputEvent::Video(v) = ev
                && v.via == "machine"
                && v.picture.planes[0].data.first() == Some(&60)
            {
                got = Some(v.picture);
            }
        }
    }
    let p = got.unwrap();
    assert!(p.planes[0].data.iter().all(|&b| b == 60));
    assert!(p.planes[1].data.iter().all(|&b| b == 90));
    assert!(p.planes[2].data.iter().all(|&b| b == 200));
}

/// A receiver on this machine keeps its pictures through the ring while a receiver over
/// the network gets SpeedHQ from the GPU (§E.26, §E.27). The output sends its samples
/// only while `samples_wanted` says so, as the switcher does: counted out while the GPU
/// encoded for it, the receiver here never had samples in the ring and was served over
/// the connection (a monitor on the switcher's PC beside a projector, 2026-11-03).
#[cfg(feature = "gpu")]
#[test]
fn a_receiver_here_keeps_the_ring_beside_one_the_gpu_encodes_for() {
    use idn_open_protocol::gpu::{GpuPicture, open_device, slot_layout};
    let Some(hand) = open_device("test", false) else {
        eprintln!("no GPU here: not tested");
        return;
    };
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let out = Output::new(&publisher, OutputConfig::new("Both")).unwrap();
    out.use_gpu(Some(hand.clone()));
    let addr = format!("127.0.0.1:{}", out.port());
    let far = Input::open(InputConfig {
        machine: false,
        ..InputConfig::new(&addr)
    })
    .unwrap();
    let here = Input::open(InputConfig::new(&addr)).unwrap();
    let (w, h) = (64u32, 36u32);
    let (at, bytes) = slot_layout(&[(w, h), (w / 2, h), (w / 2, h)]);
    let buffer = hand.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: bytes,
        usage: wgpu::BufferUsages::STORAGE
            | wgpu::BufferUsages::COPY_DST
            | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let mut data = vec![0u8; bytes as usize];
    for (p, v) in at.iter().zip([120u8, 100, 150]) {
        for y in 0..p.height as usize {
            let o = p.offset as usize + y * p.row_bytes as usize;
            data[o..o + p.width as usize].fill(v);
        }
    }
    hand.queue.write_buffer(&buffer, 0, &data);
    let pic = GpuPicture {
        buffer: Arc::new(buffer),
        planes: at.clone(),
    };
    let plane = |pw: u32, v: u8, samples: bool| Plane {
        width: pw as usize,
        height: h as usize,
        data: if samples {
            vec![v; (pw * h) as usize]
        } else {
            Vec::new()
        },
    };
    let frame = |samples: bool| PlanarFrame {
        layout: Fourcc::Shq2,
        width: w as usize,
        height: h as usize,
        planes: Arc::new(vec![
            plane(w, 120, samples),
            plane(w / 2, 100, samples),
            plane(w / 2, 150, samples),
        ]),
        timestamp: None,
        timecode: None,
        audio: None,
        coded: None,
    };
    // The last pictures the receiver here got, by how they came.
    let mut via: std::collections::VecDeque<&'static str> = Default::default();
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(
            Instant::now() < end,
            "the receiver here never kept to the ring: {via:?}"
        );
        let samples = out.samples_wanted();
        let _ = if samples {
            out.send_planes_gpu(frame(true), &pic.buffer, &pic.planes)
        } else {
            out.send_planes_gpu_sizes(frame(false), &pic.buffer, &pic.planes)
        };
        while far.recv(Duration::ZERO).is_some() {}
        while let Some(ev) = here.recv(Duration::from_millis(30)) {
            if let InputEvent::Video(v) = ev {
                via.push_back(v.via);
                if via.len() > 20 {
                    via.pop_front();
                }
            }
        }
        if via.len() == 20 && via.iter().all(|&v| v == "machine") {
            break;
        }
    }
}

/// A recorder on the GPU whose recording stopped (a gateway idle for its last seconds)
/// gets no picture with neither samples nor a copy on the GPU: the switcher's pictures,
/// left on the GPU while nothing records, reached the encoder so and it panicked, and
/// every later recording of that switcher was empty (lab 348-A).
#[cfg(feature = "gpu")]
#[test]
fn an_idle_recorder_on_the_gpu_gets_no_empty_pictures() {
    use idn_open_protocol::gpu::{GpuPicture, open_device, slot_layout};
    use std::sync::atomic::AtomicUsize;
    let Some(hand) = open_device("test", false) else {
        eprintln!("no GPU here: not tested");
        return;
    };
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let out = Output::new(&publisher, OutputConfig::new("Program")).unwrap();
    out.use_gpu(Some(hand.clone()));
    let (w, h) = (64u32, 36u32);
    let (at, bytes) = slot_layout(&[(w, h), (w / 2, h), (w / 2, h)]);
    let buffer = hand.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: bytes,
        usage: wgpu::BufferUsages::STORAGE
            | wgpu::BufferUsages::COPY_DST
            | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let pic = GpuPicture {
        buffer: Arc::new(buffer),
        planes: at,
    };
    let plane = |pw: u32| Plane {
        width: pw as usize,
        height: h as usize,
        data: Vec::new(),
    };
    let frame = || PlanarFrame {
        layout: Fourcc::Shq2,
        width: w as usize,
        height: h as usize,
        planes: Arc::new(vec![plane(w), plane(w / 2), plane(w / 2)]),
        timestamp: None,
        timecode: None,
        audio: None,
        coded: None,
    };
    let recording = Arc::new(AtomicBool::new(true));
    let (got, empty) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (r, g, e) = (recording.clone(), got.clone(), empty.clone());
    let _tap = out.tap_gpu_while(
        move || r.load(Ordering::Relaxed),
        move |f| {
            g.fetch_add(1, Ordering::Relaxed);
            if f.gpu.is_none() && f.planes.first().is_some_and(|p| p.data.is_empty()) {
                e.fetch_add(1, Ordering::Relaxed);
            }
        },
    );
    for i in 0..20 {
        if i == 10 {
            recording.store(false, Ordering::Relaxed); // the last recording stopped
        }
        let _ = out.send_planes_gpu_sizes(frame(), &pic.buffer, &pic.planes);
    }
    assert!(
        got.load(Ordering::Relaxed) >= 10,
        "recording, it took the pictures on the GPU"
    );
    assert_eq!(
        empty.load(Ordering::Relaxed),
        0,
        "pictures with nothing to take"
    );
}

/// A file's picture kept on the GPU that comes with its compressed picture too (H.264
/// passed through) reaches a tap on the GPU with its picture there: sent as a compressed
/// one, it went without it, and the switcher's input of the file had nothing to show (lab
/// 321-B, 321-C).
#[cfg(feature = "gpu")]
#[test]
fn a_picture_on_the_gpu_with_its_compressed_one_keeps_its_gpu_picture() {
    use idn_open_protocol::codec::{Codec, CodedPicture};
    use idn_open_protocol::gpu::{GpuPicture, open_device, slot_layout};
    use std::sync::atomic::AtomicUsize;
    let Some(hand) = open_device("test", false) else {
        eprintln!("no GPU here: not tested");
        return;
    };
    let publisher = Publisher::new(PublisherConfig {
        mdns: Some(false),
        ..Default::default()
    })
    .unwrap();
    let out = Output::new(&publisher, OutputConfig::new("File")).unwrap();
    out.use_gpu(Some(hand.clone()));
    let (w, h) = (64u32, 36u32);
    let (at, bytes) = slot_layout(&[(w, h), (w / 2, h), (w / 2, h)]);
    let buffer = hand.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: bytes,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let pic = GpuPicture {
        buffer: Arc::new(buffer),
        planes: at,
    };
    let plane = |pw: u32| Plane {
        width: pw as usize,
        height: h as usize,
        data: Vec::new(),
    };
    let (on_gpu, none) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (g, n) = (on_gpu.clone(), none.clone());
    let _tap = out.tap_gpu_while(
        || true,
        move |f| {
            if f.gpu.is_some() {
                g.fetch_add(1, Ordering::Relaxed);
            } else {
                n.fetch_add(1, Ordering::Relaxed);
            }
        },
    );
    for _ in 0..5 {
        let f = PlanarFrame {
            layout: Fourcc::Shq2,
            width: w as usize,
            height: h as usize,
            planes: Arc::new(vec![plane(w), plane(w / 2), plane(w / 2)]),
            timestamp: None,
            timecode: None,
            audio: None,
            coded: Some(CodedPicture {
                codec: Codec::H264,
                keyframe: true,
                data: vec![0, 0, 0, 1, 0x65],
                extra: Vec::new(),
            }),
        };
        out.send_planes_gpu_only(f, pic.clone()).unwrap();
    }
    assert_eq!(
        on_gpu.load(Ordering::Relaxed),
        5,
        "each with its picture on the GPU"
    );
    assert_eq!(none.load(Ordering::Relaxed), 0);
}

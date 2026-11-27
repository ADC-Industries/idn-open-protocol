use super::*;
use crate::compose;
use crate::frame::Ycc;

/// A GPU, or the test passes saying there is none (`IDN_SWITCHER_SOFTWARE_GPU`: a
/// software one will do).
fn gpu(w: usize, h: usize) -> Option<Gpu> {
    match Gpu::new(w, h) {
        Ok(g) => {
            eprintln!("GPU: {}", g.name);
            Some(g)
        }
        Err(e) => {
            eprintln!("skipped: {e}");
            None
        }
    }
}

/// A picture with structure (gradients), so scaling and positions show.
fn pattern(w: usize, h: usize, seed: u8) -> Frame {
    let mut f = Frame::black(w, h);
    let cw = Frame::chroma_width(w);
    for r in 0..h {
        for x in 0..w {
            f.y[r * w + x] = (16 + (x * 7 + r * 3 + seed as usize * 31) % 220) as u8;
        }
        for c in 0..cw {
            f.cb[r * cw + c] = (16 + (c * 5 + r + seed as usize * 17) % 224) as u8;
            f.cr[r * cw + c] = (16 + (c * 3 + r * 2 + seed as usize * 13) % 224) as u8;
        }
    }
    f
}

fn max_diff(a: &Frame, b: &Frame) -> u8 {
    assert_eq!((a.width, a.height), (b.width, b.height));
    let d = |x: &[u8], y: &[u8]| {
        x.iter()
            .zip(y)
            .map(|(p, q)| p.abs_diff(*q))
            .max()
            .unwrap_or(0)
    };
    d(&a.y, &b.y).max(d(&a.cb, &b.cb)).max(d(&a.cr, &b.cr))
}

/// Luma wipes (§S.14) and a stinger's clip over the bus: the GPU's picture is the
/// CPU's to the bit.
#[test]
fn luma_wipes_and_stingers_as_on_the_cpu() {
    let _one = crate::GPU_TESTS.lock_ok();
    let (w, h) = (1280, 720);
    let Some(mut g) = gpu(w, h) else { return };
    let a = Arc::new(pattern(w, h, 1));
    let b = Arc::new(pattern(w, h, 2));
    // A pattern of every level, across and down.
    let mut pat = pattern(w, h, 4);
    for r in 0..h {
        for x in 0..w {
            pat.y[r * w + x] = (16 + (x + r) * 219 / (w + h - 2)) as u8;
        }
    }
    let pat = Arc::new(pat);
    let bus = |p: &Arc<Frame>| Layer::fitted(p.clone(), w, h);
    for (soft, invert) in [(0.0, false), (0.15, false), (0.1, true)] {
        let kind = Kind::Luma {
            pattern: 0,
            soft,
            invert,
        };
        for t in [0.1, 0.5, 0.9] {
            let scene = Scene {
                width: w,
                height: h,
                program: bus(&a),
                transition: Some((bus(&b), kind, t)),
                ftb: 0.0,
                pattern: Some(bus(&pat)),
                stinger: None,
            };
            let gpu = g.compose(&scene).unwrap();
            let cpu = compose::luma(&a, &b, &pat, soft, invert, t);
            assert_eq!(max_diff(&gpu, &cpu), 0, "luma {soft} {invert} at {t}");
        }
    }
    // A stinger's clip with alpha over the program.
    let mut clip = pattern(w, h, 5);
    clip.a = Some((0..w * h).map(|i| ((i / w) * 255 / h) as u8).collect());
    let clip = Arc::new(clip);
    let scene = Scene {
        width: w,
        height: h,
        program: bus(&a),
        transition: None,
        ftb: 0.0,
        pattern: None,
        stinger: Some(bus(&clip)),
    };
    let gpu = g.compose(&scene).unwrap();
    let mut cpu = (*a).clone();
    compose::key(&mut cpu, &clip, 1.0);
    assert_eq!(max_diff(&gpu, &cpu), 0, "the stinger's clip keyed");
}

/// How far a zoomed (scaled) preview may differ between the GPU and the CPU.
const ZOOM_STEPS: u8 = 1;

#[test]
fn the_gpu_makes_what_the_cpu_makes() {
    let _one = crate::GPU_TESTS.lock_ok();
    let (w, h) = (1280, 720);
    let Some(mut g) = gpu(w, h) else { return };
    let a = Arc::new(pattern(w, h, 1));
    let b = Arc::new(pattern(w, h, 2));
    let bus = |p: &Arc<Frame>| Layer::fitted(p.clone(), w, h);
    // Unscaled: the same to the bit, for every transition.
    let kinds = [
        Kind::Mix,
        Kind::Fade,
        Kind::Wipe(Direction::Left, 0.0),
        Kind::Wipe(Direction::Right, 0.1),
        Kind::Wipe(Direction::Down, 0.05),
        Kind::Wipe(Direction::Up, 0.0),
        Kind::Wipe(Direction::Circle, 0.1),
        Kind::Zoom,
    ];
    for kind in kinds {
        for t in [0.0, 0.25, 0.5, 0.8, 1.0] {
            let scene = Scene {
                width: w,
                height: h,
                program: bus(&a),
                transition: Some((bus(&b), kind, t)),
                ftb: 0.0,
                pattern: None,
                stinger: None,
            };
            let gpu = g.compose(&scene).unwrap();
            let cpu = compose::transition(&a, &b, kind, t);
            // (The circle's soft edge takes a square root, which GPUs round their own
            // way: within a step there.)
            // A zoom's preview is scaled: the GPU's scaler and the CPU's agree within a
            // step (as for scaled pictures, below).
            let allowed = match kind {
                Kind::Wipe(Direction::Circle, _) => 1,
                Kind::Zoom => ZOOM_STEPS,
                _ => 0,
            };
            assert!(
                max_diff(&gpu, &cpu) <= allowed,
                "{kind:?} at {t}: off by {}",
                max_diff(&gpu, &cpu)
            );
        }
    }
    // A stinger's clip with alpha half on keyed over the bus, and fade to black.
    let mut lower = pattern(w, h, 3);
    lower.a = Some((0..w * h).map(|i| ((i % w) * 255 / w) as u8).collect());
    let lower = Arc::new(lower);
    let scene = Scene {
        width: w,
        height: h,
        program: bus(&a),
        transition: None,
        ftb: 0.3,
        pattern: None,
        stinger: Some(bus(&lower)),
    };
    let gpu = g.compose(&scene).unwrap();
    let mut cpu = (*a).clone();
    compose::key(&mut cpu, &lower, 1.0);
    compose::to_black(&mut cpu, 0.3);
    assert_eq!(max_diff(&gpu, &cpu), 0, "key and fade to black");
    // Scaled: a 4:3 picture on a 1080p program, letterboxed: within a step.
    let (w2, h2) = (1920, 1080);
    let square = Arc::new(pattern(640, 480, 5));
    let scene = Scene {
        width: w2,
        height: h2,
        program: Layer::fitted(square.clone(), w2, h2),
        transition: None,
        ftb: 0.0,
        pattern: None,
        stinger: None,
    };
    let gpu = g.compose(&scene).unwrap();
    let cpu = square.fitted(w2, h2);
    let d = max_diff(&gpu, &cpu);
    assert!(d <= 1, "scaled: off by {d}");
    // Black bars where the 4:3 picture doesn't reach.
    assert_eq!(gpu.y[540 * w2 + 10], 16);
}

/// Inputs that came as coefficients (perf queue 2b), 4:2:2 and 4:2:0 (NDI's pictures
/// above 1080p), odd sizes too: the program is the CPU's decode of them, to the bit
/// in f64.
#[test]
fn coefficient_inputs_come_out_as_the_cpu_decodes() {
    let _one = crate::GPU_TESTS.lock_ok();
    use idn_open_protocol::shq::encode::{EncodeInput, EncodeOptions, encode};
    use idn_open_protocol::shq::{
        Fourcc, Limits, Picture, Plane, VideoParams, decode_coefficients,
    };
    for (w, h, fourcc) in [
        (1280, 720, Fourcc::Shq2),
        (1280, 720, Fourcc::Shq0),
        (722, 301, Fourcc::Shq0),
    ] {
        let Some(mut g) = gpu(w, h) else { return };
        let src = pattern(w, h, 6);
        let cw = Frame::chroma_width(w);
        // 4:2:0: every other chroma row.
        let chroma = |p: &[u8]| -> Vec<u8> {
            match fourcc {
                Fourcc::Shq0 => (0..h.div_ceil(2))
                    .flat_map(|r| p[2 * r * cw..][..cw].iter().copied())
                    .collect(),
                _ => p.to_vec(),
            }
        };
        let ch = if fourcc == Fourcc::Shq0 {
            h.div_ceil(2)
        } else {
            h
        };
        let plane = |width, height, data| Plane {
            width,
            height,
            data,
        };
        let planes = vec![
            plane(w, h, src.y.clone()),
            plane(cw, ch, chroma(&src.cb)),
            plane(cw, ch, chroma(&src.cr)),
        ];
        let payload = encode(
            &EncodeInput {
                fourcc,
                width: w,
                height: h,
                planes: &planes,
                precise: None,
            },
            &EncodeOptions {
                quality: 92,
                ..Default::default()
            },
        );
        let vp = VideoParams {
            fourcc: if fourcc == Fourcc::Shq0 {
                *b"SHQ0"
            } else {
                *b"SHQ2"
            },
            width: w as u32,
            height: h as u32,
            field_id: 0,
        };
        let c = decode_coefficients(&vp, &payload, &Limits::default()).unwrap();
        let pic = Arc::new(Frame::from_owned(Picture::from_coefficients(c)));
        let scene = Scene {
            width: w,
            height: h,
            program: Layer::fitted(pic.clone(), w, h),
            transition: None,
            ftb: 0.0,
            pattern: None,
            stinger: None,
        };
        let gpu = g.compose(&scene).unwrap();
        let d = max_diff(&gpu, &pic.pixels());
        let allowed = match g.precision() {
            crate::idct::Precision::F64 => 0,
            crate::idct::Precision::F32 => 1,
        };
        assert!(d <= allowed, "{fourcc:?} {w}x{h}: off by {d}");
    }
}

/// Two cameras as coefficients in one frame (a dissolve between them): each
/// reconstructed from its own words. `pack_into` counted the second picture's
/// offsets from its own start, so it was made from the first's.
#[test]
fn two_coefficient_pictures_in_one_frame() {
    let _one = crate::GPU_TESTS.lock_ok();
    use idn_open_protocol::shq::encode::{EncodeInput, EncodeOptions, encode};
    use idn_open_protocol::shq::{
        Fourcc, Limits, Picture, Plane, VideoParams, decode_coefficients,
    };
    let (w, h) = (640, 360);
    let Some(mut g) = gpu(w, h) else { return };
    let cw = Frame::chroma_width(w);
    let coefs = |seed: u8| {
        let src = pattern(w, h, seed);
        let plane = |width, data| Plane {
            width,
            height: h,
            data,
        };
        let planes = vec![
            plane(w, src.y.clone()),
            plane(cw, src.cb.clone()),
            plane(cw, src.cr.clone()),
        ];
        let payload = encode(
            &EncodeInput {
                fourcc: Fourcc::Shq2,
                width: w,
                height: h,
                planes: &planes,
                precise: None,
            },
            &EncodeOptions {
                quality: 92,
                ..Default::default()
            },
        );
        let vp = VideoParams {
            fourcc: *b"SHQ2",
            width: w as u32,
            height: h as u32,
            field_id: 0,
        };
        let c = decode_coefficients(&vp, &payload, &Limits::default()).unwrap();
        Arc::new(Frame::from_owned(Picture::from_coefficients(c)))
    };
    let (a, b) = (coefs(1), coefs(7));
    let scene = Scene {
        width: w,
        height: h,
        program: Layer::fitted(a.clone(), w, h),
        transition: Some((Layer::fitted(b.clone(), w, h), Kind::Mix, 0.5)),
        ftb: 0.0,
        pattern: None,
        stinger: None,
    };
    let gpu = g.compose(&scene).unwrap();
    let cpu = compose::transition(&a.pixels(), &b.pixels(), Kind::Mix, 0.5);
    let allowed = match g.precision() {
        crate::idct::Precision::F64 => 1,
        crate::idct::Precision::F32 => 2,
    };
    let d = max_diff(&gpu, &cpu);
    assert!(d <= allowed, "off by {d}");
}

#[test]
fn pipelined_frames_come_one_behind() {
    let _one = crate::GPU_TESTS.lock_ok();
    let (w, h) = (640, 360);
    let Some(mut g) = gpu(w, h) else { return };
    let pics: Vec<Arc<Frame>> = (0..4)
        .map(|k| Arc::new(Frame::solid(w, h, Ycc(40 + 40 * k, 128, 128))))
        .collect();
    let scene = |p: &Arc<Frame>| Scene {
        width: w,
        height: h,
        program: Layer::fitted(p.clone(), w, h),
        transition: None,
        ftb: 0.0,
        pattern: None,
        stinger: None,
    };
    let mut f = Frame::black(1, 1);
    assert!(
        !g.compose_next(&scene(&pics[0]), &mut f).unwrap(),
        "nothing yet"
    );
    for k in 1..4 {
        assert!(g.compose_next(&scene(&pics[k]), &mut f).unwrap());
        assert_eq!(f, *pics[k - 1], "frame {k} gives frame {}", k - 1);
    }
    // Waiting for one drains what is in flight.
    g.compose_into(&scene(&pics[0]), &mut f).unwrap();
    assert_eq!(f, *pics[0]);
}

/// A compositor made again on the first one's device (the frame clock started again after
/// a panic, lab 452(b)) composes a picture kept on that device, made for the first one:
/// a compositor on a device of its own can't (its bind group refers to a buffer its
/// device doesn't have).
#[test]
fn a_compositor_made_again_takes_the_first_ones_gpu_pictures() {
    let _one = crate::GPU_TESTS.lock_ok();
    let (w, h) = (640, 360);
    let Some(first) = gpu(w, h) else { return };
    let (seed, hand) = (first.seed(), first.hand_in());
    let want = pattern(w, h, 4);
    let cw = Frame::chroma_width(w);
    let (planes, size) = idn_open_protocol::gpu::slot_layout(&[
        (w as u32, h as u32),
        (cw as u32, h as u32),
        (cw as u32, h as u32),
    ]);
    let buffer = hand.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("an input's picture"),
        size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: true,
    });
    let mut bytes = vec![0u8; size as usize];
    for (p, src) in planes.iter().zip([&want.y, &want.cb, &want.cr]) {
        let pw = p.width as usize;
        for r in 0..p.height as usize {
            let at = p.offset as usize + r * p.row_bytes as usize;
            bytes[at..at + pw].copy_from_slice(&src[r * pw..(r + 1) * pw]);
        }
    }
    buffer
        .slice(..)
        .get_mapped_range_mut()
        .unwrap()
        .copy_from_slice(&bytes);
    buffer.unmap();
    // The first compositor gone, as with the clock thread that panicked.
    drop(first);
    // By the engine's own path (as its clock makes it after a restart); one whose GPU
    // failed before stays on the CPU.
    let seed = Ok(seed);
    let mut again = crate::engine::gpu_from(Some(&seed), None, w, h).unwrap();
    assert!(crate::engine::gpu_from(Some(&Err("failed".into())), None, w, h).is_err());
    let pic = Arc::new(Frame::on_gpu(
        idn_open_protocol::gpu::GpuPicture {
            buffer: Arc::new(buffer),
            planes,
        },
        hand,
    ));
    let scene = Scene {
        width: w,
        height: h,
        program: Layer::fitted(pic, w, h),
        transition: None,
        ftb: 0.0,
        pattern: None,
        stinger: None,
    };
    let out = again.compose(&scene).unwrap();
    assert_eq!(
        max_diff(&out, &want),
        0,
        "the picture on the first device composed"
    );
}

use super::*;

/// A P010 picture (10-bit values in the high bits, Cb Cr interleaved) comes out as
/// each sample's high byte, Cb and Cr apart; odd sizes too.
#[test]
fn p010_comes_out_as_high_bytes() {
    for (w, h) in [(8usize, 4usize), (37, 9)] {
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let (ys, cs) = (2 * w + 6, 4 * cw + 2);
        let y: Vec<u8> = (0..ys * h).map(|i| (i * 13 + 1) as u8).collect();
        let c: Vec<u8> = (0..cs * ch).map(|i| (i * 7 + 5) as u8).collect();
        let pic = ffi::Picture {
            format: ff::AVPixelFormat::AV_PIX_FMT_P010LE,
            width: w,
            height: h,
            planes: [(&y[..], ys), (&c[..], cs), (&[][..], 0)],
            color_range: 1,
            colorspace: 9,
            uncached: false,
        };
        let d = from_ffmpeg(pic).unwrap();
        for r in 0..h {
            for x in 0..w {
                assert_eq!(d.planes[0].data[r * w + x], y[r * ys + 2 * x + 1]);
            }
        }
        for r in 0..ch {
            for x in 0..cw {
                assert_eq!(d.planes[1].data[r * cw + x], c[r * cs + 4 * x + 1]);
                assert_eq!(d.planes[2].data[r * cw + x], c[r * cs + 4 * x + 3]);
            }
        }
    }
}

/// The chroma split and the streaming copy give what plain loops give, for any
/// length and alignment.
#[test]
fn split_and_copy_match_plain_loops() {
    let src: Vec<u8> = (0..1000u32).map(|i| (i * 7 + 3) as u8).collect();
    for off in 0..3 {
        for n in [0, 1, 15, 16, 17, 33, 100, 497] {
            let s = &src[off..off + 2 * n];
            let (mut a, mut b) = (vec![0; n], vec![0; n]);
            split_pairs(s, &mut a, &mut b);
            for k in 0..n {
                assert_eq!((a[k], b[k]), (s[2 * k], s[2 * k + 1]), "n {n} off {off}");
            }
            let mut d = vec![0; 2 * n];
            ffi::copy_uncached(&mut d, s);
            assert_eq!(d, s);
        }
    }
}

/// The CPU H.264 fallback (X.12.6): an I-frame from a test picture, also as I420
/// (OpenH264's input) when that encoder is the one here.
#[test]
fn the_cpu_h264_encoder_encodes() {
    let ff = Ffmpeg::probe_device(None);
    let Some(b) = ff.cpu_h264() else {
        eprintln!("no CPU H.264 encoder in this FFmpeg");
        return;
    };
    let mut e = open_encoder(&None, b, &test_config(Codec::H264)).unwrap();
    let mut out = Vec::new();
    for i in 0..5 {
        out.extend(e.encode(&test_picture(), Fourcc::Shq2, i, i == 0).unwrap());
    }
    assert!(out.first().is_some_and(|f| f.keyframe), "{b:?}");
    eprintln!("{b:?}: {} access units", out.len());
}

#[test]
fn software_moves_to_the_gpu_only_when_it_stays_behind() {
    // Pictures every 16.7 ms: 10 ms of work keeps up, 20 ms doesn't; one slow
    // picture (a keyframe) isn't enough.
    let run = |works: &[f64]| {
        let mut p = Pace::default();
        let mut moved = 0;
        for &w in works {
            p.interval = 1.0 / 60.0;
            if p.decoded(w) {
                moved += 1;
            }
        }
        (moved, p.to_gpu)
    };
    assert_eq!(run(&[0.010; 100]), (0, false));
    let mut spike = vec![0.010; 100];
    spike[40] = 0.080;
    assert_eq!(run(&spike), (0, false));
    assert_eq!(run(&[0.020; 100]), (1, true), "said once");
    assert_eq!(run(&[0.020; 29]), (0, false), "not before 30 pictures");
}

/// The 4:2:2 and 4:2:0 fast paths give what the general average gives.
#[test]
fn nv12_fast_paths_match_the_general_average() {
    let mut seed = 3u32;
    let mut rnd = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed as u8
    };
    for (w, h, ch) in [
        (64usize, 36usize, 36usize),
        (63, 35, 35),
        (64, 36, 18),
        (63, 35, 18),
    ] {
        let cw = w.div_ceil(2);
        let mut plane = |pw: usize, ph: usize| Plane {
            width: pw,
            height: ph,
            data: (0..pw * ph).map(|_| rnd()).collect(),
        };
        let planes = vec![plane(w, h), plane(cw, ch), plane(cw, ch)];
        let f = ffi::Frame::nv12(w, h).unwrap();
        to_nv12(&planes, &f);
        let (duv, suv) = f.plane(1);
        let fy = if ch < h { 1 } else { 2 };
        for j in 0..h.div_ceil(2) {
            let (r0, r1) = ((j * fy).min(ch - 1), (j * fy + fy - 1).min(ch - 1));
            for i in 0..cw {
                for (k, p) in planes[1..].iter().enumerate() {
                    let at = |r: usize| p.data[r * cw + i] as u32;
                    let want = ((2 * at(r0) + 2 * at(r1) + 2) / 4) as u8;
                    // SAFETY: inside the NV12 frame's CbCr plane (row j < ch, byte 2i + k < 2 cw).
                    let got = unsafe { *duv.add(j * suv + 2 * i + k) };
                    assert_eq!(got, want, "{w}x{h} chroma rows {ch}: ({i}, {j}) plane {k}");
                }
            }
        }
    }
}

#[test]
fn av1_sequence_headers_are_found() {
    // Temporal delimiter (type 2, size 0), sequence header (type 1, size 3), frame.
    let tu = [0x12, 0x00, 0x0a, 0x03, 1, 2, 3, 0x32, 0x01, 9];
    assert_eq!(av1_sequence_header(&tu).unwrap(), [0x0a, 0x03, 1, 2, 3]);
    assert_eq!(av1_sequence_header(&[0x12, 0x00]), None);
    assert_eq!(av1_sequence_header(&[0x0a, 0x09, 1]), None, "truncated");
}

/// Damaged compressed video never crashes a decoder (FFmpeg's, through our FFI): every
/// decoder here is fed real H.264 access units cut short, with bits flipped, garbage and
/// nothing; each call gives a picture, nothing or an error. Then a clean keyframe still
/// decodes.
#[test]
fn damaged_access_units_never_crash_a_decoder() {
    use idn_open_protocol::codec::DecoderFactory;
    let ff = Ffmpeg::probe_device(None);
    let Some(b) = ff.cpu_h264() else {
        eprintln!("no CPU H.264 encoder in this FFmpeg");
        return;
    };
    let mut e = open_encoder(&None, b, &test_config(Codec::H264)).unwrap();
    let mut units = Vec::new();
    for i in 0..8 {
        units.extend(e.encode(&test_picture(), Fourcc::Shq2, i, i == 0).unwrap());
    }
    assert!(units.first().is_some_and(|u| u.keyframe));
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for codec in DecoderFactory::codecs(&ff) {
        let Ok(mut d) = DecoderFactory::open(&ff, codec) else {
            continue;
        };
        let mut pts = 0i64;
        let mut feed =
            |d: &mut Box<dyn idn_open_protocol::codec::VideoDecoder>, data: &[u8], key: bool| {
                pts += 1;
                let got = matches!(d.decode(data, key, pts), Ok(Some(_)));
                got || matches!(d.poll(), Ok(Some(_)))
            };
        for u in &units {
            let n = u.data.len();
            for cut in [0, 1, 4, n / 3, n / 2, n.saturating_sub(1)] {
                feed(&mut d, &u.data[..cut.min(n)], u.keyframe);
            }
            for _ in 0..8 {
                let mut x = u.data.clone();
                for _ in 0..1 + next() % 8 {
                    let i = (next() as usize) % x.len().max(1);
                    if let Some(b) = x.get_mut(i) {
                        *b ^= 1 << (next() % 8);
                    }
                }
                feed(&mut d, &x, u.keyframe);
            }
        }
        for _ in 0..50 {
            let n = (next() % 4096) as usize;
            let g: Vec<u8> = (0..n).map(|_| next() as u8).collect();
            feed(&mut d, &g, next() % 2 == 0);
        }
        if codec == Codec::H264 {
            // Recovered: the clean stream from its keyframe gives pictures again.
            let mut pictures = 0;
            for u in &units {
                pictures += feed(&mut d, &u.data, u.keyframe) as usize;
            }
            for _ in 0..10 {
                pictures += matches!(d.poll(), Ok(Some(_))) as usize;
            }
            assert!(
                pictures > 0,
                "{codec:?}: no picture from a clean stream after damage"
            );
        }
        eprintln!("{codec:?}: damaged access units survived");
    }
}

/// A file that isn't media (a text file with a video's name, an empty one, one cut off in
/// its header) is an error from the demuxer, not a crash.
#[test]
fn a_file_that_is_not_media_is_an_error() {
    let dir = idn_common::testing::TempDir::new("ff-notmedia");
    for (name, bytes) in [
        ("text.mp4", b"this is not a video".to_vec()),
        ("empty.mov", Vec::new()),
        ("cut.mkv", vec![0x1a, 0x45, 0xdf, 0xa3, 0x01, 0x00]),
    ] {
        let p = dir.join(name);
        std::fs::write(&p, &bytes).unwrap();
        let r = ffi::Demuxer::open(&p.to_string_lossy());
        if let Ok(d) = &r {
            // Opened (FFmpeg guesses generously): nothing to play in it, then.
            assert!(
                d.streams.iter().all(|s| !s.video),
                "{name}: a video stream in nothing"
            );
        }
    }
    assert!(ffi::Demuxer::open(&dir.join("missing.mp4").to_string_lossy()).is_err());
}

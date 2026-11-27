//! What arrives from the network, mutated: valid messages with bytes flipped, cut,
//! repeated and inserted, and plain noise, through every parser that reads them. None
//! may panic or run away. `IDN_FUZZ_ITERS` (default 300 a target) runs longer.

use idn_open_protocol::frame::{Frame, Limits as FrameLimits};
use idn_open_protocol::media::{AudioHeader, VideoHeader, fowt_decode, fowt_encode};
use idn_open_protocol::shq::encode::{EncodeInput, EncodeOptions, encode};
use idn_open_protocol::shq::{
    Fourcc, Limits, Plane, VideoParams, decode, decode_coefficients, decode_half, decode_precise,
    reconstruct, reconstruct_half,
};
use idn_open_protocol::transport::{tcp, udp};
use idn_open_protocol::{control, hx, regions, xml};

fn iters() -> usize {
    std::env::var("IDN_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
}

/// xorshift64*: the same runs every time.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
}

/// One mutation of `v`: flips, a cut, a repeat, an insertion, a run of 0x00/0xFF, or
/// noise of a similar length.
fn mutate(r: &mut Rng, v: &[u8]) -> Vec<u8> {
    let mut m = v.to_vec();
    match r.below(7) {
        0 => {
            for _ in 0..1 + r.below(8) {
                if !m.is_empty() {
                    let i = r.below(m.len());
                    m[i] ^= 1 << r.below(8);
                }
            }
        }
        1 => m.truncate(r.below(m.len() + 1)),
        2 => {
            if !m.is_empty() {
                let (a, b) = (r.below(m.len()), r.below(m.len()));
                let (a, b) = (a.min(b), a.max(b));
                let chunk = m[a..b].to_vec();
                let at = r.below(m.len());
                m.splice(at..at, chunk);
            }
        }
        3 => {
            let at = r.below(m.len() + 1);
            let n = 1 + r.below(16);
            let ins: Vec<u8> = (0..n).map(|_| r.byte()).collect();
            m.splice(at..at, ins);
        }
        4 => {
            if !m.is_empty() {
                let at = r.below(m.len());
                let n = (1 + r.below(32)).min(m.len() - at);
                let v = if r.below(2) == 0 { 0 } else { 0xff };
                m[at..at + n].fill(v);
            }
        }
        5 => {
            // A field's worth of bytes set to an extreme little-endian value.
            if m.len() >= 4 {
                let at = r.below(m.len() - 3);
                let v: u32 = [0, 1, 0x7fff_ffff, 0xffff_ffff, 0x8000_0000][r.below(5)];
                m[at..at + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
        _ => m = (0..r.below(v.len() * 2 + 16)).map(|_| r.byte()).collect(),
    }
    m
}

fn picture(fourcc: Fourcc, w: usize, h: usize) -> Vec<Plane> {
    let (cw, ch) = match fourcc {
        Fourcc::Shq0 => (w.div_ceil(2), h.div_ceil(2)),
        Fourcc::Shq2 | Fourcc::Shq7 => (w.div_ceil(2), h),
        Fourcc::Shq4 | Fourcc::Shq9 => (w, h),
    };
    let plane = |pw: usize, ph: usize, base: usize| Plane {
        width: pw,
        height: ph,
        data: (0..pw * ph)
            .map(|i| (16 + (i * 7 + base + i / pw * 3) % 220) as u8)
            .collect(),
    };
    let mut p = vec![plane(w, h, 0), plane(cw, ch, 40), plane(cw, ch, 90)];
    if fourcc.has_alpha() {
        p.push(plane(w, h, 5));
    }
    p
}

#[test]
fn speedhq_pictures_mutated_never_panic() {
    let mut r = Rng(0x5eed_0001);
    let limits = Limits::default();
    for (fourcc, code, w, h) in [
        (Fourcc::Shq2, *b"SHQ2", 64usize, 48usize),
        (Fourcc::Shq0, *b"SHQ0", 37, 21),
        (Fourcc::Shq4, *b"SHQ4", 33, 17),
        (Fourcc::Shq7, *b"SHQ7", 48, 32),
        (Fourcc::Shq9, *b"SHQ9", 16, 16),
    ] {
        let planes = picture(fourcc, w, h);
        let payload = encode(
            &EncodeInput {
                fourcc,
                width: w,
                height: h,
                planes: &planes,
                precise: None,
            },
            &EncodeOptions::default(),
        );
        let good = VideoParams {
            fourcc: code,
            width: w as u32,
            height: h as u32,
            field_id: 0,
        };
        decode(&good, &payload, &limits).expect("the unmutated picture decodes");
        for i in 0..iters() {
            let m = mutate(&mut r, &payload);
            // Sometimes the header lies about the picture too.
            let vp = match i % 5 {
                0 => VideoParams {
                    width: [0, 1, 7, 8192, 9000, u32::MAX][r.below(6)],
                    ..good
                },
                1 => VideoParams {
                    height: [0, 1, 7, 8192, 9000, u32::MAX][r.below(6)],
                    ..good
                },
                2 => VideoParams {
                    field_id: r.below(4) as u32,
                    ..good
                },
                _ => good,
            };
            let _ = decode(&vp, &m, &limits);
            let _ = decode_precise(&vp, &m, &limits);
            let _ = decode_half(&vp, &m, &limits);
            if let Ok(c) = decode_coefficients(&vp, &m, &limits) {
                let _ = reconstruct(&c);
                let _ = reconstruct_half(&c);
            }
        }
    }
}

#[test]
fn hx_packets_and_streams_mutated_never_panic() {
    let mut r = Rng(0x5eed_0002);
    let nal = |t: u8, body: &[u8]| [&[0u8, 0, 0, 1, t][..], body].concat();
    let stream = [
        nal(0x09, &[0x10]),
        nal(0x67, &[0x64, 0, 0x1f]),
        nal(0x68, &[0xeb]),
        nal(0x65, &[0x88, 1, 2]),
        nal(0x09, &[0x10]),
        nal(0x41, &[0x9a, 2]),
    ]
    .concat();
    let packet = hx::CompressedPacket {
        fourcc: hx::FOURCC_H264,
        pts: 1,
        dts: 1,
        counter: 1,
        flags: hx::FLAG_KEYFRAME,
        data: &stream,
        extra: &stream[5..17],
    }
    .encode();
    let adts = {
        let h = hx::adts_header(&[0x11, 0x90], 20).unwrap();
        [&h[..], &[7u8; 20]].concat().repeat(3)
    };
    for _ in 0..iters() * 4 {
        let m = mutate(&mut r, &packet);
        if let Ok(p) = hx::CompressedPacket::parse(&m) {
            let _ = p.decoder_input(false);
            let _ = p.decoder_input(true);
        }
        let s = mutate(&mut r, &stream);
        for hevc in [false, true] {
            let _ = hx::access_units(&s, hevc);
            let _ = hx::with_parameter_sets(&s, &stream[5..17], hevc);
        }
        let a = mutate(&mut r, &adts);
        let _ = hx::adts_frames(&a);
        let _ = hx::adts_header(&a, r.below(10_000));
    }
}

#[test]
fn region_updates_mutated_never_panic() {
    let mut r = Rng(0x5eed_0003);
    let rect = regions::Rect {
        x: 16,
        y: 16,
        w: 32,
        h: 22,
    };
    let p = regions::payload(*b"SHQ2", &[(rect, vec![1, 2, 3]), (rect, vec![])]);
    for _ in 0..iters() * 4 {
        let m = mutate(&mut r, &p);
        let (w, h) = (
            [0, 1, 48, 100, 65535][r.below(5)],
            [0, 1, 38, 70, 65535][r.below(5)],
        );
        if let Ok(u) = regions::Update::parse(&m, w, h) {
            for (rc, _) in u.regions {
                assert!(rc.x + rc.w <= w && rc.y + rc.h <= h);
            }
        }
    }
}

#[test]
fn xml_and_control_messages_mutated_never_panic() {
    let mut r = Rng(0x5eed_0004);
    let samples = [
        r#"<ndi_identify name="A &amp; B (x&#x41;)" other='1'/>"#,
        r#"<source><name>A &amp; B</name><groups><group>public</group><!-- c --><![CDATA[x]]></groups></source>"#,
        r#"<?xml version="1.0"?><x_idn_sync group="g" delay_ms="12.5" need_ms="inf"/>"#,
        r#"<query/><receiver/><advertiser/>"#,
    ];
    for _ in 0..iters() * 4 {
        let base = samples[r.below(samples.len())];
        let m = mutate(&mut r, base.as_bytes());
        let s = String::from_utf8_lossy(&m);
        let _ = xml::parse(&s);
        let _ = xml::parse_fragment(&s);
        let _ = control::parse_element(&s);
    }
    // Deep and wide: bounded, not a stack overflow or a long wait.
    let deep = "<a>".repeat(100_000);
    assert!(xml::parse(&deep).is_none());
    let wide = format!("<a>{}</a>", "<b/>".repeat(200_000));
    assert!(xml::parse(&wide).is_none());
}

#[test]
fn frames_and_media_headers_mutated_never_panic() {
    let mut r = Rng(0x5eed_0005);
    let vh = VideoHeader {
        fourcc: *b"SHQ2",
        width: 1920,
        height: 1080,
        frame_rate_n: 30000,
        frame_rate_d: 1001,
        aspect_ratio: 16.0 / 9.0,
        field_id: 0,
        stream: 1,
        timecode: 1,
        timestamp: 2,
        metadata: Some("<m/>".into()),
    }
    .encode();
    let audio: Vec<Vec<f32>> = (0..4)
        .map(|c| {
            (0..64)
                .map(|i| ((i * (c + 1)) as f32 * 0.01).sin())
                .collect()
        })
        .collect();
    let fowt = fowt_encode(&audio);
    let ah = AudioHeader {
        fourcc: AudioHeader::FOWT,
        samples: 64,
        channels: 4,
        sample_rate: 48000,
        timecode: 0,
        timestamp: 0,
        metadata: None,
    };
    let ahe = ah.encode();
    let frame = Frame::text(1, 0, r#"<ndi_identify name="x"/>"#).encode();
    let limits = FrameLimits::default();
    for _ in 0..iters() * 4 {
        let _ = VideoHeader::parse(&mutate(&mut r, &vh));
        if let Ok(h) = AudioHeader::parse(&mutate(&mut r, &ahe)) {
            let _ = fowt_decode(&h, &fowt);
        }
        let _ = fowt_decode(&ah, &mutate(&mut r, &fowt));
        let m = mutate(&mut r, &frame);
        if let Ok(Some((f, used))) = Frame::decode(&m, &limits) {
            assert!(used <= m.len());
            let _ = f.text_payload();
            let _ = f.timecode();
        }
    }
}

#[test]
fn udp_and_tcp_reassembly_of_mutated_packets_never_panics() {
    let mut r = Rng(0x5eed_0006);
    let limits = FrameLimits::default();
    let msgs: Vec<Vec<u8>> = (0..6)
        .map(|i| Frame::text(7, i, &"v".repeat(1 + 900 * i as usize)).encode())
        .collect();
    // UDP: the stream's packets, mutated, reordered, repeated, with others' ids.
    let mut seq = 0u16;
    let packets: Vec<Vec<u8>> = msgs
        .iter()
        .flat_map(|m| udp::packetize(m, 42, &mut seq))
        .collect();
    let mut out = Vec::new();
    for round in 0..iters() / 10 + 1 {
        let mut ra = udp::UdpReassembler::new(42, limits);
        for _ in 0..packets.len() * 3 {
            let p = &packets[r.below(packets.len())];
            let d = if r.below(3) == 0 {
                mutate(&mut r, p)
            } else {
                p.clone()
            };
            ra.push(&d, &mut out);
        }
        // A far jump forged by a few packets, then the stream again.
        if round % 2 == 0 {
            for _ in 0..r.below(5) {
                let mut d = packets[0].clone();
                let s = r.next() as u16;
                d[3..5].copy_from_slice(&s.to_le_bytes());
                ra.push(&d, &mut out);
            }
        }
        for p in &packets {
            ra.push(p, &mut out);
        }
        out.clear();
    }
    // TCP: chunks with mutated sequence numbers, flags and data.
    let mut seq = 0u32;
    let chunks: Vec<Vec<u8>> = msgs
        .iter()
        .flat_map(|m| tcp::chunkify(m, &mut seq))
        .collect();
    for _ in 0..iters() / 10 + 1 {
        let mut ra = tcp::TcpReassembler::new(limits);
        for _ in 0..chunks.len() * 3 {
            let i = r.below(chunks.len());
            let c = mutate(&mut r, &chunks[i]);
            if let Ok(Some(ch)) = tcp::read_chunk(&mut &c[..])
                && ra.push(ch, &mut out).is_err()
            {
                break; // bounds exceeded: the connection would close
            }
        }
        out.clear();
    }
}

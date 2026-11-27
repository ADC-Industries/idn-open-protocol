//! `idn-codec send`: a test card sent with an encoder.

use super::*;

pub(super) fn send(mut args: idn_open_protocol::cli::Args) {
    let mut no_node = false;
    let (mut name, mut port, mut size, mut fps, mut secs) = (
        "CODEC".to_owned(),
        0u16,
        (1920usize, 1080usize),
        30u32,
        60u64,
    );
    let (mut mbps, mut policy, mut mdns) = (8.0f64, None::<Encoding>, None);
    let (mut dests, mut own, mut ndi) = (Vec::new(), false, None);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--name" => name = args.value("--name"),
            "--port" => port = args.value("--port"),
            "--size" => {
                let v: String = args.value("--size");
                size = idn_common::text::parse_size(&v)
                    .map(|(w, h)| (w as _, h as _))
                    .unwrap_or_else(|| args.fail("bad --size"));
            }
            "--fps" => fps = args.value("--fps"),
            "--seconds" => secs = args.value("--seconds"),
            "--mbps" => mbps = args.value("--mbps"),
            "--force" => policy = Some(Encoding::Force),
            "--policy" => {
                let v: String = args.value("--policy");
                policy = Some(
                    Encoding::from_name(&v)
                        .unwrap_or_else(|| args.fail("--policy offer, auto, cpu or force")),
                );
            }
            "--no-mdns" => mdns = Some(false),
            "--no-node" => no_node = true,
            "--ndi" => {
                let v: String = args.value("--ndi");
                ndi = Some(
                    idn_open_protocol::codec::NdiVideo::from_name(&v)
                        .unwrap_or_else(|| args.fail("--ndi speedhq, h264 or hevc")),
                );
            }
            "--stream" => {
                let u: String = args.value("--stream");
                dests.push(if u.starts_with("srt://") {
                    Destination::Srt(u)
                } else {
                    Destination::Rtmp(u)
                });
            }
            "--record" => {
                let f: String = args.value("--record");
                let c = if f.ends_with(".mkv") {
                    Container::Mkv
                } else {
                    Container::Mp4
                };
                dests.push(Destination::File(f.into(), c));
            }
            "--own-encoder" => own = true,
            _ => args.help(),
        }
    }
    if fps == 0 || size.0 < 16 || size.1 < 16 {
        args.fail("--fps must be above 0 and --size at least 16x16");
    }
    // The machine's IDN Node: its site identity, and (without --policy) its encoding
    // setting, else automatic.
    let node = (!no_node)
        .then(|| idn_open_protocol::node::Node::connect("idn-codec").ok())
        .flatten();
    let encoding = policy
        .or(idn_open_protocol::node::video::machine().encoding)
        .unwrap_or(Encoding::Auto);
    eprintln!(
        "encoding: {}{}",
        encoding.name(),
        if policy.is_none() && idn_open_protocol::node::video::machine().encoding.is_some() {
            " (the machine's setting)"
        } else {
            ""
        }
    );
    let ff = Arc::new(Ffmpeg::probe());
    // As the machine's processor and format say too (§N.13), as captures do.
    let codec = idn_ffmpeg::output_codec(&ff, policy, None).map(|c| CodecOutput {
        mbps_1080p: mbps,
        ndi,
        ..c
    });
    eprintln!(
        "offering: {}",
        match &codec {
            Some(c) => {
                let offered: Vec<&str> = c.encoder.codecs().iter().map(|c| c.name()).collect();
                format!("{} ({})", offered.join(", "), c.policy.name())
            }
            None if encoding.policy().is_none() => {
                "nothing (SpeedHQ for every receiver)".to_owned()
            }
            None => "nothing (no encoder for what was chosen: SpeedHQ only)".to_owned(),
        }
    );
    let p = Publisher::new(PublisherConfig {
        mdns,
        secure: node.as_ref().map(|n| n.secure_config()),
        ..Default::default()
    })
    .unwrap_or_else(|e| args.fail(&format!("publisher: {e}")));
    let out = Output::new(
        &p,
        OutputConfig {
            port,
            frame_rate: (fps, 1),
            codec,
            ..OutputConfig::new(&name)
        },
    )
    .unwrap_or_else(|e| args.fail(&format!("output: {e}")));
    eprintln!("sending \"{}\" on port {}", name, out.port());
    let gw = Arc::new(Gateway::new(ff.clone()));
    for d in dests {
        let r = if own {
            gw.add_with(d.clone(), StreamSettings::default(), true)
        } else {
            gw.add(d.clone())
        };
        if let Err(e) = r {
            args.fail(&format!("{}: {e}", d.describe()));
        }
    }
    let _tap = gw.attach(&out);
    let (w, h) = size;
    // A detailed still (photo-like grain) with a moving bar: the usual slide-with-motion.
    let mut base = vec![0u8; w * h * 4];
    let mut seed = 0x9e37_79b9u32;
    for (i, px) in base.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let (x, y) = (i % w, i / w);
        let v = (30 + x * 120 / w + y * 40 / h) as u32 + seed % 40;
        px.copy_from_slice(&[v as u8, (v / 2 + 50) as u8, (220 - v.min(200)) as u8, 255]);
    }
    let start = Instant::now();
    let mut n = 0u64;
    while start.elapsed() < Duration::from_secs(secs) {
        let mut px = base.clone();
        let bar = (n as usize * 12) % w;
        for y in 0..h {
            for x in bar..(bar + 48).min(w) {
                px[(y * w + x) * 4..(y * w + x) * 4 + 3].copy_from_slice(&[235, 235, 235]);
            }
        }
        let mut f = VideoFrame::rgba(w, h, px.into(), AlphaMode::Opaque);
        f.timestamp = Some(now_100ns());
        let _ = out.send(f);
        n += 1;
        if n.is_multiple_of(fps as u64) {
            let gws: Vec<String> = gw
                .status()
                .iter()
                .map(|s| format!("{} {:?} {:.1} MB", s.target, s.state, s.bytes as f64 / 1e6))
                .collect();
            eprint!(
                "\r{n} frames, {} receiver(s) {}   ",
                out.receivers(),
                gws.join("; ")
            );
        }
        let due = start + Duration::from_secs_f64(n as f64 / fps as f64);
        std::thread::sleep(due.saturating_duration_since(Instant::now()));
    }
    eprintln!();
    // Recordings get their trailer before the process ends.
    drop(_tap);
    gw.close(Duration::from_secs(5));
}

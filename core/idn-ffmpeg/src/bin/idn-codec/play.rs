//! `idn-codec play`: a file sent as a source.

use super::*;

pub(super) fn play(mut args: idn_open_protocol::cli::Args) {
    let Some(file) = args.next() else { args.help() };
    let (mut name, mut port, mut secs, mut mdns) = ("FILE".to_owned(), 0u16, u64::MAX, None);
    let mut cfg = PlayerConfig::default();
    let mut no_node = false;
    let mut dests = Vec::new();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--name" => name = args.value("--name"),
            "--port" => port = args.value("--port"),
            "--seconds" => secs = args.value("--seconds"),
            "--loop" => cfg.looped = true,
            // A recording or stream of what the player sends (a tap: it needs the
            // pictures, so the player makes them whatever its --idle).
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
            "--decoder" => {
                cfg.decoder = Some(match args.value::<String>("--decoder").as_str() {
                    "auto" => idn_ffmpeg::DecoderChoice::Auto,
                    "gpu" => idn_ffmpeg::DecoderChoice::Gpu,
                    "software" => idn_ffmpeg::DecoderChoice::Software,
                    _ => args.fail("--decoder auto, gpu or software"),
                });
            }
            "--no-pass-through" => cfg.pass_through = false,
            "--speedhq" => cfg.compressed = false,
            "--idle" => {
                use idn_ffmpeg::player::IdleDecode;
                let v: String = args.value("--idle");
                cfg.idle = match v.as_str() {
                    "warm" => IdleDecode::Warm,
                    "always" => IdleDecode::Always,
                    "stop" => IdleDecode::Stop,
                    _ => match v
                        .strip_prefix("stop-after:")
                        .and_then(|n| n.parse::<f64>().ok())
                        .and_then(|n| Duration::try_from_secs_f64(n.max(0.0)).ok())
                    {
                        Some(d) => IdleDecode::StopAfter(d),
                        None => args.fail("--idle warm, always, stop or stop-after:SECS"),
                    },
                };
            }
            "--no-mdns" => mdns = Some(false),
            "--no-node" => no_node = true,
            _ => args.help(),
        }
    }
    // The machine's IDN Node: its site identity, and (without --decoder) its decoding
    // setting, heard on connecting.
    let node = (!no_node)
        .then(|| idn_open_protocol::node::Node::connect("idn-codec").ok())
        .flatten();
    let ff = Arc::new(Ffmpeg::probe());
    let p = Publisher::new(PublisherConfig {
        mdns,
        secure: node.as_ref().map(|n| n.secure_config()),
        ..Default::default()
    })
    .unwrap_or_else(|e| args.fail(&format!("publisher: {e}")));
    let player = FilePlayer::open(
        &p,
        OutputConfig {
            port,
            ..OutputConfig::new(&name)
        },
        &file,
        ff.clone(),
        cfg,
    )
    .unwrap_or_else(|e| {
        // The file, not the command line: its error alone, not the usage after it.
        eprintln!("idn-codec: {e}"); // (the error names the file)
        std::process::exit(1)
    });
    let gw = Arc::new(Gateway::new(ff));
    for d in dests {
        if let Err(e) = gw.add(d.clone()) {
            eprintln!("idn-codec: {}: {e}", d.describe());
            std::process::exit(1);
        }
    }
    let _tap = (!gw.status().is_empty()).then(|| gw.attach(player.output()));
    let st = player.status();
    eprintln!(
        "playing {file} as \"{name}\" on port {}: {}, pass-through: {:?}",
        player.output().port(),
        st.video.as_deref().unwrap_or("no video"),
        st.pass_through
    );
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_secs(1));
        let st = player.status();
        let os = player.output().stats();
        eprint!(
            "\r{:?} {:.1} s, {} receiver(s), codec/1 pictures passed {} encoded {}, decoder {} {:.1} ms, shifted {:.0} ms, replaced {}   ",
            st.state,
            st.position.as_secs_f64(),
            player.output().receivers(),
            os.codec_passed,
            os.codec_encoded,
            st.decoder.as_deref().unwrap_or("-"),
            st.decode_ms,
            st.shifted.as_secs_f64() * 1e3,
            // Pictures the output had no time for (replaced by the next one).
            os.replaced
        );
        if st.state == PlayerState::Ended || matches!(st.state, PlayerState::Failed(_)) {
            break;
        }
    }
    eprintln!();
}

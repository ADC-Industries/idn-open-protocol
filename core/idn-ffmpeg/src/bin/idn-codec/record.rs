//! `idn-codec record`: a source recorded to a file.

use super::*;

/// `record SOURCE FILE`: one source to a file (N6, idn-display.md §D.6), its pictures and
/// sound as they arrive, through the same recorder as an output's (`gateway`): H.264 on
/// the GPU where there is one, AAC. Until `--seconds`, Ctrl+C, or (`--status`) its
/// standard input closing; `--status` prints a `status {json}` line on stderr each second.
pub(super) fn record(mut args: idn_open_protocol::cli::Args) {
    use idn_open_protocol::api::TappedFrame;
    let (mut source, mut file, mut secs, mut status, mut no_node) =
        (None::<String>, None::<String>, 0u64, false, false);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--seconds" => secs = args.value("--seconds"),
            "--status" => status = true,
            "--no-node" => no_node = true,
            s if s.starts_with("--") => args.fail(&format!("unexpected argument: {s}")),
            s if source.is_none() => source = Some(s.to_owned()),
            s if file.is_none() => file = Some(s.to_owned()),
            _ => args.help(),
        }
    }
    let (Some(source), Some(file)) = (source, file) else {
        args.help()
    };
    let container = if file.to_lowercase().ends_with(".mkv") {
        Container::Mkv
    } else {
        Container::Mp4
    };
    let node = (!no_node)
        .then(|| idn_open_protocol::node::Node::connect("idn-codec").ok())
        .flatten();
    let ff = Arc::new(Ffmpeg::probe());
    // NDI|HX and IDN compressed video decoded here too (the recorder takes pictures).
    let input = Input::open(InputConfig {
        codec: Some(CodecInput::new(ff.clone(), CodecUse::Allow)),
        secure: node.as_ref().map(|n| n.secure_config()),
        receiver_name: "Recorder".into(),
        ..InputConfig::new(&source)
    })
    .unwrap_or_else(|e| args.fail(&format!("input: {e}")));
    let gw = Arc::new(Gateway::new(ff));
    gw.add(Destination::File(file.clone().into(), container))
        .unwrap_or_else(|e| args.fail(&format!("{file}: {e}")));
    // Stopped by the program that started it, however that one ends.
    let parent_gone = Arc::new(std::sync::atomic::AtomicBool::new(false));
    if status {
        let gone = parent_gone.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            let mut stdin = std::io::stdin();
            while std::io::Read::read(&mut stdin, &mut buf).is_ok_and(|n| n > 0) {}
            gone.store(true, std::sync::atomic::Ordering::Relaxed);
        });
    }
    // Stopped (Ctrl+C, the service ending): the file is finished before the process ends.
    let _closing = {
        let gw = gw.clone();
        idn_open_protocol::shutdown::register(move || gw.close(Duration::from_secs(10)))
    };
    println!("recording {source:?} to {file}");
    let start = Instant::now();
    let (mut pictures, mut rate) = (0u64, (30u32, 1u32));
    let mut next_status = Instant::now();
    let mut checked = Instant::now();
    loop {
        if parent_gone.load(std::sync::atomic::Ordering::Relaxed)
            || (secs > 0 && start.elapsed() >= Duration::from_secs(secs))
        {
            break;
        }
        // The file failed (couldn't be made, the disk full): said and ended, so whoever
        // started this sees it stop (a monitor showed "recording" over a lost file; bug
        // round E, E8-4).
        if checked.elapsed() >= Duration::from_secs(1) {
            checked = Instant::now();
            let st = gw.status();
            if !st.is_empty()
                && st
                    .iter()
                    .all(|d| matches!(d.state, idn_ffmpeg::gateway::State::Failed(_)))
            {
                let why = st
                    .iter()
                    .find_map(|d| match &d.state {
                        idn_ffmpeg::gateway::State::Failed(e) => Some(e.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                eprintln!("idn codec record: the recording failed: {why}");
                std::process::exit(3);
            }
        }
        match input.recv(Duration::from_millis(200)) {
            Some(InputEvent::Video(v)) => {
                let h = &v.header;
                if h.frame_rate_n > 0 && h.frame_rate_d > 0 {
                    rate = (h.frame_rate_n, h.frame_rate_d);
                }
                let p = v.picture;
                gw.send(&TappedFrame::picture(
                    p.fourcc,
                    p.width,
                    p.height,
                    Arc::new(p.planes),
                    rate,
                    h.timecode,
                    h.timestamp,
                ));
                pictures += 1;
            }
            Some(InputEvent::Audio(a)) => {
                gw.send(&TappedFrame::sound(
                    a.channels,
                    a.header.sample_rate,
                    rate,
                    a.header.timecode,
                    a.header.timestamp,
                ));
            }
            _ => {}
        }
        if status && Instant::now() >= next_status {
            next_status += Duration::from_secs(1);
            let st = gw.status();
            let d = st.first();
            eprintln!(
                "status {}",
                serde_json::json!({
                    "file": file,
                    "seconds": start.elapsed().as_secs(),
                    "pictures": pictures,
                    "bytes": d.map_or(0, |d| d.bytes),
                    "state": d.map(|d| format!("{:?}", d.state)),
                })
            );
        }
    }
    gw.close(Duration::from_secs(10));
    let bytes = std::fs::metadata(&file).map_or(0, |m| m.len());
    println!(
        "{file}: {pictures} pictures, {:.1} MB in {:.0} s",
        bytes as f64 / 1e6,
        start.elapsed().as_secs_f64()
    );
}

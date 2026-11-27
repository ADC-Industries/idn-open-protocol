//! `idn-codec capture`: a screen or webcam sent as a source; the displays, devices and encoders here.

use super::*;

/// `capture`: a screen or webcam as a source (R.19). Ends with status 4 when the device
/// goes away, for whoever runs it (the session agent: `idn-session`, `idn session`) to
/// start it again.
pub(super) fn capture(mut args: idn_open_protocol::cli::Args) {
    use idn_ffmpeg::capture::{Capture, CaptureConfig, Source};
    let (mut src, mut name, mut port, mut secs, mut mdns, mut no_node) =
        (None::<Source>, None::<String>, 0u16, u64::MAX, None, false);
    let mut cfg = CaptureConfig::default();
    let mut control = false;
    let mut sound = false;
    let mut fps_given = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            // The encoders that work here, by processor (the Node's panel offers their
            // formats): `gpu|cpu TAB codec TAB by`.
            "encoders" => {
                let opt = args.next();
                if opt.as_deref() == Some("--bench") {
                    #[cfg(feature = "gpu")]
                    bench_gpu_encoders();
                    #[cfg(not(feature = "gpu"))]
                    eprintln!(
                        "idn capture encoders --bench: this idn-codec was built without the GPU"
                    );
                    return;
                }
                let tsv = opt.as_deref() == Some("--tsv");
                let ff = Ffmpeg::probe();
                let all = ff
                    .encoders()
                    .iter()
                    .map(|e| ("gpu", e))
                    .chain(ff.cpu_encoders().iter().map(|e| ("cpu", e)));
                for (p, (c, b)) in all {
                    if tsv {
                        println!("{p}\t{}\t{}", c.name(), b.label());
                    } else {
                        println!("{p}: {} ({})", c.name(), b.label());
                    }
                }
                if !tsv {
                    for (c, b) in ff.gpu_alternatives() {
                        println!("gpu, also: {} ({})", c.name(), b.label());
                    }
                }
                return;
            }
            // This computer's displays with their identity (the Node's panel offers them).
            "screens" => {
                let tsv = args.next().as_deref() == Some("--tsv");
                for d in idn_ffmpeg::screens::displays() {
                    if tsv {
                        println!(
                            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                            d.id,
                            d.name,
                            d.width,
                            d.height,
                            d.hz.unwrap_or(0),
                            u8::from(d.primary),
                            d.position,
                            d.connector
                        );
                    } else {
                        println!(
                            "  {:<20} {}{}: {} (screen {}, {})",
                            d.id,
                            d.name,
                            if d.primary { " (main)" } else { "" },
                            idn_ffmpeg::screens::mode_text(&d),
                            d.position,
                            d.connector
                        );
                    }
                }
                return;
            }
            "devices" => {
                match idn_ffmpeg::capture::devices() {
                    Ok(d) => {
                        println!(
                            "webcams{}:",
                            if d.webcams.is_empty() {
                                " (none found)"
                            } else {
                                ""
                            }
                        );
                        for (n, desc) in d.webcams {
                            println!(
                                "  {n}{}",
                                if desc.is_empty() {
                                    String::new()
                                } else {
                                    format!("   ({desc})")
                                }
                            );
                        }
                        println!(
                            "screens{}:",
                            if d.screens.is_empty() {
                                " (none: no desktop session here)"
                            } else {
                                ""
                            }
                        );
                        for (n, desc) in d.screens {
                            println!("  {n}   ({desc})");
                        }
                    }
                    Err(e) => args.fail(&e.to_string()),
                }
                return;
            }
            "--screen" => src = Some(Source::Screen(args.value("--screen"))),
            "--webcam" => src = Some(Source::Webcam(args.value("--webcam"))),
            "--name" => name = Some(args.value("--name")),
            "--fps" => {
                cfg.fps = args.value("--fps");
                fps_given = true;
            }
            "--game" => cfg.game = true,
            "--size" => {
                let v: String = args.value("--size");
                cfg.size = idn_common::text::parse_size(&v).map(|(w, h)| (w as _, h as _));
                if cfg.size.is_none() {
                    args.fail("--size WxH");
                }
            }
            "--no-pointer" => cfg.pointer = false,
            // The site's receivers may use the screen's keyboard and mouse (R.25).
            "--control" => {
                control = true;
                cfg.control = true;
            }
            // This computer's own sound on the screen's source (a remote session hears
            // the computer it controls).
            "--sound" => sound = true,
            "--encoding" => {
                let e: String = args.value("--encoding");
                cfg.encoding = Some(
                    Encoding::from_name(&e)
                        .unwrap_or_else(|| args.fail("--encoding auto|offer|cpu|force")),
                );
            }
            "--format" => {
                let f: String = args.value("--format");
                cfg.format = Some(
                    idn_open_protocol::node::video::Format::from_name(&f)
                        .unwrap_or_else(|| args.fail("--format auto|speedhq|h264|hevc|av1")),
                );
            }
            "--port" => port = args.value("--port"),
            "--seconds" => secs = args.value("--seconds"),
            "--no-mdns" => mdns = Some(false),
            "--no-node" => no_node = true,
            _ => args.help(),
        }
    }
    let src = src.unwrap_or_else(|| {
        args.fail("--screen N or --webcam NAME (idn capture devices lists them)")
    });
    let name = name.unwrap_or_else(|| match &src {
        Source::Screen(n) => format!("Screen {n}"),
        Source::Webcam(n) => n.trim_start_matches("/dev/").to_owned(),
    });
    if sound && !matches!(src, Source::Screen(_)) {
        args.fail("--sound is for a screen");
    }
    // The sound first, before any thread of this program starts (on Linux it aims the
    // sound server's device through the environment while it opens, then takes that
    // back): its blocks go to the source once it is there.
    let to: Arc<std::sync::OnceLock<Arc<idn_open_protocol::api::Output>>> = Arc::default();
    let _sound = sound.then(|| desktop_sound(to.clone())).flatten();
    let node = (!no_node)
        .then(|| idn_open_protocol::node::Node::connect("idn capture").ok())
        .flatten();
    let ff = Arc::new(Ffmpeg::probe());
    let p = Publisher::new(PublisherConfig {
        mdns,
        secure: node.as_ref().map(|n| n.secure_config()),
        ..Default::default()
    })
    .unwrap_or_else(|e| args.fail(&format!("publisher: {e}")));
    if control && !matches!(src, Source::Screen(_)) {
        args.fail("--control is for a screen");
    }
    if cfg.game && !fps_given {
        cfg.fps = 120;
    }
    let mut cap = Capture::start(
        &p,
        OutputConfig {
            port,
            remote_control: control,
            ..OutputConfig::new(&name)
        },
        &src,
        cfg.clone(),
        ff,
    )
    .unwrap_or_else(|e| {
        eprintln!("idn capture: {e}");
        std::process::exit(4)
    });
    let _ = to.set(cap.output().clone());
    eprintln!(
        "capturing {} as \"{}\" on port {}",
        cap.what,
        cap.output().name(),
        cap.output().port()
    );
    // Control (R.25): the site's receivers' keyboard and mouse, onto this screen.
    let mut portal_pointer = false;
    let mut control = control.then(|| {
        // A Wayland desktop's own (the portal, with the screen): its pointer is placed in
        // the shared screen's picture. Else this machine's injector, onto the screen's
        // area of the desktop.
        let (inj, area) = match cap.take_injector() {
            Some(i) => {
                portal_pointer = true;
                (i, (0.0, 0.0, 1.0, 1.0))
            }
            None => {
                // The display the capture shows, found the same way (its id, or its
                // position in the same list): the pointer lands on the screen sent.
                let area = match &src {
                    // The screen the desktop's screen share gave (Wayland without a
                    // remote-control portal): where it says it is.
                    _ if cap.shared_area().is_some() => {
                        let (x, y, w, h) = cap.shared_area().unwrap_or_default();
                        idn_input::control::display_area(x, y, w, h)
                    }
                    Source::Screen(w) => match idn_ffmpeg::screens::resolve(w) {
                        Some(d) => idn_input::control::display_area(d.x, d.y, d.width, d.height),
                        None => idn_input::control::screen_area(
                            w.parse::<usize>().map_or(0, |n| n.saturating_sub(1)),
                        ),
                    },
                    Source::Webcam(_) => idn_input::control::screen_area(0),
                };
                (idn_input::injector(), area)
            }
        };
        eprintln!("keyboard and mouse: the site's receivers may use this screen");
        (
            inj,
            idn_input::control::ControlMapper::new(area),
            (0u64, 0usize),
        )
    });
    // (The pointer's area from the screen share's own report: followed below.)
    let follows_share = control.is_some() && cap.shared_area().is_some() && !portal_pointer;
    let start = Instant::now();
    let mut said = Instant::now();
    // For games, the input's way at real-time priority too.
    if cfg.game && control.is_some() {
        idn_common::rt::audio_this_thread();
    }
    // Latency probes (R.25 stage 2): answered after the picture sent next.
    let mut probes: Vec<(u32, u64)> = Vec::new();
    let mut pads_said = false;
    let mut sound_said = (0u64, 0u64);
    // What viewers hold down here, let go of when one of them goes (E3-1).
    let mut held = idn_input::Held::default();
    let mut viewers = cap.output().receivers();
    while start.elapsed() < Duration::from_secs(secs) {
        if control.is_none() {
            std::thread::sleep(Duration::from_secs(1));
        }
        if let Some((inj, map, (used, refused))) = control.as_mut() {
            // Each event as it comes (it used to wait up to 10 ms for the next look).
            let (events, r) = cap.output().wait_control(Duration::from_millis(5));
            for e in &events {
                if let idn_open_protocol::remote::Control::Probe(id) = e
                    && probes.len() < 64
                {
                    probes.push((*id, cap.sent()));
                }
            }
            let now_sent = cap.sent();
            probes.retain(|&(id, at)| {
                let due = now_sent > at;
                if due {
                    let _ = cap
                        .output()
                        .send_metadata(&idn_open_protocol::remote::probe_answer(id));
                }
                !due
            });
            if r > 0 && *refused == 0 {
                eprintln!("\nidn capture: control from outside the site, left out");
            }
            *refused += r;
            for e in events.iter().filter_map(|c| map.map(c)) {
                // Ctrl+Alt+Del on Windows: only a service may raise it, so the Node does
                // (it runs as one); asked here, the injector tries itself if it can't.
                if cfg!(windows) && e == idn_input::Event::Cad {
                    let by_node = idn_open_protocol::node::Node::connect("idn capture")
                        .and_then(|n| n.request(idn_open_protocol::xml::Element::new("sas")));
                    match by_node {
                        Ok(_) => {
                            *used += 1;
                            continue;
                        }
                        Err(err) => {
                            eprintln!("\nidn capture: Ctrl+Alt+Del through the Node: {err}")
                        }
                    }
                }
                if let Err(err) = inj.inject(&e) {
                    // (Gamepads where they can't be made: said once.)
                    // (No input here at all, e.g. no uinput device: said once too.)
                    let pad = matches!(e, idn_input::Event::Pad { .. })
                        || err.kind() == std::io::ErrorKind::Unsupported;
                    if !pad || !std::mem::replace(&mut pads_said, true) {
                        eprintln!("\nidn capture: input: {err}");
                    }
                } else {
                    held.note(&e);
                }
                *used += 1;
            }
            // A viewer gone (its connection dropped, it crashed): what was held let go.
            let now_viewers = cap.output().receivers();
            if now_viewers < viewers && !held.is_empty() {
                for e in held.release() {
                    let _ = inj.inject(&e);
                }
            }
            viewers = now_viewers;
        }
        if let Some(why) = cap.ended() {
            eprintln!("\nidn capture: {why}");
            if let Some((inj, _, _)) = control.as_mut() {
                for e in held.release() {
                    let _ = inj.inject(&e);
                }
            }
            std::process::exit(4);
        }
        if said.elapsed() >= Duration::from_secs(1) {
            said = Instant::now();
            // The shared screen where it is now (a mode, scale or layout change, a turned
            // monitor): the pointer follows it.
            if follows_share
                && let Some((_, mapper, _)) = control.as_mut()
                && let Some((x, y, w, h)) = cap.shared_area()
            {
                mapper.area = idn_input::control::display_area(x, y, w, h);
            }
            // The sound's losses, said as they happen (they were only counted).
            if let Some(s) = _sound.as_ref() {
                let now = sound_losses(s);
                if now != sound_said {
                    eprintln!(
                        "\nsound: {} device errors, {} resyncs (sound lost or held back)",
                        now.0, now.1
                    );
                    sound_said = now;
                }
            }
            let ctl = control.as_ref().map_or(String::new(), |(_, _, (u, r))| {
                format!(
                    ", {u} input events{}",
                    if *r > 0 {
                        format!(" ({r} refused)")
                    } else {
                        String::new()
                    }
                )
            });
            eprint!(
                "\r{:.0} s: {} pictures sent, {} receiver(s){ctl}   ",
                start.elapsed().as_secs_f64(),
                cap.sent(),
                cap.output().receivers()
            );
        }
    }
    eprintln!();
}

/// This computer's own sound (Windows: WASAPI loopback; Linux: the default output's
/// monitor), each block to the source in `to` once it is there. `None` (said) when it
/// can't start: the screen goes on without it.
#[cfg(feature = "sound")]
fn desktop_sound(
    to: Arc<std::sync::OnceLock<Arc<idn_open_protocol::api::Output>>>,
) -> Option<idn_audio::Capture> {
    let cfg = idn_audio::CaptureConfig {
        loopback: true,
        channels: Some(2),
        ..Default::default()
    };
    match idn_audio::Capture::start(&cfg, move |block, tc| {
        if let Some(o) = to.get() {
            let _ = o.send_audio(block, Some(tc));
        }
    }) {
        Ok(c) => {
            eprintln!("sound: this computer's ({})", c.stats().device);
            Some(c)
        }
        Err(e) => {
            eprintln!("idn capture: no sound with the screen: {e}");
            None
        }
    }
}

/// The sound's device errors and resyncs so far.
#[cfg(feature = "sound")]
fn sound_losses(c: &idn_audio::Capture) -> (u64, u64) {
    let s = c.stats();
    (s.errors, s.resyncs)
}

#[cfg(not(feature = "sound"))]
fn sound_losses(_: &()) -> (u64, u64) {
    (0, 0)
}

#[cfg(not(feature = "sound"))]
fn desktop_sound(_: Arc<std::sync::OnceLock<Arc<idn_open_protocol::api::Output>>>) -> Option<()> {
    eprintln!("idn capture: no sound with the screen: this idn-codec was built without it");
    None
}

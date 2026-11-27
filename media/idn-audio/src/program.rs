//! Sound cards and IDN: `idn-audio devices` lists them; `idn-audio send` makes a sound
//! card's input a source; `idn-audio recv` plays a source on a sound card. Dante Virtual
//! Soundcard appears as a sound card, so Dante works through it.

use crate::{Capture, CaptureConfig, Playback, PlaybackConfig};
use idn_common::sync::LockOk as _;
use idn_open_protocol::api::{Output, OutputConfig, Publisher, PublisherConfig};
use idn_open_protocol::input::{Input, InputConfig, InputEvent};
use std::sync::Arc;
use std::time::{Duration, Instant};

const USAGE: &str = "\
usage: idn-audio devices
       idn-audio send --name NAME [--device DEV | --loopback | --app PROGRAM] [--channels N] [--labels \"A,B,…\"] [--rate HZ]
                      [--block-ms MS | --low-latency | --ultra-low] [--device-buffer FRAMES]
                      [--port PORT] [--server HOST[:PORT]] [--no-mdns] [--no-correction]
                      [--seconds N] [--passphrase TEXT | env IDN_PASSPHRASE] [--require-encryption]
                      [--trust-dir DIR [--admin-code CODE]] [--no-node]
       idn-audio recv SOURCE [--device DEV] [--latency MS | --low-latency | --ultra-low] [--device-buffer FRAMES]
                      [--server HOST[:PORT]] [--no-correction]
                      [--baseline-audio] [--seconds N] [--passphrase TEXT | env IDN_PASSPHRASE]
                      [--require-encryption] [--trust-dir DIR [--admin-code CODE]] [--no-node]
       idn-audio recv --routable NAME [--device DEV] [--channels N] [--latency MS | --low-latency | --ultra-low]
                      [--device-buffer FRAMES]
                      [--baseline-audio] [--seconds N]
       idn-audio mic [--name NAME] [--device DEV] [--latency MS] [--seconds N]
       idn-audio speaker [--name NAME] [--channels N] [send's options]
       idn-audio apps [--tsv]
       idn-audio aes67 list [--seconds N (35)] [--interface IP] [--tsv]
       idn-audio aes67 recv NAME|SDP-FILE [--name NAME] [--jitter-ms MS (4)] [--low-latency] [--interface IP]
                      [--seconds N] [--no-node]
       idn-audio aes67 send SOURCE --name NAME [--rate 44100|48000|96000 (the machine's)] [--channels N (2)] [--address 239.69.X.Y] [--port P (5004)]
                      [--ttl T (32)] [--latency MS (20)] [--interface IP] [--sdp-file FILE] [--no-sap]
                      [--seconds N] [--no-node]

  apps      lists the programs with sound open now (send --app takes their name), the
            playing ones first; --tsv: executable, name and 1/0 for playing, tab-separated.
  devices   lists the sound cards with their default rate, channels and format, and
            the audio system's identifier in brackets where names repeat (ALSA): --device
            takes a name or an identifier, exact first, else the first containing it.
  send      captures DEV (default: the system's capture device) and sends it as the
            source NAME, 48 kHz unless --rate says otherwise; the device's clock drift
            against this machine's is corrected by resampling (--no-correction: as is).
  recv      receives SOURCE (\"HOST (name)\" or host:port) and plays its sound on DEV
            (default: the system's playback device), holding --latency ms (default 60)
            against network timing; the source's clock drift against the device's is
            corrected the same way.
  recv --routable  offers DEV's outputs (or the first N) as the routable receiver NAME on
            this machine's IDN Node, and plays whatever is routed to it: from the IDN Site
            Manager's routing matrix, or `idn-node route`. Each source plays through its
            own buffer and drift correction; the routes are kept by the Node.
  --loopback  send: capture what DEV (a playback device; default the system's) plays, so
            this computer's own sound (browser, Teams, a media player) becomes an IDN
            source with no virtual sound card (Windows).
  --app     send: one program's sound only (its .exe name, e.g. spotify, or its process
            id; with its child processes), Windows 10 2004 and later; on Linux every sound
            stream of the program by its name (firefox, chrome; Flatpak/Snap ids too),
            through PipeWire or PulseAudio, the program still playing on its speakers.
            While the program isn't playing it waits. --name defaults to the program's
            name.
  mic       IDN as a microphone for ordinary programs (Teams, Zoom, a browser): offers
            the routable receiver NAME (default \"IDN Microphone\") on the IDN Node and
            plays what is routed to it into a virtual cable (IDN's virtual sound card on
            Windows 11, the Virtual Audio Driver: idn node driver install audio; VB-Audio's
            free VB-Cable, VoiceMeeter, Virtual Audio Cable); programs record from the
            cable's other side (\"Virtual Mic Driver\", \"CABLE Output\"). DEV picks the cable
            when there are several; default 30 ms.
            On Linux it needs no cable: it makes the microphone NAME in the desktop's
            sound server (PipeWire or PulseAudio) while it runs.
  speaker   (Linux) a speaker NAME (default \"IDN Speaker\") in the desktop's sound
            server that programs play into, sent as the IDN source NAME while it runs.
  --labels  names the channels in the routing matrix.
  --low-latency  send: 2 ms blocks (default 10); recv: a 10 ms buffer (default 60). Use it
            on a wired network for monitoring and live sound; the CPU cost rises (about
            10% of a core per stream at 2 ms blocks). Explicit --block-ms / --latency win.
  --ultra-low  send: 1 ms blocks; recv: a 2 ms buffer and a 128-frame device buffer (2.7 ms
            at 48 kHz), real-time threads where the system allows. A wired network, a quiet
            machine and a sound card that keeps up with small buffers; the site clock (PTP)
            keeps the buffer steady. The Site Manager can set any routable receiver's
            latency (idn-routing.md §A.12); start one with --ultra-low for its device
            buffer to be small too. --device-buffer sets the device's buffer in frames
            (32, 64, 128, 256, 512, 1024 …; send's default: the power of two nearest
            its block, 1024 for 20 ms blocks at 48 kHz; the blocks sent are unchanged).
  aes67     AES67 equipment (stage boxes, consoles, Dante in its AES67 mode): `list` the
            streams announced on the network (SAP); `recv` one, by its name or a file
            with its SDP, as the IDN source NAME (default the stream's name); `send` an IDN
            source as an AES67 stream (L24 at the machine's rate, 48-frame packets, announced with SAP),
            timed by the site clock (PTP) when this machine follows one. --ttl 0 keeps a
            stream on this machine (tests). list --tsv: one stream a line, its name,
            group:port, channels, encoding, rate and sender separated by tabs (for
            programs: the IDN Node's panel).
  Every 10 s a line reports the counters and the correction in parts per million.
  IDN_AUDIO_HOST=jack|asio picks another audio host when built with that feature.";

pub fn main() {
    // The library's messages (a sound card's errors among them) on standard error, which
    // the log file copies (dropped, an error count climbed with no reason given).
    idn_open_protocol::log::set_logger(|m| eprintln!("{m}"));
    // Its log file and crash reports (§N.16).
    idn_open_protocol::diag::init("idn-audio");
    let mut args = idn_open_protocol::cli::Args::new(USAGE);
    match args.next().as_deref() {
        Some("devices") => devices(),
        Some("send") => send(args),
        Some("recv") => recv(args),
        Some("mic") => mic(args),
        Some("speaker") => speaker(args),
        Some("aes67") => aes67(args),
        Some("apps") => apps(args),
        Some("--help" | "-h") => args.help(),
        _ => args.fail("missing or unknown command"),
    }
}

/// `idn audio apps [--tsv]`: the programs with sound open now, the playing ones first
/// (the IDN Node's panel offers them for "Programs' sound").
fn apps(mut args: idn_open_protocol::cli::Args) {
    let tsv = args.next().as_deref() == Some("--tsv");
    let list = crate::programs_with_sound().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1)
    });
    for (exe, name, playing) in list {
        if tsv {
            println!("{exe}\t{name}\t{}", u8::from(playing));
        } else {
            println!(
                "{name:<30} {exe}{}",
                if playing { "  (playing)" } else { "" }
            );
        }
    }
}

fn devices() {
    let list = crate::devices().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1)
    });
    if list.is_empty() {
        println!("no sound cards found");
    }
    for d in list {
        let mark = |b: bool| if b { " (default)" } else { "" };
        if d.id.is_empty() || d.id == d.name {
            println!("{}", d.name);
        } else {
            println!("{}  [{}]", d.name, d.id);
        }
        if let Some(i) = &d.input {
            println!(
                "  capture:  {} ch, {} Hz, {}{}",
                i.channels,
                i.sample_rate,
                i.sample_format,
                mark(d.default_input)
            );
        }
        if let Some(o) = &d.output {
            println!(
                "  playback: {} ch, {} Hz, {}{}",
                o.channels,
                o.sample_rate,
                o.sample_format,
                mark(d.default_output)
            );
        }
    }
}

fn speaker(args: idn_open_protocol::cli::Args) {
    if !cfg!(target_os = "linux") {
        args.fail("speaker: Linux only; on Windows, send a virtual cable's playback side with `send --loopback --device`");
    }
    send_with(args, true)
}

fn send(args: idn_open_protocol::cli::Args) {
    send_with(args, false)
}

/// Removes a virtual device on Ctrl+C or a stop, before the process exits.
#[cfg(target_os = "linux")]
fn remove_on_stop(
    d: &Arc<crate::virtual_dev::VirtualDevice>,
) -> idn_open_protocol::shutdown::Registration {
    let d = d.clone();
    idn_open_protocol::shutdown::register(move || d.remove())
}

fn send_with(mut args: idn_open_protocol::cli::Args, speaker: bool) {
    let mut cfg = CaptureConfig::default();
    let (mut name, mut port, mut server, mut mdns, mut seconds) =
        (None::<String>, 0u16, None::<String>, None::<bool>, 0u64);
    let mut labels: Vec<String> = Vec::new();
    let (mut block_ms, mut low_latency) = (None::<u32>, false);
    let mut secure = idn_open_protocol::cli::SecureArgs::default();
    while let Some(a) = args.next() {
        if secure.take(&a, &mut args) {
            continue;
        }
        match a.as_str() {
            "--name" => name = Some(args.value("--name")),
            "--device" => cfg.device = Some(args.value("--device")),
            "--loopback" => cfg.loopback = true,
            "--app" => cfg.app = Some(args.value("--app")),
            "--channels" => cfg.channels = Some(args.value("--channels")),
            "--labels" => {
                labels = idn_open_protocol::node::routing::parse_labels(Some(
                    &args.value::<String>("--labels"),
                ))
            }
            "--rate" => cfg.sample_rate = args.value("--rate"),
            "--block-ms" => block_ms = Some(args.value("--block-ms")),
            "--device-buffer" => cfg.device_buffer = Some(args.value("--device-buffer")),
            "--low-latency" => low_latency = true,
            "--ultra-low" => block_ms = block_ms.or(Some(1)),
            "--no-correction" => cfg.max_correction_ppm = 0.0,
            "--port" => port = args.value("--port"),
            "--server" => server = Some(args.value("--server")),
            "--no-mdns" => mdns = Some(false),
            "--mdns" => mdns = Some(true),
            "--seconds" => seconds = args.value("--seconds"),
            "--help" | "-h" => args.help(),
            s => args.fail(&format!("unexpected argument: {s}")),
        }
    }
    // One program's sound: named after the program unless --name says otherwise.
    let from_app = cfg.app.as_deref().map(|a| {
        let stem = a.trim().trim_end_matches(".exe").trim_end_matches(".EXE");
        let mut c = stem.chars();
        c.next()
            .map(|f| f.to_uppercase().chain(c).collect::<String>())
            .unwrap_or_default()
    });
    // Where the sound comes from, for the matrix (idn-routing.md §A.16): a program's or a
    // sound card's.
    let origin_kind = if speaker || cfg.app.is_some() {
        "program"
    } else {
        "sound_card"
    };
    let name = name
        .or(from_app)
        .or_else(|| speaker.then(|| "IDN Speaker".to_owned()))
        .unwrap_or_else(|| args.fail("--name is required"));
    // A speaker programs play into (Linux): made now, recorded through ALSA.
    #[cfg(target_os = "linux")]
    let (_speaker, _remove) = if speaker {
        use crate::virtual_dev::{VirtualDevice, VirtualKind};
        let d = Arc::new(
            VirtualDevice::create(VirtualKind::Speaker, &name, cfg.channels.unwrap_or(2))
                .unwrap_or_else(|e| args.fail(&format!("speaker: {e}"))),
        );
        d.aim();
        cfg.device = Some(d.alsa_device.to_owned());
        cfg.channels = Some(cfg.channels.unwrap_or(2));
        println!(
            "{name:?}: programs play into it; what they play is sent as the IDN source {name:?}"
        );
        let r = remove_on_stop(&d);
        (Some(d), Some(r))
    } else {
        (None, None)
    };
    // One program's sound on Linux: its streams tapped into a capture sink of ours,
    // recorded like the speaker (spec/extensions/idn-node.md §N.14.6). No sound server:
    // status 5, so the bridges' supervisor doesn't start it again and again.
    #[cfg(target_os = "linux")]
    let (_tap, _tap_sink, _tap_remove) = if let Some(app) = cfg.app.take() {
        use crate::virtual_dev::{VirtualDevice, VirtualKind};
        let unable = |e: String| -> ! {
            eprintln!("idn-audio: one program's sound: {e}");
            std::process::exit(5)
        };
        let d = Arc::new(
            VirtualDevice::create(VirtualKind::Speaker, &format!("IDN capture: {name}"), 2)
                .unwrap_or_else(|e| unable(e.to_string())),
        );
        d.aim();
        cfg.device = Some(d.alsa_device.to_owned());
        cfg.channels = Some(2);
        let tap = crate::app_linux::AppTap::start(&app, &d.node, d.pipewire(), |s| println!("{s}"))
            .unwrap_or_else(|e| unable(e.to_string()));
        let r = remove_on_stop(&d);
        (Some(tap), Some(d), Some(r))
    } else {
        (None, None, None)
    };
    // Low latency (R.14 stage 2): 2 ms blocks, unless --block-ms says otherwise.
    cfg.block_ms = block_ms.unwrap_or(if low_latency { 2 } else { cfg.block_ms });
    let host = idn_open_protocol::discovery::hostname().to_uppercase();
    let secure_cfg = secure.config_with(
        &args,
        server.as_deref(),
        &format!("{host} ({name})"),
        idn_open_protocol::trust::DeviceRole::Output,
    );
    let p = Publisher::new(PublisherConfig {
        mdns,
        discovery_server: server,
        secure: secure_cfg,
        ..Default::default()
    })
    .unwrap_or_else(|e| args.fail(&format!("publisher: {e}")));
    let out = Arc::new(
        Output::new(
            &p,
            OutputConfig {
                port,
                audio_labels: labels,
                ..OutputConfig::new(&name)
            },
        )
        .unwrap_or_else(|e| args.fail(&format!("output: {e}"))),
    );
    let make = || {
        let o = out.clone();
        Capture::start(&cfg, move |block, timecode| {
            let _ = o.send_audio(block, Some(timecode));
        })
    };
    // One program's sound (Windows): while the program isn't running, this waits for it
    // (it used to exit for the supervisor to start it again every 5 s, the restarts
    // climbing in the panel).
    let waiting = |why: &dyn std::fmt::Display| {
        println!(
            "waiting for {} to play sound ({why})",
            cfg.app.as_deref().unwrap_or("?")
        )
    };
    let mut capture = match make() {
        Ok(c) => Some(c),
        Err(e) if cfg.app.is_some() => {
            waiting(&e);
            None
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1)
        }
    };
    let said = |capture: &Capture| {
        let st = capture.stats();
        // The device and its channels behind each of the source's (§A.16; beyond the
        // device's, silent: none).
        out.set_audio_origin(&idn_open_protocol::node::routing::AudioOrigin {
            kind: origin_kind.into(),
            device: st.device.clone(),
            channels: (1..=st.channels)
                .map(|k| {
                    if k <= st.device_channels {
                        format!("In {k}")
                    } else {
                        String::new()
                    }
                })
                .collect(),
        });
        println!(
            "capturing {:?} ({} ch {} Hz) -> \"{host} ({name})\" {} ch {} Hz on port {}{}",
            st.device,
            st.device_channels,
            st.device_rate,
            st.channels,
            st.sample_rate,
            out.port(),
            match st.channels.cmp(&st.device_channels) {
                std::cmp::Ordering::Greater => format!(
                    "; channels {}-{} are silent (the device has {})",
                    st.device_channels + 1,
                    st.channels,
                    st.device_channels
                ),
                std::cmp::Ordering::Less =>
                    format!("; the device's first {} channels", st.channels),
                std::cmp::Ordering::Equal => String::new(),
            }
        );
    };
    if let Some(c) = &capture {
        said(c);
    }
    // The program's state last, so it is the line a supervisor shows (the capture's line
    // came after "waiting for …" and hid it).
    #[cfg(target_os = "linux")]
    if let Some(t) = &_tap {
        println!("{}", t.status());
    }
    // Ctrl+C: the outputs' stop notices go out, then the process exits.
    idn_open_protocol::shutdown::install();
    let (start, mut next) = (Instant::now(), Instant::now() + Duration::from_secs(10));
    let mut retry = Instant::now() + Duration::from_secs(2);
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let done = seconds > 0 && start.elapsed() >= Duration::from_secs(seconds);
        if capture.is_none() && Instant::now() >= retry {
            if let Ok(c) = make() {
                said(&c);
                capture = Some(c);
            }
            retry = Instant::now() + Duration::from_secs(2);
        }
        if let Some(capture) = &capture
            && (Instant::now() >= next || done)
        {
            let st = capture.stats();
            println!(
                "{:4.0} s: {} frames in, {} blocks out, receivers {}, correction {:+.1} ppm, lead {:+.1} ms, errors {}, resyncs {}",
                start.elapsed().as_secs_f64(),
                st.frames_in,
                st.blocks_out,
                out.receivers(),
                st.correction_ppm,
                st.lead_ms,
                st.errors,
                st.resyncs
            );
            next += Duration::from_secs(10);
        }
        if done {
            break;
        }
        // The program whose sound this is has exited: waited for again.
        if let Some(why) = capture.as_ref().and_then(|c| c.stats().ended) {
            waiting(&why);
            capture = None;
            retry = Instant::now() + Duration::from_secs(2);
        }
    }
    finish(capture, out);
}

/// `recv --routable`: a receiver on the Node, playing whatever is routed to it
/// (spec/extensions/idn-routing.md §A.8).
fn recv_routable(
    name: &str,
    cfg: &PlaybackConfig,
    channels: Option<u16>,
    lossless: bool,
    seconds: u64,
    adaptive: bool,
) {
    use idn_open_protocol::node::routing::{AudioReceiver, Router, RouterConfig, RouterEvent};
    let fail = |m: String| -> ! {
        eprintln!("idn-audio: {m}");
        std::process::exit(1)
    };
    let node = Arc::new(
        idn_open_protocol::node::Node::connect("idn-audio")
            .unwrap_or_else(|e| fail(format!("a routable receiver needs the IDN Node: {e}"))),
    );
    let playback = Playback::start(cfg).unwrap_or_else(|e| fail(e.to_string()));
    let dev_ch = playback.device_channels();
    let n = channels.unwrap_or(dev_ch).clamp(1, dev_ch);
    let rx = Arc::new(
        AudioReceiver::register_as(
            &node,
            name,
            n,
            &[],
            idn_open_protocol::node::routing::ReceiverKind::SoundCard,
        )
        .unwrap_or_else(|e| fail(e.to_string())),
    );
    let st = playback.stats();
    println!(
        "receiver {name:?}: {n} of {:?}'s {} channels ({} Hz), {} ms buffer; routes: {}",
        st.device,
        dev_ch,
        st.device_rate,
        st.target_ms,
        rx.routes().len()
    );
    // One lane per transmitter, made as its sound first arrives.
    let lanes: Arc<std::sync::Mutex<std::collections::BTreeMap<String, crate::Lane>>> =
        Arc::default();
    let maker = playback.lanes();
    // Adapting: its need to the site and the site's floor for it (§A.14).
    if adaptive {
        rx.attach_mixer(&maker.mixer());
    }
    // The playback's own latency, unless the site sets the receiver's (§A.12).
    let own_ms = st.target_ms;
    let router = {
        let lanes = lanes.clone();
        Router::start(
            node.clone(),
            rx.clone(),
            RouterConfig { lossless },
            move |ev| match ev {
                RouterEvent::Block(b) => {
                    let mut l = lanes.lock_ok();
                    // Adapting (no latency asked for, R.30): from the path's start, in line
                    // with the other transmitters; the site's latency still wins.
                    let lane = l.entry(b.tx.to_owned()).or_insert_with(|| {
                        if adaptive {
                            maker.lane_adaptive(b.path.start_ms())
                        } else {
                            maker.lane()
                        }
                    });
                    if adaptive {
                        lane.follow_site(b.latency_ms);
                    } else {
                        lane.follow_latency(b.latency_ms.unwrap_or(own_ms));
                    }
                    let at = idn_open_protocol::audio::site_time(
                        b.audio.header.timestamp,
                        b.audio.header.metadata.as_deref(),
                    );
                    lane.push_timed(
                        b.audio.header.sample_rate,
                        &b.audio.channels,
                        Some(b.map),
                        at,
                    );
                }
                RouterEvent::Gone(tx) => {
                    lanes.lock_ok().remove(tx);
                }
            },
        )
        .unwrap_or_else(|e| fail(e.to_string()))
    };
    idn_open_protocol::shutdown::install();
    let (start, mut next) = (Instant::now(), Instant::now() + Duration::from_secs(10));
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let done = seconds > 0 && start.elapsed() >= Duration::from_secs(seconds);
        if Instant::now() >= next || done {
            let routes = rx.routes();
            println!(
                "{:4.0} s: {} route(s)",
                start.elapsed().as_secs_f64(),
                routes.len()
            );
            for (tx, lane) in lanes.lock_ok().iter() {
                let st = lane.stats();
                println!(
                    "      {tx}: {} blocks ({} ch {} Hz), {}, correction {:+.1} ppm, underruns {}, pauses {}, dropped {:.0} ms{}",
                    st.blocks_in,
                    st.source_channels,
                    st.source_rate,
                    level(&st),
                    st.correction_ppm,
                    st.underruns,
                    st.pauses,
                    st.dropped_ms,
                    through(&st),
                );
            }
            next += Duration::from_secs(10);
        }
        if done {
            break;
        }
    }
    drop(router);
    drop(lanes);
    drop(playback);
}

/// Ends the process once the stop notices are out: the audio system's thread may sit in
/// a blocking read on a device that has gone away, and the tool must not hang on it.
fn finish<A, B>(a: A, b: B)
where
    A: Send + 'static,
    B: Send + 'static,
{
    idn_open_protocol::shutdown::run_hooks();
    let t = std::thread::spawn(move || drop((a, b)));
    let deadline = Instant::now() + Duration::from_secs(3);
    while !t.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    std::process::exit(0);
}

/// `mic`: what the routing matrix sends "IDN Microphone", played into a virtual cable.
fn mic(mut args: idn_open_protocol::cli::Args) {
    let (mut name, mut device, mut latency, mut seconds) =
        ("IDN Microphone".to_owned(), None::<String>, 30u32, 0u64);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--name" => name = args.value("--name"),
            "--device" => device = Some(args.value("--device")),
            "--latency" => latency = args.value("--latency"),
            "--seconds" => seconds = args.value("--seconds"),
            "--help" | "-h" => args.help(),
            s => args.fail(&format!("unexpected argument: {s}")),
        }
    }
    let cables = crate::virtual_cables().unwrap_or_default();
    // Linux: a microphone of our own in the desktop's sound server, unless a device is named.
    #[cfg(target_os = "linux")]
    let (_mic, _remove) = if device.is_none() && cables.is_empty() {
        use crate::virtual_dev::{VirtualDevice, VirtualKind};
        match VirtualDevice::create(VirtualKind::Microphone, &name, 2) {
            Ok(d) => {
                let d = Arc::new(d);
                d.aim();
                device = Some(d.alsa_device.to_owned());
                println!("{name:?}: programs pick it as their microphone");
                let r = remove_on_stop(&d);
                (Some(d), Some(r))
            }
            Err(e) => {
                eprintln!("idn-audio: can't make the microphone: {e}");
                std::process::exit(1)
            }
        }
    } else {
        (None, None)
    };
    let (dev, record) = match device {
        Some(d) => {
            let rec = cables
                .iter()
                .find(|(c, _)| c.name.eq_ignore_ascii_case(&d) || c.id.eq_ignore_ascii_case(&d))
                .map(|(_, r)| r.clone());
            (d, rec.unwrap_or_else(|| "its other side".into()))
        }
        None => match cables.first() {
            Some((c, r)) => (c.id.clone(), r.clone()),
            None => {
                eprintln!(
                    "idn-audio: no virtual cable found. Install one (Windows 11: IDN's own, idn node driver install audio; else VB-Audio's VB-Cable, free: vb-audio.com/Cable), \
                     then run this again; or name a playback device with --device."
                );
                std::process::exit(1)
            }
        },
    };
    println!(
        "{name:?}: what is routed to it plays into {dev:?}; programs record it from {record:?}"
    );
    let cfg = PlaybackConfig {
        device: Some(dev),
        latency_ms: latency,
        ..Default::default()
    };
    recv_routable(&name, &cfg, Some(2), false, seconds, false);
}

fn recv(mut args: idn_open_protocol::cli::Args) {
    let mut cfg = PlaybackConfig::default();
    let (mut source, mut server, mut seconds, mut lossless) =
        (None::<String>, None::<String>, 0u64, true);
    let (mut routable, mut channels) = (None::<String>, None::<u16>);
    let (mut latency, mut low_latency, mut ultra) = (None::<u32>, false, false);
    let mut secure = idn_open_protocol::cli::SecureArgs::default();
    while let Some(a) = args.next() {
        if secure.take(&a, &mut args) {
            continue;
        }
        match a.as_str() {
            "--device" => cfg.device = Some(args.value("--device")),
            "--latency" => latency = Some(args.value("--latency")),
            "--low-latency" => low_latency = true,
            "--ultra-low" => ultra = true,
            "--device-buffer" => cfg.device_buffer = Some(args.value("--device-buffer")),
            "--no-correction" => cfg.max_correction_ppm = 0.0,
            "--server" => server = Some(args.value("--server")),
            // Lossless is the default; the flag stays for scripts.
            "--lossless-audio" => lossless = true,
            "--baseline-audio" => lossless = false,
            "--seconds" => seconds = args.value("--seconds"),
            "--routable" => routable = Some(args.value("--routable")),
            "--channels" => channels = Some(args.value("--channels")),
            "--help" | "-h" => args.help(),
            s if s.starts_with("--") => args.fail(&format!("unexpected argument: {s}")),
            s => source = Some(s.to_owned()),
        }
    }
    // No latency asked for: a routable receiver adapts each transmitter's lane to its
    // path (R.30), the device opened as for the low-latency 10 ms.
    let adaptive = latency.is_none() && !ultra && !low_latency;
    if adaptive && routable.is_some() {
        low_latency = true;
    }
    // Low latency (R.14 stage 2): a 10 ms buffer; ultra-low (§A.12): 2 ms and a small
    // device buffer; unless --latency / --device-buffer say otherwise.
    cfg.latency_ms = latency.unwrap_or(if ultra {
        2
    } else if low_latency {
        10
    } else {
        cfg.latency_ms
    });
    if ultra {
        cfg.device_buffer = cfg.device_buffer.or(Some(128));
    }
    if let Some(name) = routable {
        if source.is_some() {
            args.fail("--routable takes no SOURCE: the routes say what plays");
        }
        recv_routable(&name, &cfg, channels, lossless, seconds, adaptive);
        return;
    }
    let source = source.unwrap_or_else(|| args.fail("missing source"));
    let host = idn_open_protocol::discovery::hostname().to_uppercase();
    let secure_cfg = secure.config_with(
        &args,
        server.as_deref(),
        &format!("{host} (idn-audio)"),
        idn_open_protocol::trust::DeviceRole::Input,
    );
    let playback = Playback::start(&cfg).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1)
    });
    let input = Input::open(InputConfig {
        discovery_server: server,
        secure: secure_cfg,
        lossless_audio: lossless,
        ..InputConfig::new(&source)
    })
    .unwrap_or_else(|e| args.fail(&format!("input: {e}")));
    let st = playback.stats();
    println!(
        "playing {source:?} on {:?} ({} ch {} Hz), {} ms buffer",
        st.device, st.device_channels, st.device_rate, st.target_ms
    );
    idn_open_protocol::shutdown::install();
    let (start, mut next) = (Instant::now(), Instant::now() + Duration::from_secs(10));
    loop {
        match input.recv(Duration::from_millis(100)) {
            Some(InputEvent::Audio(a)) => playback.push_timed(
                a.header.sample_rate,
                &a.channels,
                idn_open_protocol::audio::site_time(
                    a.header.timestamp,
                    a.header.metadata.as_deref(),
                ),
            ),
            Some(InputEvent::State(s)) => {
                let st = input.stats();
                println!(
                    "{s:?}; encrypted: {}",
                    match (st.link_mode, st.encrypted, st.site_trust) {
                        (Some("open"), _, _) => "yes (IDN link, not authenticated)",
                        (Some("s1"), _, _) => "yes (IDN link, secure/1 passphrase)",
                        (Some("pair"), _, _) => "yes (IDN link, linked computers)",
                        (Some(_), _, _) => "yes (IDN link, secure/2 site trust)",
                        (None, true, _) if st.linked => "yes (pair/1, linked computers)",
                        (None, true, true) => "yes (secure/2, site trust)",
                        (None, true, false) => "yes (secure/1, passphrase)",
                        _ => "no",
                    }
                );
            }
            Some(InputEvent::Error(e)) => println!("error: {e}"),
            _ => {}
        }
        let done = seconds > 0 && start.elapsed() >= Duration::from_secs(seconds);
        if Instant::now() >= next || done {
            let st = playback.stats();
            println!(
                "{:4.0} s: {} blocks in ({} ch {} Hz), {} frames out (device block {}), {}, correction {:+.1} ppm, underruns {}, pauses {}, dropped {:.0} ms, errors {}, streams {}{}",
                start.elapsed().as_secs_f64(),
                st.blocks_in,
                st.source_channels,
                st.source_rate,
                st.frames_out,
                st.device_block,
                level(&st),
                st.correction_ppm,
                st.underruns,
                st.pauses,
                st.dropped_ms,
                st.errors,
                st.streams,
                through(&st),
            );
            next += Duration::from_secs(10);
        }
        if done {
            break;
        }
    }
    finish(playback, input);
}

/// A lane's level as the status lines say it. By site time the sound plays at its own
/// time plus the target, so what it holds is the target less how late it arrived, not
/// the level it holds otherwise ("35/63" read as a buffer far off target).
fn level(st: &idn_open_protocol::audio::LaneStats) -> String {
    if st.by_time {
        format!(
            "playing by site time at its time + {:.0} ms (late {:+.2} ms), {:.0} ms held",
            st.target_ms as f64 + st.margin_ms,
            st.late_ms,
            st.fill_ms
        )
    } else {
        format!(
            "buffer {:.0}/{:.0} ms ({} + {:.0} after underruns + half a {:.0} ms block)",
            st.fill_ms, st.hold_ms, st.target_ms, st.margin_ms, st.source_block_ms
        )
    }
}

/// Passing through (`IDN_LANE_THROUGH`), for the end of a status line: `, through, fixed
/// 0 ms` or `, filtering (drift 12.0 ppm)`; nothing when there is nothing to say.
fn through(st: &idn_open_protocol::audio::LaneStats) -> String {
    let t = st.through_line();
    if t.is_empty() { t } else { format!(", {t}") }
}

/// `aes67 list|recv|send` (spec/extensions/idn-aes67.md).
fn aes67(mut args: idn_open_protocol::cli::Args) {
    use crate::aes67::{
        Aes67Receiver, Aes67Sender, RecvConfig, SapListener, Sdp, SendConfig, local_address,
    };
    let cmd = args
        .next()
        .unwrap_or_else(|| args.fail("aes67: list, recv or send"));
    let (mut what, mut name, mut seconds, mut interface) = (
        None::<String>,
        None::<String>,
        0u64,
        None::<std::net::Ipv4Addr>,
    );
    let (mut jitter, mut low_latency, mut channels, mut address, mut port) =
        (4u32, false, 2u16, None, crate::aes67::RTP_PORT);
    let (mut ttl, mut latency, mut sdp_file, mut sap, mut no_node) =
        (32u32, 20u32, None::<String>, true, false);
    let mut tsv = false;
    let mut rate = None::<u32>;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--name" => name = Some(args.value("--name")),
            "--seconds" => seconds = args.value("--seconds"),
            // A stream sent at this rate (§N.40); default the machine's.
            "--rate" => {
                let v: String = args.value("--rate");
                rate = Some(
                    idn_open_protocol::node::rate::parse(&v)
                        .unwrap_or_else(|| args.fail("--rate: 44100, 48000 or 96000")),
                );
            }
            "--interface" => interface = Some(args.value("--interface")),
            "--jitter-ms" => jitter = args.value("--jitter-ms"),
            "--low-latency" => low_latency = true,
            "--channels" => channels = args.value("--channels"),
            "--address" => address = Some(args.value("--address")),
            "--port" => port = args.value("--port"),
            "--ttl" => ttl = args.value("--ttl"),
            "--latency" => latency = args.value("--latency"),
            "--sdp-file" => sdp_file = Some(args.value("--sdp-file")),
            "--no-sap" => sap = false,
            "--no-node" => no_node = true,
            "--tsv" => tsv = true,
            "--help" | "-h" => args.help(),
            s if s.starts_with("--") => args.fail(&format!("unexpected argument: {s}")),
            s => what = Some(s.to_owned()),
        }
    }
    let fail = |m: String| -> ! {
        eprintln!("idn-audio aes67: {m}");
        std::process::exit(1)
    };
    // The machine's IDN Node: its site identity (a site's sources only talk to members),
    // its machine name and interface, and the site clock (PTP) the streams are timed by.
    let node = (!no_node)
        .then(|| idn_open_protocol::node::Node::connect("idn-audio aes67").ok())
        .flatten();
    if node.is_some() {
        std::thread::sleep(Duration::from_millis(300)); // its <clock> arrives as it connects
    }
    let machine = node.as_ref().and_then(|n| n.enrollment().node_machine());
    let interface = interface.or(machine.as_ref().and_then(|m| m.interface));
    let iface = local_address(interface);
    let secure = node.as_ref().map(|n| n.secure_config());
    let host = machine
        .and_then(|m| m.name)
        .unwrap_or_else(idn_open_protocol::discovery::hostname)
        .to_uppercase();
    idn_open_protocol::shutdown::install();
    let start = Instant::now();
    let done = |seconds: u64| seconds > 0 && start.elapsed() >= Duration::from_secs(seconds);
    match cmd.as_str() {
        "list" => {
            let l = SapListener::start(iface)
                .unwrap_or_else(|e| fail(format!("listening for SAP on {iface}: {e}")));
            let secs = if seconds > 0 { seconds } else { 35 };
            if !tsv {
                println!("listening for AES67 streams (SAP) on {iface} for {secs} s ...");
            }
            while !done(secs) {
                std::thread::sleep(Duration::from_millis(200));
            }
            let streams = l.streams();
            if tsv {
                for a in streams {
                    let d = &a.sdp;
                    let clean = |t: &str| t.replace(['\t', '\n', '\r'], " ");
                    println!(
                        "{}\t{}:{}\t{}\t{}\t{}\t{}",
                        clean(&d.name),
                        d.group,
                        d.port,
                        d.channels,
                        d.encoding.name(),
                        d.rate,
                        a.from
                    );
                }
                return;
            }
            if streams.is_empty() {
                println!("none heard");
            }
            for a in streams {
                let d = &a.sdp;
                println!(
                    "{:?}: {}:{} {} ch {} {} Hz, {} ms packets, from {}, clock {}",
                    d.name,
                    d.group,
                    d.port,
                    d.channels,
                    d.encoding.name(),
                    d.rate,
                    d.ptime_us as f64 / 1000.0,
                    a.from,
                    d.ptp
                        .as_ref()
                        .map_or("not PTP".to_owned(), |(gm, dom)| format!(
                            "PTP {gm} domain {dom}"
                        ))
                );
            }
        }
        "recv" => {
            let what =
                what.unwrap_or_else(|| args.fail("aes67 recv: a stream's name or an SDP file"));
            // By name: the listener stays, so a stream announced again with another clock
            // (its grandmaster changed) is followed.
            let (sdp, listener) = match std::fs::read_to_string(&what) {
                Ok(text) => (
                    Sdp::parse(&text).unwrap_or_else(|e| fail(format!("{what}: {e}"))),
                    None,
                ),
                Err(_) => {
                    // A name: from the announcements (they come every 30 s).
                    let l = SapListener::start(iface)
                        .unwrap_or_else(|e| fail(format!("listening for SAP: {e}")));
                    println!("looking for {what:?} in the announcements (up to 35 s) ...");
                    let until = Instant::now() + Duration::from_secs(35);
                    loop {
                        if let Some(a) = l.newest(&what) {
                            break (a.sdp, Some(l));
                        }
                        if Instant::now() > until {
                            fail(format!(
                                "no stream {what:?} announced (and no such SDP file)"
                            ));
                        }
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            };
            let name = name.unwrap_or_else(|| sdp.name.clone());
            let p = Publisher::new(PublisherConfig {
                secure: secure.clone(),
                ..Default::default()
            })
            .unwrap_or_else(|e| fail(format!("publisher: {e}")));
            let out = Arc::new(
                Output::new(&p, OutputConfig::new(&name))
                    .unwrap_or_else(|e| fail(format!("output: {e}"))),
            );
            let media_of = |d: &Sdp| {
                (
                    d.group,
                    d.port,
                    d.payload,
                    d.encoding.name(),
                    d.rate,
                    d.channels,
                    d.ptime_us,
                )
            };
            let mut sdp = sdp;
            let rx = loop {
                let o = out.clone();
                let cfg = RecvConfig {
                    sdp: sdp.clone(),
                    interface,
                    jitter_ms: jitter,
                    block_ms: if low_latency { 2 } else { 10 },
                };
                let rx = Aes67Receiver::start(&cfg, move |block, ts| {
                    let _ = o.send_audio_at(block, None, ts);
                })
                .unwrap_or_else(|e| fail(format!("receiving {}:{}: {e}", sdp.group, sdp.port)));
                println!(
                    "receiving {:?} ({}:{} {} ch {} {} Hz, {} ms packets) as \"{host} ({name})\"",
                    sdp.name,
                    sdp.group,
                    sdp.port,
                    sdp.channels,
                    sdp.encoding.name(),
                    sdp.rate,
                    sdp.ptime_us as f64 / 1000.0
                );
                let (mut clock, mut clock_at) = (sdp.ptp.clone(), Instant::now());
                let mut next = Instant::now() + Duration::from_secs(10);
                let mut changed = None;
                loop {
                    std::thread::sleep(Duration::from_millis(100));
                    // The stream's clock as announced now, once a second.
                    if clock_at.elapsed() >= Duration::from_secs(1) {
                        clock_at = Instant::now();
                        // Announced again with other media (group, port, format): the
                        // receiver starts over on it; it went on with the old one, silent
                        // (deep bug run 2026-11-09).
                        if let Some(a) = listener.as_ref().and_then(|l| l.newest(&sdp.name))
                            && media_of(&a.sdp) != media_of(&sdp)
                        {
                            changed = Some(a.sdp);
                            break;
                        }
                        if let Some(a) = listener.as_ref().and_then(|l| l.newest(&sdp.name))
                            && a.sdp.ptp != clock
                        {
                            println!(
                                "{:?} is announced with another clock: {}",
                                sdp.name,
                                a.sdp
                                    .ptp
                                    .as_ref()
                                    .map_or("not PTP".to_owned(), |(gm, d)| format!(
                                        "PTP {gm} domain {d}"
                                    ))
                            );
                            clock = a.sdp.ptp.clone();
                            rx.set_clock(clock.clone());
                        }
                    }
                    if Instant::now() >= next || done(seconds) {
                        let st = rx.stats();
                        println!(
                            "{:4.0} s: {} packets, {} lost, {} late, {} foreign, {} blocks out, receivers {}, timestamps {}",
                            start.elapsed().as_secs_f64(),
                            st.packets,
                            st.lost,
                            st.late,
                            st.foreign,
                            st.blocks,
                            out.receivers(),
                            if st.ptp {
                                "from the media clock (the site's PTP)"
                            } else {
                                "from the stream's own clock"
                            }
                        );
                        next += Duration::from_secs(10);
                    }
                    if done(seconds) {
                        break;
                    }
                }
                match changed {
                    Some(n) => {
                        println!(
                            "{:?} is announced again with other media: receiving that",
                            sdp.name
                        );
                        drop(rx);
                        sdp = n;
                    }
                    None => break rx,
                }
            };
            finish(rx, out);
        }
        "send" => {
            let source = what.unwrap_or_else(|| args.fail("aes67 send: an IDN source"));
            let name = name.unwrap_or_else(|| args.fail("aes67 send: --name is required"));
            let input = Input::open(InputConfig {
                secure: secure.clone(),
                ..InputConfig::new(&source)
            })
            .unwrap_or_else(|e| fail(format!("input: {e}")));
            // The site clock still settling (a Node just started: listening, a slave not
            // locked yet): waited for, up to 15 s, so the stream is announced on it.
            // Announced on this machine's clock, receivers never played it by site time
            // (after a Node restart).
            let settling = |c: &idn_open_protocol::node::clock::ClockInfo| {
                matches!(c.state.as_str(), "listening" | "slave") && !c.usable()
            };
            if idn_open_protocol::node::clock::status().is_some_and(|c| settling(&c)) {
                println!("waiting for the site clock to settle (up to 15 s) ...");
                let until = Instant::now() + Duration::from_secs(15);
                while Instant::now() < until
                    && idn_open_protocol::node::clock::status().is_some_and(|c| settling(&c))
                {
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
            let cfg = SendConfig {
                group: address,
                port,
                channels,
                interface,
                ttl,
                latency_ms: latency,
                announce: sap,
                // 48 kHz unless asked (AES67's rate; bug run N7-3: it followed the machine's).
                rate: rate.unwrap_or(idn_open_protocol::node::rate::DEFAULT),
                ..SendConfig::new(&name)
            };
            let mut tx = Aes67Sender::start(&cfg).unwrap_or_else(|e| fail(format!("sending: {e}")));
            let mut clock_at = Instant::now();
            let text = tx.sdp().to_text();
            if let Some(f) = &sdp_file {
                std::fs::write(f, &text).unwrap_or_else(|e| fail(format!("{f}: {e}")));
            }
            println!(
                "sending {source:?} as the AES67 stream {name:?}{}:\n{}",
                if sap { " (announced)" } else { "" },
                text.trim_end()
            );
            let mut next = Instant::now() + Duration::from_secs(10);
            loop {
                match input.recv(Duration::from_millis(100)) {
                    Some(InputEvent::Audio(a)) => {
                        let block = idn_open_protocol::api::AudioBlock {
                            sample_rate: a.header.sample_rate,
                            channels: a.channels,
                        };
                        tx.push(
                            &block,
                            idn_open_protocol::audio::site_time(
                                a.header.timestamp,
                                a.header.metadata.as_deref(),
                            ),
                        );
                    }
                    // Said, so a source that refuses this machine isn't silence unexplained.
                    Some(InputEvent::State(st)) => println!("{source:?}: {st:?}"),
                    Some(InputEvent::Error(e)) => println!("{source:?}: {e}"),
                    _ => {}
                }
                // The site clock's grandmaster, once a second: changed, announced again.
                if clock_at.elapsed() >= Duration::from_secs(1) {
                    clock_at = Instant::now();
                    let version = tx.sdp().version;
                    if let Some(gm) = tx.follow_clock() {
                        println!(
                            "the site clock's grandmaster is now {gm}: the stream is announced with it"
                        );
                    }
                    // Its description changed (a new grandmaster, or none: local): the SDP
                    // file too, for receivers that read it rather than SAP.
                    if tx.sdp().version != version {
                        if tx.sdp().ptp.is_none() {
                            println!(
                                "no PTP clock for 10 s: the stream is announced as timed by this machine (ts-refclk:local)"
                            );
                        }
                        if let Some(f) = &sdp_file
                            && let Err(e) = std::fs::write(f, tx.sdp().to_text())
                        {
                            println!("{f}: {e}");
                        }
                    }
                }
                if Instant::now() >= next || done(seconds) {
                    let (st, l) = (tx.stats(), tx.lane_stats());
                    println!(
                        "{:4.0} s: {} packets, {} skips, timed by {}; {}, correction {:+.1} ppm, underruns {}, pauses {}",
                        start.elapsed().as_secs_f64(),
                        st.packets,
                        st.skips,
                        if st.ptp {
                            "the site clock (PTP)"
                        } else {
                            "this machine's clock"
                        },
                        level(&l),
                        l.correction_ppm,
                        l.underruns,
                        l.pauses,
                    );
                    next += Duration::from_secs(10);
                }
                if done(seconds) {
                    break;
                }
            }
            finish(tx, input);
        }
        other => args.fail(&format!("aes67: unknown command {other}")),
    }
}

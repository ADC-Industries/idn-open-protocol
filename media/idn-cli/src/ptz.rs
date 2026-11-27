//! Sends camera control and recording commands to a source (spec §1.7), connecting as
//! a metadata-only receiver.
//!
//! Options: `idn-ptz --help`.

use idn_open_protocol::control::{self, parse_element, ptz};
use idn_open_protocol::frame::{Frame, Limits, TIMECODE_SYNTHESIZE, kind, read_frame};
use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

pub fn main() -> std::io::Result<()> {
    idn_open_protocol::log::set_logger(idn_open_protocol::log::print_logger);
    let mut args = idn_open_protocol::cli::Args::new(
        "usage: idn-ptz <ip[:port] | \"HOST (source)\"> [--server HOST[:PORT]] [--name NAME] COMMAND...\n\
         commands: zoom=Z zoom-speed=V pan-tilt=P,T pan-tilt-speed=P,T store=N recall=N[,S] focus=auto|D\n\
         \x20         focus-speed=V wb=auto|indoor|outdoor|one_shot|R,B exposure=auto|E|I,G,S\n\
         \x20         record-start=HINT record-level=DB|-inf record-stop wait=SECONDS",
    );
    let (mut target, mut server, mut name, mut cmds) =
        (None::<String>, None::<String>, "ptz".to_owned(), Vec::new());
    while let Some(a) = args.next() {
        match a.as_str() {
            "--server" => server = Some(args.value("--server")),
            "--name" => name = args.value("--name"),
            "-h" | "--help" => args.help(),
            s if target.is_none() && !s.starts_with('-') => target = Some(s.into()),
            s if !s.starts_with('-') => cmds.push(s.to_owned()),
            s => args.fail(&format!("unexpected argument: {s}")),
        }
    }
    let Some(target) = target else {
        args.fail("missing source")
    };
    if cmds.is_empty() {
        args.fail("no command given");
    }
    let f = |v: &str| -> f64 {
        v.parse()
            .unwrap_or_else(|_| args.fail(&format!("bad number: {v}")))
    };
    let mut msgs: Vec<Result<String, f64>> = Vec::new(); // Err = wait seconds
    for c in &cmds {
        let (k, v) = c.split_once('=').unwrap_or((c, ""));
        let pair = |v: &str| -> (f64, f64) {
            let (a, b) = v
                .split_once(',')
                .unwrap_or_else(|| args.fail(&format!("{k} needs two values")));
            (f(a), f(b))
        };
        msgs.push(Ok(match k {
            "zoom" => ptz::zoom(f(v)),
            "zoom-speed" => ptz::zoom_speed(f(v)),
            "pan-tilt" => {
                let (p, t) = pair(v);
                ptz::pan_tilt(p, t)
            }
            "pan-tilt-speed" => {
                let (p, t) = pair(v);
                ptz::pan_tilt_speed(p, t)
            }
            "store" => ptz::store_preset(f(v) as u32),
            "recall" => match v.split_once(',') {
                Some((i, s)) => ptz::recall_preset(f(i) as u32, f(s)),
                None => ptz::recall_preset(f(v) as u32, 1.0),
            },
            "focus" if v == "auto" => ptz::focus_auto(),
            "focus" => ptz::focus_manual(f(v)),
            "focus-speed" => ptz::focus_speed(f(v)),
            "wb" if ["auto", "indoor", "outdoor", "one_shot"].contains(&v) => ptz::white_balance(v),
            "wb" => {
                let (r, b) = pair(v);
                ptz::white_balance_manual(r, b)
            }
            "exposure" if v == "auto" => ptz::exposure_auto(),
            "exposure" if v.contains(',') => {
                let p: Vec<f64> = v.split(',').map(f).collect();
                if p.len() != 3 {
                    args.fail("exposure=I,G,S needs three values")
                }
                ptz::exposure_manual_v2(p[0], p[1], p[2])
            }
            "exposure" => ptz::exposure_manual(f(v)),
            "record-start" => ptz::record_start(v),
            "record-level" if v == "-inf" => ptz::record_set_level(f64::NEG_INFINITY),
            "record-level" => ptz::record_set_level(f(v)),
            "record-stop" => ptz::record_stop(),
            "wait" => {
                // Checked now: a bad wait used to panic half-way through the sequence,
                // leaving a camera panning (TL-3).
                let secs = f(v);
                if !(secs.is_finite() && (0.0..=86_400.0).contains(&secs)) {
                    args.fail(&format!("wait={v}: use 0 to 86400 seconds"));
                }
                msgs.push(Err(secs));
                continue;
            }
            _ => args.fail(&format!("unknown command: {c}")),
        }));
    }

    let addr = match target.parse::<std::net::IpAddr>() {
        Ok(ip) => std::net::SocketAddr::new(ip, 5961),
        Err(_) => match target.to_socket_addrs().ok().and_then(|mut i| i.next()) {
            Some(a) => a,
            None => {
                let src = match &server {
                    Some(sv) => idn_open_protocol::discovery::ds::query_sources(
                        idn_open_protocol::discovery::ds::server_addr(sv)?,
                        Duration::from_secs(5),
                    )?
                    .into_iter()
                    .find(|s| s.name.eq_ignore_ascii_case(&target)),
                    None => idn_open_protocol::discovery::find(
                        &target,
                        Duration::from_secs(5),
                        &Default::default(),
                    )?,
                };
                src.and_then(|s| s.socket_addr()).ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("source \"{target}\" not found"),
                    )
                })?
            }
        },
    };
    let host = idn_open_protocol::discovery::hostname();
    let send = |s: &mut TcpStream, k: u16, xml: &str| -> std::io::Result<()> {
        println!("-> kind {k}: {xml}");
        let tc = if k == kind::METADATA {
            TIMECODE_SYNTHESIZE
        } else {
            0
        };
        s.write_all(&Frame::text(k, tc, xml).encode())
    };
    // A metadata-only receiver (§1.7): no ndi_video, no media, name suffix " M".
    let open = |addr: std::net::SocketAddr| -> std::io::Result<TcpStream> {
        let mut sock = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
        send(&mut sock, kind::CONTROL, &control::ndi_version())?;
        send(
            &mut sock,
            kind::CONTROL,
            &control::ndi_identify(&format!("{host} ({name} M)")),
        )?;
        send(
            &mut sock,
            kind::CONTROL,
            &control::EnabledStreams {
                text: true,
                ..Default::default()
            }
            .to_xml(),
        )?;
        send(&mut sock, kind::CONTROL, &control::ndi_tally(false, false))?;
        Ok(sock)
    };
    let mut sock = open(addr)?;
    // Show what the source says (its capabilities in particular) for a moment: at most
    // 700 ms in all, however chatty it is. A routing source (§1.8) names the camera to
    // talk to instead (R1-128).
    let mut hops = 0;
    'show: loop {
        let mut rx = sock.try_clone()?;
        let until = std::time::Instant::now() + Duration::from_millis(700);
        loop {
            let left = until.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break 'show;
            }
            rx.set_read_timeout(Some(left))?;
            let Ok(Some(fr)) = read_frame(&mut rx, &Limits::default()) else {
                break 'show;
            };
            let Ok(x) = fr.text_payload() else { continue };
            println!("<- kind {}: {x}", fr.kind);
            let Some(e) = parse_element(x) else { continue };
            if e.name == "ndi_capabilities" && !e.attrs.keys().any(|k| k.starts_with("ntk_")) {
                println!("   (source advertises no ntk_* capabilities)");
            }
            if e.name == "ndi_routing"
                && hops < 3
                && let Some(t) = e.attr("ip").and_then(|a| a.parse().ok())
            {
                println!(
                    "   routed to {} at {t}: commands go there",
                    e.attr("name").unwrap_or("?")
                );
                hops += 1;
                sock = open(t)?;
                continue 'show;
            }
        }
    }
    for m in msgs {
        match m {
            Ok(x) => send(&mut sock, kind::METADATA, &x)?,
            Err(secs) => std::thread::sleep(Duration::try_from_secs_f64(secs).unwrap_or_default()),
        }
    }
    std::thread::sleep(Duration::from_millis(300));
    Ok(())
}
